use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    cluster::{
        AdminAction, AdminController, AdminExecutor, AdminFuture, ClusterEvent, ClusterEventKind,
        ClusterHandle, DistributedManifest, FingerprintProfile, ModeRuntime,
        PERSISTENT_STATE_SCHEMA_VERSION, PersistentChild, PersistentClusterState, PersistentMode,
        PersistentProxyTarget, ProductionClusterRuntime, RestartDecision, StandaloneManifest,
        StandaloneSupervisor, StateStore, StateStoreError, build_standalone_command,
        detect_cluster_role, fingerprint_file, platform_process_controller, reconcile_restart,
        required_port_available,
    },
    config::{ModeAwareConfig, ModelVariant, Residency},
    metrics::{MetricSnapshot, Metrics},
    proxy::{
        ModeAwareProxyOptions, ModeAwareProxyState, PeerProxyToken, mode_aware_proxy_handler,
        peer_ingress_handler,
    },
    target::{ClusterState, LocalRole, ProxyTarget, StableMode, UnavailableReason},
};
use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    routing::{any, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tracing::info;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub public_listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub node_id: String,
    pub cluster_enabled: bool,
    pub interface: String,
    pub standalone_profile_id: String,
    pub standalone_model_variant: ModelVariant,
    pub standalone_residency: Residency,
}

pub struct AppState {
    pub config: Arc<AppConfig>,
    pub proxy: Arc<ModeAwareProxyState>,
    pub metrics: Arc<Metrics>,
    cluster: RwLock<Option<ClusterHandle>>,
    admin: RwLock<Option<AdminController>>,
}

impl AppState {
    pub fn from_config(config: ModeAwareConfig) -> anyhow::Result<Arc<Self>> {
        let metrics = Arc::new(Metrics::default());
        let local_address = SocketAddr::new(config.ds4.http_host, config.ds4.http_port);
        let coordinator_address = SocketAddr::new(
            config.cluster.coordinator_address,
            config.cluster.peer_ingress_port,
        );
        let local_upstream = url::Url::parse(&format!("http://{local_address}"))?;
        let coordinator_upstream = url::Url::parse(&format!("http://{coordinator_address}"))?;
        let proxy = Arc::new(ModeAwareProxyState::new(
            local_upstream,
            coordinator_upstream,
            ModeAwareProxyOptions {
                max_in_flight: config.proxy.max_in_flight,
                request_body_limit_bytes: config.proxy.request_body_limit_bytes,
                response_header_timeout: config.proxy.timeouts.response_headers,
                first_body_byte_timeout: config.proxy.timeouts.first_body_byte,
                stream_idle_timeout: config.proxy.timeouts.stream_idle,
                connect_timeout: config.proxy.timeouts.connect,
            },
        )?);
        proxy.configure_metrics(metrics.clone());
        if config.cluster.enabled {
            let token = std::fs::read(&config.cluster.security.peer_proxy_token_file)?;
            proxy.configure_peer_proxy(
                PeerProxyToken::new(token)
                    .map_err(|_| anyhow::anyhow!("peer proxy token must contain 32 bytes"))?,
                config.cluster.worker_address,
            );
        }

        // Cluster lifecycle/supervisor接続前のP1 baseline。P2で実DS4 readinessに置換する。
        if !config.cluster.enabled {
            proxy.set_target(ProxyTarget::LocalStandalone, true);
            proxy.admission().start_serving();
        }

        Ok(Arc::new(Self {
            config: Arc::new(AppConfig {
                public_listen: config.proxy.public_listen,
                admin_listen: config.proxy.admin_listen,
                node_id: config.cluster.node_id,
                cluster_enabled: config.cluster.enabled,
                interface: config.cluster.interface,
                standalone_profile_id: config.ds4.standalone.profile_id,
                standalone_model_variant: config.ds4.standalone.model_variant,
                standalone_residency: config.ds4.standalone.residency,
            }),
            proxy,
            metrics,
            cluster: RwLock::new(None),
            admin: RwLock::new(None),
        }))
    }

    fn attach_cluster(&self, cluster: ClusterHandle) {
        *self
            .cluster
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cluster);
    }

    fn cluster_snapshot(&self) -> Option<crate::cluster::ClusterSnapshot> {
        self.cluster
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(ClusterHandle::snapshot)
    }

    fn attach_admin(&self, admin: AdminController) {
        *self
            .admin
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(admin);
    }

    fn admin_controller(&self) -> Option<AdminController> {
        self.admin
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

struct RuntimeAdminExecutor {
    runtime: Arc<ModeRuntime>,
    supervisor: Arc<StandaloneSupervisor>,
    standalone_model: PathBuf,
    distributed_model: PathBuf,
    production: Option<ProductionClusterRuntime>,
}

impl AdminExecutor for RuntimeAdminExecutor {
    fn execute(&self, action: AdminAction) -> AdminFuture {
        let runtime = self.runtime.clone();
        let supervisor = self.supervisor.clone();
        let standalone_model = self.standalone_model.clone();
        let distributed_model = self.distributed_model.clone();
        let production = self.production.clone();
        Box::pin(async move {
            match action {
                AdminAction::Reconcile => {
                    let current = runtime.snapshot();
                    if current.state == ClusterState::ManualInterventionRequired {
                        runtime
                            .cluster_handle()
                            .apply(ClusterEvent {
                                expected_generation: current.generation,
                                kind: ClusterEventKind::OperatorReconcile,
                            })
                            .await?;
                    }
                    let snapshot = if let Some(production) = &production {
                        production.reconcile().await?
                    } else {
                        runtime.reconcile_local().await?
                    };
                    Ok(snapshot_json(snapshot))
                }
                AdminAction::Restart => {
                    anyhow::ensure!(
                        runtime.snapshot().stable_mode == StableMode::SoloStandalone,
                        "restart of a distributed profile requires its active lifecycle owner"
                    );
                    crate::cluster::LocalStandaloneLifecycle::stop(supervisor.as_ref()).await?;
                    let snapshot = runtime.reconcile_local().await?;
                    Ok(snapshot_json(snapshot))
                }
                AdminAction::Fingerprint { profile } => {
                    let path = match profile {
                        FingerprintProfile::Standalone => standalone_model,
                        FingerprintProfile::Distributed => distributed_model,
                    };
                    let fingerprint = fingerprint_file(&path).await?;
                    Ok(serde_json::to_value(fingerprint)?)
                }
                AdminAction::Pair => Ok(snapshot_json(
                    production
                        .context("cluster runtime is disabled")?
                        .pair()
                        .await?,
                )),
                AdminAction::Promote => Ok(snapshot_json(
                    production
                        .context("cluster runtime is disabled")?
                        .promote()
                        .await?,
                )),
                AdminAction::Demote { .. } => Ok(snapshot_json(
                    production
                        .context("cluster runtime is disabled")?
                        .demote()
                        .await?,
                )),
            }
        })
    }
}

fn snapshot_json(snapshot: crate::cluster::ClusterSnapshot) -> Value {
    json!({
        "generation": snapshot.generation,
        "role": snapshot.role.name(),
        "mode": snapshot.stable_mode.name(),
        "state": snapshot.state.name(),
        "target": target_name(snapshot.target),
    })
}

pub async fn serve(config: ModeAwareConfig) -> anyhow::Result<()> {
    validate_dspark_binding(&config).await?;
    let boot = BootInputs::load(&config).await?;
    let state_store = Arc::new(StateStore::acquire(&config.cluster.state_path)?);
    let persisted = load_persisted_state(&state_store)?;
    let local_address = SocketAddr::new(config.ds4.http_host, config.ds4.http_port);
    let restart = reconcile_restart(
        persisted.as_ref(),
        &platform_process_controller(),
        config.cluster.timeouts.stop,
        config.ds4.allow_sigkill,
        || required_port_available(local_address),
    )
    .await?;
    let state = AppState::from_config(config.clone())?;
    state.proxy.admission().block();
    state.proxy.set_target(
        ProxyTarget::Unavailable {
            reason: UnavailableReason::Transition,
        },
        false,
    );
    let supervisor = build_standalone_supervisor(&config)?;
    let role = boot.role;
    let runtime = spawn_runtime(&config, role, &state, &supervisor, restart).await?;
    let production = attach_control_plane(&config, boot, &state, &runtime, &supervisor)?;
    let transition_monitor = spawn_transition_monitor(
        &state,
        &state_store,
        &runtime,
        &supervisor,
        production.as_ref(),
        &config.ds4.standalone.profile_id,
    );
    persist_runtime_state(
        &state_store,
        &runtime,
        &supervisor,
        production.as_ref(),
        &config.ds4.standalone.profile_id,
    )
    .await?;
    let local_monitor = spawn_local_monitor(
        &state,
        &state_store,
        &runtime,
        &supervisor,
        &config.ds4.standalone.profile_id,
    );
    run_servers(
        &config,
        state,
        production,
        role,
        supervisor,
        runtime,
        transition_monitor,
        local_monitor,
    )
    .await
}

/// Child起動前にmutation認証材料を確定し、起動途中のorphanを防ぐDSparkバインド検証。
async fn validate_dspark_binding(config: &ModeAwareConfig) -> anyhow::Result<()> {
    if config.ds4.dspark.enabled {
        let manifest_bytes = std::fs::read(&config.ds4.standalone.model_manifest)?;
        let manifest = serde_json::from_slice::<StandaloneManifest>(&manifest_bytes)?;
        let support_model = config
            .ds4
            .dspark
            .support_model
            .as_deref()
            .context("DSpark support model is unavailable")?;
        let fingerprint = fingerprint_file(support_model).await?;
        manifest.validate_dspark_binding(
            &fingerprint,
            config.ds4.dspark.confidence,
            config.ds4.dspark.strict,
        )?;
    }
    Ok(())
}

/// 起動前に一度だけ読み取る認証材料と役割のまとまり。
struct BootInputs {
    admin_token: Vec<u8>,
    control_secret: Option<Vec<u8>>,
    distributed_manifest: Option<DistributedManifest>,
    role: LocalRole,
}

impl BootInputs {
    async fn load(config: &ModeAwareConfig) -> anyhow::Result<Self> {
        let admin_token = std::fs::read(&config.cluster.security.admin_token_file)?;
        let control_secret = if config.cluster.enabled {
            Some(std::fs::read(&config.cluster.security.control_secret_file)?)
        } else {
            None
        };
        let distributed_manifest = if config.cluster.enabled {
            let bytes = std::fs::read(&config.ds4.mxfp4.model_manifest)?;
            Some(serde_json::from_slice::<DistributedManifest>(&bytes)?)
        } else {
            None
        };
        let role = if config.cluster.enabled {
            detect_cluster_role(
                &config.cluster.interface,
                config.cluster.coordinator_address,
                config.cluster.worker_address,
            )?
        } else {
            LocalRole::Unknown
        };
        Ok(Self {
            admin_token,
            control_secret,
            distributed_manifest,
            role,
        })
    }
}

fn load_persisted_state(
    state_store: &StateStore,
) -> anyhow::Result<Option<PersistentClusterState>> {
    match state_store.load() {
        Ok(state) => Ok(state),
        Err(StateStoreError::CorruptPreserved { path, reason }) => {
            tracing::warn!(preserved = %path.display(), reason, "corrupt cluster state preserved");
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn build_standalone_supervisor(
    config: &ModeAwareConfig,
) -> anyhow::Result<Arc<StandaloneSupervisor>> {
    let command = build_standalone_command(&config.ds4)?;
    let models_url = url::Url::parse(&format!(
        "http://{}:{}/v1/models",
        config.ds4.http_host, config.ds4.http_port
    ))?;
    Ok(Arc::new(StandaloneSupervisor::new(
        command,
        models_url,
        config.cluster.timeouts.standalone_startup,
        std::time::Duration::from_millis(250),
        config.cluster.timeouts.stop,
        config.ds4.allow_sigkill,
    )))
}

async fn spawn_runtime(
    config: &ModeAwareConfig,
    role: LocalRole,
    state: &Arc<AppState>,
    supervisor: &Arc<StandaloneSupervisor>,
    restart: RestartDecision,
) -> anyhow::Result<Arc<ModeRuntime>> {
    Ok(Arc::new(match restart {
        RestartDecision::StartSolo {
            baseline_generation,
        } => {
            ModeRuntime::spawn_ready_at(
                role,
                state.proxy.clone(),
                supervisor.clone(),
                config.cluster.timeouts.drain,
                baseline_generation,
            )
            .await?
        }
        RestartDecision::ManualIntervention {
            baseline_generation,
            reason,
        } => {
            tracing::error!(?reason, "restart reconcile requires manual intervention");
            ModeRuntime::spawn_manual_at(
                role,
                state.proxy.clone(),
                supervisor.clone(),
                config.cluster.timeouts.drain,
                baseline_generation,
            )
            .await?
        }
    }))
}

fn attach_control_plane(
    config: &ModeAwareConfig,
    boot: BootInputs,
    state: &Arc<AppState>,
    runtime: &Arc<ModeRuntime>,
    supervisor: &Arc<StandaloneSupervisor>,
) -> anyhow::Result<Option<ProductionClusterRuntime>> {
    let production = if config.cluster.enabled {
        Some(ProductionClusterRuntime::new(
            config.clone(),
            boot.role,
            runtime.clone(),
            state.proxy.clone(),
            supervisor.clone(),
            boot.distributed_manifest
                .context("distributed manifest is unavailable")?,
            boot.control_secret
                .context("control secret is unavailable")?,
        )?)
    } else {
        None
    };
    state.attach_cluster(runtime.cluster_handle());
    state.attach_admin(AdminController::new(
        boot.admin_token,
        Arc::new(RuntimeAdminExecutor {
            runtime: runtime.clone(),
            supervisor: supervisor.clone(),
            standalone_model: config.ds4.standalone.model.clone(),
            distributed_model: config.ds4.mxfp4.model.clone(),
            production: production.clone(),
        }),
    )?);
    Ok(production)
}

fn spawn_transition_monitor(
    state: &Arc<AppState>,
    state_store: &Arc<StateStore>,
    runtime: &Arc<ModeRuntime>,
    supervisor: &Arc<StandaloneSupervisor>,
    production: Option<&ProductionClusterRuntime>,
    profile: &str,
) -> tokio::task::JoinHandle<()> {
    let mut transition_snapshots = runtime.cluster_handle().subscribe();
    let transition_metrics = state.metrics.clone();
    let transition_store = state_store.clone();
    let transition_runtime = runtime.clone();
    let transition_supervisor = supervisor.clone();
    let transition_production = production.cloned();
    let transition_profile = profile.to_string();
    tokio::spawn(async move {
        let mut previous = *transition_snapshots.borrow_and_update();
        let mut transition_started = std::time::Instant::now();
        while transition_snapshots.changed().await.is_ok() {
            let current = *transition_snapshots.borrow_and_update();
            transition_metrics.transition(
                previous.state,
                current.state,
                "success",
                "state-change",
                transition_started.elapsed().as_secs_f64(),
            );
            previous = current;
            transition_started = std::time::Instant::now();
            if let Err(error) = persist_runtime_state(
                &transition_store,
                &transition_runtime,
                &transition_supervisor,
                transition_production.as_ref(),
                &transition_profile,
            )
            .await
            {
                tracing::error!(error = %error, "persistent cluster transition write failed");
            }
        }
    })
}

fn spawn_local_monitor(
    state: &Arc<AppState>,
    state_store: &Arc<StateStore>,
    runtime: &Arc<ModeRuntime>,
    supervisor: &Arc<StandaloneSupervisor>,
    profile: &str,
) -> tokio::task::JoinHandle<()> {
    let local_monitor_runtime = runtime.clone();
    let local_monitor_supervisor = supervisor.clone();
    let local_monitor_store = state_store.clone();
    let local_monitor_profile = profile.to_string();
    let local_monitor_metrics = state.metrics.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let before = local_monitor_runtime.snapshot().generation;
            match local_monitor_runtime.reconcile_local().await {
                Ok(snapshot) if snapshot.generation != before => {
                    local_monitor_metrics.child_restart("standalone", "unexpected-exit");
                    if let Err(error) = persist_runtime_state(
                        &local_monitor_store,
                        &local_monitor_runtime,
                        &local_monitor_supervisor,
                        None,
                        &local_monitor_profile,
                    )
                    .await
                    {
                        tracing::error!(error = %error, "persistent cluster state write failed");
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "local standalone reconcile failed");
                }
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_servers(
    config: &ModeAwareConfig,
    state: Arc<AppState>,
    production: Option<ProductionClusterRuntime>,
    role: LocalRole,
    supervisor: Arc<StandaloneSupervisor>,
    runtime: Arc<ModeRuntime>,
    transition_monitor: tokio::task::JoinHandle<()>,
    local_monitor: tokio::task::JoinHandle<()>,
) -> anyhow::Result<()> {
    let public_addr = state.config.public_listen;
    let admin_addr = state.config.admin_listen;
    let public = public_router(state.clone());
    let admin = admin_router(state.clone());
    let public_listener = tokio::net::TcpListener::bind(public_addr).await?;
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    let control_listener = if let Some(production) = &production {
        Some(tokio::net::TcpListener::bind(production.listen_addr()).await?)
    } else {
        None
    };
    let peer_listener = if role == LocalRole::Coordinator {
        Some(
            tokio::net::TcpListener::bind(SocketAddr::new(
                config.cluster.coordinator_address,
                config.cluster.peer_ingress_port,
            ))
            .await?,
        )
    } else {
        None
    };
    info!(addr = %public_addr, "public listener started");
    info!(addr = %admin_addr, "admin listener started");

    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let reconcile_task = production
        .as_ref()
        .map(ProductionClusterRuntime::start_reconcile_task);
    let control_task = control_listener
        .zip(production.clone())
        .map(|(listener, production)| {
            let shutdown = shutdown_receiver.clone();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    production
                        .router()
                        .into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(shutdown_requested(shutdown))
                .await
            })
        });
    let peer_task = peer_listener.map(|listener| {
        let shutdown = shutdown_receiver.clone();
        let router = peer_ingress_router(state.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_requested(shutdown))
            .await
        })
    });
    let public_server = axum::serve(
        public_listener,
        public.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_requested(shutdown_receiver.clone()));
    let admin_server = axum::serve(admin_listener, admin)
        .with_graceful_shutdown(shutdown_requested(shutdown_receiver));
    let servers = async { tokio::try_join!(public_server, admin_server).map(|_| ()) };
    tokio::pin!(servers);
    let serve_result = tokio::select! {
        result = &mut servers => result,
        signal = termination_signal() => {
            signal?;
            info!("termination signal received; draining listeners and stopping owned child");
            state.proxy.admission().block();
            shutdown_sender.send_replace(true);
            (&mut servers).await
        }
    };
    shutdown_sender.send_replace(true);
    local_monitor.abort();
    transition_monitor.abort();
    if let Some(task) = reconcile_task {
        task.abort();
    }
    if let Some(task) = control_task {
        task.await??;
    }
    if let Some(task) = peer_task {
        task.await??;
    }
    if let Some(production) = &production {
        production.stop_distributed().await?;
    }
    let stop_result = crate::cluster::LocalStandaloneLifecycle::stop(supervisor.as_ref()).await;
    drop(runtime);
    serve_result?;
    stop_result?;
    Ok(())
}

async fn shutdown_requested(mut receiver: tokio::sync::watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

#[cfg(unix)]
async fn termination_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => {},
        _ = interrupt.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn termination_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

async fn persist_runtime_state(
    store: &StateStore,
    runtime: &ModeRuntime,
    supervisor: &StandaloneSupervisor,
    production: Option<&ProductionClusterRuntime>,
    active_profile: &str,
) -> anyhow::Result<()> {
    let snapshot = runtime.snapshot();
    let distributed = if snapshot.stable_mode == StableMode::DistributedMxfp4 {
        match production {
            Some(production) => production.distributed_child_identity().await,
            None => None,
        }
    } else {
        None
    };
    let child = match distributed {
        Some(child) => Some(child),
        None => supervisor.child_identity().await,
    }
    .map(|identity| PersistentChild {
        pid: identity.pid,
        executable: identity.executable,
        argv_sha256: identity
            .argv_sha256
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        spawned_at_millis: identity.spawned_at_millis,
        process_start_micros: identity.process_start_micros,
    });
    store.save(&PersistentClusterState {
        schema_version: PERSISTENT_STATE_SCHEMA_VERSION,
        generation: snapshot.generation,
        desired_mode: persistent_mode(snapshot.stable_mode),
        last_stable_mode: persistent_mode(snapshot.stable_mode),
        cluster_state: snapshot.state.name().into(),
        proxy_target: match snapshot.target {
            ProxyTarget::LocalStandalone => PersistentProxyTarget::LocalStandalone,
            ProxyTarget::Coordinator => PersistentProxyTarget::Coordinator,
            ProxyTarget::Unavailable { .. } => PersistentProxyTarget::Unavailable,
        },
        active_profile: Some(if snapshot.stable_mode == StableMode::DistributedMxfp4 {
            "distributed-mxfp4".into()
        } else {
            active_profile.into()
        }),
        child,
        last_failure: None,
    })?;
    Ok(())
}

fn persistent_mode(mode: StableMode) -> PersistentMode {
    match mode {
        StableMode::SoloStandalone => PersistentMode::SoloStandalone,
        StableMode::PairedStandalone => PersistentMode::PairedStandalone,
        StableMode::DistributedMxfp4 => PersistentMode::DistributedMxfp4,
    }
}

pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", any(mode_aware_proxy_handler))
        .route("/{*path}", any(mode_aware_proxy_handler))
        .with_state(state.proxy.clone())
}

pub fn peer_ingress_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", any(peer_ingress_handler))
        .route("/{*path}", any(peer_ingress_handler))
        .with_state(state.proxy.clone())
}

pub async fn serve_peer_ingress(
    state: Arc<AppState>,
    role: LocalRole,
    listen: SocketAddr,
    coordinator_address: std::net::IpAddr,
) -> anyhow::Result<()> {
    validate_peer_ingress_bind(role, listen, coordinator_address)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(addr = %listen, "peer ingress listener started");
    axum::serve(
        listener,
        peer_ingress_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn validate_peer_ingress_bind(
    role: LocalRole,
    listen: SocketAddr,
    coordinator_address: std::net::IpAddr,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        role == LocalRole::Coordinator,
        "peer ingress may only be started by the coordinator"
    );
    anyhow::ensure!(
        listen.ip() == coordinator_address,
        "peer ingress must bind the fixed coordinator address"
    );
    Ok(())
}

pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/cluster", get(cluster))
        .route("/metrics", get(metrics))
        .route("/cluster/reconcile", post(reconcile))
        .route("/cluster/pair", post(pair))
        .route("/cluster/promote", post(promote))
        .route("/cluster/demote", post(demote))
        .route("/cluster/restart", post(restart))
        .route("/cluster/fingerprint", post(fingerprint))
        .with_state(state)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DemoteRequest {
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FingerprintRequest {
    profile: String,
}

async fn reconcile(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response<Body> {
    start_admin_job(&headers, &state, AdminAction::Reconcile)
}

async fn pair(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response<Body> {
    start_admin_job(&headers, &state, AdminAction::Pair)
}

async fn promote(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response<Body> {
    start_admin_job(&headers, &state, AdminAction::Promote)
}

async fn demote(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response<Body> {
    let admin = match authorized_admin(&headers, &state) {
        Ok(admin) => admin,
        Err(response) => return *response,
    };
    let body = if body.is_empty() {
        DemoteRequest::default()
    } else {
        match serde_json::from_slice::<DemoteRequest>(&body) {
            Ok(body) => body,
            Err(error) => {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("invalid demote request: {error}")}),
                );
            }
        }
    };
    start_job(
        admin,
        AdminAction::Demote {
            reason: body.reason,
        },
    )
}

async fn restart(headers: HeaderMap, State(state): State<Arc<AppState>>) -> Response<Body> {
    start_admin_job(&headers, &state, AdminAction::Restart)
}

async fn fingerprint(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response<Body> {
    let admin = match authorized_admin(&headers, &state) {
        Ok(admin) => admin,
        Err(response) => return *response,
    };
    let body = match serde_json::from_slice::<FingerprintRequest>(&body) {
        Ok(body) => body,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("invalid fingerprint request: {error}")}),
            );
        }
    };
    let profile = match body.profile.as_str() {
        "standalone" => FingerprintProfile::Standalone,
        "distributed" => FingerprintProfile::Distributed,
        _ => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": "profile must be standalone or distributed"}),
            );
        }
    };
    start_job(admin, AdminAction::Fingerprint { profile })
}

fn start_admin_job(headers: &HeaderMap, state: &AppState, action: AdminAction) -> Response<Body> {
    let admin = match authorized_admin(headers, state) {
        Ok(admin) => admin,
        Err(response) => return *response,
    };
    start_job(admin, action)
}

fn authorized_admin(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AdminController, Box<Response<Body>>> {
    let admin = state.admin_controller().ok_or_else(|| {
        Box::new(json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "admin lifecycle unavailable"}),
        ))
    })?;
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !admin.authorize(authorization) {
        return Err(Box::new(json_response(
            StatusCode::UNAUTHORIZED,
            json!({"error": "unauthorized"}),
        )));
    }
    Ok(admin)
}

fn start_job(admin: AdminController, action: AdminAction) -> Response<Body> {
    match admin.start(action) {
        Ok(job) => json_response(
            StatusCode::ACCEPTED,
            serde_json::to_value(job).expect("admin job serializes"),
        ),
        Err(profile) => json_response(
            StatusCode::CONFLICT,
            json!({"error": format!("fingerprint job already running for {}", profile.as_str())}),
        ),
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn ready(State(state): State<Arc<AppState>>) -> Response<Body> {
    let target = state.proxy.target_snapshot();
    let admission = state.proxy.admission().snapshot();
    let ready = target.ready && admission.state == AdmissionState::Serving;
    json_response(
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        json!({
            "status": if ready { "ready" } else { "not_ready" },
            "target": target_name(target.target),
            "target_ready": target.ready,
            "admission": admission_state_name(admission.state),
        }),
    )
}

async fn cluster(State(state): State<Arc<AppState>>) -> Json<Value> {
    let target = state.proxy.target_snapshot();
    let admission = state.proxy.admission().snapshot();
    let solo = !state.config.cluster_enabled;
    let snapshot = state.cluster_snapshot();
    Json(json!({
        "node_id": state.config.node_id,
        "role": snapshot.map_or("unknown", |snapshot| snapshot.role.name()),
        "mode": snapshot.map_or(if solo { "solo-standalone" } else { "unknown" }, |snapshot| snapshot.stable_mode.name()),
        "state": snapshot.map_or(if solo { "solo-standalone-ready" } else { "booting" }, |snapshot| snapshot.state.name()),
        "generation": snapshot.map_or(0, |snapshot| snapshot.generation),
        "target": target_name(target.target),
        "target_ready": target.ready,
        "admission": admission_json(admission),
        "peer_ingress_ready": false,
        "interface": state.config.interface,
        "active_standalone_profile": {
            "profile_id": state.config.standalone_profile_id,
            "model_variant": state.config.standalone_model_variant.name(),
            "residency": state.config.standalone_residency.name(),
        },
        "child": Value::Null,
    }))
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response<Body> {
    let body = state.metrics.render_mode_aware(MetricSnapshot {
        node_id: &state.config.node_id,
        interface: &state.config.interface,
        target: state.proxy.target_snapshot(),
        admission: state.proxy.admission().snapshot(),
        cluster: state.cluster_snapshot(),
        peer_lease_seconds: 0.0,
        thunderbolt_ip_state: "unknown",
        discovery_results: 0,
        model_variant: state.config.standalone_model_variant,
        residency: state.config.standalone_residency,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(body))
        .expect("valid metrics response")
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&value).expect("JSON value must serialize"),
        ))
        .expect("valid JSON response")
}

fn admission_json(snapshot: AdmissionSnapshot) -> Value {
    json!({
        "state": admission_state_name(snapshot.state),
        "in_flight": snapshot.in_flight,
        "max_in_flight": snapshot.max_in_flight,
        "drain_generation": snapshot.drain_generation,
    })
}

fn admission_state_name(state: AdmissionState) -> &'static str {
    match state {
        AdmissionState::Serving => "serving",
        AdmissionState::Draining => "draining",
        AdmissionState::Blocked => "blocked",
    }
}

pub(crate) fn target_name(target: ProxyTarget) -> &'static str {
    match target {
        ProxyTarget::LocalStandalone => "local-standalone",
        ProxyTarget::Coordinator => "coordinator",
        ProxyTarget::Unavailable {
            reason: UnavailableReason::Transition,
        } => "unavailable-transition",
        ProxyTarget::Unavailable {
            reason: UnavailableReason::InconsistentStableState,
        } => "unavailable-inconsistent-state",
        ProxyTarget::Unavailable {
            reason: UnavailableReason::UnknownRoleWithoutLocalStandalone,
        } => "unavailable-unknown-role",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    struct TestAdminExecutor;

    impl AdminExecutor for TestAdminExecutor {
        fn execute(&self, action: AdminAction) -> AdminFuture {
            Box::pin(async move {
                if matches!(action, AdminAction::Fingerprint { .. }) {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Ok(json!({"executed": true}))
            })
        }
    }

    #[tokio::test]
    async fn shutdown_notification_reaches_both_listeners() {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let public = tokio::spawn(shutdown_requested(receiver.clone()));
        let admin = tokio::spawn(shutdown_requested(receiver));
        tokio::task::yield_now().await;

        sender.send_replace(true);

        tokio::time::timeout(std::time::Duration::from_secs(1), public)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), admin)
            .await
            .unwrap()
            .unwrap();
    }

    fn test_state(serving: bool) -> Arc<AppState> {
        let proxy = Arc::new(
            ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:8000").unwrap(),
                url::Url::parse("http://10.99.0.1:18082").unwrap(),
                ModeAwareProxyOptions {
                    max_in_flight: 1,
                    request_body_limit_bytes: 4096,
                    response_header_timeout: std::time::Duration::from_secs(1),
                    first_body_byte_timeout: std::time::Duration::from_secs(1),
                    stream_idle_timeout: std::time::Duration::from_secs(1),
                    connect_timeout: std::time::Duration::from_secs(1),
                },
            )
            .unwrap(),
        );
        if serving {
            proxy.set_target(ProxyTarget::LocalStandalone, true);
            proxy.admission().start_serving();
        }
        let state = Arc::new(AppState {
            config: Arc::new(AppConfig {
                public_listen: "127.0.0.1:18080".parse().unwrap(),
                admin_listen: "127.0.0.1:18081".parse().unwrap(),
                node_id: "test-node".into(),
                cluster_enabled: false,
                interface: "bridge0".into(),
                standalone_profile_id: "test-profile".into(),
                standalone_model_variant: ModelVariant::Q2Q4,
                standalone_residency: Residency::SsdStreaming,
            }),
            proxy,
            metrics: Arc::new(Metrics::default()),
            cluster: RwLock::new(None),
            admin: RwLock::new(None),
        });
        state.attach_admin(AdminController::new(vec![3; 32], Arc::new(TestAdminExecutor)).unwrap());
        state
    }

    async fn get(state: Arc<AppState>, path: &'static str) -> (StatusCode, String) {
        let response = admin_router(state)
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn post(
        state: Arc<AppState>,
        path: &'static str,
        token: Option<&str>,
        body: &'static str,
    ) -> (StatusCode, String) {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", token);
        }
        let response = admin_router(state)
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn basic_admin_endpoints_report_serving_solo_state() {
        let state = test_state(true);
        let (health_status, health_body) = get(state.clone(), "/healthz").await;
        let (ready_status, ready_body) = get(state.clone(), "/readyz").await;
        let (cluster_status, cluster_body) = get(state.clone(), "/cluster").await;
        let (metrics_status, metrics_body) = get(state, "/metrics").await;

        assert_eq!(health_status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&health_body).unwrap()["status"],
            "ok"
        );
        assert_eq!(ready_status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&ready_body).unwrap()["target"],
            "local-standalone"
        );
        assert_eq!(cluster_status, StatusCode::OK);
        let cluster: Value = serde_json::from_str(&cluster_body).unwrap();
        assert_eq!(cluster["mode"], "solo-standalone");
        assert_eq!(
            cluster["active_standalone_profile"]["profile_id"],
            "test-profile"
        );
        assert_eq!(metrics_status, StatusCode::OK);
        assert!(metrics_body.contains("ds4_proxy_target_ready{target=\"local-standalone\"} 1"));
    }

    #[tokio::test]
    async fn readyz_returns_503_when_target_is_blocked() {
        let (status, body) = get(test_state(false), "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["admission"], "blocked");
    }

    #[tokio::test]
    async fn cluster_endpoint_reports_manual_restart_state() {
        let state = test_state(false);
        let (handle, task) = crate::cluster::spawn_state_machine(
            crate::cluster::ClusterSnapshot::booting_at(LocalRole::Unknown, 12),
            2,
        );
        handle
            .apply(crate::cluster::ClusterEvent {
                expected_generation: 12,
                kind: crate::cluster::ClusterEventKind::RequireManualIntervention,
            })
            .await
            .unwrap();
        state.attach_cluster(handle);
        let (_, body) = get(state, "/cluster").await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["state"], "manual-intervention-required");
        assert_eq!(body["generation"], 13);
        task.abort();
    }

    #[tokio::test]
    async fn mutations_require_bearer_token_and_return_async_job() {
        let state = test_state(true);
        let (missing, _) = post(state.clone(), "/cluster/reconcile", None, "{}").await;
        let (wrong, _) = post(
            state.clone(),
            "/cluster/reconcile",
            Some("Bearer deadbeef"),
            "{}",
        )
        .await;
        let token = format!("Bearer {}", crate::cluster::encode_token(&[3; 32]));
        for (path, body) in [
            ("/cluster/reconcile", "{}"),
            ("/cluster/pair", "{}"),
            ("/cluster/promote", "{}"),
            ("/cluster/demote", r#"{"reason":"operator"}"#),
            ("/cluster/restart", "{}"),
        ] {
            let (status, body) = post(state.clone(), path, Some(&token), body).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{path}: {body}");
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["state"], "running");
            assert!(body["job_id"].as_str().is_some_and(|id| !id.is_empty()));
        }
        assert_eq!(missing, StatusCode::UNAUTHORIZED);
        assert_eq!(wrong, StatusCode::UNAUTHORIZED);

        let (malformed_unauthorized, _) =
            post(state, "/cluster/fingerprint", None, "not-json").await;
        assert_eq!(malformed_unauthorized, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn duplicate_fingerprint_for_same_profile_is_rejected() {
        let state = test_state(true);
        let token = format!("Bearer {}", crate::cluster::encode_token(&[3; 32]));
        let (first, _) = post(
            state.clone(),
            "/cluster/fingerprint",
            Some(&token),
            r#"{"profile":"standalone"}"#,
        )
        .await;
        let (duplicate, _) = post(
            state,
            "/cluster/fingerprint",
            Some(&token),
            r#"{"profile":"standalone"}"#,
        )
        .await;
        assert_eq!(first, StatusCode::ACCEPTED);
        assert_eq!(duplicate, StatusCode::CONFLICT);
    }

    #[test]
    fn peer_ingress_bind_is_coordinator_only_and_address_scoped() {
        let coordinator = std::net::IpAddr::from([10, 99, 0, 1]);
        let listen = SocketAddr::new(coordinator, 18082);
        assert!(validate_peer_ingress_bind(LocalRole::Coordinator, listen, coordinator).is_ok());
        assert!(validate_peer_ingress_bind(LocalRole::Worker, listen, coordinator).is_err());
        assert!(
            validate_peer_ingress_bind(
                LocalRole::Coordinator,
                "0.0.0.0:18082".parse().unwrap(),
                coordinator,
            )
            .is_err()
        );
    }
}
