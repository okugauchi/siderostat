use super::{RoleControl, now_millis};
use crate::{
    cluster::EventOwner,
    target::{ClusterState, LocalRole},
};
use anyhow::Context;

impl super::ProductionClusterRuntime {
    pub async fn reconcile(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        if self.planned_restart_active() {
            return Ok(self.inner.mode.snapshot());
        }
        match self.inner.client.node().await {
            Ok(response) => {
                self.inner.lease.update(&response);
                self.reconcile_periodic().await
            }
            Err(error) => {
                self.invalidate_route().await;
                let snapshot = self.reconcile_periodic().await?;
                tracing::warn!(error = %error, "peer control reconciliation failed");
                Ok(snapshot)
            }
        }
    }

    /// Periodic reconcile entry (B-02). On the coordinator in Backoff, peer loss is prioritized
    /// over the backoff deadline: if the peer is gone the shared recovery owner recovers to Solo
    /// first. Otherwise the backoff deadline recovers exactly once to the stable paired state.
    /// Backoff never runs pair/promote concurrently because it is not a pairing/promotion
    /// trigger state in the periodic task.
    async fn reconcile_periodic(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        if self.inner.role == LocalRole::Coordinator
            && self.inner.mode.snapshot().state == ClusterState::Backoff
        {
            let now = now_millis();
            let peer_present = match &self.inner.control {
                RoleControl::Coordinator(control) => {
                    control.lock().await.peer_lease().peer_present(now)
                }
                RoleControl::Worker(_) => unreachable!("coordinator-only backoff recovery"),
            };
            if !peer_present {
                return self
                    .recover_from_peer_loss(EventOwner::PeriodicReconcile)
                    .await;
            }
            let runtime = self
                .inner
                .coordinator_runtime
                .get()
                .context("coordinator runtime unavailable")?;
            return runtime.reconcile_backoff(now).await.map_err(Into::into);
        }
        self.reconcile_peer(EventOwner::PeriodicReconcile).await
    }

    pub(super) async fn reconcile_peer(
        &self,
        owner: EventOwner,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        if self.planned_restart_active() {
            return Ok(self.inner.mode.snapshot());
        }
        let now = now_millis();
        // Detect peer loss before delegating to the pure state machine so the production
        // recovery owner stops the distributed child and restarts the standalone. The pure
        // `ModeRuntime::fallback_to_solo` cannot touch distributed children, so it is only used
        // by unit tests.
        let (state, peer_present) = match &self.inner.control {
            RoleControl::Coordinator(control) => {
                let control = control.lock().await;
                (
                    self.inner.mode.snapshot().state,
                    control.peer_lease().peer_present(now),
                )
            }
            RoleControl::Worker(control) => {
                let control = control.lock().await;
                (
                    self.inner.mode.snapshot().state,
                    control.peer_lease().peer_present(now),
                )
            }
        };
        if !peer_present && self.requires_solo_fallback(state) {
            return self.recover_from_peer_loss(owner).await;
        }
        let peer_present = match &self.inner.control {
            RoleControl::Coordinator(control) => {
                control.lock().await.peer_lease().peer_present(now)
            }
            RoleControl::Worker(control) => control.lock().await.peer_lease().peer_present(now),
        };
        if suppress_automatic_pairing(state, peer_present, self.automatic_pairing_blocked()) {
            tracing::debug!(
                owner = owner.name(),
                state = ?state,
                "automatic pairing remains blocked after deployment mismatch"
            );
            return Ok(self.inner.mode.snapshot());
        }
        Ok(self
            .inner
            .mode
            .reconcile_peer_with_presence(owner, peer_present)
            .await?)
    }

    /// Whether a cluster state must fall back to a solo standalone when the peer is gone.
    fn requires_solo_fallback(&self, state: crate::target::ClusterState) -> bool {
        use crate::target::ClusterState;
        matches!(
            state,
            ClusterState::Pairing
                | ClusterState::PairedStandaloneReady
                | ClusterState::AwaitingWorkerHello
                | ClusterState::Promoting
                | ClusterState::DistributedStarting
                | ClusterState::DistributedReady
                | ClusterState::Demoting
                | ClusterState::Backoff
                | ClusterState::SoloStandaloneStarting
        )
    }

    async fn invalidate_route(&self) {
        match &self.inner.control {
            RoleControl::Coordinator(control) => control.lock().await.invalidate_route(),
            RoleControl::Worker(control) => control.lock().await.invalidate_route(),
        }
    }
}

fn suppress_automatic_pairing(
    state: ClusterState,
    peer_present: bool,
    automatic_pairing_blocked: bool,
) -> bool {
    state == ClusterState::SoloStandaloneReady && peer_present && automatic_pairing_blocked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_mismatch_latch_suppresses_periodic_pairing() {
        assert!(suppress_automatic_pairing(
            ClusterState::SoloStandaloneReady,
            true,
            true,
        ));
    }

    #[test]
    fn pairing_remains_available_without_the_mismatch_latch() {
        assert!(!suppress_automatic_pairing(
            ClusterState::SoloStandaloneReady,
            true,
            false,
        ));
        assert!(!suppress_automatic_pairing(
            ClusterState::SoloStandaloneReady,
            false,
            true,
        ));
        assert!(!suppress_automatic_pairing(
            ClusterState::PairedStandaloneReady,
            true,
            true,
        ));
    }
}
