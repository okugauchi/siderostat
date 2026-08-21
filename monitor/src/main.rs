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
mod metrics;
mod migration;
mod service_management;
mod settings;
mod state;
mod tray;

use crate::{client::MetricsClient, config::MonitorConfig, state::DisplayState, tray::MonitorTray};
use anyhow::Result;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
    thread,
};
use tray_icon::menu::MenuEvent;

/// Main-thread refresh context for the CFRunLoop timer. The tray pointer stays
/// valid because the tray is created on the main thread and lives until the
/// process exits (`app.run()` blocks on the same thread).
struct UpdateContext {
    shared: Arc<Mutex<DisplayState>>,
    tray: *const MonitorTray,
}

fn main() -> Result<()> {
    initialize_logging();
    let (config, config_path) = MonitorConfig::load()?;
    if let Some(path) = &config_path {
        tracing::info!(config_path = %path.display(), "monitor configuration loaded");
    } else {
        tracing::info!("monitor configuration loaded with defaults");
    }

    let client = MetricsClient::new(&config)?;
    let shared = Arc::new(Mutex::new(DisplayState::default()));

    // Polling runs on a separate thread with its own Tokio runtime so the
    // main thread stays free for the AppKit menu bar event loop.
    // The menu-bar graceful-restart path needs its own handle to the client.
    let menu_client = Arc::new(client.clone());
    let poll_client = client;
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

    // LaunchAgent operations run on dedicated threads so the AppKit main loop is
    // never blocked. In bundle mode (C-05a) the runtime and monitor are managed
    // through Service Management and the graceful-restart admin endpoint, never
    // through launchctl.
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
            tracing::info!("runtime restart requested from menu");
            let client = menu_client.clone();
            let _ = thread::Builder::new()
                .name("siderostat-runtime-restart".into())
                .spawn(move || {
                    if launchd::is_bundle_mode() {
                        // bundle mode: graceful restart via the authenticated
                        // /admin/restart endpoint (C-04). launchctl kickstart
                        // is not used (C-05a).
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        runtime.block_on(async {
                            match client.graceful_restart().await {
                                Ok((status, body)) => tracing::info!(
                                    status = %status,
                                    response = %body,
                                    "graceful restart requested"
                                ),
                                Err(error) => tracing::warn!(
                                    error = %error,
                                    "graceful restart request failed"
                                ),
                            }
                            Ok::<(), anyhow::Error>(())
                        })?;
                        Ok::<(), anyhow::Error>(())
                    } else {
                        if let Err(error) = launchd::kickstart(launchd::RUNTIME_LABEL) {
                            tracing::warn!(error = %error, "Runtime restart failed");
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                });
        } else if MonitorTray::is_bg_start_event(&event) {
            tracing::info!("background start requested from menu");
            register_runtime(true);
        } else if MonitorTray::is_bg_stop_event(&event) {
            tracing::info!("background stop requested from menu");
            register_runtime(false);
        } else if MonitorTray::is_open_config_event(&event) {
            tracing::info!("open configuration requested from menu");
            let _ = thread::Builder::new()
                .name("siderostat-open-config".into())
                .spawn(move || {
                    if let Err(error) = settings::open_runtime_config() {
                        tracing::warn!(error = %error, "Open configuration failed");
                    }
                });
        } else if MonitorTray::is_open_login_items_event(&event) {
            tracing::info!("open login items requested from menu");
            let _ = thread::Builder::new()
                .name("siderostat-open-login-items".into())
                .spawn(move || {
                    if let Err(error) = settings::open_login_items() {
                        tracing::warn!(error = %error, "Open Login Items failed");
                    }
                });
        }
    }));

    // Periodic tray refresh on the main run loop via a CFRunLoop timer.
    let update = Box::new(UpdateContext {
        shared: shared.clone(),
        tray: &tray as *const MonitorTray,
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
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: `context.tray` points to the tray created on the main thread,
    // alive until the process exits.
    let tray = unsafe { &*context.tray };
    tray.update(&display);
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
                tracing::warn!(error = %error, "metrics poll failed; monitor offline");
                let mut guard = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.mark_offline();
                true
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

/// Register (`start = true`) or unregister (`start = false`) the runtime
/// background service through Service Management (C-02/C-05a). This runs on
/// the main thread (AppKit requirement) so it must be called from the menu
/// event handler directly, not from a background thread.
#[cfg(target_os = "macos")]
fn register_runtime(start: bool) {
    use crate::service_management::{ServiceKind, ServiceManagement, ServiceManagementAdapter};
    let adapter = ServiceManagement::new();
    let outcome = if start {
        adapter.register(ServiceKind::RuntimeAgent)
    } else {
        match adapter.unregister(ServiceKind::RuntimeAgent) {
            crate::service_management::UnregisterOutcome::Unregistered
            | crate::service_management::UnregisterOutcome::AlreadyNotRegistered => {
                tracing::info!("runtime background service unregistered");
                return;
            }
            crate::service_management::UnregisterOutcome::Error(message) => {
                tracing::warn!(message = %message, "runtime background unregister failed");
                return;
            }
        }
    };
    match outcome {
        crate::service_management::RegisterOutcome::Registered => {
            tracing::info!("runtime background service registered");
        }
        crate::service_management::RegisterOutcome::RequiresApproval => {
            tracing::warn!("runtime background service requires approval in System Settings");
        }
        crate::service_management::RegisterOutcome::DeniedByUser => {
            tracing::warn!("runtime background service registration denied by user");
        }
        crate::service_management::RegisterOutcome::Error(message) => {
            tracing::warn!(message = %message, "runtime background register failed");
        }
    }
}

fn initialize_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("siderostat_monitor=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
