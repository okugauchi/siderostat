//! siderostat-monitor: macOS menu bar monitor for siderostat.
//!
//! Polls the siderostat admin metrics endpoints and renders the state through a
//! tray-icon menu bar item. Worker/coordinator routing is selected from `/cluster`.
//! Polling runs on a separate thread; the AppKit
//! event loop runs on the main thread (tray-icon macOS requirement) and a
//! CFRunLoop timer refreshes the tray from the shared state.

mod client;
mod config;
mod launchd;
mod localization;
mod metrics;
mod migration;
mod operation;
mod service_management;
mod settings;
mod state;
mod tray;

use crate::{
    client::MetricsClient,
    config::MonitorConfig,
    localization::{app_metadata_info, text},
    migration::LegacyInventory,
    operation::{OperationKind, OperationOutcome, OperationState},
    service_management::ServiceStatus,
    state::{
        DisplayState, FirstLaunchEvent, FirstLaunchState, VersionHandshake, first_launch_reducer,
        version_handshake_with_build,
    },
    tray::MonitorTray,
};
use anyhow::{Context, Result};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use siderostat_core::config::ModeAwareConfig;
use siderostat_core::notify::{
    DesktopNotificationService, Notification, NotifyPlatform, build_notifier,
    start_notification_relay,
};
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tray_icon::menu::MenuEvent;

const SERVICE_STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Main-thread refresh context for the CFRunLoop timer. The tray pointer stays
/// valid because the tray is created on the main thread and lives until the
/// process exits (`app.run()` blocks on the same thread).
struct UpdateContext {
    shared: Arc<Mutex<DisplayState>>,
    operation: Arc<Mutex<OperationState>>,
    first_launch: Arc<Mutex<FirstLaunchState>>,
    first_launch_client: MetricsClient,
    first_launch_readiness_started: Arc<AtomicBool>,
    tray: *const MonitorTray,
    service_statuses: Cell<(ServiceStatus, ServiceStatus)>,
    service_status_checked_at: Cell<Instant>,
}

fn main() -> Result<()> {
    initialize_logging();
    let (config, config_path, monitor_config_valid) = match MonitorConfig::load() {
        Ok((config, path)) => (config, path, true),
        Err(error) => {
            // Keep the accessory app usable so the user can open and repair an
            // invalid configuration from the menu. Registration is blocked by
            // the first-launch reducer until validation succeeds.
            tracing::error!(error = %error, "monitor configuration validation failed");
            (MonitorConfig::default(), None, false)
        }
    };
    if let Some(path) = &config_path {
        tracing::info!(config_path = %path.display(), "monitor configuration loaded");
    } else {
        tracing::info!("monitor configuration loaded with defaults");
    }
    let config_valid = if launchd::is_bundle_mode() {
        match validate_runtime_configuration() {
            Ok(()) => monitor_config_valid,
            Err(error) => {
                tracing::error!(error = %error, "runtime configuration validation failed");
                false
            }
        }
    } else {
        monitor_config_valid
    };

    // Runtime is a helper executable rather than an app bundle, so it forwards
    // desktop notifications to the signed Siderostat.app process over this
    // per-user Unix socket. The native UserNotifications call is made by the
    // relay thread while this process owns the app bundle and AppKit session.
    let _notification_relay = if launchd::is_bundle_mode() {
        match start_notification_relay(true, true) {
            Ok(handle) => Some(handle),
            Err(error) => {
                tracing::warn!(error = %error, "notification relay was not started");
                None
            }
        }
    } else {
        None
    };

    let client = MetricsClient::new(&config)?;
    let shared = Arc::new(Mutex::new(DisplayState::default()));
    let operation = Arc::new(Mutex::new(OperationState::default()));
    let first_launch = Arc::new(Mutex::new(FirstLaunchState::VersionShown));
    let first_launch_readiness_started = Arc::new(AtomicBool::new(false));
    let app_metadata = app_metadata_info();

    // Polling runs on a separate thread with its own Tokio runtime so the
    // main thread stays free for the AppKit menu bar event loop.
    // The menu-bar graceful-restart path needs its own handle to the client.
    let menu_client = Arc::new(client.clone());
    let poll_client = client.clone();
    let poll_state = shared.clone();
    thread::Builder::new()
        .name("siderostat-monitor-poll".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(poll_loop(poll_client, poll_state));
            Ok::<(), anyhow::Error>(())
        })?;
    spawn_version_handshake_poll(
        client.clone(),
        app_metadata.version,
        app_metadata.build,
        operation.clone(),
    )?;

    // AppKit setup on the main thread (tray-icon macOS requirement): create the
    // NSApplication, run as a background menu-bar (accessory) app, then create
    // the tray. Without the AppKit event loop below, the status item is created
    // (and appears in System Settings) but never drawn in the menu bar.
    let mtm = MainThreadMarker::new().expect("tray creation requires the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    tracing::info!("NSApplication ready (accessory policy)");

    let tray = MonitorTray::new(config.show_decode_tps, config.live_metric)?;
    {
        let display = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tray.update(&display);
    }
    let (runtime_status, login_item_status) = service_statuses();
    tray.update_registration(runtime_status, login_item_status);
    tray.update_operation(&OperationState::default());
    {
        let state = first_launch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tray.update_first_launch(&state);
    }
    initialize_first_launch(
        &first_launch,
        &tray,
        &client,
        config_valid,
        runtime_status,
        login_item_status,
        &first_launch_readiness_started,
    );

    // LaunchAgent operations run on dedicated threads so the AppKit main loop is
    // never blocked. In bundle mode (C-05a) the runtime and monitor are managed
    // through Service Management and the graceful-restart admin endpoint, never
    // through launchctl.
    let menu_operation = operation.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if MonitorTray::is_quit_event(&event) {
            tracing::info!("quit requested from menu");
            let _ = thread::Builder::new()
                .name("siderostat-monitor-stop".into())
                .spawn(move || {
                    if launchd::is_bundle_mode() {
                        // bundle mode: stop only this monitor process; the runtime
                        // keeps running. launchctl bootout is not used (C-05a).
                        tracing::info!("exiting monitor process (bundle mode)");
                        std::process::exit(0);
                    } else if let Err(error) = launchd::bootout_runtime_and_monitor() {
                        tracing::warn!(error = %error, "LaunchAgent stop failed");
                    }
                });
        } else if MonitorTray::is_runtime_restart_event(&event) {
            if runtime_service_status() != ServiceStatus::Enabled {
                tracing::info!(
                    "runtime restart ignored because the runtime service is not enabled"
                );
                return;
            }
            if !begin_operation(&menu_operation, OperationKind::RuntimeRestart) {
                return;
            }
            tracing::info!("runtime restart requested from menu");
            let client = menu_client.clone();
            let operation = menu_operation.clone();
            let spawn_result = thread::Builder::new()
                .name("siderostat-runtime-restart".into())
                .spawn(move || {
                    let outcome = if launchd::is_bundle_mode() {
                        // bundle mode: graceful restart via the authenticated
                        // /admin/restart endpoint (C-04). launchctl kickstart
                        // is not used (C-05a).
                        let runtime = match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(runtime) => runtime,
                            Err(error) => {
                                tracing::warn!(error = %error, "restart runtime could not be built");
                                finish_operation(
                                    &operation,
                                    OperationKind::RuntimeRestart,
                                    OperationOutcome::Failed,
                                );
                                return;
                            }
                        };
                        runtime.block_on(async {
                            match client.graceful_restart().await {
                                Ok((status, body)) if status.is_success() => {
                                    tracing::info!(status = %status, response = %body, "graceful restart requested");
                                    OperationOutcome::Succeeded
                                }
                                Ok((status, body))
                                    if status == reqwest::StatusCode::UNAUTHORIZED
                                        || status == reqwest::StatusCode::FORBIDDEN =>
                                {
                                    tracing::warn!(status = %status, response = %body, "graceful restart request was denied");
                                    OperationOutcome::Denied
                                }
                                Ok((status, body)) => {
                                    tracing::warn!(status = %status, response = %body, "graceful restart request failed");
                                    OperationOutcome::Failed
                                }
                                Err(error) => {
                                    tracing::warn!(error = %error, "graceful restart request failed");
                                    OperationOutcome::Failed
                                }
                            }
                        })
                    } else {
                        match launchd::kickstart(launchd::RUNTIME_LABEL) {
                            Ok(()) => OperationOutcome::Succeeded,
                            Err(error) => {
                                tracing::warn!(error = %error, "Runtime restart failed");
                                OperationOutcome::Failed
                            }
                        }
                    };
                    finish_operation(&operation, OperationKind::RuntimeRestart, outcome);
                });
            if let Err(error) = spawn_result {
                tracing::warn!(error = %error, "could not spawn runtime restart operation");
                finish_operation(
                    &menu_operation,
                    OperationKind::RuntimeRestart,
                    OperationOutcome::Failed,
                );
            }
        } else if MonitorTray::is_bg_toggle_event(&event) {
            let start = match runtime_service_status() {
                ServiceStatus::Enabled => false,
                ServiceStatus::NotRegistered | ServiceStatus::RequiresApproval => true,
                status => {
                    tracing::info!(%status, "background toggle ignored for unavailable service");
                    return;
                }
            };
            let kind = if start {
                OperationKind::BackgroundStart
            } else {
                OperationKind::BackgroundStop
            };
            if !begin_operation(&menu_operation, kind) {
                return;
            }
            tracing::info!(start, "background execution toggle requested from menu");
            let outcome = register_runtime(start);
            finish_operation(&menu_operation, kind, outcome);
        } else if MonitorTray::is_open_config_event(&event) {
            if !begin_operation(&menu_operation, OperationKind::OpenConfig) {
                return;
            }
            tracing::info!("open configuration requested from menu");
            let operation = menu_operation.clone();
            let spawn_result = thread::Builder::new()
                .name("siderostat-open-config".into())
                .spawn(move || {
                    let outcome = match settings::open_runtime_config() {
                        Ok(()) => OperationOutcome::Succeeded,
                        Err(error) => {
                            tracing::warn!(error = %error, "Open configuration failed");
                            OperationOutcome::Failed
                        }
                    };
                    finish_operation(&operation, OperationKind::OpenConfig, outcome);
                });
            if let Err(error) = spawn_result {
                tracing::warn!(error = %error, "could not spawn open configuration operation");
                finish_operation(
                    &menu_operation,
                    OperationKind::OpenConfig,
                    OperationOutcome::Failed,
                );
            }
        } else if MonitorTray::is_open_login_items_event(&event) {
            if !begin_operation(&menu_operation, OperationKind::OpenLoginItems) {
                return;
            }
            tracing::info!("open login items requested from menu");
            let operation = menu_operation.clone();
            let spawn_result = thread::Builder::new()
                .name("siderostat-open-login-items".into())
                .spawn(move || {
                    let outcome = match settings::open_login_items() {
                        Ok(()) => OperationOutcome::Succeeded,
                        Err(error) => {
                            tracing::warn!(error = %error, "Open Login Items failed");
                            OperationOutcome::Failed
                        }
                    };
                    finish_operation(&operation, OperationKind::OpenLoginItems, outcome);
                });
            if let Err(error) = spawn_result {
                tracing::warn!(error = %error, "could not spawn open Login Items operation");
                finish_operation(
                    &menu_operation,
                    OperationKind::OpenLoginItems,
                    OperationOutcome::Failed,
                );
            }
        }
    }));

    // Periodic tray refresh on the main run loop via a CFRunLoop timer.
    let update = Box::new(UpdateContext {
        shared: shared.clone(),
        operation,
        first_launch,
        first_launch_client: client,
        first_launch_readiness_started,
        tray: &tray as *const MonitorTray,
        service_statuses: Cell::new((runtime_status, login_item_status)),
        service_status_checked_at: Cell::new(Instant::now()),
    });
    let update_ptr: *mut UpdateContext = Box::into_raw(update);
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: update_ptr.cast::<c_void>(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let timer =
        unsafe { CFRunLoopTimer::new(None, 0.0, 0.5, 0, 0, Some(refresh_callback), &mut context) }
            .expect("create tray refresh timer");
    if let Some(run_loop) = CFRunLoop::main() {
        unsafe {
            run_loop.add_timer(Some(&*timer), kCFRunLoopCommonModes);
        }
    }

    // Run the AppKit main event loop. This is what actually draws the menu bar
    // item and processes menu clicks. It returns only after the quit menu item
    // terminates the app.
    tracing::info!("running AppKit main loop");
    app.run();

    tracing::info!("monitor exiting");
    Ok(())
}

/// CFRunLoop timer callback: refresh the tray from shared state on the main thread.
unsafe extern "C-unwind" fn refresh_callback(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
    // SAFETY: `info` points to the leaked UpdateContext, valid for the timer lifetime.
    let context = unsafe { &*info.cast::<UpdateContext>() };
    let display = context
        .shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // SAFETY: `context.tray` points to the tray created on the main thread,
    // alive until the process exits.
    let tray = unsafe { &*context.tray };
    tray.update(&display);
    let (runtime_status, login_item_status) = refresh_service_statuses(context);
    tray.update_registration(runtime_status, login_item_status);
    if resume_first_launch_after_approval(&context.first_launch, runtime_status, login_item_status)
    {
        spawn_first_launch_readiness(
            &context.first_launch,
            context.first_launch_client.clone(),
            &context.first_launch_readiness_started,
        );
    }
    let first_launch = context
        .first_launch
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tray.update_first_launch(&first_launch);
    let operation = context
        .operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tray.update_operation(&operation);
}

/// Feed one event into the shared first-launch reducer.
fn reduce_first_launch(
    first_launch: &Arc<Mutex<FirstLaunchState>>,
    event: FirstLaunchEvent,
) -> FirstLaunchState {
    let mut state = first_launch
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let next = first_launch_reducer(state.clone(), event);
    *state = next.clone();
    next
}

/// Update the first-launch status while the tray is still being initialized on
/// the AppKit main thread.
fn reduce_first_launch_with_tray(
    first_launch: &Arc<Mutex<FirstLaunchState>>,
    tray: &MonitorTray,
    event: FirstLaunchEvent,
) -> FirstLaunchState {
    let next = reduce_first_launch(first_launch, event);
    tray.update_first_launch(&next);
    next
}

/// Run the non-blocking startup portion of the first-launch sequence.
///
/// Inventory and Service Management status are read on the AppKit thread. The
/// runtime registration call is also kept on that thread as required by
/// `SMAppService`. Only the potentially long admin/readiness waits are moved
/// to a worker thread, so the menu remains usable while the model starts.
fn initialize_first_launch(
    first_launch: &Arc<Mutex<FirstLaunchState>>,
    tray: &MonitorTray,
    client: &MetricsClient,
    config_valid: bool,
    runtime_status: ServiceStatus,
    main_app_login_status: ServiceStatus,
    readiness_started: &Arc<AtomicBool>,
) {
    let legacy_present = match collect_legacy_inventory() {
        Ok(inventory) => !inventory.is_empty(),
        Err(error) => {
            tracing::warn!(error = %error, "legacy inventory could not be collected");
            false
        }
    };
    reduce_first_launch_with_tray(
        first_launch,
        tray,
        FirstLaunchEvent::InventoryChecked { legacy_present },
    );
    reduce_first_launch_with_tray(
        first_launch,
        tray,
        FirstLaunchEvent::ConfigChecked {
            valid: config_valid,
        },
    );
    if !config_valid {
        return;
    }

    reduce_first_launch_with_tray(
        first_launch,
        tray,
        FirstLaunchEvent::ServiceStatusesChecked {
            runtime_status,
            main_app_login_status,
        },
    );
    reduce_first_launch_with_tray(first_launch, tray, FirstLaunchEvent::RegisterRequested);

    let outcome = register_first_launch_services(runtime_status, main_app_login_status);
    let event = match outcome {
        OperationOutcome::Succeeded => FirstLaunchEvent::RegisterSucceeded,
        OperationOutcome::RequiresApproval => FirstLaunchEvent::RegisterRequiresApproval,
        OperationOutcome::Denied | OperationOutcome::Failed => FirstLaunchEvent::RegisterFailed,
    };
    let state = reduce_first_launch_with_tray(first_launch, tray, event);
    if state == FirstLaunchState::Registered {
        let _ = reduce_first_launch_with_tray(
            first_launch,
            tray,
            FirstLaunchEvent::MonitorLoginConfirmed,
        );
        spawn_first_launch_readiness(first_launch, client.clone(), readiness_started);
    }
}

/// Continue the first-launch flow after the user approves either service in
/// System Settings. Both independent registrations must be enabled before the
/// readiness worker starts.
fn resume_first_launch_after_approval(
    first_launch: &Arc<Mutex<FirstLaunchState>>,
    runtime_status: ServiceStatus,
    main_app_login_status: ServiceStatus,
) -> bool {
    let mut state = first_launch
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *state != FirstLaunchState::RequiresApproval
        || runtime_status != ServiceStatus::Enabled
        || main_app_login_status != ServiceStatus::Enabled
    {
        return false;
    }
    *state = first_launch_reducer(
        state.clone(),
        FirstLaunchEvent::ServiceStatusesChecked {
            runtime_status,
            main_app_login_status,
        },
    );
    *state = first_launch_reducer(state.clone(), FirstLaunchEvent::MonitorLoginConfirmed);
    true
}

/// Wait for the runtime admin API and then for `/readyz`. Neither wait blocks
/// the AppKit event loop or loads the model in the monitor process.
fn spawn_first_launch_readiness(
    first_launch: &Arc<Mutex<FirstLaunchState>>,
    client: MetricsClient,
    readiness_started: &Arc<AtomicBool>,
) {
    if readiness_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let first_launch = first_launch.clone();
    let worker_readiness_started = readiness_started.clone();
    let spawn_result = thread::Builder::new()
        .name("siderostat-first-launch".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(error = %error, "first-launch runtime could not be built");
                    worker_readiness_started.store(false, Ordering::Release);
                    return;
                }
            };
            runtime.block_on(async move {
                loop {
                    match client.health().await {
                        Ok(version) => {
                            tracing::info!(
                                version = %version.version,
                                build_number = %version.build_number,
                                "first-launch runtime admin API is ready"
                            );
                            break;
                        }
                        Err(error) => {
                            tracing::debug!(error = %error, "waiting for first-launch runtime admin API");
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
                reduce_first_launch(&first_launch, FirstLaunchEvent::RuntimeAdminReady);

                loop {
                    match client.ready().await {
                        Ok(true) => break,
                        Ok(false) => {
                            tracing::debug!("waiting for first-launch model readiness");
                        }
                        Err(error) => {
                            tracing::debug!(error = %error, "waiting for first-launch readiness endpoint");
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                reduce_first_launch(&first_launch, FirstLaunchEvent::ModelReady);
                tracing::info!("first-launch readiness complete");
            });
        });
    if let Err(error) = spawn_result {
        tracing::warn!(error = %error, "could not spawn first-launch readiness worker");
        readiness_started.store(false, Ordering::Release);
    }
}

/// Collect the D-01 read-only legacy inventory on the main thread. The
/// SMAppService legacy status lookup is intentionally not attempted from the
/// readiness worker because the framework requires the AppKit thread.
fn collect_legacy_inventory() -> Result<LegacyInventory> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let usr_local_bin = Path::new("/usr/local/bin");
    let mut inventory = migration::inventory_legacy(&home, usr_local_bin, &BTreeMap::new())?;
    #[cfg(target_os = "macos")]
    for plist in &inventory.plists {
        if let Some(status) = migration::legacy_plist_status(&plist.path) {
            inventory.legacy_status.insert(plist.path.clone(), status);
        }
    }
    Ok(inventory)
}

/// Validate the runtime's user configuration and all referenced secrets and
/// manifests without starting the runtime or exposing their contents. The
/// application bundle uses the same schema validator as the runtime so a
/// configuration accepted here will not be presented as valid when the
/// background service starts.
fn validate_runtime_configuration() -> Result<()> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home).join("Library/Application Support/siderostat/config.toml");
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("read runtime configuration {}", path.display()))?;
    let mut config = ModeAwareConfig::parse(&contents).context("parse runtime configuration")?;
    config
        .expand_paths()
        .context("expand runtime configuration paths")?;
    config.validate().context("validate runtime configuration")
}

#[cfg(target_os = "macos")]
fn service_statuses() -> (ServiceStatus, ServiceStatus) {
    use crate::service_management::{ServiceKind, ServiceManagement, ServiceManagementAdapter};

    let adapter = ServiceManagement::new();
    (
        adapter.status(ServiceKind::RuntimeAgent),
        adapter.status(ServiceKind::MainAppLoginItem),
    )
}

#[cfg(target_os = "macos")]
fn refresh_service_statuses(context: &UpdateContext) -> (ServiceStatus, ServiceStatus) {
    let now = Instant::now();
    if now.duration_since(context.service_status_checked_at.get())
        >= SERVICE_STATUS_REFRESH_INTERVAL
    {
        let statuses = service_statuses();
        context.service_statuses.set(statuses);
        context.service_status_checked_at.set(now);
    }
    context.service_statuses.get()
}

#[cfg(not(target_os = "macos"))]
fn refresh_service_statuses(context: &UpdateContext) -> (ServiceStatus, ServiceStatus) {
    context.service_statuses.get()
}

#[cfg(not(target_os = "macos"))]
fn service_statuses() -> (ServiceStatus, ServiceStatus) {
    (ServiceStatus::NotFound, ServiceStatus::NotFound)
}

fn runtime_service_status() -> ServiceStatus {
    service_statuses().0
}

fn begin_operation(operation: &Arc<Mutex<OperationState>>, kind: OperationKind) -> bool {
    let mut state = operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.begin(kind) {
        tracing::info!(
            ?kind,
            "menu operation ignored while another operation is running"
        );
        return false;
    }
    true
}

fn finish_operation(
    operation: &Arc<Mutex<OperationState>>,
    kind: OperationKind,
    outcome: OperationOutcome,
) {
    let mut state = operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.finish(kind, outcome);
}

/// Start the read-only app/runtime version handshake poll. A mismatch emits a
/// state-change notification; restart remains an explicit user action in the
/// existing runtime-restart menu item.
fn spawn_version_handshake_poll(
    client: MetricsClient,
    app_version: String,
    app_build: String,
    operation: Arc<Mutex<OperationState>>,
) -> Result<()> {
    thread::Builder::new()
        .name("siderostat-version-handshake".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let notifier = build_notifier(true, true, NotifyPlatform::detect());
            runtime.block_on(version_handshake_loop(
                client,
                app_version,
                app_build,
                operation,
                DesktopNotificationService::new(notifier),
            ));
            Ok::<(), anyhow::Error>(())
        })?;
    Ok(())
}

async fn version_handshake_loop(
    client: MetricsClient,
    app_version: String,
    app_build: String,
    operation: Arc<Mutex<OperationState>>,
    mut notifications: DesktopNotificationService,
) {
    let mut last_notified = None;
    let mut operation_context = VersionOperationContext::default();
    loop {
        refresh_version_operation_context(&mut operation_context, &operation);
        match client.health().await {
            Ok(runtime) => {
                let handshake = version_handshake_with_build(
                    &app_version,
                    &app_build,
                    &runtime.version,
                    &runtime.build_number,
                );
                if should_notify_version(
                    last_notified,
                    handshake,
                    operation_context.runtime_matched,
                ) {
                    if let Some(notification) = version_notification(
                        handshake,
                        &app_version,
                        &app_build,
                        &runtime.version,
                        &runtime.build_number,
                    ) {
                        notifications.notify(notification);
                    }
                    last_notified = Some(handshake);
                } else if handshake == VersionHandshake::Matched {
                    last_notified = None;
                }
                if handshake == VersionHandshake::Matched {
                    operation_context.runtime_matched = true;
                    operation_context.awaiting_startup_match = false;
                    operation_context.intentional_stop = false;
                }
                tokio::time::sleep(client.poll_interval()).await;
            }
            Err(error) => {
                tracing::debug!(error = %error, "runtime version handshake unavailable");
                refresh_version_operation_context(&mut operation_context, &operation);
                if suppress_version_unavailable(&operation, &operation_context) {
                    tracing::debug!(
                        "runtime version notification suppressed during menu operation"
                    );
                    last_notified = Some(VersionHandshake::Unavailable);
                } else if should_notify_version(
                    last_notified,
                    VersionHandshake::Unavailable,
                    operation_context.runtime_matched,
                ) {
                    if let Some(notification) = version_notification(
                        VersionHandshake::Unavailable,
                        &app_version,
                        &app_build,
                        "--",
                        "--",
                    ) {
                        notifications.notify(notification);
                    }
                    last_notified = Some(VersionHandshake::Unavailable);
                }
                tokio::time::sleep(client.offline_backoff()).await;
            }
        }
    }
}

fn should_notify_version(
    last_notified: Option<VersionHandshake>,
    current: VersionHandshake,
    runtime_matched: bool,
) -> bool {
    if current == VersionHandshake::Matched {
        return false;
    }
    if current == VersionHandshake::Unavailable && !runtime_matched {
        return false;
    }
    last_notified != Some(current)
}

#[derive(Debug, Clone, Copy, Default)]
struct VersionOperationContext {
    previous_operation: OperationState,
    runtime_matched: bool,
    intentional_stop: bool,
    awaiting_startup_match: bool,
}

fn refresh_version_operation_context(
    context: &mut VersionOperationContext,
    operation: &Arc<Mutex<OperationState>>,
) {
    let state = operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current = *state;
    if current == context.previous_operation {
        return;
    }
    match current {
        OperationState::Running(OperationKind::BackgroundStart)
        | OperationState::Running(OperationKind::RuntimeRestart) => {
            context.intentional_stop = false;
            context.runtime_matched = false;
            context.awaiting_startup_match = true;
        }
        OperationState::Running(OperationKind::BackgroundStop) => {
            context.runtime_matched = false;
            context.awaiting_startup_match = false;
        }
        OperationState::Finished {
            kind: OperationKind::BackgroundStart,
            outcome: OperationOutcome::Succeeded,
        }
        | OperationState::Finished {
            kind: OperationKind::RuntimeRestart,
            outcome: OperationOutcome::Succeeded,
        } => {
            context.intentional_stop = false;
            context.runtime_matched = false;
            context.awaiting_startup_match = true;
        }
        OperationState::Finished {
            kind: OperationKind::BackgroundStop,
            outcome: OperationOutcome::Succeeded,
        } => {
            context.runtime_matched = false;
            context.intentional_stop = true;
            context.awaiting_startup_match = false;
        }
        OperationState::Finished {
            kind: OperationKind::BackgroundStart,
            outcome:
                OperationOutcome::RequiresApproval | OperationOutcome::Denied | OperationOutcome::Failed,
        }
        | OperationState::Finished {
            kind: OperationKind::RuntimeRestart,
            outcome: OperationOutcome::Denied | OperationOutcome::Failed,
        } => {
            context.awaiting_startup_match = false;
        }
        OperationState::Finished {
            kind: OperationKind::BackgroundStop,
            outcome: OperationOutcome::Denied | OperationOutcome::Failed,
        } => {
            context.intentional_stop = false;
        }
        _ => {}
    }
    context.previous_operation = current;
}

fn suppress_version_unavailable(
    operation: &Arc<Mutex<OperationState>>,
    context: &VersionOperationContext,
) -> bool {
    if context.intentional_stop || context.awaiting_startup_match {
        return true;
    }
    let state = operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    matches!(
        *state,
        OperationState::Running(OperationKind::BackgroundStart)
            | OperationState::Running(OperationKind::BackgroundStop)
            | OperationState::Running(OperationKind::RuntimeRestart)
    )
}

fn version_notification(
    handshake: VersionHandshake,
    app_version: &str,
    app_build: &str,
    runtime_version: &str,
    runtime_build: &str,
) -> Option<Notification> {
    let title = text("notification.version.title", "Siderostat Version");
    let details = text(
        "notification.version.details",
        " (Siderostat={app_version} build={app_build}, siderostat-runtime={runtime_version} build={runtime_build})",
    )
    .replace("{app_version}", app_version)
    .replace("{app_build}", app_build)
    .replace("{runtime_version}", runtime_version)
    .replace("{runtime_build}", runtime_build);
    let body = match handshake {
        VersionHandshake::RuntimeOlder => format!(
            "{}{}",
            text(
                "notification.version.older",
                "Siderostat is newer than siderostat-runtime. Restart siderostat-runtime from the menu to apply the Siderostat update.",
            ),
            details
        ),
        VersionHandshake::RuntimeNewer => format!(
            "{}{}",
            text(
                "notification.version.newer",
                "siderostat-runtime is newer than Siderostat. Data is not migrated automatically during rollback.",
            ),
            details
        ),
        VersionHandshake::Unavailable => text(
            "notification.version.unavailable",
            "Cannot read the siderostat-runtime version.",
        ),
        VersionHandshake::Matched => return None,
    };
    Some(Notification::new(title, body))
}

/// Poll the metrics endpoint and update the shared display state.
async fn poll_loop(client: MetricsClient, shared: Arc<Mutex<DisplayState>>) {
    loop {
        let offline = match client.fetch_metrics().await {
            Ok(snapshot) => {
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.apply_metrics(&snapshot);
                tracing::debug!(
                    mode = ?guard.cluster_mode,
                    cluster_state = ?guard.cluster_state,
                    prefill_active = guard.prefill_active,
                    "metrics poll succeeded"
                );
                false
            }
            Err(error) => {
                let health_error = client.health().await.err();
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(health_error) = health_error {
                    tracing::warn!(
                        metrics_error = %error,
                        health_error = %health_error,
                        "runtime health poll failed; monitor offline"
                    );
                    guard.mark_offline();
                    true
                } else {
                    tracing::warn!(
                        metrics_error = %error,
                        "runtime is reachable but metrics poll failed; monitor degraded"
                    );
                    guard.mark_degraded();
                    false
                }
            }
        };
        let interval = if offline {
            client.offline_backoff()
        } else {
            client.poll_interval()
        };
        tokio::time::sleep(interval).await;
    }
}

/// Register both first-launch services independently through Service
/// Management. This runs on the main thread (AppKit requirement) so it must be
/// called from the startup path directly, not from a background thread.
#[cfg(target_os = "macos")]
fn register_first_launch_services(
    runtime_status: ServiceStatus,
    main_app_login_status: ServiceStatus,
) -> OperationOutcome {
    use crate::service_management::ServiceKind;

    let runtime = register_service_if_needed(ServiceKind::RuntimeAgent, runtime_status);
    let main_app = register_service_if_needed(ServiceKind::MainAppLoginItem, main_app_login_status);
    combine_registration_outcomes(runtime, main_app)
}

#[cfg(target_os = "macos")]
fn register_service_if_needed(
    kind: crate::service_management::ServiceKind,
    status: ServiceStatus,
) -> OperationOutcome {
    match status {
        ServiceStatus::Enabled => {
            tracing::info!(service = kind.label(), "service already enabled");
            OperationOutcome::Succeeded
        }
        ServiceStatus::RequiresApproval => {
            tracing::info!(
                service = kind.label(),
                "service is waiting for user approval"
            );
            OperationOutcome::RequiresApproval
        }
        _ => register_service(kind),
    }
}

#[cfg(target_os = "macos")]
fn combine_registration_outcomes(
    runtime: OperationOutcome,
    main_app: OperationOutcome,
) -> OperationOutcome {
    if [runtime, main_app].contains(&OperationOutcome::Failed) {
        OperationOutcome::Failed
    } else if [runtime, main_app].contains(&OperationOutcome::Denied) {
        OperationOutcome::Denied
    } else if [runtime, main_app].contains(&OperationOutcome::RequiresApproval) {
        OperationOutcome::RequiresApproval
    } else {
        OperationOutcome::Succeeded
    }
}

#[cfg(target_os = "macos")]
fn register_service(kind: crate::service_management::ServiceKind) -> OperationOutcome {
    use crate::service_management::{RegisterOutcome, ServiceManagement, ServiceManagementAdapter};

    let adapter = ServiceManagement::new();
    match adapter.register(kind) {
        RegisterOutcome::Registered => {
            tracing::info!(service = kind.label(), "service registered");
            OperationOutcome::Succeeded
        }
        RegisterOutcome::RequiresApproval => {
            tracing::warn!(
                service = kind.label(),
                "service requires approval in System Settings"
            );
            OperationOutcome::RequiresApproval
        }
        RegisterOutcome::DeniedByUser => {
            tracing::warn!(
                service = kind.label(),
                "service registration denied by user"
            );
            OperationOutcome::Denied
        }
        RegisterOutcome::Error(message) => {
            tracing::warn!(
                service = kind.label(),
                message,
                "service registration failed"
            );
            OperationOutcome::Failed
        }
    }
}

#[cfg(target_os = "macos")]
fn register_runtime(start: bool) -> OperationOutcome {
    use crate::service_management::{ServiceKind, ServiceManagement, ServiceManagementAdapter};

    if start {
        register_service(ServiceKind::RuntimeAgent)
    } else {
        match ServiceManagement::new().unregister(ServiceKind::RuntimeAgent) {
            crate::service_management::UnregisterOutcome::Unregistered
            | crate::service_management::UnregisterOutcome::AlreadyNotRegistered => {
                tracing::info!("runtime background service unregistered");
                OperationOutcome::Succeeded
            }
            crate::service_management::UnregisterOutcome::Error(message) => {
                tracing::warn!(message = %message, "runtime background unregister failed");
                OperationOutcome::Failed
            }
        }
    }
}

fn initialize_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("siderostat_monitor=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod version_notification_tests {
    use super::*;

    #[test]
    fn matched_state_has_no_notification() {
        assert!(!should_notify_version(
            None,
            VersionHandshake::Matched,
            true
        ));
        assert!(
            version_notification(VersionHandshake::Matched, "0.3.0", "1", "0.3.0", "1").is_none()
        );
    }

    #[test]
    fn each_mismatch_state_notifies_only_once_until_recovery() {
        assert!(should_notify_version(
            None,
            VersionHandshake::RuntimeOlder,
            true
        ));
        assert!(!should_notify_version(
            Some(VersionHandshake::RuntimeOlder),
            VersionHandshake::RuntimeOlder,
            true
        ));
        assert!(!should_notify_version(
            Some(VersionHandshake::RuntimeOlder),
            VersionHandshake::Matched,
            true
        ));
        assert!(should_notify_version(
            None,
            VersionHandshake::RuntimeNewer,
            true
        ));
    }

    #[test]
    fn initial_unavailable_state_is_silent_but_later_unavailable_state_notifies() {
        assert!(!should_notify_version(
            None,
            VersionHandshake::Unavailable,
            false
        ));
        assert!(should_notify_version(
            None,
            VersionHandshake::Unavailable,
            true
        ));
        assert!(!should_notify_version(
            Some(VersionHandshake::Unavailable),
            VersionHandshake::Unavailable,
            true
        ));
    }

    #[test]
    fn intentional_background_stop_suppresses_unavailable_version_notification() {
        let operation = Arc::new(Mutex::new(OperationState::default()));
        let mut context = VersionOperationContext::default();
        assert!(begin_operation(&operation, OperationKind::BackgroundStop));
        refresh_version_operation_context(&mut context, &operation);
        assert!(suppress_version_unavailable(&operation, &context));
        finish_operation(
            &operation,
            OperationKind::BackgroundStop,
            OperationOutcome::Succeeded,
        );
        refresh_version_operation_context(&mut context, &operation);
        assert!(suppress_version_unavailable(&operation, &context));
    }

    #[test]
    fn failed_background_stop_does_not_suppress_unavailable_version_notification() {
        let operation = Arc::new(Mutex::new(OperationState::default()));
        let mut context = VersionOperationContext::default();
        assert!(begin_operation(&operation, OperationKind::BackgroundStop));
        finish_operation(
            &operation,
            OperationKind::BackgroundStop,
            OperationOutcome::Failed,
        );
        refresh_version_operation_context(&mut context, &operation);
        assert!(!suppress_version_unavailable(&operation, &context));
    }

    #[test]
    fn start_and_restart_suppress_unavailable_until_a_matching_health_response() {
        for kind in [
            OperationKind::BackgroundStart,
            OperationKind::RuntimeRestart,
        ] {
            let operation = Arc::new(Mutex::new(OperationState::default()));
            let mut context = VersionOperationContext::default();
            assert!(begin_operation(&operation, kind));
            refresh_version_operation_context(&mut context, &operation);
            assert!(suppress_version_unavailable(&operation, &context));
            finish_operation(&operation, kind, OperationOutcome::Succeeded);
            refresh_version_operation_context(&mut context, &operation);
            assert!(suppress_version_unavailable(&operation, &context));
            context.runtime_matched = true;
            context.awaiting_startup_match = false;
            assert!(!suppress_version_unavailable(&operation, &context));
        }
    }

    #[test]
    fn mismatch_notification_includes_app_and_runtime_metadata() {
        let notification =
            version_notification(VersionHandshake::RuntimeOlder, "0.3.0", "12", "0.2.1", "7")
                .expect("runtime older should produce a notification");
        assert!(notification.body.contains("0.3.0"));
        assert!(notification.body.contains("0.2.1"));
        assert!(notification.body.contains("12"));
        assert!(notification.body.contains("7"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_launch_registration_requires_both_independent_services() {
        assert_eq!(
            combine_registration_outcomes(OperationOutcome::Succeeded, OperationOutcome::Succeeded),
            OperationOutcome::Succeeded
        );
        assert_eq!(
            combine_registration_outcomes(
                OperationOutcome::Succeeded,
                OperationOutcome::RequiresApproval
            ),
            OperationOutcome::RequiresApproval
        );
        assert_eq!(
            combine_registration_outcomes(
                OperationOutcome::RequiresApproval,
                OperationOutcome::Succeeded
            ),
            OperationOutcome::RequiresApproval
        );
        assert_eq!(
            combine_registration_outcomes(OperationOutcome::Succeeded, OperationOutcome::Failed),
            OperationOutcome::Failed
        );
        assert_eq!(
            combine_registration_outcomes(OperationOutcome::Denied, OperationOutcome::Succeeded),
            OperationOutcome::Denied
        );
    }
}
