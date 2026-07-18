mod config;
mod error;
mod probe;
mod proxy;
mod state;

use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode},
    routing::{any, get},
};
use clap::Parser;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::probe::heartbeat_loop;
use crate::proxy::proxy_handler;
use crate::state::AppState;

#[derive(Parser)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ds4_smart_proxy=info")),
        )
        .init();

    let args = Args::parse();

    // Load configuration
    let config_content = tokio::fs::read_to_string(&args.config).await?;
    let config: Config = toml::from_str(&config_content)?;
    info!(
        listen = %config.listen,
        self_name = %config.self_name,
        backends = ?config.backends.iter().map(|b| &b.name).collect::<Vec<_>>(),
        heartbeat_interval_ms = config.heartbeat_interval.as_millis(),
        heartbeat_timeout_ms = config.heartbeat_timeout.as_millis(),
        active_probe_timeout_ms = config.active_probe_timeout.as_millis(),
        backend_connect_timeout_ms = 10_000u64,
        response_headers_timeout = "none",
        first_body_byte_timeout = "none",
        stream_idle_timeout = "none",
        "config loaded"
    );

    // Build application state
    let state = Arc::new(AppState::from_config(&config));

    // Spawn lightweight heartbeat background task
    let probe_state = state.clone();
    tokio::spawn(async move {
        heartbeat_loop(probe_state).await;
    });

    // Build axum router
    let app = Router::new()
        .route("/healthz", get(health_handler))
        .route("/backends", get(backends_handler))
        .route("/metrics", get(metrics_handler))
        .route("/{*path}", any(proxy_handler))
        .with_state(state);

    // Start server
    let addr: std::net::SocketAddr = config.listen.parse()?;
    info!(addr = %addr, "starting server");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Health endpoints ─────────────────────────────────────────────

async fn health_handler() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("ok"))
        .unwrap()
}

async fn backends_handler(State(state): State<Arc<AppState>>) -> Json<Vec<JsonValue>> {
    let states = state.states.lock().unwrap();
    let mut list: Vec<JsonValue> = Vec::new();
    for (name, bs) in states.iter() {
        list.push(serde_json::json!({
            "name": name,
            "healthy": bs.healthy,
            "busy": bs.is_busy(),
            "in_flight": bs.in_flight,
            "latency_ms": bs.average_latency_ms,
            "last_heartbeat": format_system_time(bs.last_heartbeat),
            "last_failure": format_system_time(bs.last_failure),
        }));
    }
    Json(list)
}

fn format_system_time(value: Option<std::time::SystemTime>) -> Option<String> {
    value.and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| format_unix_timestamp(duration.as_secs()))
    })
}

fn format_unix_timestamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };

    (year as i32, month as u32, day as u32)
}

async fn metrics_handler() -> Response<Body> {
    // Minimal placeholder – future extension
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("# ds4-smart-proxy metrics\n"))
        .unwrap()
}
