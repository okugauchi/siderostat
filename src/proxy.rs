use crate::error::format_error_chain;
use crate::probe::{active_probe_backend, mark_failure, mark_success};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, Response, StatusCode},
};
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, warn};
use uuid::Uuid;

/// RAII guard: increments in_flight on construction, decrements on drop.
pub struct InFlightGuard {
    backend_name: String,
    state: Arc<AppState>,
}

impl InFlightGuard {
    pub fn new(backend_name: String, state: Arc<AppState>) -> Self {
        if let Ok(mut states) = state.states.lock()
            && let Some(entry) = states.get_mut(&backend_name)
        {
            entry.in_flight = entry.in_flight.saturating_add(1);
        }
        Self {
            backend_name,
            state,
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut states) = self.state.states.lock()
            && let Some(entry) = states.get_mut(&self.backend_name)
        {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
    }
}

/// List backend candidates according to local-first routing policy.
fn candidate_backends(state: &AppState, exclude: Option<&str>) -> Vec<String> {
    let states = match state.states.lock() {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let is_candidate = |name: &str| {
        if exclude.is_some_and(|exclude| exclude == name) {
            return false;
        }
        states
            .get(name)
            .is_some_and(|s| !s.is_busy() && (s.healthy || heartbeat_after_failure(s)))
    };
    let mut candidates = Vec::new();

    for b in &state.backends {
        if b.name == state.self_name && is_candidate(&b.name) {
            candidates.push(b.name.clone());
        }
    }

    for b in &state.backends {
        if b.name != state.self_name && is_candidate(&b.name) {
            candidates.push(b.name.clone());
        }
    }

    candidates
}

fn heartbeat_after_failure(backend: &crate::state::BackendState) -> bool {
    match (backend.last_heartbeat, backend.last_failure) {
        (Some(last_heartbeat), Some(last_failure)) => last_heartbeat > last_failure,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Select the first candidate whose request-time active probe succeeds.
async fn select_backend(state: &Arc<AppState>, exclude: Option<&str>) -> Option<String> {
    for name in candidate_backends(state, exclude) {
        if let Some(url) = backend_url(state, &name)
            && active_probe_backend(state, &name, &url).await
        {
            return Some(name);
        }
    }
    None
}

/// Get backend URL by name.
fn backend_url(state: &AppState, name: &str) -> Option<String> {
    state
        .backends
        .iter()
        .find(|b| b.name == name)
        .map(|b| b.url.clone())
}

/// Convert axum Method to reqwest Method.
fn convert_method(m: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(m.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

/// Build upstream headers, skipping hop-by-hop headers.
fn build_upstream_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in incoming.iter() {
        let key = k.as_str();
        match key {
            "host" | "connection" | "transfer-encoding" | "upgrade" | "keep-alive" => {}
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Forward a single request to a named backend.
async fn forward_to_backend(
    client: &reqwest::Client,
    backend_url: &str,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let upstream_url = format!("{}{}", backend_url.trim_end_matches('/'), path);
    let req_method = convert_method(method);
    let mut builder = client
        .request(req_method, &upstream_url)
        .headers(headers.clone());

    if *method != Method::GET && *method != Method::HEAD {
        builder = builder.body(body.clone());
    }

    builder.send().await
}

/// Retry on an alternative backend when the primary attempt fails.
async fn retry_on_alt(
    state: &Arc<AppState>,
    exclude: &str,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(reqwest::Response, String, InFlightGuard), Box<dyn std::error::Error + Send + Sync>> {
    let alt = match select_backend(state, Some(exclude)).await {
        Some(n) => n,
        None => return Err("no alternative backend available".into()),
    };
    let alt_url = match backend_url(state, &alt) {
        Some(u) => u,
        None => return Err("alternative backend not found".into()),
    };

    let guard = InFlightGuard::new(alt.clone(), state.clone());
    let resp = forward_to_backend(&state.client, &alt_url, method, path, headers, body).await?;
    Ok((resp, alt, guard))
}

/// Build an axum Response from a reqwest Response.
async fn build_response(
    upstream: reqwest::Response,
    request_id: &str,
    backend: &str,
    start: &Instant,
    retry_count: u32,
    guard: InFlightGuard,
) -> Response<Body> {
    let status = upstream.status();
    let resp_headers = upstream.headers().clone();
    let is_streaming = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.contains("text/event-stream"));

    let latency = start.elapsed();
    if status.is_success() {
        mark_success(&guard.state, backend, latency.as_millis() as u64);
    } else if status.is_server_error() {
        mark_failure(&guard.state, backend);
    }
    info!(
        request_id = %request_id,
        backend = %backend,
        latency_ms = latency.as_millis(),
        status = status.as_u16(),
        retry = retry_count,
        "request completed"
    );

    let body = if is_streaming {
        let stream_state = guard.state.clone();
        let stream_backend = backend.to_string();
        let stream_request_id = request_id.to_string();
        let stream = upstream.bytes_stream().map(move |result| {
            let _keep_guard_alive = &guard;
            result.map_err(|error| {
                mark_failure(&stream_state, &stream_backend);
                warn!(
                    request_id = %stream_request_id,
                    backend = %stream_backend,
                    error = %error,
                    "upstream stream error"
                );
                std::io::Error::other("stream error")
            })
        });
        Body::from_stream(stream)
    } else {
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                error!(request_id = %request_id, error = %e, "failed to read upstream body");
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("upstream read error"))
                    .unwrap();
            }
        };
        drop(guard);
        Body::from(bytes)
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    for (k, v) in resp_headers.iter() {
        resp.headers_mut().insert(k, v.clone());
    }
    resp
}

/// Read an axum Body into Bytes by collecting data frames.
async fn body_to_bytes(body: Body) -> Result<Bytes, hyper::Error> {
    let data_stream = body.into_data_stream();
    let mut buf = Vec::new();
    let mut stream = data_stream;
    loop {
        let item = stream.next().await;
        match item {
            Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
            Some(Err(_)) => continue,
            None => break,
        }
    }
    Ok(Bytes::from(buf))
}

/// Main proxy handler – catch-all route for all incoming requests.
pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    let request_id = Uuid::new_v4().to_string();
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();

    // Read incoming body as bytes (may be empty for GET/HEAD)
    let body_bytes = match body_to_bytes(body).await {
        Ok(b) => b,
        Err(e) => {
            error!(request_id = %request_id, error = %e, "failed to read request body");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("failed to read request body"))
                .unwrap();
        }
    };

    // Select backend
    let backend_name = match select_backend(&state, None).await {
        Some(b) => b,
        None => {
            warn!(request_id = %request_id, "no available backends");
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("no available backends"))
                .unwrap();
        }
    };

    let backend_url = match backend_url(&state, &backend_name) {
        Some(u) => u,
        None => {
            error!(request_id = %request_id, backend = %backend_name, "backend url not found");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("backend configuration error"))
                .unwrap();
        }
    };

    let guard = InFlightGuard::new(backend_name.clone(), state.clone());

    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let upstream_headers = build_upstream_headers(&headers);
    let start = Instant::now();

    let result = forward_to_backend(
        &state.client,
        &backend_url,
        &method,
        path,
        &upstream_headers,
        &body_bytes,
    )
    .await;

    let upstream = match result {
        Ok(r) => r,
        Err(e) => {
            // Connection failure → retry on another healthy backend
            let error = format_error_chain(&e);
            mark_failure(&state, &backend_name);
            warn!(
                request_id = %request_id,
                backend = %backend_name,
                error = %error,
                "connection failed, retrying"
            );
            let (resp, alt, alt_guard) = match retry_on_alt(
                &state,
                &backend_name,
                &method,
                path,
                &upstream_headers,
                &body_bytes,
            )
            .await
            {
                Ok((r, alt_name, alt_guard)) => (r, alt_name, alt_guard),
                Err(e2) => {
                    error!(request_id = %request_id, error = %e2, "retry failed");
                    return Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .body(Body::from("upstream error"))
                        .unwrap();
                }
            };
            drop(guard);
            return build_response(resp, &request_id, &alt, &start, 1, alt_guard).await;
        }
    };

    let status = upstream.status();

    // 4xx: never retry
    if status.as_u16() >= 400 && status.as_u16() < 500 {
        return build_response(upstream, &request_id, &backend_name, &start, 0, guard).await;
    }

    // 5xx: retry once on another backend (response not yet sent to client)
    if status.as_u16() >= 500 {
        mark_failure(&state, &backend_name);
        warn!(
            request_id = %request_id,
            backend = %backend_name,
            status = status.as_u16(),
            "5xx, retrying on alt backend"
        );
        let (resp, alt, alt_guard) = match retry_on_alt(
            &state,
            &backend_name,
            &method,
            path,
            &upstream_headers,
            &body_bytes,
        )
        .await
        {
            Ok((r, alt_name, alt_guard)) => (r, alt_name, alt_guard),
            Err(e) => {
                error!(request_id = %request_id, error = %e, "retry failed");
                return Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::from("upstream error"))
                    .unwrap();
            }
        };
        drop(guard);
        return build_response(resp, &request_id, &alt, &start, 1, alt_guard).await;
    }

    build_response(upstream, &request_id, &backend_name, &start, 0, guard).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendConfig, Config};
    use std::time::Duration;

    fn test_state() -> AppState {
        AppState::from_config(&Config {
            listen: "127.0.0.1:18080".to_string(),
            self_name: "local".to_string(),
            tls_accept_invalid_certs: false,
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_timeout: Duration::from_secs(5),
            heartbeat_path: "/v1/models".to_string(),
            active_probe_timeout: Duration::from_secs(3),
            backends: vec![
                BackendConfig {
                    name: "local".to_string(),
                    url: "http://127.0.0.1:8000".to_string(),
                    max_in_flight: 1,
                },
                BackendConfig {
                    name: "remote".to_string(),
                    url: "http://127.0.0.1:8001".to_string(),
                    max_in_flight: 1,
                },
            ],
        })
    }

    #[test]
    fn candidate_backends_prefers_local_when_available() {
        let state = test_state();

        assert_eq!(candidate_backends(&state, None), vec!["local", "remote"]);
    }

    #[test]
    fn candidate_backends_uses_remote_when_local_is_busy() {
        let state = test_state();
        {
            let mut states = state.states.lock().unwrap();
            states.get_mut("local").unwrap().in_flight = 1;
        }

        assert_eq!(candidate_backends(&state, None), vec!["remote"]);
    }

    #[test]
    fn candidate_backends_returns_empty_when_all_backends_are_busy() {
        let state = test_state();
        {
            let mut states = state.states.lock().unwrap();
            states.get_mut("local").unwrap().in_flight = 1;
            states.get_mut("remote").unwrap().in_flight = 1;
        }

        assert!(candidate_backends(&state, None).is_empty());
    }

    #[test]
    fn candidate_backends_allows_recent_heartbeat_after_failure() {
        let state = test_state();
        {
            let mut states = state.states.lock().unwrap();
            let local = states.get_mut("local").unwrap();
            local.healthy = false;
            local.last_failure = Some(std::time::SystemTime::UNIX_EPOCH);
            local.last_heartbeat = Some(std::time::SystemTime::now());
        }

        assert_eq!(candidate_backends(&state, None), vec!["local", "remote"]);
    }
}
