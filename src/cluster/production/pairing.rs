use super::{RoleControl, now_millis};
use crate::{
    cluster::{
        ClusterEvent, ClusterEventKind, ControlCommand, ControlMessage, ControlMode,
        DistributedControlPhase, Ds4Hello, NodeDescriptor, RendezvousControlSnapshot,
        RendezvousListener, WorkerHelloExpectation,
    },
    target::{ClusterState, LocalRole, StableMode},
};
use anyhow::{Context, ensure};
use std::{net::SocketAddr, sync::Arc, time::Duration};

impl super::ProductionClusterRuntime {
    pub async fn pair(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "pairing must be initiated by the coordinator"
        );
        let message = ControlMessage {
            request_id: uuid::Uuid::new_v4().to_string(),
            generation: self.control_generation().await,
            deployment_id: self.inner.descriptor.deployment_id.clone(),
            command: ControlCommand::Pair {
                descriptor: self.local_descriptor().await,
            },
        };
        let response = self.inner.client.send(&message).await?;
        self.inner.lease.update(&response);
        tokio::time::sleep(self.inner.config.cluster.policy.required_peer_stability).await;
        self.reconcile_peer().await
    }

    pub async fn promote(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "only the coordinator may promote"
        );
        let current = self.inner.mode.snapshot();
        ensure!(
            current.state == ClusterState::PairedStandaloneReady,
            "cluster is not paired standalone"
        );
        let awaiting = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::BeginPromotion,
            })
            .await?;
        let hello = match self.prepare_and_accept_hello(awaiting).await {
            Ok(hello) => hello,
            Err(error) => {
                self.recover_preflight_promotion().await;
                return Err(error);
            }
        };
        let prerequisites = {
            let RoleControl::Coordinator(control) = &self.inner.control else {
                unreachable!()
            };
            let control = control.lock().await;
            control.phase() == DistributedControlPhase::WorkerReady
                && control.peer_present(now_millis())
        };
        let lease = self.inner.lease.clone();
        let snapshot = self
            .inner
            .coordinator_runtime
            .get()
            .context("coordinator runtime unavailable")?
            .promote_validated(
                hello,
                prerequisites,
                Arc::new(move || lease.valid()),
                now_millis(),
            )
            .await?;
        let ready = {
            let RoleControl::Coordinator(control) = &self.inner.control else {
                unreachable!()
            };
            control
                .lock()
                .await
                .distributed_ready_message(uuid::Uuid::new_v4().to_string())?
        };
        self.inner.client.send(&ready).await?;
        if self.inner.config.cluster.policy.auto_demote {
            let coordinator = self
                .inner
                .coordinator_runtime
                .get()
                .context("coordinator runtime unavailable")?
                .clone();
            tokio::spawn(async move {
                if let Err(error) = coordinator.wait_route_loss_and_demote().await {
                    tracing::error!(error = %error, "automatic distributed demotion failed");
                }
            });
        }
        Ok(snapshot)
    }

    pub async fn demote(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "only the coordinator may demote"
        );
        self.inner
            .coordinator_runtime
            .get()
            .context("coordinator runtime unavailable")?
            .demote()
            .await
            .map_err(Into::into)
    }

    async fn prepare_and_accept_hello(
        &self,
        awaiting: crate::cluster::ClusterSnapshot,
    ) -> anyhow::Result<Ds4Hello> {
        let control_snapshot = RendezvousControlSnapshot {
            state: awaiting.state,
            generation: awaiting.generation,
            deployment_id: self.inner.descriptor.deployment_id.clone(),
            lease_valid: self.inner.lease.valid(),
        };
        let worker_start = self
            .inner
            .config
            .ds4
            .mxfp4
            .worker_layers
            .split_once(':')
            .context("invalid worker layer range")?
            .0
            .parse::<u32>()?;
        let listener = RendezvousListener::bind(
            SocketAddr::new(
                self.inner.local_address,
                self.inner.config.cluster.ds4_distributed_port,
            ),
            WorkerHelloExpectation {
                coordinator_address: self.inner.local_address,
                worker_address: self.inner.peer_address,
                control: control_snapshot.clone(),
                layer_start: worker_start,
                layer_end: u32::MAX,
                has_output: true,
                context_size: self.inner.config.ds4.mxfp4.context_size,
                model_name: self.inner.manifest.model_family.clone(),
            },
        )
        .await?;
        let prepare = {
            let RoleControl::Coordinator(control) = &self.inner.control else {
                unreachable!()
            };
            let mut control = control.lock().await;
            let message = control.prepare_worker_message(uuid::Uuid::new_v4().to_string())?;
            control.note_prepare_sent(message.generation)?;
            message
        };
        let response = self.inner.client.send(&prepare).await?;
        self.inner.lease.update(&response);
        let runtime = self.clone();
        let hello = listener
            .accept_one(
                self.inner.config.cluster.timeouts.rendezvous_hello,
                move || RendezvousControlSnapshot {
                    state: runtime.inner.mode.snapshot().state,
                    generation: runtime.inner.mode.snapshot().generation,
                    deployment_id: runtime.inner.descriptor.deployment_id.clone(),
                    lease_valid: runtime.inner.lease.valid(),
                },
            )
            .await?;
        let deadline =
            tokio::time::Instant::now() + self.inner.config.cluster.timeouts.worker_startup;
        loop {
            let ready = {
                let RoleControl::Coordinator(control) = &self.inner.control else {
                    unreachable!()
                };
                control.lock().await.phase() == DistributedControlPhase::WorkerReady
            };
            if ready {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "worker Ready timed out"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(hello)
    }

    async fn recover_preflight_promotion(&self) {
        let cancel = match &self.inner.control {
            RoleControl::Coordinator(control) => control
                .lock()
                .await
                .cancel_generation_message(uuid::Uuid::new_v4().to_string())
                .ok(),
            RoleControl::Worker(_) => None,
        };
        if let Some(cancel) = cancel
            && let Err(error) = self.inner.client.send(&cancel).await
        {
            tracing::warn!(error = %error, "worker promotion cancellation failed");
        }
        let current = self.inner.mode.snapshot();
        if matches!(
            current.state,
            ClusterState::AwaitingWorkerHello
                | ClusterState::Promoting
                | ClusterState::DistributedStarting
        ) && let Ok(paired) = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::PromotionFailed,
            })
            .await
        {
            self.inner.proxy.set_target(paired.target, true);
            self.inner.proxy.admission().start_serving();
        }
    }

    pub(super) async fn control_generation(&self) -> u64 {
        match &self.inner.control {
            RoleControl::Coordinator(control) => control
                .lock()
                .await
                .peer_lease()
                .descriptor()
                .map_or(self.inner.descriptor.generation, |d| d.generation),
            RoleControl::Worker(control) => control
                .lock()
                .await
                .peer_lease()
                .descriptor()
                .map_or(self.inner.descriptor.generation, |d| d.generation),
        }
    }

    pub(super) async fn local_descriptor(&self) -> NodeDescriptor {
        let mut descriptor = self.inner.descriptor.clone();
        descriptor.generation = self.control_generation().await;
        descriptor.mode = match self.inner.mode.snapshot().stable_mode {
            StableMode::SoloStandalone => ControlMode::SoloStandalone,
            StableMode::PairedStandalone => ControlMode::PairedStandalone,
            StableMode::DistributedMxfp4 => ControlMode::DistributedMxfp4,
        };
        descriptor
    }
}
