use super::{
    AuthError, ControlAuthenticator, ControlCommand, ControlEndpoint, ControlMessage,
    ControlResponse, ControlRole, ControlSecret, CoordinatorControl, CoordinatorDistributedRuntime,
    CoordinatorPeerLifecycle, CoordinatorRuntimeTimeouts, DistributedCoordinatorSupervisor,
    DistributedManifest, DistributedWorkerSupervisor, HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE,
    HEADER_TIMESTAMP, ModeRuntime, NodeDescriptor, PromotionRetryPolicy, StandaloneSupervisor,
    WorkerControl, WorkerDistributedRuntime,
};
use crate::{config::ModeAwareConfig, proxy::ModeAwareProxyState, target::LocalRole};
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
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

mod effects;
mod pairing;
mod reconcile;
mod worker;

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
    use crate::cluster::{ControlRequest, SignedControlHeaders};

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
