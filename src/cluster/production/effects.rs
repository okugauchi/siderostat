use super::{ControlHttpError, RoleControl, endpoint_path, header, now_millis};
use crate::{
    cluster::{
        AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
        ControlRequest, ControlResponse, EventOwner, HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE,
        HEADER_TIMESTAMP, SignedControlHeaders,
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

    pub(super) async fn handle_metrics(
        &self,
        source: SocketAddr,
        headers: HeaderMap,
    ) -> Result<String, ControlHttpError> {
        self.authenticate("GET", super::CONTROL_METRICS_PATH, &[], source, &headers)
            .await?;
        if self.inner.role != LocalRole::Coordinator {
            return Err(ControlError::CommandNotAllowed.into());
        }
        if !self.inner.network.route_scoped() {
            return Err(ControlError::RouteNotScoped.into());
        }
        Ok(self.render_control_metrics())
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
        // N-02: derive `route_scoped` from the latest verified network evidence instead of a
        // hard-coded `true`. Fail-closed until a fresh observation is applied, so the control
        // plane rejects establish/renew with `RouteNotScoped` when no valid bridge0-scoped
        // peer candidate is measured.
        let route_scoped = self.inner.network.route_scoped();
        if endpoint == ControlEndpoint::Node {
            return match &self.inner.control {
                RoleControl::Coordinator(control) => {
                    Ok(control
                        .lock()
                        .await
                        .node_descriptor(&authenticated, route_scoped, now)?)
                }
                RoleControl::Worker(control) => {
                    Ok(control
                        .lock()
                        .await
                        .node_descriptor(&authenticated, route_scoped, now)?)
                }
            };
        }
        let message: ControlMessage = serde_json::from_slice(&body)
            .map_err(|error| ControlHttpError::BadJson(error.to_string()))?;
        let command = message.command.clone();
        if self.inner.role == LocalRole::Worker
            && matches!(&command, ControlCommand::PrepareWorker)
            && self.planned_restart_blocks_worker_prepare()
        {
            // Demote deliberately acknowledges before the child stop completes. Do not let the
            // control phase advance to WorkerPreparing while the lifecycle owner is still
            // stopping the old distributed child.
            return Err(ControlError::PlannedRestartInProgress.into());
        }
        let cluster_generation = self.inner.mode.snapshot().generation;
        let response = match &self.inner.control {
            RoleControl::Coordinator(control) => {
                match control.lock().await.handle(
                    endpoint,
                    message,
                    &authenticated,
                    route_scoped,
                    now,
                ) {
                    Ok(response) => response,
                    Err(ControlError::GenerationMismatch { expected, received }) => {
                        log_pair_generation_mismatch(expected, received, cluster_generation);
                        return Err(ControlError::GenerationMismatch { expected, received }.into());
                    }
                    Err(error) => {
                        if error == ControlError::DeploymentMismatch {
                            self.schedule_deployment_mismatch_recovery();
                        }
                        return Err(error.into());
                    }
                }
            }
            RoleControl::Worker(control) => {
                match control.lock().await.handle(
                    endpoint,
                    message,
                    &authenticated,
                    route_scoped,
                    now,
                ) {
                    Ok(response) => response,
                    Err(ControlError::GenerationMismatch { expected, received }) => {
                        log_pair_generation_mismatch(expected, received, cluster_generation);
                        return Err(ControlError::GenerationMismatch { expected, received }.into());
                    }
                    Err(error) => {
                        if error == ControlError::DeploymentMismatch {
                            self.schedule_deployment_mismatch_recovery();
                        }
                        return Err(error.into());
                    }
                }
            }
        };
        let response =
            if super::effect_requires_ack_for_runtime(&command, self.planned_restart_active()) {
                let response_status = response.status;
                let runtime = self.clone();
                tokio::spawn(async move { runtime.apply_effect(command).await })
                    .await
                    .map_err(|error| ControlHttpError::Effect(error.to_string()))?
                    .map_err(|error| ControlHttpError::Effect(error.to_string()))?;
                // Lifecycle effects can take long enough to cross the lease interval. Renew the
                // inbound control lease after the effect completes and return that fresh expiry to
                // the caller; otherwise a reciprocal Pair can finish successfully but the next
                // /v1/node renewal observes the lease as expired.
                let mut refreshed = self
                    .refresh_control_response(&authenticated, route_scoped)
                    .await?;
                refreshed.status = response_status;
                refreshed
            } else {
                self.spawn_effect(command);
                response
            };
        self.inner.lease.update(&response);
        Ok(response)
    }

    async fn refresh_control_response(
        &self,
        authenticated: &AuthenticatedPeer,
        route_scoped: bool,
    ) -> Result<ControlResponse, ControlHttpError> {
        let now = now_millis();
        match &self.inner.control {
            RoleControl::Coordinator(control) => {
                Ok(control
                    .lock()
                    .await
                    .node_descriptor(authenticated, route_scoped, now)?)
            }
            RoleControl::Worker(control) => {
                Ok(control
                    .lock()
                    .await
                    .node_descriptor(authenticated, route_scoped, now)?)
            }
        }
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
                let planned_completion = if self.inner.role == LocalRole::Worker {
                    match self.planned_restart_pair_action() {
                        super::PlannedRestartPairAction::Defer
                        | super::PlannedRestartPairAction::AlreadyCompleting => return Ok(()),
                        super::PlannedRestartPairAction::Run => false,
                        super::PlannedRestartPairAction::RunAfterChildStop => true,
                    }
                } else {
                    false
                };
                self.complete_pair_effect(planned_completion).await?;
            }
            ControlCommand::PrepareWorker => self.prepare_worker().await?,
            ControlCommand::BeginDrain => self.worker_drained().await?,
            ControlCommand::CancelGeneration | ControlCommand::Demote => self.stop_worker().await?,
            ControlCommand::DistributedReady => {
                self.inner.proxy.admission().start_serving();
            }
            ControlCommand::PrepareRestart => self.begin_planned_restart(),
            ControlCommand::CancelRestart => self.cancel_planned_restart().await?,
            ControlCommand::Drained | ControlCommand::WorkerEvent { .. } => {}
        }
        Ok(())
    }

    pub(super) async fn complete_pair_effect(
        &self,
        planned_completion: bool,
    ) -> anyhow::Result<()> {
        let result = async {
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
            self.clear_planned_restart();
            tokio::time::sleep(self.inner.config.cluster.policy.required_peer_stability).await;
            self.reconcile_peer(EventOwner::Control).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if result.is_err() && planned_completion {
            // Keep the completed-child state so a later Pair can retry if the restarting
            // coordinator was still between process generations when the reciprocal Pair was
            // sent.
            self.inner.planned_restart.retry_pair_after_failure();
        }
        result
    }

    fn schedule_deployment_mismatch_recovery(&self) {
        // Close the periodic-reconcile pairing gate before yielding to the recovery task.
        // Otherwise a fast reconcile tick can begin another Pairing transition while the
        // mismatch recovery is still waiting to run.
        self.block_automatic_pairing();
        let runtime = self.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.recover_from_deployment_mismatch().await {
                tracing::error!(error = %error, "deployment mismatch recovery failed");
            }
        });
    }
}

/// Emit the contract's `pair-generation-mismatch` structured log for a Pair 409.
/// Records only the expected/received generations (never secrets or signatures).
fn log_pair_generation_mismatch(expected: u64, received: u64, cluster_generation: u64) {
    tracing::warn!(
        event = "pair-generation-mismatch",
        owner = EventOwner::Control.name(),
        result = "rejected",
        expected = expected,
        received = received,
        cluster_generation = cluster_generation,
        control_session_generation = expected,
        "reconnect cluster event"
    );
}
