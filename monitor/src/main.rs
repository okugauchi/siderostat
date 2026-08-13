//! siderostat-monitor: macOS menu bar monitor for siderostat.
//!
//! Polls the siderostat `/metrics` endpoint and renders the state through a
//! tray-icon menu bar item. Polling runs on a separate thread; the AppKit
//! event loop runs on the main thread (tray-icon macOS requirement) and a
//! CFRunLoop timer refreshes the tray from the shared state.

mod client;
mod config;
mod metrics;
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicPtr, Ordering},
    },
    thread,
};
use tray_icon::menu::MenuEvent;

/// Raw pointer to the shared NSApplication, so the Send+Sync menu event handler
/// can terminate the app without capturing the non-Sync object.
static APP_PTR: AtomicPtr<NSApplication> = AtomicPtr::new(core::ptr::null_mut());

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
    // main thread stays free for the AppKit menu bar event loop. The restart
    // menu handler shares a clone of the client with the polling task.
    let restart_client = Arc::new(client.clone());
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

    let tray = MonitorTray::new()?;
    {
        let display = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tray.update(&display);
    }

    // Quit from the menu terminates the app. The Send+Sync event-handler
    // closure cannot capture `Retained<NSApplication>` (not Send), so reach it
    // through an AtomicPtr global; the app lives for the process lifetime.
    // Restart from the menu posts to the authenticated admin API on a
    // dedicated thread so the AppKit main loop is never blocked.
    APP_PTR.store(
        (&*app as *const NSApplication).cast_mut(),
        Ordering::Relaxed,
    );
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if MonitorTray::is_quit_event(&event) {
            tracing::info!("quit requested from menu");
            let ptr = APP_PTR.load(Ordering::Relaxed);
            // SAFETY: APP_PTR is set before the handler can fire and the app
            // object outlives the process (main blocks in `app.run()`).
            unsafe { (*ptr).terminate(None) };
        } else if MonitorTray::is_restart_event(&event) {
            tracing::info!("restart requested from menu");
            let client = restart_client.clone();
            let _ = thread::Builder::new()
                .name("siderostat-monitor-restart".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(async move {
                        match client.request_restart().await {
                            Ok(job) => {
                                let job_id = job
                                    .get("job_id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("?");
                                tracing::info!(
                                    job_id = job_id,
                                    "siderostat runtime restart accepted"
                                );
                            }
                            Err(error) => tracing::warn!(
                                error = %error,
                                "siderostat runtime restart failed"
                            ),
                        }
                    });
                    Ok::<(), anyhow::Error>(())
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

fn initialize_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("siderostat_monitor=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
