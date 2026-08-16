#[cfg(feature = "test-support")]
use super::PairTiming;
use super::{RoleControl, now_millis};
use crate::{
    cluster::{
        ClusterEvent, ClusterEventKind, ClusterFailure, ControlCommand, ControlError,
        ControlMessage, ControlMode, DistributedControlPhase, Ds4Hello, Ds4HelloError, EventOwner,
        NodeDescriptor, RendezvousControlSnapshot, RendezvousListener, WorkerHelloExpectation,
    },
    target::{ClusterState, LocalRole, StableMode},
};
use anyhow::{Context, ensure};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use thiserror::Error;

/// Errors raised while accepting the worker HELLO and waiting for the worker control plane
/// to become Ready. Only the timeout variants are retried with backoff; all other preflight
/// rejections stay Paired Standalone (spec.md §31).
#[derive(Debug, Error)]
enum PromotionHelloError {
    #[error(transparent)]
    Rendezvous(#[from] Ds4HelloError),
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("promotion preflight control failed: {0}")]
    Preflight(#[source] anyhow::Error),
    #[error("worker did not become Ready before the startup deadline")]
    WorkerStartupTimeout,
}

impl From<anyhow::Error> for PromotionHelloError {
    fn from(error: anyhow::Error) -> Self {
        PromotionHelloError::Preflight(error)
    }
}

impl super::ProductionClusterRuntime {
    pub async fn pair(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "pairing must be initiated by the coordinator"
        );
        // Offer/confirm negotiation (design §4): the coordinator, as session authority, first
        // learns the worker's control session generation from /v1/node, computes a candidate
        // that is no lower than either side, and advances its own generation before offering so
        // a higher worker generation converges direction-independently.
        let peer = self.inner.client.node().await?;
        let candidate = match &self.inner.control {
            RoleControl::Coordinator(control) => {
                let mut control = control.lock().await;
                control.propose_candidate(peer.generation)?
            }
            RoleControl::Worker(_) => {
                unreachable!("pairing must be initiated by the coordinator")
            }
        };
        let message = ControlMessage {
            request_id: uuid::Uuid::new_v4().to_string(),
            generation: candidate,
            deployment_id: self.inner.descriptor.deployment_id.clone(),
            command: ControlCommand::Pair {
                descriptor: self.local_descriptor().await,
            },
        };
        #[cfg(feature = "test-support")]
        let offer_sent_at = now_millis();
        let response = self.inner.client.send(&message).await?;
        #[cfg(feature = "test-support")]
        let confirm_received_at = now_millis();
        self.inner.lease.update(&response);
        #[cfg(feature = "test-support")]
        let lease_established_at = now_millis();
        tokio::time::sleep(self.inner.config.cluster.policy.required_peer_stability).await;
        #[cfg(feature = "test-support")]
        let stability_achieved_at = now_millis();
        let snapshot = self.reconcile_peer(EventOwner::Control).await?;
        #[cfg(feature = "test-support")]
        let pairing_ready_at = now_millis();
        #[cfg(feature = "test-support")]
        self.inner
            .pair_timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(PairTiming {
                offer_sent_at,
                confirm_received_at,
                lease_established_at,
                stability_achieved_at,
                pairing_ready_at,
            });
        Ok(snapshot)
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
        tracing::info!(
            event = "promotion-started",
            owner = EventOwner::Admin.name(),
            from = ?current.state,
            result = "success",
            cluster_generation = current.generation,
            "reconnect cluster event"
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
                let retry_with_backoff = matches!(
                    error,
                    PromotionHelloError::Rendezvous(Ds4HelloError::Timeout)
                        | PromotionHelloError::WorkerStartupTimeout
                );
                self.recover_preflight_promotion().await;
                // spec.md §31: HELLO timeout backs off and retries promotion. Record it in the
                // same coordinator promotion tracker as the other backoff targets so the
                // reconnect is not misreported as a reconnect (peer loss) failure.
                if retry_with_backoff
                    && let Some(runtime) = self.inner.coordinator_runtime.get()
                    && let Err(record_error) = runtime
                        .record_promotion_failure(ClusterFailure::HelloTimeout, now_millis())
                        .await
                {
                    tracing::warn!(
                        error = %record_error,
                        "failed to record promotion HELLO timeout"
                    );
                }
                return Err(error.into());
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
            let runtime = self.clone();
            tokio::spawn(async move {
                if let Err(error) = runtime.handle_route_loss().await {
                    tracing::error!(error = %error, "automatic distributed route-loss demotion failed");
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
    ) -> Result<Ds4Hello, PromotionHelloError> {
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
            .parse::<u32>()
            .map_err(anyhow::Error::from)?;
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
            if tokio::time::Instant::now() >= deadline {
                return Err(PromotionHelloError::WorkerStartupTimeout);
            }
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
        ) {
            tracing::warn!(
                event = "promotion-failed",
                owner = EventOwner::Promotion.name(),
                from = ?current.state,
                result = "failed",
                cluster_generation = current.generation,
                reason = "PromotionFailed",
                "reconnect cluster event"
            );
        }
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

    /// The current control session generation (P0-B). Persisted separately from the cluster
    /// generation so a negotiated session survives restart (design §7).
    pub async fn control_session_generation(&self) -> u64 {
        self.control_generation().await
    }

    pub(super) async fn control_generation(&self) -> u64 {
        match &self.inner.control {
            // Prefer the processor's live control session generation; the peer lease descriptor
            // fallback preserves the paired session's generation even before it is reflected in
            // the local descriptor.
            RoleControl::Coordinator(control) => {
                let control = control.lock().await;
                control
                    .peer_lease()
                    .descriptor()
                    .map_or(control.generation(), |d| d.generation)
            }
            RoleControl::Worker(control) => {
                let control = control.lock().await;
                control
                    .peer_lease()
                    .descriptor()
                    .map_or(control.generation(), |d| d.generation)
            }
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
