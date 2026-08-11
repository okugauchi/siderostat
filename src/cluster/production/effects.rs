use super::{
    ControlHttpError, RoleControl, effect_requires_ack, endpoint_path, header, now_millis,
};
use crate::{
    cluster::{
        AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlMessage, ControlRequest,
        ControlResponse, HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP,
        SignedControlHeaders,
    },
    target::LocalRole,
};
use axum::body::Bytes;
use axum::http::HeaderMap;
use std::net::SocketAddr;

impl super::ProductionClusterRuntime {
    async fn authenticate(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        source: SocketAddr,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedPeer, ControlHttpError> {
        let signed = SignedControlHeaders::from_header_values(
            header(headers, HEADER_NODE)?,
            header(headers, HEADER_TIMESTAMP)?,
            header(headers, HEADER_NONCE)?,
            header(headers, HEADER_SIGNATURE)?,
        )?;
        Ok(ControlRequest {
            method,
            path_and_query: path,
            body,
            source_ip: source.ip(),
            headers: &signed,
        }
        .authenticate(&self.inner.authenticator, now_millis())?)
    }

    pub(super) async fn handle(
        &self,
        endpoint: ControlEndpoint,
        method: &str,
        body: Bytes,
        source: SocketAddr,
        headers: HeaderMap,
    ) -> Result<ControlResponse, ControlHttpError> {
        let path = endpoint_path(endpoint);
        let authenticated = self
            .authenticate(method, path, &body, source, &headers)
            .await?;
        let now = now_millis();
        if endpoint == ControlEndpoint::Node {
            return match &self.inner.control {
                RoleControl::Coordinator(control) => {
                    Ok(control
                        .lock()
                        .await
                        .node_descriptor(&authenticated, true, now)?)
                }
                RoleControl::Worker(control) => {
                    Ok(control
                        .lock()
                        .await
                        .node_descriptor(&authenticated, true, now)?)
                }
            };
        }
        let message: ControlMessage = serde_json::from_slice(&body)
            .map_err(|error| ControlHttpError::BadJson(error.to_string()))?;
        let command = message.command.clone();
        let response = match &self.inner.control {
            RoleControl::Coordinator(control) => {
                control
                    .lock()
                    .await
                    .handle(endpoint, message, &authenticated, true, now)?
            }
            RoleControl::Worker(control) => {
                control
                    .lock()
                    .await
                    .handle(endpoint, message, &authenticated, true, now)?
            }
        };
        self.inner.lease.update(&response);
        if effect_requires_ack(&command) {
            let runtime = self.clone();
            tokio::spawn(async move { runtime.apply_effect(command).await })
                .await
                .map_err(|error| ControlHttpError::Effect(error.to_string()))?
                .map_err(|error| ControlHttpError::Effect(error.to_string()))?;
        } else {
            self.spawn_effect(command);
        }
        Ok(response)
    }

    fn spawn_effect(&self, command: ControlCommand) {
        let runtime = self.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.apply_effect(command).await {
                tracing::error!(error = %error, "peer control side effect failed");
            }
        });
    }

    async fn apply_effect(&self, command: ControlCommand) -> anyhow::Result<()> {
        match command {
            ControlCommand::Pair { .. } => {
                if self.inner.role == LocalRole::Worker {
                    let reply = ControlMessage {
                        request_id: uuid::Uuid::new_v4().to_string(),
                        generation: self.control_generation().await,
                        deployment_id: self.inner.descriptor.deployment_id.clone(),
                        command: ControlCommand::Pair {
                            descriptor: self.local_descriptor().await,
                        },
                    };
                    let response = self.inner.client.send(&reply).await?;
                    self.inner.lease.update(&response);
                }
                tokio::time::sleep(self.inner.config.cluster.policy.required_peer_stability).await;
                self.reconcile_peer().await?;
            }
            ControlCommand::PrepareWorker => self.prepare_worker().await?,
            ControlCommand::BeginDrain => self.worker_drained().await?,
            ControlCommand::CancelGeneration | ControlCommand::Demote => self.stop_worker().await?,
            ControlCommand::DistributedReady => {
                self.inner.proxy.admission().start_serving();
            }
            ControlCommand::Drained | ControlCommand::WorkerEvent { .. } => {}
        }
        Ok(())
    }
}
