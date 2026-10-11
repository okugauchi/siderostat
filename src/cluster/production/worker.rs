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
                tp_session: None,
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
                tp_session: None,
            })
            .await?;
        let starting = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: promoting.generation,
                kind: ClusterEventKind::DistributedChildStarted,
                tp_session: None,
            })
            .await?;
        let ready = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::DistributedRouteReady,
                tp_session: None,
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
                            tp_session: None,
                        })
                        .await?;
                    self.inner
                        .mode
                        .cluster_handle()
                        .apply(ClusterEvent {
                            expected_generation: demoting.generation,
                            kind: ClusterEventKind::PairingReady,
                            tp_session: None,
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
                            tp_session: None,
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
        if runtime.inner.role == LocalRole::Worker {
            let monitor_runtime = runtime.clone();
            tokio::spawn(async move {
                monitor_runtime.monitor_worker_child().await;
            });
        }
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

    async fn monitor_worker_child(&self) {
        loop {
            if self.planned_restart_active() || self.policy_pending() {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let snapshot = self.inner.mode.snapshot();
            if snapshot.state != ClusterState::DistributedReady {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let generation = snapshot.generation;
            let Some(worker_runtime) = &self.inner.worker_runtime else {
                return;
            };
            let lease_runtime = self.clone();
            let lease: Arc<dyn crate::cluster::WorkerLeaseStatus> = Arc::new(move || {
                lease_runtime.inner.lease.valid()
                    && lease_runtime.inner.mode.snapshot().state == ClusterState::DistributedReady
                    && lease_runtime.inner.mode.snapshot().generation == generation
            });
            let failure = worker_runtime.wait_for_failure(lease).await;
            if self.planned_restart_active() {
                continue;
            }
            tracing::warn!(
                event = "distributed-child-exited",
                reason = %failure,
                generation,
                "distributed worker child exited; beginning safe recovery"
            );
            // Keep the worker in Solo until the coordinator has observed the child-loss event
            // and its lease has expired. Otherwise the periodic worker reconcile can immediately
            // form a new Pairing while the coordinator is still DistributedReady.
            self.block_automatic_pairing();
            if let Err(error) = self.notify_worker_child_exit().await {
                tracing::warn!(error = %error, "failed to notify coordinator of worker child exit");
            }
            if let Err(error) = self.recover_from_peer_loss(EventOwner::Recovery).await {
                tracing::error!(error = %error, "worker child crash recovery failed");
            }
            let mut peer_released = false;
            for _ in 0..150 {
                if !self.peer_present().await {
                    peer_released = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !peer_released {
                tracing::warn!(
                    event = "distributed-child-exit-peer-lease-timeout",
                    generation,
                    "coordinator lease remained present during worker recovery grace period"
                );
            }
            self.unblock_automatic_pairing();
        }
    }

    async fn notify_worker_child_exit(&self) -> anyhow::Result<()> {
        let message = match &self.inner.control {
            RoleControl::Worker(control) => control
                .lock()
                .await
                .child_exited_message(uuid::Uuid::new_v4().to_string())?,
            RoleControl::Coordinator(_) => return Ok(()),
        };
        self.inner.client.send(&message).await?;
        Ok(())
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
