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
use crate::probe::probe_loop;
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
        .with_env_filter(EnvFilter::from_default_env())
        .with_env_filter(EnvFilter::new("ds4_smart_proxy=info"))
        .init();

    let args = Args::parse();

    // Load configuration
    let config_content = tokio::fs::read_to_string(&args.config).await?;
    let config: Config = toml::from_str(&config_content)?;
    info!(
        listen = %config.listen,
        self_name = %config.self_name,
        backends = ?config.backends.iter().map(|b| &b.name).collect::<Vec<_>>(),
        "config loaded"
    );

    // Build application state
    let state = Arc::new(AppState::from_config(&config));

    // Spawn health probe background task
    let probe_state = state.clone();
    tokio::spawn(async move {
        probe_loop(probe_state).await;
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
        }));
    }
    Json(list)
}

async fn metrics_handler() -> Response<Body> {
    // Minimal placeholder – future extension
    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("# ds4-smart-proxy metrics\n"))
        .unwrap()
}
