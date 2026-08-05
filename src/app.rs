use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    config::{ModeAwareConfig, ModelVariant, Residency},
    metrics::Metrics,
    proxy::{ModeAwareProxyOptions, ModeAwareProxyState, mode_aware_proxy_handler},
    target::{ProxyTarget, UnavailableReason},
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode},
    routing::{any, get},
};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
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
}

impl AppState {
    pub fn from_config(config: ModeAwareConfig) -> anyhow::Result<Arc<Self>> {
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
            metrics: Arc::new(Metrics::default()),
        }))
    }
}

pub async fn serve(config: ModeAwareConfig) -> anyhow::Result<()> {
    let state = AppState::from_config(config)?;
    let public_addr = state.config.public_listen;
    let admin_addr = state.config.admin_listen;
    let public = public_router(state.clone());
    let admin = admin_router(state);
    let public_listener = tokio::net::TcpListener::bind(public_addr).await?;
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    info!(addr = %public_addr, "public listener started");
    info!(addr = %admin_addr, "admin listener started");

    tokio::try_join!(
        axum::serve(
            public_listener,
            public.into_make_service_with_connect_info::<SocketAddr>()
        ),
        axum::serve(admin_listener, admin),
    )?;
    Ok(())
}

pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", any(mode_aware_proxy_handler))
        .route("/{*path}", any(mode_aware_proxy_handler))
        .with_state(state.proxy.clone())
}

pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/cluster", get(cluster))
        .route("/metrics", get(metrics))
        .with_state(state)
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
    Json(json!({
        "node_id": state.config.node_id,
        "role": "unknown",
        "mode": if solo { "solo-standalone" } else { "unknown" },
        "state": if solo { "solo-standalone-ready" } else { "booting" },
        "generation": 0,
        "target": target_name(target.target),
        "target_ready": target.ready,
        "admission": admission_json(admission),
        "peer_ingress_ready": false,
        "interface": state.config.interface,
        "active_standalone_profile": {
            "profile_id": state.config.standalone_profile_id,
            "model_variant": model_variant_name(state.config.standalone_model_variant),
            "residency": residency_name(state.config.standalone_residency),
        },
        "child": Value::Null,
    }))
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response<Body> {
    let body = state.metrics.render_mode_aware(
        &state.config.node_id,
        state.proxy.target_snapshot(),
        state.proxy.admission().snapshot(),
    );
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

fn model_variant_name(value: ModelVariant) -> &'static str {
    match value {
        ModelVariant::Q2 => "q2",
        ModelVariant::Q2Q4 => "q2-q4",
        ModelVariant::Mxfp4 => "mxfp4",
    }
}

fn residency_name(value: Residency) -> &'static str {
    match value {
        Residency::Resident => "resident",
        Residency::SsdStreaming => "ssd-streaming",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

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
        Arc::new(AppState {
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
        })
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
}
