mod affinity;
mod app;
mod backend;
mod config;
mod error;
mod heartbeat;
mod metrics;
mod persistence;
mod proxy;
mod routing;

use crate::{
    app::AppState,
    config::{Config, LogFormat},
    heartbeat::spawn_heartbeat_tasks,
    proxy::proxy_handler,
};
use anyhow::Result;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{Response, StatusCode},
    routing::{any, delete, get},
};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(long, short)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (config, config_path) = Config::load(args.config.as_deref()).await?;
    initialize_logging(&config);
    info!(
        config_path = %config_path.display(),
        listen = %config.listen,
        admin_listen = %config.admin_listen,
        backends = ?config.backends.iter().map(|backend| &backend.id).collect::<Vec<_>>(),
        "configuration loaded"
    );

    let public_addr = config.listen;
    let admin_addr = config.admin_listen;
    let state = AppState::from_config(config)?;
    spawn_heartbeat_tasks(
        state.registry.clone(),
        state.config.heartbeat.clone(),
        state.config.cooldown.clone(),
        state.metrics.clone(),
    );
    spawn_affinity_cleanup(state.clone());

    let public = Router::new()
        .route("/{*path}", any(proxy_handler))
        .with_state(state.clone());
    let admin = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/backends", get(backends))
        .route("/metrics", get(metrics))
        .route("/affinity", get(affinity_summary))
        .route("/affinity/{key_hash}", delete(delete_affinity))
        .with_state(state);

    let public_listener = tokio::net::TcpListener::bind(public_addr).await?;
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    info!(addr = %public_addr, "public listener started");
    info!(addr = %admin_addr, "admin listener started");

    tokio::try_join!(
        axum::serve(
            public_listener,
            public.into_make_service_with_connect_info::<std::net::SocketAddr>()
        ),
        axum::serve(admin_listener, admin),
    )?;
    Ok(())
}

fn initialize_logging(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("ds4_smart_proxy={}", config.logging.level)));
    match config.logging.format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_target(false)
            .with_env_filter(filter)
            .init(),
        LogFormat::Text => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_target(false)
            .with_env_filter(filter)
            .init(),
    }
}

fn spawn_affinity_cleanup(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            state.affinity.cleanup();
        }
    });
}

async fn health() -> &'static str {
    "OK"
}

async fn ready(State(state): State<Arc<AppState>>) -> Response<Body> {
    let available = state
        .registry
        .all()
        .iter()
        .any(|backend| backend.is_available());
    let ready = available && state.affinity.persistence_healthy();
    Response::builder()
        .status(if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        })
        .body(Body::from(if ready { "READY" } else { "NOT READY" }))
        .expect("valid readiness response")
}

async fn backends(State(state): State<Arc<AppState>>) -> Json<Vec<Value>> {
    Json(
        state
            .registry
            .all()
            .iter()
            .map(|backend| {
                let snapshot = backend.snapshot();
                json!({
                    "id": backend.config.id,
                    "kind": format!("{:?}", backend.config.kind).to_ascii_lowercase(),
                    "local": backend.config.local,
                    "health": snapshot.health.as_str(),
                    "in_flight": backend.in_flight(),
                    "max_in_flight": backend.config.max_in_flight,
                    "tags": backend.config.tags,
                    "ewma_latency_ms": snapshot.ewma_latency_ms,
                    "last_heartbeat_at": iso_time(snapshot.last_heartbeat_at),
                    "last_success_at": iso_time(snapshot.last_success_at),
                    "last_failure_at": iso_time(snapshot.last_failure_at),
                    "cooldown_until": iso_time(snapshot.cooldown_until.map(instant_to_system_time)),
                    "last_failure_kind": snapshot.last_failure_kind.map(|kind| kind.as_str()),
                })
            })
            .collect(),
    )
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response<Body> {
    let body = state.metrics.render(&state.registry, &state.affinity);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4")
        .body(Body::from(body))
        .expect("valid metrics response")
}

async fn affinity_summary(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "entries": state.affinity.len(),
        "by_backend": state.affinity.counts_by_backend(),
        "persistence_healthy": state.affinity.persistence_healthy(),
    }))
}

async fn delete_affinity(
    State(state): State<Arc<AppState>>,
    Path(key_hash): Path<String>,
) -> StatusCode {
    let removed = if key_hash.len() == 12 {
        state.affinity.remove_by_tag(&key_hash)
    } else if let Some(hash) = parse_hash(&key_hash) {
        state.affinity.remove(hash)
    } else {
        return StatusCode::BAD_REQUEST;
    };
    if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

fn parse_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(output)
}

fn instant_to_system_time(value: Instant) -> SystemTime {
    SystemTime::now()
        + value
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO)
}

fn iso_time(value: Option<SystemTime>) -> Option<String> {
    value.and_then(|value| {
        let duration = value.duration_since(UNIX_EPOCH).ok()?;
        let seconds = duration.as_secs();
        let days = (seconds / 86_400) as i64;
        let seconds_of_day = seconds % 86_400;
        let (year, month, day) = civil_from_days(days);
        Some(format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            seconds_of_day / 3_600,
            (seconds_of_day % 3_600) / 60,
            seconds_of_day % 60
        ))
    })
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
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}
