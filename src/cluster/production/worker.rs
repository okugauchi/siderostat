use super::RoleControl;
use crate::{
    cluster::{
        ChildIdentity, ClusterEvent, ClusterEventKind, DistributedCoordinatorLifecycle,
        DistributedWorkerLifecycle, EventOwner,
    },
    target::{ClusterState, LocalRole},
};
use anyhow::{Context, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

impl super::ProductionClusterRuntime {
    pub(super) async fn prepare_worker(&self) -> anyhow::Result<()> {
        let worker = self
            .inner
            .worker_runtime
            .as_ref()
            .context("worker lifecycle unavailable")?;
        let current = self.inner.mode.snapshot();
        ensure!(
            current.state == ClusterState::PairedStandaloneReady,
            "worker promotion requires paired standalone readiness, current state is {:?}",
            current.state
        );
        tracing::info!(
            event = "promotion-started",
            owner = EventOwner::Control.name(),
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
        let generation = awaiting.generation;
        let lease = self.inner.lease.clone();
        worker
            .prepare(generation, Arc::new(move || lease.valid()))
            .await?;
        let promoting = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: generation,
                kind: ClusterEventKind::WorkerHelloAccepted,
            })
            .await?;
        let starting = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: promoting.generation,
                kind: ClusterEventKind::DistributedChildStarted,
            })
            .await?;
        let ready = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::DistributedRouteReady,
            })
            .await?;
        self.inner.proxy.set_target(ready.target, true);
        let message = match &self.inner.control {
            RoleControl::Worker(control) => control
                .lock()
                .await
                .worker_ready_message(uuid::Uuid::new_v4().to_string(), generation)?,
            RoleControl::Coordinator(_) => anyhow::bail!("prepare-worker received by coordinator"),
        };
        self.inner.client.send(&message).await?;
        Ok(())
    }

    pub(super) async fn worker_drained(&self) -> anyhow::Result<()> {
        // `WorkerDistributedRuntime::prepare` already drained this ingress before stopping the
        // standalone child. BeginDrain is therefore an acknowledgement barrier, not a second
        // drain with the worker's later state-machine generation.
        let message = match &self.inner.control {
            RoleControl::Worker(control) => control
                .lock()
                .await
                .drained_message(uuid::Uuid::new_v4().to_string())?,
            RoleControl::Coordinator(_) => anyhow::bail!("begin-drain received by coordinator"),
        };
        self.inner.client.send(&message).await?;
        Ok(())
    }

    pub(super) async fn stop_worker(&self) -> anyhow::Result<()> {
        if let Some(worker) = &self.inner.worker_runtime {
            let current = self.inner.mode.snapshot();
            worker.cancel().await?;
            let paired = match current.state {
                ClusterState::DistributedReady => {
                    tracing::info!(
                        event = "demotion-started",
                        owner = EventOwner::Control.name(),
                        from = ?current.state,
                        result = "success",
                        cluster_generation = current.generation,
                        "reconnect cluster event"
                    );
                    let demoting = self
                        .inner
                        .mode
                        .cluster_handle()
                        .apply(ClusterEvent {
                            expected_generation: current.generation,
                            kind: ClusterEventKind::BeginDemotion,
                        })
                        .await?;
                    self.inner
                        .mode
                        .cluster_handle()
                        .apply(ClusterEvent {
                            expected_generation: demoting.generation,
                            kind: ClusterEventKind::PairingReady,
                        })
                        .await?
                }
                ClusterState::AwaitingWorkerHello
                | ClusterState::Promoting
                | ClusterState::DistributedStarting => {
                    tracing::warn!(
                        event = "promotion-failed",
                        owner = EventOwner::Promotion.name(),
                        from = ?current.state,
                        result = "failed",
                        cluster_generation = current.generation,
                        reason = "PromotionFailed",
                        "reconnect cluster event"
                    );
                    self.inner
                        .mode
                        .cluster_handle()
                        .apply(ClusterEvent {
                            expected_generation: current.generation,
                            kind: ClusterEventKind::PromotionFailed,
                        })
                        .await?
                }
                ClusterState::PairedStandaloneReady => current,
                state => anyhow::bail!("worker cannot stop distributed child from {state:?}"),
            };
            self.inner.proxy.set_target(paired.target, true);
            self.inner.proxy.admission().start_serving();
            if self.note_planned_restart_child_stopped() {
                // Pair may have arrived while Demote was being acknowledged early. The stop
                // task owns completion in that case, so the reciprocal Pair and stability gate
                // run only after the child is actually gone.
                self.complete_pair_effect(true).await?;
            }
        }
        Ok(())
    }

    pub fn start_reconcile_task(&self) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        tokio::spawn(async move {
            let promotion_running = Arc::new(AtomicBool::new(false));
            let lease_refresh = runtime.inner.config.cluster.timeouts.control_lease / 3;
            let period = runtime
                .inner
                .config
                .cluster
                .discovery
                .reconcile_interval
                .min(lease_refresh.max(Duration::from_millis(100)));
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if runtime.planned_restart_active() {
                    continue;
                }
                if let Err(error) = runtime.reconcile().await {
                    tracing::error!(error = %error, "production cluster reconcile failed");
                }
                if runtime.planned_restart_active() {
                    continue;
                }
                let snapshot = runtime.inner.mode.snapshot();
                if snapshot.state == ClusterState::SoloStandaloneReady
                    && runtime.inner.role == LocalRole::Coordinator
                    && runtime.inner.config.cluster.policy.auto_pair
                    && !runtime.automatic_pairing_blocked()
                {
                    if let Err(error) = runtime.pair().await {
                        tracing::debug!(error = %error, "automatic pairing attempt failed");
                    }
                } else if snapshot.state == ClusterState::PairedStandaloneReady
                    && runtime.inner.role == LocalRole::Coordinator
                    && runtime.inner.config.cluster.policy.auto_promote
                    && !runtime.recovery_owner_active()
                    && promotion_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let promotion_runtime = runtime.clone();
                    let promotion_running = promotion_running.clone();
                    tokio::spawn(async move {
                        if let Err(error) = promotion_runtime.promote().await {
                            tracing::error!(error = %error, "automatic promotion failed");
                        }
                        promotion_running.store(false, Ordering::Release);
                    });
                }
            }
        })
    }

    pub async fn stop_distributed(&self) -> anyhow::Result<()> {
        if let Some(worker) = &self.inner.distributed_worker {
            DistributedWorkerLifecycle::stop(worker.as_ref()).await?;
        }
        if let Some(coordinator) = self.inner.distributed_coordinator.get() {
            DistributedCoordinatorLifecycle::stop(coordinator.as_ref()).await?;
        }
        Ok(())
    }

    pub async fn distributed_child_identity(&self) -> Option<ChildIdentity> {
        match self.inner.role {
            LocalRole::Coordinator => match self.inner.distributed_coordinator.get() {
                Some(child) => child.child_identity().await,
                None => None,
            },
            LocalRole::Worker => match &self.inner.distributed_worker {
                Some(child) => child.child_identity().await,
                None => None,
            },
            LocalRole::Unknown => None,
        }
    }
}
