use super::{
    AuthError, ControlAuthenticator, ControlCommand, ControlEndpoint, ControlMessage,
    ControlRequest, ControlResponse, ControlRole, ControlSecret, CoordinatorControl,
    CoordinatorDistributedRuntime, CoordinatorPeerLifecycle, CoordinatorRuntimeTimeouts,
    DistributedCoordinatorSupervisor, DistributedManifest, DistributedWorkerSupervisor,
    HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, ModeRuntime, NodeDescriptor,
    PromotionRetryPolicy, RendezvousControlSnapshot, RendezvousListener, SignedControlHeaders,
    StandaloneSupervisor, WorkerControl, WorkerDistributedRuntime, WorkerHelloExpectation,
};
use crate::{
    config::ModeAwareConfig,
    proxy::ModeAwareProxyState,
    target::{ClusterState, LocalRole, StableMode},
};
use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::future::BoxFuture;
use reqwest::Url;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

/// The production control transport. Requests are pinned to the cluster address and authenticated
/// independently from the public/admin planes.
#[derive(Clone)]
pub struct ProductionControlClient {
    inner: Arc<ControlClientInner>,
}

struct ControlClientInner {
    client: reqwest::Client,
    base: Url,
    local_node_id: String,
    authenticator: ControlAuthenticator,
    lifecycle_timeout: Duration,
}

impl ProductionControlClient {
    pub fn new(
        local_node_id: String,
        local_address: IpAddr,
        peer_address: IpAddr,
        port: u16,
        secret: Vec<u8>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .local_address(local_address)
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()?;
        Ok(Self {
            inner: Arc::new(ControlClientInner {
                client,
                base: Url::parse(&format!("http://{peer_address}:{port}/"))?,
                local_node_id,
                authenticator: ControlAuthenticator::new_at_source(
                    ControlSecret::new(secret)?,
                    peer_address,
                ),
                lifecycle_timeout: request_timeout,
            }),
        })
    }

    fn with_lifecycle_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("control client is not cloned during construction")
            .lifecycle_timeout = timeout;
        self
    }

    pub async fn send(&self, message: &ControlMessage) -> anyhow::Result<ControlResponse> {
        let endpoint = message.command.endpoint();
        let path = endpoint_path(endpoint);
        let body = serde_json::to_vec(message)?;
        let timeout = effect_requires_ack(&message.command).then_some(self.inner.lifecycle_timeout);
        self.request(reqwest::Method::POST, path, body, timeout)
            .await
    }

    pub async fn node(&self) -> anyhow::Result<ControlResponse> {
        self.request(reqwest::Method::GET, "/v1/node", Vec::new(), None)
            .await
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<ControlResponse> {
        let timestamp = now_millis();
        let signed = self.inner.authenticator.sign(
            self.inner.local_node_id.clone(),
            method.as_str(),
            path,
            timestamp,
            uuid::Uuid::new_v4().simple().to_string(),
            &body,
        )?;
        let request = self
            .inner
            .client
            .request(method, self.inner.base.join(path.trim_start_matches('/'))?)
            .header(HEADER_NODE, signed.node_id())
            .header(HEADER_TIMESTAMP, signed.timestamp_millis())
            .header(HEADER_NONCE, signed.nonce())
            .header(HEADER_SIGNATURE, signed.signature())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        let response = match timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        }
        .send()
        .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            anyhow::bail!(
                "peer control {} returned {}: {}",
                path,
                status,
                String::from_utf8_lossy(&bytes)
            );
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Clone)]
struct LeaseStatus {
    expires_at: Arc<AtomicU64>,
}

impl LeaseStatus {
    fn new() -> Self {
        Self {
            expires_at: Arc::new(AtomicU64::new(0)),
        }
    }

    fn update(&self, response: &ControlResponse) {
        if let Some(value) = response.lease_expires_at_millis {
            self.expires_at.store(value, Ordering::Release);
        }
    }

    fn valid(&self) -> bool {
        now_millis() < self.expires_at.load(Ordering::Acquire)
    }
}

enum RoleControl {
    Coordinator(Mutex<CoordinatorControl>),
    Worker(Mutex<WorkerControl>),
}

#[derive(Clone)]
pub struct ProductionClusterRuntime {
    inner: Arc<ProductionInner>,
}

struct ProductionInner {
    role: LocalRole,
    local_address: IpAddr,
    peer_address: IpAddr,
    descriptor: NodeDescriptor,
    authenticator: ControlAuthenticator,
    control: RoleControl,
    client: ProductionControlClient,
    lease: LeaseStatus,
    mode: Arc<ModeRuntime>,
    proxy: Arc<ModeAwareProxyState>,
    standalone: Arc<StandaloneSupervisor>,
    worker_runtime: Option<WorkerDistributedRuntime>,
    coordinator_runtime: OnceLock<CoordinatorDistributedRuntime>,
    distributed_coordinator: Option<Arc<DistributedCoordinatorSupervisor>>,
    distributed_worker: Option<Arc<DistributedWorkerSupervisor>>,
    config: ModeAwareConfig,
    manifest: DistributedManifest,
}

impl ProductionClusterRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ModeAwareConfig,
        role: LocalRole,
        mode: Arc<ModeRuntime>,
        proxy: Arc<ModeAwareProxyState>,
        standalone: Arc<StandaloneSupervisor>,
        manifest: DistributedManifest,
        control_secret: Vec<u8>,
    ) -> anyhow::Result<Self> {
        ensure!(role != LocalRole::Unknown, "cluster role is unknown");
        manifest.validate()?;
        let deployment_id = manifest.deployment_id()?;
        let (local_address, peer_address, control_role) = match role {
            LocalRole::Coordinator => (
                config.cluster.coordinator_address,
                config.cluster.worker_address,
                ControlRole::Coordinator,
            ),
            LocalRole::Worker => (
                config.cluster.worker_address,
                config.cluster.coordinator_address,
                ControlRole::Worker,
            ),
            LocalRole::Unknown => unreachable!(),
        };
        let local_control_id = config.cluster.node_id.clone();
        let descriptor = NodeDescriptor {
            protocol_version: 1,
            node_id: local_control_id.clone(),
            role: control_role,
            generation: mode.snapshot().generation,
            mode: super::ControlMode::SoloStandalone,
            deployment_id: Some(deployment_id),
        };
        let control = match role {
            LocalRole::Coordinator => {
                RoleControl::Coordinator(Mutex::new(CoordinatorControl::new(
                    descriptor.clone(),
                    config.cluster.timeouts.control_lease,
                    config.cluster.policy.required_peer_stability,
                )?))
            }
            LocalRole::Worker => RoleControl::Worker(Mutex::new(WorkerControl::new(
                descriptor.clone(),
                config.cluster.timeouts.control_lease,
                config.cluster.policy.required_peer_stability,
            )?)),
            LocalRole::Unknown => unreachable!(),
        };
        let client = ProductionControlClient::new(
            local_control_id,
            local_address,
            peer_address,
            config.cluster.control_port,
            control_secret.clone(),
            config.cluster.timeouts.peer_connect,
            config.cluster.timeouts.peer_request,
        )?
        .with_lifecycle_timeout(
            config
                .cluster
                .timeouts
                .stop
                .saturating_add(config.cluster.timeouts.peer_request),
        );
        let lease = LeaseStatus::new();
        let (worker_runtime, distributed_worker) = if role == LocalRole::Worker {
            let child = Arc::new(DistributedWorkerSupervisor::new(
                super::build_distributed_worker_command(
                    &config.ds4,
                    config.cluster.coordinator_address,
                    config.cluster.ds4_distributed_port,
                )?,
                config.cluster.timeouts.stop,
                config.ds4.allow_sigkill,
            ));
            (
                Some(WorkerDistributedRuntime::new(
                    proxy.admission().clone(),
                    standalone.clone(),
                    child.clone(),
                    config.cluster.timeouts.drain,
                    config.cluster.timeouts.worker_startup,
                    Duration::from_secs(1),
                )?),
                Some(child),
            )
        } else {
            (None, None)
        };
        let mut inner = ProductionInner {
            role,
            local_address,
            peer_address,
            descriptor,
            authenticator: ControlAuthenticator::new_at_source(
                ControlSecret::new(control_secret)?,
                peer_address,
            ),
            control,
            client,
            lease,
            mode,
            proxy,
            standalone,
            worker_runtime,
            coordinator_runtime: OnceLock::new(),
            distributed_coordinator: None,
            distributed_worker,
            config,
            manifest,
        };
        if role == LocalRole::Coordinator {
            inner.distributed_coordinator = Some(Arc::new(DistributedCoordinatorSupervisor::new(
                super::build_distributed_coordinator_command(
                    &inner.config.ds4,
                    inner.config.cluster.coordinator_address,
                    inner.config.cluster.ds4_distributed_port,
                )?,
                Url::parse(&format!(
                    "http://{}:{}/v1/models",
                    inner.config.ds4.http_host, inner.config.ds4.http_port
                ))?,
                inner.config.cluster.timeouts.coordinator_startup,
                Duration::from_secs(1),
                inner.config.cluster.timeouts.stop,
                inner.config.ds4.allow_sigkill,
            )));
        }
        let runtime = Self {
            inner: Arc::new(inner),
        };
        runtime.finish_coordinator()?;
        Ok(runtime)
    }

    fn finish_coordinator(&self) -> anyhow::Result<()> {
        if self.inner.role != LocalRole::Coordinator {
            return Ok(());
        }
        let coordinator = self
            .inner
            .distributed_coordinator
            .as_ref()
            .context("coordinator supervisor unavailable")?
            .clone();
        let peer = Arc::new(PeerLifecycle {
            inner: Arc::downgrade(&self.inner),
        });
        let runtime = CoordinatorDistributedRuntime::new(
            self.inner.mode.cluster_handle(),
            self.inner.proxy.clone(),
            self.inner.standalone.clone(),
            coordinator,
            peer,
            CoordinatorRuntimeTimeouts {
                drain: self.inner.config.cluster.timeouts.drain,
                startup: self.inner.config.cluster.timeouts.coordinator_startup,
                complete_route: self.inner.config.cluster.timeouts.complete_route,
                route_loss_grace: self.inner.config.cluster.policy.route_loss_grace,
            },
            PromotionRetryPolicy {
                backoff: self.inner.config.cluster.policy.promotion_backoff,
                maximum_consecutive_failures: self
                    .inner
                    .config
                    .cluster
                    .policy
                    .max_consecutive_promotion_failures,
            },
        )?;
        self.inner
            .coordinator_runtime
            .set(runtime)
            .map_err(|_| anyhow::anyhow!("coordinator runtime initialized twice"))
    }

    pub fn role(&self) -> LocalRole {
        self.inner.role
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(
            self.inner.local_address,
            self.inner.config.cluster.control_port,
        )
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/node", get(control_node))
            .route("/v1/pair", post(control_pair))
            .route("/v1/prepare-worker", post(control_prepare))
            .route("/v1/begin-drain", post(control_begin_drain))
            .route("/v1/drained", post(control_drained))
            .route("/v1/cancel-generation", post(control_cancel))
            .route("/v1/worker-event", post(control_worker_event))
            .route("/v1/distributed-ready", post(control_distributed_ready))
            .route("/v1/demote", post(control_demote))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .with_state(self.clone())
    }

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
            .apply(super::ClusterEvent {
                expected_generation: current.generation,
                kind: super::ClusterEventKind::BeginPromotion,
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
            control.phase() == super::DistributedControlPhase::WorkerReady
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

    async fn prepare_and_accept_hello(
        &self,
        awaiting: crate::cluster::ClusterSnapshot,
    ) -> anyhow::Result<super::Ds4Hello> {
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
                control.lock().await.phase() == super::DistributedControlPhase::WorkerReady
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
            .apply(super::ClusterEvent {
                expected_generation: current.generation,
                kind: super::ClusterEventKind::PromotionFailed,
            })
            .await
        {
            self.inner.proxy.set_target(paired.target, true);
            self.inner.proxy.admission().start_serving();
        }
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

    pub async fn reconcile(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        match self.inner.client.node().await {
            Ok(response) => {
                self.inner.lease.update(&response);
                self.reconcile_peer().await
            }
            Err(error) => {
                self.invalidate_route().await;
                let snapshot = self.reconcile_peer().await?;
                tracing::warn!(error = %error, "peer control reconciliation failed");
                Ok(snapshot)
            }
        }
    }

    async fn reconcile_peer(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let now = now_millis();
        Ok(match &self.inner.control {
            RoleControl::Coordinator(control) => {
                self.inner
                    .mode
                    .reconcile_peer(&mut *control.lock().await, now)
                    .await?
            }
            RoleControl::Worker(control) => {
                self.inner
                    .mode
                    .reconcile_peer(&mut *control.lock().await, now)
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

    async fn control_generation(&self) -> u64 {
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

    async fn local_descriptor(&self) -> NodeDescriptor {
        let mut descriptor = self.inner.descriptor.clone();
        descriptor.generation = self.control_generation().await;
        descriptor.mode = match self.inner.mode.snapshot().stable_mode {
            StableMode::SoloStandalone => super::ControlMode::SoloStandalone,
            StableMode::PairedStandalone => super::ControlMode::PairedStandalone,
            StableMode::DistributedMxfp4 => super::ControlMode::DistributedMxfp4,
        };
        descriptor
    }

    async fn authenticate(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        source: SocketAddr,
        headers: &HeaderMap,
    ) -> Result<super::AuthenticatedPeer, ControlHttpError> {
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

    async fn handle(
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

    async fn prepare_worker(&self) -> anyhow::Result<()> {
        let worker = self
            .inner
            .worker_runtime
            .as_ref()
            .context("worker lifecycle unavailable")?;
        let current = self.inner.mode.snapshot();
        let awaiting = self
            .inner
            .mode
            .cluster_handle()
            .apply(super::ClusterEvent {
                expected_generation: current.generation,
                kind: super::ClusterEventKind::BeginPromotion,
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
            .apply(super::ClusterEvent {
                expected_generation: generation,
                kind: super::ClusterEventKind::WorkerHelloAccepted,
            })
            .await?;
        let starting = self
            .inner
            .mode
            .cluster_handle()
            .apply(super::ClusterEvent {
                expected_generation: promoting.generation,
                kind: super::ClusterEventKind::DistributedChildStarted,
            })
            .await?;
        let ready = self
            .inner
            .mode
            .cluster_handle()
            .apply(super::ClusterEvent {
                expected_generation: starting.generation,
                kind: super::ClusterEventKind::DistributedRouteReady,
            })
            .await?;
        self.inner.proxy.set_target(ready.target, true);
        let message = match &self.inner.control {
            RoleControl::Worker(control) => control
                .lock()
                .await
                .worker_ready_message(uuid::Uuid::new_v4().to_string())?,
            RoleControl::Coordinator(_) => anyhow::bail!("prepare-worker received by coordinator"),
        };
        self.inner.client.send(&message).await?;
        Ok(())
    }

    async fn worker_drained(&self) -> anyhow::Result<()> {
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

    async fn stop_worker(&self) -> anyhow::Result<()> {
        if let Some(worker) = &self.inner.worker_runtime {
            let current = self.inner.mode.snapshot();
            worker.cancel().await?;
            let paired = match current.state {
                ClusterState::DistributedReady => {
                    let demoting = self
                        .inner
                        .mode
                        .cluster_handle()
                        .apply(super::ClusterEvent {
                            expected_generation: current.generation,
                            kind: super::ClusterEventKind::BeginDemotion,
                        })
                        .await?;
                    self.inner
                        .mode
                        .cluster_handle()
                        .apply(super::ClusterEvent {
                            expected_generation: demoting.generation,
                            kind: super::ClusterEventKind::PairingReady,
                        })
                        .await?
                }
                ClusterState::AwaitingWorkerHello
                | ClusterState::Promoting
                | ClusterState::DistributedStarting => {
                    self.inner
                        .mode
                        .cluster_handle()
                        .apply(super::ClusterEvent {
                            expected_generation: current.generation,
                            kind: super::ClusterEventKind::PromotionFailed,
                        })
                        .await?
                }
                ClusterState::PairedStandaloneReady => current,
                state => anyhow::bail!("worker cannot stop distributed child from {state:?}"),
            };
            self.inner.proxy.set_target(paired.target, true);
            self.inner.proxy.admission().start_serving();
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
                if let Err(error) = runtime.reconcile().await {
                    tracing::error!(error = %error, "production cluster reconcile failed");
                }
                let snapshot = runtime.inner.mode.snapshot();
                if snapshot.state == ClusterState::SoloStandaloneReady
                    && runtime.inner.config.cluster.policy.auto_pair
                {
                    if let Err(error) = runtime.pair().await {
                        tracing::debug!(error = %error, "automatic pairing attempt failed");
                    }
                } else if snapshot.state == ClusterState::PairedStandaloneReady
                    && runtime.inner.role == LocalRole::Coordinator
                    && runtime.inner.config.cluster.policy.auto_promote
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
            super::DistributedWorkerLifecycle::stop(worker.as_ref()).await?;
        }
        if let Some(coordinator) = &self.inner.distributed_coordinator {
            super::DistributedCoordinatorLifecycle::stop(coordinator.as_ref()).await?;
        }
        Ok(())
    }

    pub async fn distributed_child_identity(&self) -> Option<super::ChildIdentity> {
        match self.inner.role {
            LocalRole::Coordinator => match &self.inner.distributed_coordinator {
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

// This type is filled out together with coordinator promotion below; keeping transport lifecycle
// behavior here makes it impossible for admin operations to bypass authenticated control.
struct PeerLifecycle {
    inner: Weak<ProductionInner>,
}

impl CoordinatorPeerLifecycle for PeerLifecycle {
    fn begin_drain(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let inner = inner.upgrade().context("production runtime stopped")?;
            let RoleControl::Coordinator(control) = &inner.control else {
                anyhow::bail!("coordinator control unavailable")
            };
            let message = {
                let mut control = control.lock().await;
                let message = control.begin_drain_message(uuid::Uuid::new_v4().to_string())?;
                control.note_begin_drain_sent(message.generation)?;
                message
            };
            inner.client.send(&message).await?;
            let deadline = tokio::time::Instant::now() + inner.config.cluster.timeouts.drain;
            loop {
                if control.lock().await.phase() == super::DistributedControlPhase::Drained {
                    return Ok(());
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "worker drain acknowledgement timed out"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }

    fn stop_worker(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let inner = inner.upgrade().context("production runtime stopped")?;
            let RoleControl::Coordinator(control) = &inner.control else {
                anyhow::bail!("coordinator control unavailable")
            };
            let message = {
                control
                    .lock()
                    .await
                    .demote_message(uuid::Uuid::new_v4().to_string())?
            };
            inner.client.send(&message).await?;
            control
                .lock()
                .await
                .note_demote_complete(message.generation)?;
            Ok(())
        })
    }
}

#[derive(Debug)]
enum ControlHttpError {
    Auth(AuthError),
    Control(super::ControlError),
    Effect(String),
    MissingHeader(&'static str),
    BadJson(String),
}

impl From<AuthError> for ControlHttpError {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}

impl From<super::ControlError> for ControlHttpError {
    fn from(value: super::ControlError) -> Self {
        Self::Control(value)
    }
}

impl IntoResponse for ControlHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Auth(error) => (StatusCode::UNAUTHORIZED, error.to_string()),
            Self::Control(error) => (
                StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST),
                error.to_string(),
            ),
            Self::Effect(error) => (StatusCode::INTERNAL_SERVER_ERROR, error),
            Self::MissingHeader(name) => (StatusCode::UNAUTHORIZED, format!("missing {name}")),
            Self::BadJson(error) => (StatusCode::BAD_REQUEST, error),
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

fn effect_requires_ack(command: &ControlCommand) -> bool {
    matches!(
        command,
        ControlCommand::CancelGeneration
            | ControlCommand::DistributedReady
            | ControlCommand::Demote
    )
}

fn header<'a>(headers: &'a HeaderMap, name: &'static str) -> Result<&'a str, ControlHttpError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(ControlHttpError::MissingHeader(name))
}

macro_rules! control_handler {
    ($name:ident, $endpoint:expr, $method:literal) => {
        async fn $name(
            State(runtime): State<ProductionClusterRuntime>,
            ConnectInfo(source): ConnectInfo<SocketAddr>,
            headers: HeaderMap,
            body: Bytes,
        ) -> Result<Json<ControlResponse>, ControlHttpError> {
            Ok(Json(
                runtime
                    .handle($endpoint, $method, body, source, headers)
                    .await?,
            ))
        }
    };
}

control_handler!(control_node, ControlEndpoint::Node, "GET");
control_handler!(control_pair, ControlEndpoint::Pair, "POST");
control_handler!(control_prepare, ControlEndpoint::PrepareWorker, "POST");
control_handler!(control_begin_drain, ControlEndpoint::BeginDrain, "POST");
control_handler!(control_drained, ControlEndpoint::Drained, "POST");
control_handler!(control_cancel, ControlEndpoint::CancelGeneration, "POST");
control_handler!(control_worker_event, ControlEndpoint::WorkerEvent, "POST");
control_handler!(
    control_distributed_ready,
    ControlEndpoint::DistributedReady,
    "POST"
);
control_handler!(control_demote, ControlEndpoint::Demote, "POST");

fn endpoint_path(endpoint: ControlEndpoint) -> &'static str {
    match endpoint {
        ControlEndpoint::Node => "/v1/node",
        ControlEndpoint::Pair => "/v1/pair",
        ControlEndpoint::PrepareWorker => "/v1/prepare-worker",
        ControlEndpoint::BeginDrain => "/v1/begin-drain",
        ControlEndpoint::Drained => "/v1/drained",
        ControlEndpoint::CancelGeneration => "/v1/cancel-generation",
        ControlEndpoint::WorkerEvent => "/v1/worker-event",
        ControlEndpoint::DistributedReady => "/v1/distributed-ready",
        ControlEndpoint::Demote => "/v1/demote",
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(target_os = "macos")]
pub fn detect_cluster_role(
    interface: &str,
    coordinator: IpAddr,
    worker: IpAddr,
) -> anyhow::Result<LocalRole> {
    use std::ffi::CStr;
    let mut raw = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes a linked list owned until freeifaddrs.
    if unsafe { libc::getifaddrs(&mut raw) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut matches = Vec::new();
    let mut current = raw;
    while !current.is_null() {
        // SAFETY: current belongs to the live getifaddrs list.
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null()
            && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET
        {
            let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
            if name == interface {
                let socket = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in>()) };
                matches.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    socket.sin_addr.s_addr,
                ))));
            }
        }
        current = entry.ifa_next;
    }
    // SAFETY: raw is the head returned by getifaddrs and is freed exactly once.
    unsafe { libc::freeifaddrs(raw) };
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [address] if *address == coordinator => Ok(LocalRole::Coordinator),
        [address] if *address == worker => Ok(LocalRole::Worker),
        [] => anyhow::bail!("cluster interface {interface} has no IPv4 address"),
        _ => anyhow::bail!(
            "cluster interface {interface} has conflicting IPv4 addresses: {matches:?}"
        ),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn detect_cluster_role(
    _interface: &str,
    _coordinator: IpAddr,
    _worker: IpAddr,
) -> anyhow::Result<LocalRole> {
    anyhow::bail!("production cluster role detection requires macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn signed_pair(
        State(authenticator): State<Arc<ControlAuthenticator>>,
        ConnectInfo(source): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Json<ControlResponse>, ControlHttpError> {
        let signed = SignedControlHeaders::from_header_values(
            header(&headers, HEADER_NODE)?,
            header(&headers, HEADER_TIMESTAMP)?,
            header(&headers, HEADER_NONCE)?,
            header(&headers, HEADER_SIGNATURE)?,
        )?;
        ControlRequest {
            method: "POST",
            path_and_query: "/v1/pair",
            body: &body,
            source_ip: source.ip(),
            headers: &signed,
        }
        .authenticate(&authenticator, now_millis())?;
        let message: ControlMessage = serde_json::from_slice(&body)
            .map_err(|error| ControlHttpError::BadJson(error.to_string()))?;
        Ok(Json(ControlResponse {
            status: super::super::ControlResponseStatus::Applied,
            generation: message.generation,
            descriptor: NodeDescriptor {
                protocol_version: 1,
                node_id: "worker-node".into(),
                role: ControlRole::Worker,
                generation: message.generation,
                mode: super::super::ControlMode::SoloStandalone,
                deployment_id: message.deployment_id,
            },
            lease_expires_at_millis: Some(now_millis() + 1_000),
        }))
    }

    #[tokio::test]
    async fn real_http_client_pins_source_and_authenticates_control_body() {
        let secret = vec![0x61; 32];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/v1/pair", post(signed_pair))
            .with_state(Arc::new(ControlAuthenticator::new_at_source(
                ControlSecret::new(secret.clone()).unwrap(),
                "127.0.0.1".parse().unwrap(),
            )));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = ProductionControlClient::new(
            "coordinator-node".into(),
            "127.0.0.1".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            port,
            secret,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let response = client
            .send(&ControlMessage {
                request_id: "production-http-pair".into(),
                generation: 4,
                deployment_id: Some("deployment-production".into()),
                command: ControlCommand::Pair {
                    descriptor: NodeDescriptor {
                        protocol_version: 1,
                        node_id: "coordinator-node".into(),
                        role: ControlRole::Coordinator,
                        generation: 4,
                        mode: super::super::ControlMode::SoloStandalone,
                        deployment_id: Some("deployment-production".into()),
                    },
                },
            })
            .await
            .unwrap();
        assert_eq!(response.descriptor.node_id, "worker-node");
        assert_eq!(response.generation, 4);
        server.abort();
    }

    #[test]
    fn destructive_worker_effects_are_acknowledged_after_completion() {
        assert!(effect_requires_ack(&ControlCommand::Demote));
        assert!(effect_requires_ack(&ControlCommand::CancelGeneration));
        assert!(effect_requires_ack(&ControlCommand::DistributedReady));
        assert!(!effect_requires_ack(&ControlCommand::PrepareWorker));
        assert!(!effect_requires_ack(&ControlCommand::BeginDrain));
    }
}
