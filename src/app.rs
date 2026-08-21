#[cfg(target_os = "macos")]
use crate::cluster::MacOsDynamicStoreWatcher;
use crate::{
    admission::{AdmissionSnapshot, AdmissionState, DrainError},
    cluster::{
        AdminAction, AdminController, AdminExecutor, AdminFuture, ChildDiagnostics,
        ChildrenDiagnostics, ClusterHandle, ControlMode, ControlRole, ControlSessionDiagnostics,
        DistributedControlPhase, DistributedManifest, FingerprintProfile, LeaseDiagnostics,
        ModeRuntime, OperatorReconcileOutcome, PERSISTENT_STATE_SCHEMA_VERSION, PeerDiagnostics,
        PersistentChild, PersistentClusterState, PersistentMode, PersistentProxyTarget,
        ProcessControlError, ProductionClusterRuntime, RestartDecision, StandaloneManifest,
        StandaloneSupervisor, StateStore, StateStoreError, build_standalone_command,
        detect_cluster_role, fingerprint_file, platform_process_controller, reconcile_restart,
        required_port_available, spawn_network_event_monitor,
    },
    config::{ModeAwareConfig, ModelVariant, Residency},
    metrics::{MetricSnapshot, Metrics},
    notify::{DesktopNotificationService, NotifyPlatform, build_notifier},
    proxy::{
        ModeAwareProxyOptions, ModeAwareProxyState, PeerProxyToken, mode_aware_proxy_handler,
        peer_ingress_handler,
    },
    target::{LocalRole, ProxyTarget, StableMode, UnavailableReason},
};

use anyhow::Context;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Response, StatusCode, header::CONTENT_TYPE},
    routing::{any, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
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
    production: RwLock<Option<ProductionClusterRuntime>>,
    supervisor: RwLock<Option<Arc<StandaloneSupervisor>>>,
    graceful_restart_in_progress: std::sync::atomic::AtomicBool,
    /// Default in-flight drain timeout for `/admin/restart` when the request
    /// omits `drain_timeout_ms` (C-04a). Taken from the cluster stop timeout.
    default_drain_timeout: std::time::Duration,
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
            production: RwLock::new(None),
            supervisor: RwLock::new(None),
            graceful_restart_in_progress: std::sync::atomic::AtomicBool::new(false),
            default_drain_timeout: config.cluster.timeouts.stop,
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

    fn attach_production(&self, production: ProductionClusterRuntime) {
        *self
            .production
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(production);
    }

    async fn production_diagnostics(&self) -> Option<crate::cluster::ProductionDiagnostics> {
        let production = self
            .production
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match production {
            Some(production) => Some(production.diagnostics().await),
            None => None,
        }
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

    fn attach_supervisor(&self, supervisor: Arc<StandaloneSupervisor>) {
        *self
            .supervisor
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(supervisor);
    }

    fn supervisor(&self) -> Option<Arc<StandaloneSupervisor>> {
        self.supervisor
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Try to claim the single in-flight graceful restart slot. Returns `true`
    /// when this caller is the first to claim it (C-04a duplicate guard).
    fn try_claim_graceful_restart(&self) -> bool {
        !self
            .graceful_restart_in_progress
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    }

    /// Release the graceful restart slot. The process usually exits right after
    /// a successful restart, but a drain-timeout or identity-mismatch failure
    /// must release it so an operator can retry (C-04b).
    fn release_graceful_restart(&self) {
        self.graceful_restart_in_progress
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

struct RuntimeAdminExecutor {
    runtime: Arc<ModeRuntime>,
    supervisor: Arc<StandaloneSupervisor>,
    standalone_model: PathBuf,
    distributed_model: PathBuf,
    production: Option<ProductionClusterRuntime>,
    interface: String,
    coordinator_address: IpAddr,
    worker_address: IpAddr,
}

impl AdminExecutor for RuntimeAdminExecutor {
    fn execute(&self, action: AdminAction) -> AdminFuture {
        let runtime = self.runtime.clone();
        let supervisor = self.supervisor.clone();
        let standalone_model = self.standalone_model.clone();
        let distributed_model = self.distributed_model.clone();
        let production = self.production.clone();
        let interface = self.interface.clone();
        let coordinator_address = self.coordinator_address;
        let worker_address = self.worker_address;
        Box::pin(async move {
            match action {
                AdminAction::Reconcile => {
                    // B-03: route through the coordinator runtime so the promotion tracker reset
                    // and the OperatorReconcile state event are one atomic operation, instead of
                    // applying the event directly to the state machine. Worker / role-unknown /
                    // non-manual states are reported explicitly in the response.
                    let note = if let Some(production) = &production {
                        match production.operator_reconcile().await? {
                            OperatorReconcileOutcome::Coordinator { manual_cleared } => {
                                if manual_cleared {
                                    "coordinator promotion tracker reset; manual intervention cleared"
                                } else {
                                    "coordinator promotion tracker reset"
                                }
                            }
                            OperatorReconcileOutcome::NotCoordinator { manual_cleared } => {
                                if manual_cleared {
                                    "no coordinator promotion tracker; local manual state cleared"
                                } else {
                                    "no coordinator promotion tracker on this node"
                                }
                            }
                        }
                    } else {
                        "cluster disabled; no coordinator promotion tracker"
                    };
                    let snapshot = if let Some(production) = &production {
                        production.reconcile().await?
                    } else {
                        runtime.reconcile_local().await?
                    };
                    let mut value = snapshot_json(snapshot);
                    value["reconcile"] = json!(note);
                    Ok(value)
                }
                AdminAction::Restart => {
                    let new_role =
                        detect_cluster_role(&interface, coordinator_address, worker_address)?;
                    let current_role = production
                        .as_ref()
                        .map(ProductionClusterRuntime::role)
                        .unwrap_or(LocalRole::Unknown);
                    if new_role != current_role {
                        tracing::warn!(from = ?current_role, to = ?new_role, "cluster role changed on restart; restarting process to apply new role");
                        spawn_process_restart();
                        return Ok(json!({
                            "restart": "process",
                            "role": new_role.name(),
                            "reason": "role-change",
                        }));
                    }
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

fn spawn_process_restart() {
    // LaunchAgent の KeepAlive に依存して siderostat プロセス全体を再起動する。
    // role は起動時に一度だけ判定されるため、role 変更を適用するにはプロセスごと
    // 再起動して起動時判定をやり直す必要がある。
    // HTTP 応答を返す猶予を確保してから exit する。
    tokio::task::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::process::exit(0);
    });
}

/// macOS のネットワーク変更（Thunderbolt bridge0 の IPv4 付与・除去）を監視し、
/// role が初期値から変化したらプロセス全体を再起動する。起動時に一度だけ判定される
/// role を、ケーブル抜き差し後も LaunchAgent 再起動を介して更新するための配線。
#[cfg(target_os = "macos")]
fn spawn_role_change_monitor(
    config: &ModeAwareConfig,
    initial_role: LocalRole,
) -> (
    Option<tokio::task::JoinHandle<()>>,
    Option<MacOsDynamicStoreWatcher>,
) {
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
            tracing::warn!(error = %error, "network event monitor unavailable");
            return (None, None);
        }
    };
    let watcher = match MacOsDynamicStoreWatcher::start(&config.cluster.interface, handle) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(error = %error, "dynamic store watcher unavailable");
            return (Some(monitor_task), None);
        }
    };
    let interface = config.cluster.interface.clone();
    let coordinator = config.cluster.coordinator_address;
    let worker = config.cluster.worker_address;
    let task = tokio::spawn(async move {
        while let Some(request) = rescan_receiver.recv().await {
            match detect_cluster_role(&interface, coordinator, worker) {
                Ok(new_role) if new_role != initial_role => {
                    tracing::warn!(
                        from = ?initial_role,
                        to = ?new_role,
                        reason = ?request.reason,
                        "cluster role changed; restarting process to apply new role"
                    );
                    spawn_process_restart();
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "cluster role re-detection failed");
                }
            }
        }
    });
    (Some(task), Some(watcher))
}

#[cfg(not(target_os = "macos"))]
fn spawn_role_change_monitor(
    _config: &ModeAwareConfig,
    _initial_role: LocalRole,
) -> (Option<tokio::task::JoinHandle<()>>, Option<()>) {
    (None, None)
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

#[derive(Debug, Clone, Copy, Default)]
pub struct ServeOptions {
    pub decline_startup_cleanup: bool,
}

pub async fn serve(config: ModeAwareConfig) -> anyhow::Result<()> {
    serve_with_options(config, ServeOptions::default()).await
}

pub async fn serve_with_options(
    config: ModeAwareConfig,
    options: ServeOptions,
) -> anyhow::Result<()> {
    validate_dspark_binding(&config).await?;
    let boot = BootInputs::load(&config).await?;
    match crate::startup_cleanup::cleanup_startup_processes(
        std::process::id(),
        &config.ds4.binary,
        &platform_process_controller(),
        config.cluster.timeouts.stop,
        crate::startup_cleanup::StartupCleanupOptions {
            decline: options.decline_startup_cleanup,
            auto_restart: config.startup_cleanup.auto_restart,
            notifications_enabled: config.notifications.enabled,
            notification_sound: config.notifications.sound,
        },
    )
    .await?
    {
        crate::startup_cleanup::StartupCleanupOutcome::NoCandidates
        | crate::startup_cleanup::StartupCleanupOutcome::Approved { .. } => {}
        crate::startup_cleanup::StartupCleanupOutcome::Declined { count } => {
            anyhow::bail!(
                "startup cleanup was declined for {count} existing siderostat/ds4 process(es); refusing to start"
            );
        }
    }
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
    let supervisor = build_standalone_supervisor(&config, &state)?;
    // C-04a: graceful restart は solo-standalone の owned DS4 child を停止するため、
    // supervisor への参照を AppState に保持する。cluster 有効時は lifecycle owner
    // (ProductionClusterRuntime) が child を所有するため graceful restart は拒否される。
    state.attach_supervisor(supervisor.clone());
    let role = boot.role;
    let runtime = spawn_runtime(&config, role, &state, &supervisor, restart).await?;
    let control_session_generation = persisted
        .as_ref()
        .map(|persisted| persisted.control_session_generation);
    let production = attach_control_plane(
        &config,
        boot,
        &state,
        &runtime,
        &supervisor,
        control_session_generation,
    )?;
    let notifier = build_notifier(
        config.notifications.enabled,
        config.notifications.sound,
        NotifyPlatform::detect(),
    );
    let notification_service = DesktopNotificationService::new(notifier);
    if config.notifications.enabled {
        notification_service.log_session_status().await;
    }
    let notification_service = Arc::new(std::sync::Mutex::new(notification_service));
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
    let desktop_notifier = spawn_desktop_notifier(&runtime, notification_service.clone());
    let local_monitor = spawn_local_monitor(
        &state,
        &state_store,
        &runtime,
        &supervisor,
        &config.ds4.standalone.profile_id,
        notification_service.clone(),
    );
    // ネットワーク変更（Thunderbolt bridge0 の IPv4 付与・除去）を監視し、
    // role が起動時の初期値から変化したらプロセス全体を再起動する。
    let (_role_change_task, _role_change_watcher) = spawn_role_change_monitor(&config, role);
    run_servers(
        &config,
        state,
        production,
        role,
        supervisor,
        runtime,
        transition_monitor,
        local_monitor,
        desktop_notifier,
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
    state: &Arc<AppState>,
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
        state.metrics.clone(),
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
    control_session_generation: Option<u64>,
) -> anyhow::Result<Option<ProductionClusterRuntime>> {
    // detect_cluster_role は対象 interface に IPv4 が無い場合などに
    // LocalRole::Unknown を返す。Unknown は standalone 運用として想定済み
    // なので、production を生成せず起動を継続する。
    let production = if config.cluster.enabled && boot.role != LocalRole::Unknown {
        Some(ProductionClusterRuntime::new(
            config.clone(),
            boot.role,
            runtime.clone(),
            state.proxy.clone(),
            supervisor.clone(),
            state.metrics.clone(),
            boot.distributed_manifest
                .context("distributed manifest is unavailable")?,
            boot.control_secret
                .context("control secret is unavailable")?,
            control_session_generation,
        )?)
    } else {
        None
    };
    state.attach_cluster(runtime.cluster_handle());
    if let Some(production) = &production {
        state.attach_production(production.clone());
    }
    state.attach_admin(AdminController::new(
        boot.admin_token,
        Arc::new(RuntimeAdminExecutor {
            runtime: runtime.clone(),
            supervisor: supervisor.clone(),
            standalone_model: config.ds4.standalone.model.clone(),
            distributed_model: config.ds4.mxfp4.model.clone(),
            production: production.clone(),
            interface: config.cluster.interface.clone(),
            coordinator_address: config.cluster.coordinator_address,
            worker_address: config.cluster.worker_address,
        }),
    )?);
    Ok(production)
}

#[cfg(feature = "test-support")]
/// Run one admin `/cluster/reconcile` HTTP request against a production-backed
/// `RuntimeAdminExecutor` and return the completed job result (test-support only). Builds an
/// `AppState` from `config` (writing the peer-proxy token file the harness config omits), wires
/// the same executor construction as `attach_control_plane`, POSTs `/cluster/reconcile` with
/// `admin_token`, and polls the async admin job to completion. Lets an integration test drive
/// the real admin HTTP endpoint against an already-built `ProductionClusterRuntime` from the
/// two-node harness without needing `axum` in the integration crate.
pub async fn admin_http_reconcile(
    config: &ModeAwareConfig,
    runtime: &Arc<ModeRuntime>,
    production: Option<Arc<ProductionClusterRuntime>>,
    admin_token: Vec<u8>,
) -> anyhow::Result<Value> {
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    std::fs::write(
        &config.cluster.security.peer_proxy_token_file,
        vec![0x42; 32],
    )?;
    let state = AppState::from_config(config.clone())?;
    let supervisor = build_standalone_supervisor(config, &state)?;
    state.attach_cluster(runtime.cluster_handle());
    if let Some(production) = &production {
        state.attach_production((**production).clone());
    }
    state.attach_admin(AdminController::new(
        admin_token.clone(),
        Arc::new(RuntimeAdminExecutor {
            runtime: runtime.clone(),
            supervisor,
            standalone_model: config.ds4.standalone.model.clone(),
            distributed_model: config.ds4.mxfp4.model.clone(),
            production: production.as_ref().map(|p| (**p).clone()),
            interface: config.cluster.interface.clone(),
            coordinator_address: config.cluster.coordinator_address,
            worker_address: config.cluster.worker_address,
        }),
    )?);

    let bearer = format!("Bearer {}", crate::cluster::encode_token(&admin_token));
    let request = Request::post("/cluster/reconcile")
        .header("content-type", "application/json")
        .header("authorization", bearer)
        .body(Body::from("{}"))
        .context("build admin reconcile request")?;
    let response = admin_router(state.clone())
        .oneshot(request)
        .await
        .context("admin reconcile HTTP request")?;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = to_bytes(response.into_body(), 64 * 1024).await?;
    let job: Value = serde_json::from_slice(&body)?;
    let job_id = job["job_id"]
        .as_str()
        .context("reconcile job id")?
        .to_string();

    let admin = state.admin_controller().context("admin controller")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let job = admin.job(&job_id).context("admin job lookup")?;
        match job.state {
            crate::cluster::AdminJobState::Complete => {
                return job.result.context("reconcile job result missing");
            }
            crate::cluster::AdminJobState::Failed => {
                anyhow::bail!("admin reconcile failed: {:?}", job.error)
            }
            crate::cluster::AdminJobState::Running => {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "admin reconcile timed out"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
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

/// Subscribe to the cluster state watch channel and forward important
/// transitions to the desktop notification service. Runs until the channel
/// closes; failures are non-fatal and never affect the cluster.
fn spawn_desktop_notifier(
    runtime: &Arc<ModeRuntime>,
    service: Arc<std::sync::Mutex<DesktopNotificationService>>,
) -> tokio::task::JoinHandle<()> {
    let mut snapshots = runtime.cluster_handle().subscribe();
    let startup_state = runtime.snapshot().state;
    {
        let mut service = service
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        service.observe_startup(startup_state);
    }
    tokio::spawn(async move {
        let mut previous = *snapshots.borrow_and_update();
        while snapshots.changed().await.is_ok() {
            let current = *snapshots.borrow_and_update();
            let mut service = service
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            service.observe_snapshot_transition(previous, current);
            drop(service);
            previous = current;
        }
    })
}

fn spawn_local_monitor(
    state: &Arc<AppState>,
    state_store: &Arc<StateStore>,
    runtime: &Arc<ModeRuntime>,
    supervisor: &Arc<StandaloneSupervisor>,
    profile: &str,
    notifier: Arc<std::sync::Mutex<DesktopNotificationService>>,
) -> tokio::task::JoinHandle<()> {
    let local_monitor_runtime = runtime.clone();
    let local_monitor_supervisor = supervisor.clone();
    let local_monitor_store = state_store.clone();
    let local_monitor_profile = profile.to_string();
    let local_monitor_metrics = state.metrics.clone();
    let local_monitor_notifier = notifier;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let before = local_monitor_runtime.snapshot().generation;
            match local_monitor_runtime.reconcile_local().await {
                Ok(snapshot) if snapshot.generation != before => {
                    local_monitor_metrics.child_restart("standalone", "unexpected-exit");
                    local_monitor_notifier
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .observe_child_restart();
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
    desktop_notifier: tokio::task::JoinHandle<()>,
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
    desktop_notifier.abort();
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
    let control_session_generation = match production {
        Some(production) => production.control_session_generation().await,
        None => snapshot.generation,
    };
    store.save(&PersistentClusterState {
        schema_version: PERSISTENT_STATE_SCHEMA_VERSION,
        generation: snapshot.generation,
        control_session_generation,
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
        .route("/metrics/coordinator", get(coordinator_metrics))
        .route("/cluster/reconcile", post(reconcile))
        .route("/cluster/pair", post(pair))
        .route("/cluster/promote", post(promote))
        .route("/cluster/demote", post(demote))
        .route("/cluster/restart", post(restart))
        .route("/cluster/fingerprint", post(fingerprint))
        .route("/admin/restart", post(graceful_restart))
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GracefulRestartRequest {
    /// Optional in-flight drain timeout in milliseconds (C-04a). When omitted,
    /// the cluster stop timeout is used as the default.
    drain_timeout_ms: Option<u64>,
}

/// `/admin/restart` — authenticated graceful runtime restart (A-01 contract).
///
/// The handler authenticates with the shared admin bearer token, parses the
/// optional `drain_timeout_ms`, rejects overlapping requests, and then hands
/// the validated request to `perform_graceful_restart`. The actual
/// admission-block → drain → child-stop → process-exit sequence is
/// implemented in C-04b.
async fn graceful_restart(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response<Body> {
    let admin = match authorized_admin(&headers, &state) {
        Ok(admin) => admin,
        Err(response) => return *response,
    };
    let drain_timeout = match resolve_drain_timeout(&body, state.default_drain_timeout) {
        Ok(timeout) => timeout,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("invalid restart request: {error}")}),
            );
        }
    };

    // 進行中の graceful restart と重複しない。drain timeout / identity
    // mismatch で失敗した場合は C-04b がフラグを解放する。
    if !state.try_claim_graceful_restart() {
        return json_response(
            StatusCode::CONFLICT,
            json!({"error": "restart_in_progress"}),
        );
    }

    // cluster 有効・distributed 等で supervisor 不在の場合は graceful restart を拒否する。
    let Some(supervisor) = state.supervisor() else {
        state.release_graceful_restart();
        return json_response(
            StatusCode::CONFLICT,
            json!({"error": "graceful_restart_unavailable"}),
        );
    };
    let _ = admin;
    perform_graceful_restart(&state, &supervisor, drain_timeout).await
}

/// Resolve the drain timeout for `/admin/restart`: use the request's
/// `drain_timeout_ms` when present, otherwise the cluster stop timeout default.
/// Empty body uses the default. Malformed JSON is an error. Pure so it can be
/// unit-tested without HTTP / exit side effects (C-04a/C-04b).
fn resolve_drain_timeout(
    body: &Bytes,
    default: std::time::Duration,
) -> Result<std::time::Duration, String> {
    if body.is_empty() {
        return Ok(default);
    }
    let request = serde_json::from_slice::<GracefulRestartRequest>(body)
        .map_err(|error| error.to_string())?;
    Ok(request
        .drain_timeout_ms
        .map_or(default, std::time::Duration::from_millis))
}

/// Outcome of the graceful restart sequence. Deliberately free of HTTP / exit
/// side effects so the sequence is unit-testable without killing the process.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GracefulRestartOutcome {
    /// Drain completed and the owned DS4 child was stopped.
    Ready { drain_timeout_ms: u128 },
    /// In-flight requests did not drain within the timeout.
    DrainTimeout {
        in_flight: usize,
        drain_timeout_ms: u128,
    },
    /// The owned child's identity no longer matches; do not force-kill.
    ChildIdentityMismatch,
    /// The owned child could not be stopped.
    ChildStopFailed,
}

/// C-04b: admission block → in-flight drain → owned DS4 child stop を実行する。
///
/// - drain timeout 時は強制 kill せず `DrainTimeout` を返す。
/// - owned child identity mismatch 時は強制 kill せず `ChildIdentityMismatch` を返す。
/// - process exit は行わない（`perform_graceful_restart` が成功時に exit を予約する）。
///
/// テストはこの sequence を直接呼び、exit 副作用なしで drain / timeout /
/// identity mismatch を検証できる。
async fn graceful_restart_sequence(
    state: &Arc<AppState>,
    supervisor: &Arc<StandaloneSupervisor>,
    drain_timeout: std::time::Duration,
) -> GracefulRestartOutcome {
    // 1. admission block: 新規リクエストを受け付けない。
    state.proxy.admission().block();

    // 2. in-flight drain: 進行中のリクエストが 0 になるまで待つ。timeout 時は
    //    強制 kill せず、進行中フラグを解放して再試行可能にする（呼び出し側）。
    if matches!(
        state.proxy.admission().drain(0, drain_timeout).await,
        Err(DrainError::Timeout)
    ) {
        let in_flight = state.proxy.admission().snapshot().in_flight;
        return GracefulRestartOutcome::DrainTimeout {
            in_flight,
            drain_timeout_ms: drain_timeout.as_millis(),
        };
    }

    // 3. owned DS4 child stop: supervisor が所有する child のみを停止する。
    //    identity mismatch 時は強制 kill せず error を返す。cluster lifecycle
    //    owner を迂回した signal や unknown PID の kill は行わない。
    match crate::cluster::LocalStandaloneLifecycle::stop(supervisor.as_ref()).await {
        Ok(()) => GracefulRestartOutcome::Ready {
            drain_timeout_ms: drain_timeout.as_millis(),
        },
        Err(error) if is_graceful_identity_mismatch(&error) => {
            GracefulRestartOutcome::ChildIdentityMismatch
        }
        Err(error) => {
            tracing::warn!(error = %error, "graceful restart child stop failed");
            GracefulRestartOutcome::ChildStopFailed
        }
    }
}

/// `ProcessControlError::IdentityMismatch` を検出する。強制 kill を避け、
/// unknown PID の kill を行わないため、owned child の identity が期待と
/// 一致しない場合は `ChildIdentityMismatch` へ写像する。
fn is_graceful_identity_mismatch(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ProcessControlError>(),
        Some(ProcessControlError::IdentityMismatch)
    )
}

/// `/admin/restart` の graceful restart 処理。`graceful_restart_sequence` の
/// 結果を HTTP response へ写像し、`Ready` 時のみ exit を予約する。exit は
/// `spawn_process_restart` が 100ms 後に launchd KeepAlive 経由で新 binary を
/// 起動する。response 返却前に exit しないことで、client へ曖昧な
/// transport error を見せない。失敗時は進行中フラグを解放して再試行可能にする。
async fn perform_graceful_restart(
    state: &Arc<AppState>,
    supervisor: &Arc<StandaloneSupervisor>,
    drain_timeout: std::time::Duration,
) -> Response<Body> {
    match graceful_restart_sequence(state, supervisor, drain_timeout).await {
        GracefulRestartOutcome::Ready { drain_timeout_ms } => {
            let response = json_response(
                StatusCode::ACCEPTED,
                json!({
                    "restart": "accepted",
                    "drain_timeout_ms": drain_timeout_ms,
                }),
            );
            spawn_process_restart();
            response
        }
        GracefulRestartOutcome::DrainTimeout {
            in_flight,
            drain_timeout_ms,
        } => {
            state.release_graceful_restart();
            json_response(
                StatusCode::CONFLICT,
                json!({
                    "error": "drain_timeout",
                    "in_flight": in_flight,
                    "drain_timeout_ms": drain_timeout_ms,
                }),
            )
        }
        GracefulRestartOutcome::ChildIdentityMismatch => {
            state.release_graceful_restart();
            json_response(
                StatusCode::CONFLICT,
                json!({"error": "child_identity_mismatch"}),
            )
        }
        GracefulRestartOutcome::ChildStopFailed => {
            state.release_graceful_restart();
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "child_stop_failed"}),
            )
        }
    }
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
        Ok(job) => match serde_json::to_value(job) {
            Ok(value) => json_response(StatusCode::ACCEPTED, value),
            Err(error) => {
                tracing::error!(error = %error, "failed to serialize admin job");
                json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "failed to serialize admin job"}),
                )
            }
        },
        Err(profile) => json_response(
            StatusCode::CONFLICT,
            json!({"error": format!("fingerprint job already running for {}", profile.as_str())}),
        ),
    }
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": runtime_version(),
        "git_commit": runtime_git_commit(),
        "build_number": runtime_build_number(),
    }))
}

/// runtime crate version。ビルド時 metadata は配布 bundle の Info.plist と比較するために
/// read-only admin response から取得できるようにする（B-01）。git commit と build number は
/// ビルド時に環境変数から注入され、未設定時は "unknown" を返す。user data を作成しない。
fn runtime_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn runtime_git_commit() -> &'static str {
    option_env!("SIDEROSTAT_GIT_COMMIT").unwrap_or("unknown")
}

fn runtime_build_number() -> &'static str {
    option_env!("SIDEROSTAT_BUILD_NUMBER").unwrap_or("unknown")
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
    let generation = snapshot.map_or(0, |snapshot| snapshot.generation);
    let diagnostics = state.production_diagnostics().await;
    let (control_session, children) = match &diagnostics {
        Some(diagnostics) => (
            control_session_json(&diagnostics.control_session),
            children_json(&diagnostics.children),
        ),
        None => (Value::Null, Value::Null),
    };
    Json(json!({
        "node_id": state.config.node_id,
        "role": snapshot.map_or("unknown", |snapshot| snapshot.role.name()),
        "mode": snapshot.map_or(if solo { "solo-standalone" } else { "unknown" }, |snapshot| snapshot.stable_mode.name()),
        "state": snapshot.map_or(if solo { "solo-standalone-ready" } else { "booting" }, |snapshot| snapshot.state.name()),
        "generation": generation,
        "cluster_generation": generation,
        "target": target_name(target.target),
        "target_ready": target.ready,
        "admission": admission_json(admission),
        "control_session": control_session,
        "children": children,
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
    response_with_body(
        StatusCode::OK,
        "text/plain; version=0.0.4",
        Body::from(body),
    )
}

/// Return coordinator metrics for the worker-side monitor without exposing the coordinator's
/// loopback-only admin listener. The production runtime performs the authenticated control-plane
/// request to the coordinator and returns the Prometheus text unchanged.
async fn coordinator_metrics(State(state): State<Arc<AppState>>) -> Response<Body> {
    let production = state
        .production
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(production) = production else {
        return response_with_body(
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain; charset=utf-8",
            Body::from("coordinator metrics unavailable"),
        );
    };
    if production.role() != LocalRole::Worker {
        return response_with_body(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            Body::from("coordinator metrics are only available on a worker"),
        );
    }
    match production.fetch_coordinator_metrics().await {
        Ok(body) => response_with_body(
            StatusCode::OK,
            "text/plain; version=0.0.4",
            Body::from(body),
        ),
        Err(error) => {
            tracing::warn!(error = %error, "coordinator metrics fetch failed");
            response_with_body(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                Body::from("coordinator metrics unavailable"),
            )
        }
    }
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let body = match serde_json::to_vec(&value) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(error = %error, "failed to serialize JSON response");
            br#"{"error":"failed to serialize JSON response"}"#.to_vec()
        }
    };
    response_with_body(status, "application/json", body)
}

fn response_with_body(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Body>,
) -> Response<Body> {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
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

fn control_session_json(session: &ControlSessionDiagnostics) -> Value {
    json!({
        "generation": session.generation,
        "phase": distributed_phase_name(session.phase),
        "role": control_role_json(session.role),
        "lease": lease_json(&session.lease),
    })
}

fn lease_json(lease: &LeaseDiagnostics) -> Value {
    json!({
        "valid": lease.valid,
        "expires-at-millis": lease.expires_at_millis,
        "route-scoped": lease.route_scoped,
        "peer-present": lease.peer_present,
        "peer": lease.peer.as_ref().map(peer_json).unwrap_or(Value::Null),
    })
}

fn peer_json(peer: &PeerDiagnostics) -> Value {
    json!({
        "node-id": peer.node_id,
        "role": control_role_json(peer.role),
        "generation": peer.generation,
        "mode": control_mode_json(peer.mode),
    })
}

fn children_json(children: &ChildrenDiagnostics) -> Value {
    json!({
        "standalone": children.standalone.as_ref().map(child_json).unwrap_or(Value::Null),
        "distributed-coordinator": children.distributed_coordinator.as_ref().map(child_json).unwrap_or(Value::Null),
        "distributed-worker": children.distributed_worker.as_ref().map(child_json).unwrap_or(Value::Null),
    })
}

fn child_json(child: &ChildDiagnostics) -> Value {
    json!({
        "pid": child.pid,
        "profile": child.profile,
        "generation": child.generation,
        "running": child.running,
        "ready": child.ready,
    })
}

fn control_role_json(role: ControlRole) -> Value {
    serde_json::to_value(role).unwrap_or(Value::Null)
}

fn control_mode_json(mode: ControlMode) -> Value {
    serde_json::to_value(mode).unwrap_or(Value::Null)
}

fn distributed_phase_name(phase: DistributedControlPhase) -> &'static str {
    match phase {
        DistributedControlPhase::Unpaired => "unpaired",
        DistributedControlPhase::Paired => "paired",
        DistributedControlPhase::WorkerPreparing => "worker-preparing",
        DistributedControlPhase::WorkerReady => "worker-ready",
        DistributedControlPhase::Draining => "draining",
        DistributedControlPhase::Drained => "drained",
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
            production: RwLock::new(None),
            supervisor: RwLock::new(None),
            graceful_restart_in_progress: std::sync::atomic::AtomicBool::new(false),
            default_drain_timeout: std::time::Duration::from_millis(5000),
        });
        state.attach_admin(AdminController::new(vec![3; 32], Arc::new(TestAdminExecutor)).unwrap());
        state
    }

    /// Attach a standalone supervisor so `/admin/restart` sees an owned DS4
    /// child owner (C-04a). The command is never spawned by these tests; it
    /// only needs to satisfy `StandaloneSupervisor::new`'s construction.
    fn attach_test_supervisor(state: &Arc<AppState>) {
        use crate::cluster::Ds4Command;
        use std::ffi::OsString;
        let command = Ds4Command {
            executable: std::path::PathBuf::from("/bin/sleep"),
            working_directory: std::path::PathBuf::from("/"),
            argv: vec![OsString::from("3600")],
            profile: crate::cluster::Ds4Profile {
                profile_id: "test-profile".into(),
                model_variant: ModelVariant::Q2Q4,
                residency: Residency::SsdStreaming,
                dspark_required: false,
            },
        };
        let supervisor = Arc::new(StandaloneSupervisor::new(
            command,
            url::Url::parse("http://127.0.0.1:8000/v1/models").unwrap(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_millis(250),
            std::time::Duration::from_secs(5),
            false,
            Arc::new(Metrics::default()),
        ));
        state.attach_supervisor(supervisor);
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
        let health: Value = serde_json::from_str(&health_body).unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], runtime_version());
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
        assert!(health["git_commit"].is_string());
        assert!(health["build_number"].is_string());
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
    async fn cluster_endpoint_exposes_diagnostic_fields_without_production_runtime() {
        let state = test_state(true);
        let (_, body) = get(state, "/cluster").await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["generation"], body["cluster_generation"]);
        assert_eq!(body["control_session"], Value::Null);
        assert_eq!(body["children"], Value::Null);
    }

    #[test]
    fn diagnostics_serialize_to_contract_shape() {
        let diagnostics = crate::cluster::ProductionDiagnostics {
            control_session: ControlSessionDiagnostics {
                generation: 7,
                phase: DistributedControlPhase::WorkerPreparing,
                role: ControlRole::Coordinator,
                lease: LeaseDiagnostics {
                    valid: true,
                    expires_at_millis: Some(1_770_000_000_000_u64),
                    route_scoped: true,
                    peer_present: true,
                    peer: Some(PeerDiagnostics {
                        node_id: "worker-node".into(),
                        role: ControlRole::Worker,
                        generation: 7,
                        mode: ControlMode::DistributedMxfp4,
                    }),
                },
            },
            children: ChildrenDiagnostics {
                standalone: Some(ChildDiagnostics {
                    pid: Some(1234),
                    profile: Some("standalone".into()),
                    generation: Some(12),
                    running: true,
                    ready: Some(true),
                }),
                distributed_coordinator: Some(ChildDiagnostics {
                    pid: Some(5678),
                    profile: Some("distributed".into()),
                    generation: Some(12),
                    running: true,
                    ready: None,
                }),
                distributed_worker: None,
            },
        };

        let session = control_session_json(&diagnostics.control_session);
        assert_eq!(session["generation"], 7);
        assert_eq!(session["phase"], "worker-preparing");
        assert_eq!(session["role"], "coordinator");
        assert_eq!(session["lease"]["valid"], true);
        assert_eq!(session["lease"]["expires-at-millis"], 1_770_000_000_000_i64);
        assert_eq!(session["lease"]["route-scoped"], true);
        assert_eq!(session["lease"]["peer-present"], true);
        assert_eq!(session["lease"]["peer"]["node-id"], "worker-node");
        assert_eq!(session["lease"]["peer"]["role"], "worker");
        assert_eq!(session["lease"]["peer"]["generation"], 7);
        assert_eq!(session["lease"]["peer"]["mode"], "distributed-mxfp4");

        let children = children_json(&diagnostics.children);
        assert_eq!(children["standalone"]["pid"], 1234);
        assert_eq!(children["standalone"]["profile"], "standalone");
        assert_eq!(children["standalone"]["generation"], 12);
        assert_eq!(children["standalone"]["running"], true);
        assert_eq!(children["standalone"]["ready"], true);
        assert_eq!(children["distributed-coordinator"]["pid"], 5678);
        assert_eq!(children["distributed-coordinator"]["running"], true);
        // Distributed children omit the `ready` field.
        assert!(children["distributed-coordinator"]["ready"].is_null());
        assert_eq!(children["distributed-worker"], Value::Null);
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

    // ---- C-04a: `/admin/restart` route, auth, body parse, duplicate guard ----

    async fn post_admin_restart(
        state: Arc<AppState>,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut request =
            Request::post("/admin/restart").header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", token);
        }
        let response = admin_router(state)
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn graceful_restart_requires_bearer_token() {
        let state = test_state(true);
        attach_test_supervisor(&state);
        let (missing, missing_body) = post_admin_restart(state.clone(), None, "").await;
        let (wrong, _) = post_admin_restart(state.clone(), Some("Bearer deadbeef"), "").await;
        assert_eq!(missing, StatusCode::UNAUTHORIZED);
        assert!(missing_body.contains("unauthorized"));
        assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn resolve_drain_timeout_uses_default_when_body_omitted() {
        let default = std::time::Duration::from_millis(5000);
        // body 省略時は cluster stop timeout (test では 5000ms) を既定値にする。
        let timeout = resolve_drain_timeout(&Bytes::new(), default).unwrap();
        assert_eq!(timeout, default);
    }

    #[test]
    fn resolve_drain_timeout_uses_requested_value_when_present() {
        let default = std::time::Duration::from_millis(5000);
        let body = Bytes::from(r#"{"drain_timeout_ms": 12000}"#);
        let timeout = resolve_drain_timeout(&body, default).unwrap();
        assert_eq!(timeout, std::time::Duration::from_millis(12000));
    }

    #[test]
    fn resolve_drain_timeout_rejects_malformed_or_unknown_body_fields() {
        let default = std::time::Duration::from_millis(5000);
        let malformed = resolve_drain_timeout(&Bytes::from("not-json"), default);
        let unknown = resolve_drain_timeout(
            &Bytes::from(r#"{"drain_timeout_ms": 1000, "surprise": true}"#),
            default,
        );
        assert!(malformed.is_err());
        assert!(unknown.is_err());
    }

    #[tokio::test]
    async fn duplicate_graceful_restart_is_rejected_while_one_is_in_progress() {
        let state = test_state(true);
        attach_test_supervisor(&state);
        // 進行中フラグを直接立てて、handler が重複を 409 で拒否することを確認する。
        // （成功 path は exit を伴うため handler 経由では実行しない。）
        assert!(state.try_claim_graceful_restart());
        let token = format!("Bearer {}", crate::cluster::encode_token(&[3; 32]));
        let (duplicate, body) = post_admin_restart(state.clone(), Some(&token), "").await;
        assert_eq!(duplicate, StatusCode::CONFLICT);
        assert!(body.contains("restart_in_progress"));
    }

    #[tokio::test]
    async fn graceful_restart_rejected_without_standalone_supervisor() {
        // cluster 有効・distributed 等で supervisor 不在の場合は graceful restart を拒否する。
        let state = test_state(true);
        let token = format!("Bearer {}", crate::cluster::encode_token(&[3; 32]));
        let (status, body) = post_admin_restart(state, Some(&token), "").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("graceful_restart_unavailable"));
    }

    // ---- C-04b: graceful restart sequence (block → drain → child stop) ----
    // 成功 path は exit を伴うため、HTTP 応答を直接検証せず、exit 副作用のない
    // `graceful_restart_sequence` で drain / timeout / identity mismatch を検証する。

    #[tokio::test]
    async fn graceful_restart_sequence_blocks_admission_and_readies_without_child() {
        let state = test_state(true);
        attach_test_supervisor(&state);
        let supervisor = state.supervisor().unwrap();
        let outcome =
            graceful_restart_sequence(&state, &supervisor, std::time::Duration::from_secs(5)).await;
        assert_eq!(
            outcome,
            GracefulRestartOutcome::Ready {
                drain_timeout_ms: 5000
            }
        );
        // admission は drain 完了後に Blocked になる。
        assert_eq!(
            state.proxy.admission().snapshot().state,
            AdmissionState::Blocked
        );
        assert_eq!(state.proxy.admission().snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn graceful_restart_sequence_returns_drain_timeout_with_in_flight() {
        let state = test_state(true);
        attach_test_supervisor(&state);
        // 進行中リクエストを 1 つ保持し、drain を timeout させる。
        state.proxy.admission().start_serving();
        let _permit = state
            .proxy
            .admission()
            .try_acquire(true)
            .expect("permit should be acquirable");
        let supervisor = state.supervisor().unwrap();
        let outcome =
            graceful_restart_sequence(&state, &supervisor, std::time::Duration::from_millis(10))
                .await;
        assert_eq!(
            outcome,
            GracefulRestartOutcome::DrainTimeout {
                in_flight: 1,
                drain_timeout_ms: 10,
            }
        );
        // timeout 時は in-flight permit を drop せず、強制 kill しない。
        assert_eq!(state.proxy.admission().snapshot().in_flight, 1);
    }

    #[test]
    fn graceful_identity_mismatch_is_detected_without_force_kill() {
        // owned child の identity が期待と一致しない場合、ChildIdentityMismatch
        // へ写像され、強制 kill や unknown PID の kill を行わない。
        let error = anyhow::anyhow!(crate::cluster::ProcessControlError::IdentityMismatch);
        assert!(is_graceful_identity_mismatch(&error));
        let other = anyhow::anyhow!("generic failure");
        assert!(!is_graceful_identity_mismatch(&other));
    }
}
