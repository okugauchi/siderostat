use super::{
    AuthError, ControlAuthenticator, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlMode, ControlResponse, ControlRole, ControlSecret, CoordinatorControl,
    CoordinatorDistributedRuntime, CoordinatorPeerLifecycle, CoordinatorRuntimeTimeouts,
    DistributedControlPhase, DistributedCoordinatorLifecycle, DistributedCoordinatorSupervisor,
    DistributedManifest, DistributedWorkerLifecycle, DistributedWorkerSupervisor, HEADER_NODE,
    HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, InterfaceObservation, Ipv4Assignment,
    LocalStandaloneLifecycle, MacOsDynamicStoreWatcher, ModeRuntime, NetworkEvidence,
    NetworkObservation, NetworkServiceObservation, NetworkSnapshot, NodeDescriptor,
    PeerObservation, PromotionRetryPolicy, StandaloneSupervisor, WorkerControl,
    WorkerDistributedRuntime, spawn_network_event_monitor,
};
#[cfg(feature = "test-support")]
use crate::cluster::{ClusterFailure, ClusterSnapshot, PromotionFailureStatus};
use crate::{
    cluster::{ClusterEvent, ClusterEventKind},
    config::{ModeAwareConfig, SpeculativeSupport},
    metrics::{MetricSnapshot, Metrics},
    proxy::ModeAwareProxyState,
    target::{ClusterState, LocalRole},
};
use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE},
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

mod effects;
mod pairing;
mod reconcile;
mod recovery;
mod worker;

const CONTROL_METRICS_PATH: &str = "/v1/metrics";

/// The production control transport. Requests are pinned to the cluster address and authenticated
/// independently from the public/admin planes.
#[cfg(feature = "test-support")]
use super::ThunderboltIpState;

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

/// Outcome of an operator reconcile on the production runtime (B-03). Distinguishes whether a
/// coordinator promotion tracker was present so the admin response can be explicit for worker /
/// role-unknown nodes and for non-manual coordinator states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorReconcileOutcome {
    /// A coordinator runtime existed: the promotion tracker was reset and, if the node was in
    /// `ManualInterventionRequired`, the state was cleared as one atomic operation.
    Coordinator { manual_cleared: bool },
    /// No coordinator promotion tracker on this node (worker or unknown role). The local manual
    /// state, if any, was cleared through the mode runtime.
    NotCoordinator { manual_cleared: bool },
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

    fn with_lifecycle_timeout(mut self, timeout: Duration) -> anyhow::Result<Self> {
        Arc::get_mut(&mut self.inner)
            .context("control client was unexpectedly cloned during construction")?
            .lifecycle_timeout = timeout;
        Ok(self)
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

    async fn metrics(&self) -> anyhow::Result<String> {
        let timestamp = now_millis();
        let body = Vec::new();
        let signed = self.inner.authenticator.sign(
            self.inner.local_node_id.clone(),
            reqwest::Method::GET.as_str(),
            CONTROL_METRICS_PATH,
            timestamp,
            uuid::Uuid::new_v4().simple().to_string(),
            &body,
        )?;
        let response = self
            .inner
            .client
            .get(
                self.inner
                    .base
                    .join(CONTROL_METRICS_PATH.trim_start_matches('/'))?,
            )
            .header(HEADER_NODE, signed.node_id())
            .header(HEADER_TIMESTAMP, signed.timestamp_millis())
            .header(HEADER_NONCE, signed.nonce())
            .header(HEADER_SIGNATURE, signed.signature())
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            anyhow::bail!(
                "peer control {} returned {}: {}",
                CONTROL_METRICS_PATH,
                status,
                String::from_utf8_lossy(&body)
            );
        }
        Ok(String::from_utf8(body.to_vec())?)
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
            if status == StatusCode::PRECONDITION_FAILED {
                return Err(anyhow::Error::new(ControlError::DeploymentMismatch));
            }
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
    metrics: Arc<Metrics>,
    control: RoleControl,
    client: ProductionControlClient,
    lease: LeaseStatus,
    mode: Arc<ModeRuntime>,
    proxy: Arc<ModeAwareProxyState>,
    standalone: Arc<dyn LocalStandaloneLifecycle>,
    worker_runtime: Option<WorkerDistributedRuntime>,
    coordinator_runtime: OnceLock<CoordinatorDistributedRuntime>,
    distributed_coordinator: Option<Arc<dyn DistributedCoordinatorLifecycle>>,
    distributed_worker: Option<Arc<dyn DistributedWorkerLifecycle>>,
    config: ModeAwareConfig,
    manifest: DistributedManifest,
    recovery: Arc<recovery::PeerLossRecovery>,
    automatic_pairing_blocked: AtomicBool,
    /// Shared, latest verified network snapshot. The control handler derives `route_scoped`
    /// from this instead of a hard-coded `true` (N-02), so peer-present gating comes from
    /// actual production input. Fail-closed until a fresh observation is applied.
    network: Arc<NetworkEvidence>,
    /// Test-support only: recorded pair-phase timings across pair sessions (Q-01).
    #[cfg(feature = "test-support")]
    pair_timings: std::sync::Mutex<Vec<PairTiming>>,
}

impl ProductionClusterRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ModeAwareConfig,
        role: LocalRole,
        mode: Arc<ModeRuntime>,
        proxy: Arc<ModeAwareProxyState>,
        standalone: Arc<StandaloneSupervisor>,
        metrics: Arc<Metrics>,
        manifest: DistributedManifest,
        control_secret: Vec<u8>,
        control_session_generation: Option<u64>,
    ) -> anyhow::Result<Self> {
        let worker_child: Option<Arc<dyn DistributedWorkerLifecycle>> = if role == LocalRole::Worker
        {
            Some(Arc::new(DistributedWorkerSupervisor::new(
                super::build_distributed_worker_command(
                    &config.ds4,
                    config.cluster.coordinator_address,
                    config.cluster.ds4_distributed_port,
                )?,
                config.cluster.timeouts.stop,
                config.ds4.allow_sigkill,
                metrics.clone(),
            )))
        } else {
            None
        };
        let coordinator_child: Option<Arc<dyn DistributedCoordinatorLifecycle>> =
            if role == LocalRole::Coordinator {
                Some(Arc::new(DistributedCoordinatorSupervisor::new(
                    super::build_distributed_coordinator_command(
                        &config.ds4,
                        config.cluster.coordinator_address,
                        config.cluster.ds4_distributed_port,
                    )?,
                    Url::parse(&format!(
                        "http://{}:{}/v1/models",
                        config.ds4.http_host, config.ds4.http_port
                    ))?,
                    config.cluster.timeouts.coordinator_startup,
                    Duration::from_secs(1),
                    config.cluster.timeouts.stop,
                    config.ds4.allow_sigkill,
                    metrics.clone(),
                )))
            } else {
                None
            };
        let peer_control_port = config.cluster.control_port;
        let runtime = Self::new_inner(
            config,
            role,
            mode,
            proxy,
            standalone,
            metrics,
            manifest,
            control_secret,
            peer_control_port,
            control_session_generation,
            worker_child,
            coordinator_child,
        )?;
        // N-02: share verified network observations into the control plane so `route_scoped` is
        // measured, not hard-coded. The monitor runs only for production (`new`); tests inject
        // evidence directly via `new_with_lifecycles`.
        runtime.start_network_evidence_monitor();
        Ok(runtime)
    }

    /// Shared constructor. Production uses [`Self::new`]; tests inject fake child lifecycles via
    /// [`Self::new_with_lifecycles`] (test-support only).
    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        config: ModeAwareConfig,
        role: LocalRole,
        mode: Arc<ModeRuntime>,
        proxy: Arc<ModeAwareProxyState>,
        standalone: Arc<dyn LocalStandaloneLifecycle>,
        metrics: Arc<Metrics>,
        manifest: DistributedManifest,
        control_secret: Vec<u8>,
        peer_control_port: u16,
        control_session_generation: Option<u64>,
        worker_child: Option<Arc<dyn DistributedWorkerLifecycle>>,
        coordinator_child: Option<Arc<dyn DistributedCoordinatorLifecycle>>,
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
        let control_session_generation =
            control_session_generation.unwrap_or_else(|| mode.snapshot().generation);
        let descriptor = NodeDescriptor {
            protocol_version: 1,
            node_id: local_control_id.clone(),
            role: control_role,
            generation: control_session_generation,
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
            peer_control_port,
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
        )?;
        let lease = LeaseStatus::new();
        let (worker_runtime, distributed_worker) = if role == LocalRole::Worker {
            let child = worker_child.context("worker child lifecycle missing")?;
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
        let inner = ProductionInner {
            role,
            local_address,
            peer_address,
            descriptor,
            authenticator: ControlAuthenticator::new_at_source(
                ControlSecret::new(control_secret)?,
                peer_address,
            ),
            metrics,
            control,
            client,
            lease,
            mode,
            proxy,
            standalone,
            worker_runtime,
            coordinator_runtime: OnceLock::new(),
            distributed_coordinator: coordinator_child,
            distributed_worker,
            config,
            manifest,
            recovery: Arc::new(recovery::PeerLossRecovery::default()),
            automatic_pairing_blocked: AtomicBool::new(false),
            network: Arc::new(NetworkEvidence::new()),
            #[cfg(feature = "test-support")]
            pair_timings: std::sync::Mutex::new(Vec::new()),
        };
        let runtime = Self {
            inner: Arc::new(inner),
        };
        runtime.finish_coordinator()?;
        Ok(runtime)
    }

    #[cfg(feature = "test-support")]
    #[allow(clippy::too_many_arguments)]
    /// Build a production-equivalent runtime for tests with injected fake child lifecycles.
    /// The control HTTP layer and state machine are identical to production; only the child
    /// process supervisors are replaced so tests can record start/stop and inject faults.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_lifecycles(
        config: ModeAwareConfig,
        role: LocalRole,
        mode: Arc<ModeRuntime>,
        proxy: Arc<ModeAwareProxyState>,
        standalone: Arc<dyn LocalStandaloneLifecycle>,
        manifest: DistributedManifest,
        control_secret: Vec<u8>,
        peer_control_port: u16,
        control_session_generation: Option<u64>,
        worker_child: Option<Arc<dyn DistributedWorkerLifecycle>>,
        coordinator_child: Option<Arc<dyn DistributedCoordinatorLifecycle>>,
    ) -> anyhow::Result<Self> {
        Self::new_inner(
            config,
            role,
            mode,
            proxy,
            standalone,
            Arc::new(Metrics::default()),
            manifest,
            control_secret,
            peer_control_port,
            control_session_generation,
            worker_child,
            coordinator_child,
        )
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

    #[cfg(feature = "test-support")]
    /// Record a promotion failure on the coordinator's promotion tracker (test-support only).
    /// Used to drive a production runtime into Backoff so the periodic-reconcile recovery path
    /// (B-02) can be exercised without waiting on a real promotion timeout.
    pub async fn record_promotion_failure(
        &self,
        failure: ClusterFailure,
        now_millis: u64,
    ) -> anyhow::Result<ClusterSnapshot> {
        let runtime = self
            .inner
            .coordinator_runtime
            .get()
            .context("coordinator runtime unavailable")?;
        runtime
            .record_promotion_failure(failure, now_millis)
            .await
            .map_err(Into::into)
    }

    #[cfg(feature = "test-support")]
    /// Read the coordinator's promotion tracker status (test-support only). Used to assert that
    /// operator reconcile resets the manual failure count (B-03 completion condition).
    pub async fn promotion_failure_status(&self) -> Option<PromotionFailureStatus> {
        let runtime = self.inner.coordinator_runtime.get()?;
        Some(runtime.promotion_failure_status().await)
    }

    #[cfg(feature = "test-support")]
    /// Inject a verified network snapshot into the shared network evidence (test-support only).
    /// Used by the reconnect harness to give each node a valid, bridge0-scoped, authenticated
    /// peer candidate so the production control plane derives `route_scoped` from production
    /// input rather than a hard-coded constant (N-02). Returns `false` when the injected
    /// snapshot is stale relative to the latest applied epoch.
    pub fn set_network_evidence(&self, snapshot: NetworkSnapshot) -> bool {
        self.inner.network.update(snapshot)
    }

    #[cfg(feature = "test-support")]
    /// Read the current network gate state derived from the shared evidence (test-support only):
    /// `(route_scoped, peer_present, epoch, state)`. Used by the N-03 truth-table tests to
    /// assert that every `ThunderboltIpState` maps to the expected route/peer-present outcome.
    pub fn network_gate_status(&self) -> (bool, bool, u64, ThunderboltIpState) {
        let snapshot = self.inner.network.snapshot();
        (
            self.inner.network.route_scoped(),
            snapshot.peer_present,
            snapshot.epoch,
            snapshot.state,
        )
    }

    #[cfg(feature = "test-support")]
    /// Read the recorded pair-phase timings (test-support only, Q-01). Returns a copy so tests
    /// can aggregate confirm-before-stability and convergence without holding the lock.
    pub fn pair_timings(&self) -> Vec<PairTiming> {
        self.inner
            .pair_timings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect()
    }

    /// Operator reconcile (B-03). On the coordinator the promotion tracker reset and the
    /// `OperatorReconcile` state event are one atomic operation through
    /// [`CoordinatorDistributedRuntime::operator_reconcile`], so the manual failure count does
    /// not carry over after the state is cleared. On a worker / role-unknown node there is no
    /// coordinator promotion tracker, so the local manual state is cleared through the mode
    /// runtime instead.
    pub async fn operator_reconcile(&self) -> anyhow::Result<OperatorReconcileOutcome> {
        self.inner
            .automatic_pairing_blocked
            .store(false, Ordering::Release);
        let was_manual =
            self.inner.mode.snapshot().state == ClusterState::ManualInterventionRequired;
        if let Some(runtime) = self.inner.coordinator_runtime.get() {
            runtime.operator_reconcile().await?;
            return Ok(OperatorReconcileOutcome::Coordinator {
                manual_cleared: was_manual,
            });
        }
        if was_manual {
            let current = self.inner.mode.snapshot();
            self.inner
                .mode
                .cluster_handle()
                .apply(ClusterEvent {
                    expected_generation: current.generation,
                    kind: ClusterEventKind::OperatorReconcile,
                })
                .await?;
        }
        Ok(OperatorReconcileOutcome::NotCoordinator {
            manual_cleared: was_manual,
        })
    }

    pub fn role(&self) -> LocalRole {
        self.inner.role
    }

    pub(super) fn block_automatic_pairing(&self) {
        self.inner
            .automatic_pairing_blocked
            .store(true, Ordering::Release);
    }

    pub(super) fn automatic_pairing_blocked(&self) -> bool {
        self.inner.automatic_pairing_blocked.load(Ordering::Acquire)
    }

    /// Fetch the coordinator's Prometheus metrics through the authenticated control plane.
    ///
    /// The worker must not connect to the coordinator's loopback-only admin listener directly.
    /// This read-only path keeps the admin API private while reusing the existing source-pinned
    /// HMAC control channel.
    pub async fn fetch_coordinator_metrics(&self) -> anyhow::Result<String> {
        ensure!(
            self.inner.role == LocalRole::Worker,
            "coordinator metrics can only be fetched from a worker"
        );
        self.inner.client.metrics().await
    }

    pub(super) fn render_control_metrics(&self) -> String {
        self.inner.metrics.render_mode_aware(MetricSnapshot {
            node_id: &self.inner.descriptor.node_id,
            interface: &self.inner.config.cluster.interface,
            target: self.inner.proxy.target_snapshot(),
            admission: self.inner.proxy.admission().snapshot(),
            cluster: Some(self.inner.mode.snapshot()),
            peer_lease_seconds: 0.0,
            thunderbolt_ip_state: "unknown",
            discovery_results: 0,
            quantization: self.inner.config.ds4.standalone.quantization,
            speculative_support: if self.inner.config.ds4.dspark.enabled {
                SpeculativeSupport::Dspark
            } else {
                SpeculativeSupport::None
            },
            residency: self.inner.config.ds4.standalone.residency,
        })
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
            .route("/v1/metrics", get(control_metrics))
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

    /// Read-only diagnostics snapshot for the reconnect diagnostics contract
    /// (`docs/reconnect-diagnostics-contract.md`). Never mutates the runtime and never exposes
    /// secrets, signatures, nonces, or full deployment IDs.
    pub async fn diagnostics(&self) -> ProductionDiagnostics {
        let inner = &self.inner;
        let now = now_millis();
        let (generation, phase, role, lease) = match &inner.control {
            RoleControl::Coordinator(control) => {
                let control = control.lock().await;
                (
                    control.generation(),
                    control.phase(),
                    ControlRole::Coordinator,
                    lease_diagnostics(control.peer_lease(), now),
                )
            }
            RoleControl::Worker(control) => {
                let control = control.lock().await;
                (
                    control.generation(),
                    control.phase(),
                    ControlRole::Worker,
                    lease_diagnostics(control.peer_lease(), now),
                )
            }
        };
        let standalone_ready = inner.mode.snapshot().local_standalone_ready;
        let children = ChildrenDiagnostics {
            standalone: Some(child_diagnostics(
                inner.standalone.child_identity().await,
                inner.standalone.is_running().await.ok(),
                Some(standalone_ready),
            )),
            distributed_coordinator: match &inner.distributed_coordinator {
                Some(supervisor) => Some(child_diagnostics(
                    supervisor.child_identity().await,
                    supervisor.is_running().await.ok(),
                    None,
                )),
                None => None,
            },
            distributed_worker: match &inner.distributed_worker {
                Some(supervisor) => Some(child_diagnostics(
                    supervisor.child_identity().await,
                    supervisor.is_running().await.ok(),
                    None,
                )),
                None => None,
            },
        };
        ProductionDiagnostics {
            control_session: ControlSessionDiagnostics {
                generation,
                phase,
                role,
                lease,
            },
            children,
        }
    }
}

fn lease_diagnostics(lease: &super::PeerLease, now: u64) -> LeaseDiagnostics {
    let expires_at = lease.expires_at_millis();
    let valid = expires_at.is_some_and(|expires| now < expires);
    LeaseDiagnostics {
        valid,
        expires_at_millis: expires_at,
        route_scoped: lease.route_scoped(),
        peer_present: lease.peer_present(now),
        peer: lease.descriptor().map(|descriptor| PeerDiagnostics {
            node_id: descriptor.node_id.clone(),
            role: descriptor.role,
            generation: descriptor.generation,
            mode: descriptor.mode,
        }),
    }
}

fn child_diagnostics(
    identity: Option<crate::cluster::ChildIdentity>,
    running: Option<bool>,
    ready: Option<bool>,
) -> ChildDiagnostics {
    ChildDiagnostics {
        pid: identity.as_ref().map(|identity| identity.pid),
        profile: identity
            .as_ref()
            .map(|identity| identity.profile_id.clone()),
        generation: identity.as_ref().map(|identity| identity.generation),
        running: running.unwrap_or(false),
        ready,
    }
}

/// Read-only reconnect diagnostics. Field semantics are fixed by
/// `docs/reconnect-diagnostics-contract.md`; only additive changes are allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionDiagnostics {
    pub control_session: ControlSessionDiagnostics,
    pub children: ChildrenDiagnostics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSessionDiagnostics {
    pub generation: u64,
    pub phase: DistributedControlPhase,
    pub role: ControlRole,
    pub lease: LeaseDiagnostics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseDiagnostics {
    pub valid: bool,
    pub expires_at_millis: Option<u64>,
    pub route_scoped: bool,
    pub peer_present: bool,
    pub peer: Option<PeerDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDiagnostics {
    pub node_id: String,
    pub role: ControlRole,
    pub generation: u64,
    pub mode: ControlMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildrenDiagnostics {
    pub standalone: Option<ChildDiagnostics>,
    pub distributed_coordinator: Option<ChildDiagnostics>,
    pub distributed_worker: Option<ChildDiagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildDiagnostics {
    pub pid: Option<u32>,
    pub profile: Option<String>,
    pub generation: Option<u64>,
    pub running: bool,
    /// `Some` for the standalone child (readiness confirmed); `None` for distributed children.
    pub ready: Option<bool>,
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
        ControlCommand::Pair { .. }
            | ControlCommand::PrepareWorker
            | ControlCommand::CancelGeneration
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

async fn control_metrics(
    State(runtime): State<ProductionClusterRuntime>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ControlHttpError> {
    let body = runtime.handle_metrics(source, headers).await?;
    let mut response = Response::new(body.into());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(response)
}

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
            // SAFETY: `ifa_addr` is non-null and points to the live getifaddrs entry.
            && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET
        {
            // SAFETY: `ifa_name` is a valid NUL-terminated interface name in the live list.
            let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
            if name == interface {
                // SAFETY: the address family was checked as AF_INET above.
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
        // Thunderboltケーブル未接続などでIPv4アドレスが未設定の場合、クラスタ判定は
        // 保留(Unknown)として扱う。siderostatはstandaloneで動作できる設計であり、起動を失敗させてはならない。
        // アドレスが複数ある場合のみ設定矛盾としてbailする。
        [] => Ok(LocalRole::Unknown),
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

/// IPv4 addresses and link state observed on an interface via `getifaddrs`.
#[cfg(target_os = "macos")]
struct ObservedInterfaceIpv4 {
    addresses: Vec<Ipv4Assignment>,
    up: bool,
}

#[cfg(target_os = "macos")]
fn collect_interface_ipv4(interface: &str) -> anyhow::Result<ObservedInterfaceIpv4> {
    use std::ffi::CStr;
    let mut raw = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes a linked list owned until freeifaddrs.
    if unsafe { libc::getifaddrs(&mut raw) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut up = false;
    let mut addresses = Vec::new();
    let mut current = raw;
    while !current.is_null() {
        // SAFETY: current belongs to the live getifaddrs list.
        let entry = unsafe { &*current };
        // SAFETY: `ifa_name` is a valid NUL-terminated interface name in the live list.
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }.to_string_lossy();
        if name == interface {
            if entry.ifa_flags & libc::IFF_UP as u32 != 0 {
                up = true;
            }
            if !entry.ifa_addr.is_null()
                // SAFETY: `ifa_addr` is non-null and points to the live getifaddrs entry.
                && unsafe { (*entry.ifa_addr).sa_family } as i32 == libc::AF_INET
            {
                // SAFETY: the address family was checked as AF_INET above.
                let socket = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in>()) };
                let address = Ipv4Addr::from(u32::from_be(socket.sin_addr.s_addr));
                let prefix = if entry.ifa_netmask.is_null() {
                    0
                } else {
                    // SAFETY: the non-null netmask is an IPv4 sockaddr for an AF_INET address.
                    let mask = unsafe { &*(entry.ifa_netmask.cast::<libc::sockaddr_in>()) };
                    u32::from_be(mask.sin_addr.s_addr).count_ones() as u8
                };
                addresses.push(Ipv4Assignment { address, prefix });
            }
        }
        current = entry.ifa_next;
    }
    // SAFETY: raw is the head returned by getifaddrs and is freed exactly once.
    unsafe { libc::freeifaddrs(raw) };
    addresses.sort_by_key(|a| a.address);
    addresses.dedup_by_key(|a| a.address);
    Ok(ObservedInterfaceIpv4 { addresses, up })
}

/// Build a [`NetworkObservation`] from the live interface state (N-02). Uses only `getifaddrs`
/// (spec §13.1), never shell output parsing or network configuration changes (N-02 停止条件).
/// The peer candidate is the configured counterpart; its route is treated as `bridge0`-scoped
/// when the peer shares a subnet with a local `bridge0` address (fixed topology, spec §13.1
/// item 6). Fail-closed: if the interface is missing, down, or has no IPv4 address, the service
/// observation is `None`, which maps to `ServiceMissing`/`InterfaceUnavailable` and never to a
/// peer-present state.
#[cfg(target_os = "macos")]
pub fn observe_network_observation(
    interface: &str,
    coordinator: IpAddr,
    worker: IpAddr,
    peer_address: IpAddr,
) -> anyhow::Result<NetworkObservation> {
    let IpAddr::V4(coordinator) = coordinator else {
        anyhow::bail!("cluster coordinator address must be IPv4");
    };
    let IpAddr::V4(worker) = worker else {
        anyhow::bail!("cluster worker address must be IPv4");
    };
    let IpAddr::V4(peer) = peer_address else {
        anyhow::bail!("cluster peer address must be IPv4");
    };
    let observed = collect_interface_ipv4(interface)?;
    let service = if !observed.up || observed.addresses.is_empty() {
        None
    } else {
        Some(NetworkServiceObservation {
            enabled: true,
            ipv4_enabled: true,
            configured_addresses: observed.addresses.clone(),
        })
    };
    let interface_observation = InterfaceObservation {
        name: interface.to_string(),
        up: observed.up,
        ipv4_addresses: observed.addresses.clone(),
    };
    let route_scoped = observed.addresses.iter().any(|assignment| {
        let mask = if assignment.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - assignment.prefix)
        };
        u32::from(assignment.address) & mask == u32::from(peer) & mask
    });
    let peer_observation = PeerObservation {
        candidate_address: Some(peer),
        route_scoped_to_interface: route_scoped,
        authenticated: false,
    };
    Ok(NetworkObservation {
        // The caller (monitor) fills the monotonic epoch per rescan.
        epoch: 0,
        expected_interface: interface.to_string(),
        coordinator_address: coordinator,
        worker_address: worker,
        // The initial topology fixes the peer pair on a /30 (spec §13.1). The interface
        // observation carries the actual netmask-derived prefix for role assessment.
        expected_prefix: 30,
        service,
        interface: Some(interface_observation),
        peer: peer_observation,
    })
}

#[cfg(target_os = "macos")]
impl ProductionClusterRuntime {
    /// Start the production network-evidence monitor (N-02 action 1 & 4). Reuses
    /// `spawn_network_event_monitor` + `MacOsDynamicStoreWatcher` so a fresh observation is
    /// produced on the initial rescan, after debounced link/ipv4/setup events, and on the
    /// periodic reconcile interval (recovering lost events). Each rescan assigns a strictly
    /// increasing epoch; the shared [`NetworkEvidence`] rejects stale observations. The task is
    /// detached and owns the dynamic-store watcher, living for the process lifetime.
    fn start_network_evidence_monitor(&self) {
        let config = &self.inner.config;
        let evidence = self.inner.network.clone();
        let interface = config.cluster.interface.clone();
        let coordinator = config.cluster.coordinator_address;
        let worker = config.cluster.worker_address;
        let peer = match self.inner.role {
            LocalRole::Coordinator => worker,
            LocalRole::Worker => coordinator,
            LocalRole::Unknown => return,
        };
        let (rescans, mut rescan_receiver) = tokio::sync::mpsc::channel(16);
        let (handle, monitor_task) = match spawn_network_event_monitor(
            0,
            config.cluster.discovery.event_debounce,
            config.cluster.discovery.reconcile_interval,
            64,
            rescans,
        ) {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(error = %error, "network evidence monitor unavailable");
                return;
            }
        };
        let watcher = match MacOsDynamicStoreWatcher::start(&interface, handle) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "dynamic store watcher unavailable; network evidence stays fail-closed until reconcile"
                );
                None
            }
        };
        // monitor_task (debounce + reconcile) runs independently once started.
        std::mem::drop(monitor_task);
        tokio::spawn(async move {
            // Keep the dynamic-store watcher alive for the lifetime of the monitor task.
            let _watcher = watcher;
            let mut epoch: u64 = 0;
            while let Some(request) = rescan_receiver.recv().await {
                epoch = epoch.saturating_add(1);
                match observe_network_observation(&interface, coordinator, worker, peer) {
                    Ok(mut observation) => {
                        observation.epoch = epoch;
                        let snapshot = NetworkSnapshot::from_observation(&observation);
                        if evidence.update(snapshot) {
                            tracing::debug!(
                                reason = ?request.reason,
                                epoch,
                                state = ?snapshot.state,
                                "network evidence updated"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "network observation failed; evidence stays fail-closed"
                        );
                    }
                }
            }
        });
    }
}

#[cfg(not(target_os = "macos"))]
impl ProductionClusterRuntime {
    /// Non-macOS has no network evidence provider; the shared evidence stays fail-closed, which
    /// is safe (no peer is ever considered route-scoped).
    fn start_network_evidence_monitor(&self) {}
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

    async fn signed_metrics(
        State(authenticator): State<Arc<ControlAuthenticator>>,
        ConnectInfo(source): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
    ) -> Result<Response, ControlHttpError> {
        let signed = SignedControlHeaders::from_header_values(
            header(&headers, HEADER_NODE)?,
            header(&headers, HEADER_TIMESTAMP)?,
            header(&headers, HEADER_NONCE)?,
            header(&headers, HEADER_SIGNATURE)?,
        )?;
        ControlRequest {
            method: "GET",
            path_and_query: CONTROL_METRICS_PATH,
            body: &[],
            source_ip: source.ip(),
            headers: &signed,
        }
        .authenticate(&authenticator, now_millis())?;
        Ok((StatusCode::OK, "coordinator metrics").into_response())
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

    #[tokio::test]
    async fn metrics_client_uses_the_authenticated_control_path() {
        let secret = vec![0x62; 32];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/v1/metrics", get(signed_metrics))
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
            "worker-node".into(),
            "127.0.0.1".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            port,
            secret,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(client.metrics().await.unwrap(), "coordinator metrics");
        server.abort();
    }

    #[test]
    fn lifecycle_effects_are_acknowledged_after_completion() {
        assert!(effect_requires_ack(&ControlCommand::Pair {
            descriptor: NodeDescriptor {
                protocol_version: 1,
                node_id: "coordinator-node".into(),
                role: ControlRole::Coordinator,
                generation: 1,
                mode: super::super::ControlMode::SoloStandalone,
                deployment_id: Some("deployment-production".into()),
            },
        }));
        assert!(effect_requires_ack(&ControlCommand::PrepareWorker));
        assert!(effect_requires_ack(&ControlCommand::Demote));
        assert!(effect_requires_ack(&ControlCommand::CancelGeneration));
        assert!(effect_requires_ack(&ControlCommand::DistributedReady));
        assert!(!effect_requires_ack(&ControlCommand::BeginDrain));
    }
}

/// Test-support only: timestamps of the phases in one [`ProductionClusterRuntime::pair`]
/// session, captured to measure whether confirm completes before the stability sleep expires
/// (Q-01). Never contains secrets, nonces, or request bodies.
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Copy)]
pub struct PairTiming {
    pub offer_sent_at: u64,
    pub confirm_received_at: u64,
    pub lease_established_at: u64,
    pub stability_achieved_at: u64,
    pub pairing_ready_at: u64,
}
