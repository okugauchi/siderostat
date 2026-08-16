//! Single PeerLost recovery owner shared by the control-reconcile path and the route-loss
//! demotion monitor. See `docs/reconnect-peer-loss-design.md`.

use super::{RoleControl, now_millis};
use crate::{
    cluster::{ClusterEvent, ClusterEventKind, DistributedCoordinatorLifecycle, EventOwner},
    target::{ClusterState, LocalRole, ProxyTarget, UnavailableReason},
};
use anyhow::Context;
use std::sync::atomic::{AtomicU64, Ordering};
/// Serializes PeerLost recovery and records the last cluster generation recovered to
/// SoloReady, so duplicate same-generation recovery is idempotent and an older generation is a
/// no-op.
#[derive(Default)]
pub struct PeerLossRecovery {
    lock: tokio::sync::Mutex<()>,
    completed_generation: AtomicU64,
}

impl PeerLossRecovery {
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }

    fn completed_generation(&self) -> u64 {
        self.completed_generation.load(Ordering::Acquire)
    }

    fn note_completed(&self, generation: u64) {
        self.completed_generation
            .store(generation, Ordering::Release);
    }
}
impl super::ProductionClusterRuntime {
    /// The single PeerLost recovery entry point. Both the control-reconcile path and the
    /// route-loss demotion monitor call this. Order is fixed by the design:
    /// admission block -> distributed stop -> standalone start -> publish SoloReady.
    pub async fn recover_from_peer_loss(
        &self,
        owner: EventOwner,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let recovery = self.inner.recovery.clone();
        let _guard = recovery.lock().await;
        let current = self.inner.mode.snapshot();

        // Idempotent: an already-solo node has no distributed child to recover.
        if current.state == ClusterState::SoloStandaloneReady {
            return Ok(current);
        }
        // Stale: this cluster generation was already recovered to SoloReady; do not touch
        // newer state/children.
        if current.generation <= recovery.completed_generation() {
            tracing::debug!(
                event = "recovery-skipped",
                owner = owner.name(),
                from = ?current.state,
                result = "noop",
                cluster_generation = current.generation,
                "reconnect cluster event"
            );
            return Ok(current);
        }

        // 1. Block future admission and take the proxy target down before touching children.
        self.inner.proxy.admission().block();
        self.inner.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            false,
        );

        tracing::info!(
            event = "peer-lost",
            owner = owner.name(),
            from = ?current.state,
            result = "success",
            cluster_generation = current.generation,
            "reconnect cluster event"
        );
        tracing::info!(
            event = "recovery-started",
            owner = EventOwner::Recovery.name(),
            from = ?current.state,
            result = "success",
            cluster_generation = current.generation,
            "reconnect cluster event"
        );

        // 3. Transition to SoloStandaloneStarting unless a previous attempt already stopped
        //    there (retry after a distributed-stop / standalone-start failure).
        let starting = if current.state == ClusterState::SoloStandaloneStarting {
            current
        } else {
            self.inner
                .mode
                .cluster_handle()
                .apply(ClusterEvent {
                    expected_generation: current.generation,
                    kind: ClusterEventKind::PeerLost,
                })
                .await?
        };

        // PeerLost invalidates the in-flight distributed control sequence. Keep the negotiated
        // session/lease, but reset only its command phase so a subsequent Pair can establish a
        // fresh promotion sequence without allowing a delayed Pair to rewind an active one.
        match &self.inner.control {
            RoleControl::Coordinator(control) => control.lock().await.reset_for_repair(),
            RoleControl::Worker(control) => control.lock().await.reset_for_repair(),
        }

        // 2. Stop the local distributed child (identity-verified, idempotent). On failure the
        //    node stays SoloStandaloneStarting + Unavailable and the next reconcile retries.
        self.stop_distributed_child().await?;

        // 4. Start the local standalone at the recovery generation.
        self.inner
            .standalone
            .start(starting.generation)
            .await
            .map_err(|error| anyhow::anyhow!("standalone recovery failed: {error}"))?;

        // 5. LocalStandaloneReady -> SoloStandaloneReady.
        let ready = self
            .inner
            .mode
            .cluster_handle()
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::LocalStandaloneReady,
            })
            .await?;

        // 6. Publish the recovered target and resume serving.
        self.inner.proxy.set_target(ready.target, true);
        self.inner.proxy.admission().start_serving();

        recovery.note_completed(ready.generation);
        tracing::info!(
            event = "recovery-completed",
            owner = EventOwner::Recovery.name(),
            from = ?current.state,
            to = ?ready.state,
            result = "success",
            cluster_generation = ready.generation,
            "reconnect cluster event"
        );
        Ok(ready)
    }

    async fn stop_distributed_child(&self) -> anyhow::Result<()> {
        match self.inner.role {
            LocalRole::Coordinator => {
                if let Some(coordinator) = &self.inner.distributed_coordinator {
                    DistributedCoordinatorLifecycle::stop(coordinator.as_ref()).await?;
                }
            }
            LocalRole::Worker => {
                if let Some(worker) = &self.inner.distributed_worker {
                    crate::cluster::DistributedWorkerLifecycle::stop(worker.as_ref()).await?;
                }
            }
            LocalRole::Unknown => {}
        }
        Ok(())
    }

    /// Route-loss monitor. Waits for the coordinator's DS4 route to drop, then after the grace
    /// period either demotes gracefully (worker still reachable) or recovers to solo through the
    /// shared recovery owner (peer lost). Serialization with the reconcile path is provided by
    /// the owner; a stale monitor is a no-op once the node is already solo.
    pub(super) async fn handle_route_loss(
        &self,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let coordinator = self
            .inner
            .distributed_coordinator
            .as_ref()
            .context("coordinator supervisor unavailable")?
            .clone();
        loop {
            match coordinator.wait_route_loss().await {
                Ok(()) => {}
                Err(error) => {
                    let current = self.inner.mode.snapshot();
                    if current.state == ClusterState::SoloStandaloneReady {
                        return Ok(current);
                    }
                    return Err(error);
                }
            }
            match tokio::time::timeout(
                self.inner.config.cluster.policy.route_loss_grace,
                coordinator.wait_ready(),
            )
            .await
            {
                Ok(Ok(())) => continue, // transient blip: route recovered
                Ok(Err(error)) => {
                    let current = self.inner.mode.snapshot();
                    if current.state == ClusterState::SoloStandaloneReady {
                        return Ok(current);
                    }
                    return Err(error);
                }
                Err(_) => {
                    // Route stayed down past grace. Peer reachable -> graceful demote to Paired;
                    // peer lost -> solo recovery through the single owner.
                    let now = now_millis();
                    let peer_present = match &self.inner.control {
                        RoleControl::Coordinator(control) => {
                            control.lock().await.peer_lease().peer_present(now)
                        }
                        RoleControl::Worker(_) => false,
                    };
                    let state = self.inner.mode.snapshot().state;
                    let distributed = matches!(
                        state,
                        ClusterState::DistributedReady | ClusterState::DistributedStarting
                    );
                    if !peer_present || !distributed {
                        return self
                            .recover_from_peer_loss(EventOwner::RouteLossMonitor)
                            .await;
                    }
                    let demoted = self
                        .inner
                        .coordinator_runtime
                        .get()
                        .context("coordinator runtime unavailable")?
                        .demote()
                        .await?;
                    return Ok(demoted);
                }
            }
        }
    }
}
