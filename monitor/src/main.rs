//! siderostat-monitor: macOS menu bar monitor for siderostat.
//!
//! Polls the siderostat `/metrics` endpoint and renders the state through a
//! tray-icon menu bar item. The tray icon is created and updated on the main
//! thread (macOS AppKit requirement); polling runs on a separate thread.

mod client;
mod config;
mod metrics;
mod state;
mod tray;

use crate::{client::MetricsClient, config::MonitorConfig, state::DisplayState, tray::MonitorTray};
use anyhow::Result;
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tray_icon::menu::MenuEvent;

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
    // main thread stays free for AppKit menu bar event processing.
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

    // Build the tray on the main thread and keep it alive for the process
    // lifetime. The returned guard is intentionally held until exit.
    let tray = MonitorTray::new()?;

    // Main-thread event loop: refresh the menu from shared state and process
    // menu events (quit).
    loop {
        {
            let display = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tray.update(&display);
        }
        if let Ok(event) = MenuEvent::receiver().try_recv()
            && MonitorTray::is_quit_event(&event)
        {
            tracing::info!("quit requested from menu");
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }

    Ok(())
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
