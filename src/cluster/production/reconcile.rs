use super::{RoleControl, now_millis};
use crate::cluster::EventOwner;

impl super::ProductionClusterRuntime {
    pub async fn reconcile(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        match self.inner.client.node().await {
            Ok(response) => {
                self.inner.lease.update(&response);
                self.reconcile_peer(EventOwner::PeriodicReconcile).await
            }
            Err(error) => {
                self.invalidate_route().await;
                let snapshot = self.reconcile_peer(EventOwner::PeriodicReconcile).await?;
                tracing::warn!(error = %error, "peer control reconciliation failed");
                Ok(snapshot)
            }
        }
    }

    pub(super) async fn reconcile_peer(
        &self,
        owner: EventOwner,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
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
        Ok(match &self.inner.control {
            RoleControl::Coordinator(control) => {
                self.inner
                    .mode
                    .reconcile_peer(owner, &mut *control.lock().await, now)
                    .await?
            }
            RoleControl::Worker(control) => {
                self.inner
                    .mode
                    .reconcile_peer(owner, &mut *control.lock().await, now)
                    .await?
            }
        })
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
