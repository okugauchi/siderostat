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

    async fn invalidate_route(&self) {
        match &self.inner.control {
            RoleControl::Coordinator(control) => control.lock().await.invalidate_route(),
            RoleControl::Worker(control) => control.lock().await.invalidate_route(),
        }
    }
}
