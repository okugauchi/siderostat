use crate::{
    admission::{AdmissionGate, AdmissionPermit},
    affinity::{AffinityKey, AffinityStore},
    backend::{BackendRegistry, BackendRuntime},
    config::Config,
    error::{FailureKind, ProxyError, format_error_chain},
    heartbeat::try_active_probe,
    metrics::Metrics,
    routing::{Router as TargetRouter, Selection},
    target::ProxyTarget,
};
use axum::{
    body::{Body, to_bytes},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use std::{
    collections::HashSet,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{error, info, warn};
use uuid::Uuid;

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

/// P1-07で削除する旧load-balancer request path用の互換state。
pub struct LegacyAppState {
    pub config: Arc<Config>,
    pub registry: Arc<BackendRegistry>,
    pub affinity: Arc<AffinityStore>,
    pub router: Arc<TargetRouter>,
    pub metrics: Arc<Metrics>,
}

impl LegacyAppState {
    pub fn from_config(config: Config) -> anyhow::Result<Arc<Self>> {
        let registry = Arc::new(BackendRegistry::from_config(&config)?);
        let affinity = Arc::new(AffinityStore::new(&config.affinity)?);
        let router = Arc::new(TargetRouter::new(
            registry.clone(),
            affinity.clone(),
            config.routing.clone(),
        ));
        Ok(Arc::new(Self {
            config: Arc::new(config),
            registry,
            affinity,
            router,
            metrics: Arc::new(Metrics::default()),
        }))
    }
}

struct ForwardInput<'a> {
    method: &'a Method,
    uri: &'a Uri,
    headers: &'a HeaderMap,
    body: &'a Bytes,
    request_id: &'a str,
}

struct PreparedResponse {
    status: StatusCode,
    headers: HeaderMap,
    stream: UpstreamStream,
    first_chunk: Option<Bytes>,
}

struct StreamingRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
    peer: SocketAddr,
    request_id: String,
    inference: bool,
    started: Instant,
}

struct StreamLifecycle {
    request_id: String,
    backend_id: String,
    started: Instant,
    completed: AtomicBool,
    bytes: AtomicU64,
    metrics: Arc<crate::metrics::Metrics>,
    backend: Arc<BackendRuntime>,
    failure_threshold: u32,
    cooldown: std::time::Duration,
    status: u16,
}

impl Drop for StreamLifecycle {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        self.metrics.observe_duration(elapsed.as_secs_f64());
        if !self.completed.load(Ordering::Relaxed) {
            warn!(
                request_id = %self.request_id,
                backend_id = %self.backend_id,
                total_ms = elapsed.as_millis(),
                bytes_out = self.bytes.load(Ordering::Relaxed),
                error_kind = FailureKind::ClientCancelled.as_str(),
                "request stream dropped"
            );
        }
    }
}

pub async fn proxy_handler(
    State(state): State<Arc<LegacyAppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let request_id = request_id(&parts.headers);
    let started = Instant::now();
    let inference = is_inference_request(&parts.method, &parts.uri, &state);
    let declared_length = parts
        .headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > state.config.request_body_limit_bytes) {
        return ProxyError::BodyTooLarge.response(&request_id);
    }

    if declared_length.is_some_and(|length| length > state.config.max_replayable_body_bytes) {
        return handle_streaming_request(
            state,
            StreamingRequest {
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body,
                peer,
                request_id,
                inference,
                started,
            },
        )
        .await;
    }

    let body = match to_bytes(body, state.config.request_body_limit_bytes).await {
        Ok(body) => body,
        Err(error) => {
            let proxy_error = if error
                .to_string()
                .to_ascii_lowercase()
                .contains("length limit")
            {
                ProxyError::BodyTooLarge
            } else {
                ProxyError::InvalidBody
            };
            return proxy_error.response(&request_id);
        }
    };
    let replayable = body.len() <= state.config.max_replayable_body_bytes;
    let affinity = match state.affinity.extract(&parts.headers, &body) {
        Ok(affinity) => affinity,
        Err(error) => return error.response(&request_id),
    };
    let upstream_headers =
        match build_upstream_headers(&parts.headers, peer, &parts.uri, &request_id) {
            Ok(headers) => headers,
            Err(error) => return error.response(&request_id),
        };
    let input = ForwardInput {
        method: &parts.method,
        uri: &parts.uri,
        headers: &upstream_headers,
        body: &body,
        request_id: &request_id,
    };

    info!(
        request_id = %request_id,
        method = %parts.method,
        path = %parts.uri.path(),
        affinity_source = affinity.as_ref().map(|key| key.source.as_str()),
        affinity_key_tag = affinity.as_ref().map(|key| key.tag.as_str()),
        "request received"
    );

    match execute(
        &state,
        &input,
        affinity.as_ref(),
        inference,
        replayable,
        started,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .metrics
                .observe_duration(started.elapsed().as_secs_f64());
            warn!(
                request_id = %request_id,
                error_kind = error.code(),
                error = %error,
                "request failed"
            );
            error.response(&request_id)
        }
    }
}

async fn handle_streaming_request(
    state: Arc<LegacyAppState>,
    request: StreamingRequest,
) -> Response<Body> {
    let affinity = match state.affinity.extract(&request.headers, &[]) {
        Ok(affinity) => affinity,
        Err(error) => return error.response(&request.request_id),
    };
    let upstream_headers = match build_upstream_headers(
        &request.headers,
        request.peer,
        &request.uri,
        &request.request_id,
    ) {
        Ok(headers) => headers,
        Err(error) => return error.response(&request.request_id),
    };
    let empty = Bytes::new();
    let input = ForwardInput {
        method: &request.method,
        uri: &request.uri,
        headers: &upstream_headers,
        body: &empty,
        request_id: &request.request_id,
    };
    let excluded = HashSet::new();
    let selection = match state
        .router
        .select(affinity.as_ref(), &excluded, request.inference, false)
        .await
    {
        Ok(selection) => selection,
        Err(error) if request.inference => {
            let restored = try_active_probe(
                &state.registry,
                &state.config.active_probe,
                &state.config.cooldown,
                &state.metrics,
            )
            .await;
            if restored {
                match state
                    .router
                    .select(affinity.as_ref(), &excluded, request.inference, false)
                    .await
                {
                    Ok(selection) => selection,
                    Err(error) => {
                        return error.response(&request.request_id);
                    }
                }
            } else {
                match state.router.select_recovery(affinity.as_ref(), &excluded) {
                    Ok(selection) => selection,
                    Err(_) => return error.response(&request.request_id),
                }
            }
        }
        Err(error) => return error.response(&request.request_id),
    };
    info!(
        request_id = %request.request_id,
        backend_id = %selection.backend.config.id,
        routing_reason = selection.reason.as_str(),
        replayable = false,
        "streaming request body selected backend"
    );
    match forward_streaming_body(
        &state,
        &input,
        selection,
        affinity.as_ref(),
        request.inference,
        request.started,
        request.body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            state
                .metrics
                .observe_duration(request.started.elapsed().as_secs_f64());
            error.response(&request.request_id)
        }
    }
}

async fn execute(
    state: &Arc<LegacyAppState>,
    input: &ForwardInput<'_>,
    affinity: Option<&AffinityKey>,
    inference: bool,
    replayable: bool,
    started: Instant,
) -> Result<Response<Body>, ProxyError> {
    let mut excluded = HashSet::new();
    let mut retry_from: Option<(String, &'static str)> = None;
    let max_attempts = if replayable {
        state.config.routing.max_attempts
    } else {
        1
    };

    for attempt in 1..=max_attempts {
        let selection = match state
            .router
            .select(affinity, &excluded, inference, attempt > 1)
            .await
        {
            Ok(selection) => selection,
            Err(error) if attempt == 1 && inference => {
                let restored = try_active_probe(
                    &state.registry,
                    &state.config.active_probe,
                    &state.config.cooldown,
                    &state.metrics,
                )
                .await;
                if restored {
                    state
                        .router
                        .select(affinity, &excluded, inference, false)
                        .await?
                } else {
                    state
                        .router
                        .select_recovery(affinity, &excluded)
                        .map_err(|_| error)?
                }
            }
            Err(error) => return Err(error),
        };

        let backend_id = selection.backend.config.id.clone();
        if attempt == 1
            && let Some(key) = affinity
        {
            state.metrics.increment(
                "ds4_proxy_affinity_lookup_total",
                &[
                    ("source", key.source.as_str()),
                    (
                        "result",
                        if selection.affinity_hit {
                            "hit"
                        } else {
                            "miss"
                        },
                    ),
                ],
            );
        }
        if let Some((from_backend, reason)) = retry_from.take() {
            state.metrics.increment(
                "ds4_proxy_retries_total",
                &[
                    ("from_backend", &from_backend),
                    ("to_backend", &backend_id),
                    ("reason", reason),
                ],
            );
        }
        let in_flight_before = selection.backend.in_flight().saturating_sub(1);
        info!(
            request_id = %input.request_id,
            backend_id = %backend_id,
            backend_kind = ?selection.backend.config.kind,
            routing_reason = selection.reason.as_str(),
            affinity_hit = selection.affinity_hit,
            in_flight_before,
            bytes_in = input.body.len(),
            attempt,
            "backend selected"
        );

        match forward_once(state, input, selection, affinity, inference, started).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                let retryable = can_retry(&error, attempt, max_attempts);
                excluded.insert(backend_id.clone());
                if let Some(key) = affinity {
                    state.affinity.mark_failure(key);
                }
                if retryable {
                    retry_from = Some((backend_id.clone(), error.code()));
                    warn!(
                        request_id = %input.request_id,
                        backend_id = %backend_id,
                        attempt,
                        error_kind = error.code(),
                        "retrying on a different backend"
                    );
                    continue;
                }
                return Err(error);
            }
        }
    }
    Err(ProxyError::NoBackendAvailable)
}

async fn forward_once(
    state: &Arc<LegacyAppState>,
    input: &ForwardInput<'_>,
    selection: Selection,
    affinity: Option<&AffinityKey>,
    inference: bool,
    started: Instant,
) -> Result<Response<Body>, ProxyError> {
    let backend = selection.backend.clone();
    let url = upstream_url(&backend, input.uri)?;
    let mut builder = backend
        .client
        .request(convert_method(input.method), url)
        .headers(input.headers.clone());
    for (name, value) in &backend.config.static_headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| ProxyError::Internal)?;
        let value = HeaderValue::from_str(value).map_err(|_| ProxyError::Internal)?;
        builder = builder.header(name, value);
    }
    if *input.method != Method::GET && *input.method != Method::HEAD {
        builder = builder.body(input.body.clone());
    }

    let response_header_timeout = phase_timeout(
        state.config.timeouts.response_headers,
        state.config.timeouts.total,
        started,
    );
    let upstream = match tokio::time::timeout(response_header_timeout, builder.send()).await {
        Err(_) => {
            record_failure(state, &backend, FailureKind::ResponseHeaderTimeout);
            return Err(ProxyError::ResponseHeaderTimeout);
        }
        Ok(Err(error)) => {
            let kind = classify_reqwest_error(&error);
            record_failure(state, &backend, kind);
            warn!(
                request_id = %input.request_id,
                backend_id = %backend.config.id,
                error = %format_error_chain(&error),
                error_kind = kind.as_str(),
                "upstream request failed"
            );
            return Err(if kind == FailureKind::Protocol {
                ProxyError::Protocol
            } else {
                ProxyError::Connect
            });
        }
        Ok(Ok(response)) => response,
    };

    let status = upstream.status();
    info!(
        request_id = %input.request_id,
        backend_id = %backend.config.id,
        response_header_ms = started.elapsed().as_millis(),
        status = status.as_u16(),
        "upstream response headers received"
    );
    let has_alternative = state
        .registry
        .all()
        .iter()
        .any(|candidate| candidate.config.id != backend.config.id && candidate.is_available());
    if retryable_status(status) && has_alternative {
        record_failure(state, &backend, FailureKind::Http5xx);
        return Err(ProxyError::RetryableUpstreamStatus);
    }

    let prepared = prepare_first_chunk(state, input, &backend, upstream, started).await?;
    let ttfb = started.elapsed();
    state.metrics.observe_ttfb(ttfb.as_secs_f64());
    info!(
        request_id = %input.request_id,
        backend_id = %backend.config.id,
        first_body_byte_ms = ttfb.as_millis(),
        status = status.as_u16(),
        "upstream first body byte ready"
    );

    if status.is_server_error() {
        record_failure(state, &backend, FailureKind::Http5xx);
    } else {
        if inference && (status.is_success() || status.is_redirection()) {
            backend.record_success(ttfb);
        }
        if let Some(key) = affinity {
            state.affinity.assign(key, &backend.config.id);
        }
    }

    state.metrics.increment(
        "ds4_proxy_requests_total",
        &[
            ("backend", &backend.config.id),
            ("status_class", status_class(status)),
        ],
    );
    Ok(build_response(
        state,
        input.request_id,
        backend,
        prepared,
        selection.permit,
        started,
    ))
}

async fn forward_streaming_body(
    state: &Arc<LegacyAppState>,
    input: &ForwardInput<'_>,
    selection: Selection,
    affinity: Option<&AffinityKey>,
    inference: bool,
    started: Instant,
    body: Body,
) -> Result<Response<Body>, ProxyError> {
    let backend = selection.backend.clone();
    let url = upstream_url(&backend, input.uri)?;
    let mut builder = backend
        .client
        .request(convert_method(input.method), url)
        .headers(input.headers.clone());
    for (name, value) in &backend.config.static_headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| ProxyError::Internal)?;
        let value = HeaderValue::from_str(value).map_err(|_| ProxyError::Internal)?;
        builder = builder.header(name, value);
    }
    builder = builder.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let response_header_timeout = phase_timeout(
        state.config.timeouts.response_headers,
        state.config.timeouts.total,
        started,
    );
    let upstream = match tokio::time::timeout(response_header_timeout, builder.send()).await {
        Err(_) => {
            record_failure(state, &backend, FailureKind::ResponseHeaderTimeout);
            return Err(ProxyError::ResponseHeaderTimeout);
        }
        Ok(Err(error)) => {
            let kind = classify_reqwest_error(&error);
            record_failure(state, &backend, kind);
            return Err(if kind == FailureKind::Protocol {
                ProxyError::Protocol
            } else {
                ProxyError::Connect
            });
        }
        Ok(Ok(response)) => response,
    };
    let status = upstream.status();
    let prepared = prepare_first_chunk(state, input, &backend, upstream, started).await?;
    let ttfb = started.elapsed();
    state.metrics.observe_ttfb(ttfb.as_secs_f64());
    if status.is_server_error() {
        record_failure(state, &backend, FailureKind::Http5xx);
    } else {
        if inference && (status.is_success() || status.is_redirection()) {
            backend.record_success(ttfb);
        }
        if let Some(key) = affinity {
            state.affinity.assign(key, &backend.config.id);
        }
    }
    state.metrics.increment(
        "ds4_proxy_requests_total",
        &[
            ("backend", &backend.config.id),
            ("status_class", status_class(status)),
        ],
    );
    Ok(build_response(
        state,
        input.request_id,
        backend,
        prepared,
        selection.permit,
        started,
    ))
}

async fn prepare_first_chunk(
    state: &LegacyAppState,
    input: &ForwardInput<'_>,
    backend: &BackendRuntime,
    upstream: reqwest::Response,
    started: Instant,
) -> Result<PreparedResponse, ProxyError> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let no_body = *input.method == Method::HEAD
        || status == StatusCode::NO_CONTENT
        || upstream.content_length() == Some(0);
    let mut stream: UpstreamStream = Box::pin(upstream.bytes_stream());
    if no_body {
        return Ok(PreparedResponse {
            status,
            headers,
            stream,
            first_chunk: None,
        });
    }

    let first_byte_timeout = phase_timeout(
        state.config.timeouts.first_body_byte,
        state.config.timeouts.total,
        started,
    );
    match tokio::time::timeout(first_byte_timeout, stream.next()).await {
        Ok(Some(Ok(first_chunk))) => Ok(PreparedResponse {
            status,
            headers,
            stream,
            first_chunk: Some(first_chunk),
        }),
        Ok(None) => Ok(PreparedResponse {
            status,
            headers,
            stream,
            first_chunk: None,
        }),
        Ok(Some(Err(error))) => {
            record_failure(state, backend, FailureKind::Protocol);
            warn!(
                request_id = %input.request_id,
                backend_id = %backend.config.id,
                error = %error,
                "upstream failed before first body byte"
            );
            Err(ProxyError::Protocol)
        }
        Err(_) => {
            record_failure(state, backend, FailureKind::FirstByteTimeout);
            Err(ProxyError::FirstByteTimeout)
        }
    }
}

fn phase_timeout(
    phase: std::time::Duration,
    total: Option<std::time::Duration>,
    started: Instant,
) -> std::time::Duration {
    total
        .map(|total| total.saturating_sub(started.elapsed()).min(phase))
        .unwrap_or(phase)
}

fn build_response(
    state: &Arc<LegacyAppState>,
    request_id: &str,
    backend: Arc<BackendRuntime>,
    prepared: PreparedResponse,
    permit: OwnedSemaphorePermit,
    started: Instant,
) -> Response<Body> {
    let status = prepared.status;
    let headers = prepared.headers;
    let upstream_stream = prepared.stream;
    let lifecycle = Arc::new(StreamLifecycle {
        request_id: request_id.to_string(),
        backend_id: backend.config.id.clone(),
        started,
        completed: AtomicBool::new(false),
        bytes: AtomicU64::new(0),
        metrics: state.metrics.clone(),
        backend: backend.clone(),
        failure_threshold: state.config.cooldown.consecutive_failure_threshold,
        cooldown: state.config.cooldown.duration,
        status: status.as_u16(),
    });
    let idle_timeout = state.config.timeouts.stream_idle;
    let total_timeout = state.config.timeouts.total;
    let stream = stream::unfold(
        (
            prepared.first_chunk,
            upstream_stream,
            Some(permit),
            lifecycle,
        ),
        move |(first, mut upstream, permit, lifecycle)| async move {
            if let Some(chunk) = first {
                lifecycle
                    .bytes
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                return Some((Ok(chunk), (None, upstream, permit, lifecycle)));
            }
            let remaining_total =
                total_timeout.map(|total| total.saturating_sub(lifecycle.started.elapsed()));
            let total_limited = remaining_total.is_some_and(|remaining| remaining <= idle_timeout);
            let wait_timeout =
                remaining_total.map_or(idle_timeout, |remaining| remaining.min(idle_timeout));
            match tokio::time::timeout(wait_timeout, upstream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    lifecycle
                        .bytes
                        .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    Some((Ok(chunk), (None, upstream, permit, lifecycle)))
                }
                Ok(Some(Err(error))) => {
                    lifecycle.backend.record_failure(
                        FailureKind::Protocol,
                        lifecycle.failure_threshold,
                        lifecycle.cooldown,
                    );
                    lifecycle.metrics.increment(
                        "ds4_proxy_stream_errors_total",
                        &[
                            ("backend", &lifecycle.backend_id),
                            ("reason", "upstream_error"),
                        ],
                    );
                    error!(
                        request_id = %lifecycle.request_id,
                        backend_id = %lifecycle.backend_id,
                        error = %error,
                        "upstream stream error"
                    );
                    lifecycle.completed.store(true, Ordering::Relaxed);
                    upstream = Box::pin(stream::empty());
                    Some((
                        Err(io::Error::other("upstream stream error")),
                        (None, upstream, None, lifecycle),
                    ))
                }
                Err(_) => {
                    lifecycle.backend.record_failure(
                        FailureKind::StreamIdleTimeout,
                        lifecycle.failure_threshold,
                        lifecycle.cooldown,
                    );
                    let timeout_reason = if total_limited {
                        "total_timeout"
                    } else {
                        "stream_idle_timeout"
                    };
                    lifecycle.metrics.increment(
                        "ds4_proxy_stream_errors_total",
                        &[
                            ("backend", &lifecycle.backend_id),
                            ("reason", timeout_reason),
                        ],
                    );
                    warn!(
                        request_id = %lifecycle.request_id,
                        backend_id = %lifecycle.backend_id,
                        error_kind = FailureKind::StreamIdleTimeout.as_str(),
                        "upstream stream idle timeout"
                    );
                    lifecycle.completed.store(true, Ordering::Relaxed);
                    upstream = Box::pin(stream::empty());
                    Some((
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "upstream stream idle timeout",
                        )),
                        (None, upstream, None, lifecycle),
                    ))
                }
                Ok(None) => {
                    lifecycle.completed.store(true, Ordering::Relaxed);
                    info!(
                        request_id = %lifecycle.request_id,
                        backend_id = %lifecycle.backend_id,
                        total_ms = lifecycle.started.elapsed().as_millis(),
                        bytes_out = lifecycle.bytes.load(Ordering::Relaxed),
                        status = lifecycle.status,
                        "request finished"
                    );
                    None
                }
            }
        },
    );

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    copy_response_headers(&headers, response.headers_mut());
    insert_request_id(response.headers_mut(), request_id);
    response
}

fn build_upstream_headers(
    incoming: &HeaderMap,
    peer: SocketAddr,
    uri: &Uri,
    request_id: &str,
) -> Result<HeaderMap, ProxyError> {
    let mut output = HeaderMap::new();
    let connection_tokens = connection_header_tokens(incoming);
    for (name, value) in incoming {
        if !is_hop_by_hop(name.as_str(), &connection_tokens) && name != "host" {
            output.append(name.clone(), value.clone());
        }
    }

    append_forwarded(&mut output, "x-forwarded-for", &peer.ip().to_string())?;
    if let Some(host) = incoming.get("host") {
        output.insert("x-forwarded-host", host.clone());
    }
    if !output.contains_key("x-forwarded-proto") {
        let protocol = uri.scheme_str().unwrap_or("http");
        output.insert(
            "x-forwarded-proto",
            HeaderValue::from_str(protocol).map_err(|_| ProxyError::Internal)?,
        );
    }
    append_forwarded(&mut output, "via", "1.1 ds4-smart-proxy")?;
    insert_request_id(&mut output, request_id);
    Ok(output)
}

fn copy_response_headers(source: &HeaderMap, destination: &mut HeaderMap) {
    let connection_tokens = connection_header_tokens(source);
    for (name, value) in source {
        if !is_hop_by_hop(name.as_str(), &connection_tokens) {
            destination.append(name.clone(), value.clone());
        }
    }
}

fn connection_header_tokens(headers: &HeaderMap) -> HashSet<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .collect()
}

fn is_hop_by_hop(name: &str, connection_tokens: &HashSet<String>) -> bool {
    connection_tokens.contains(name)
        || matches!(
            name,
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

fn append_forwarded(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ProxyError> {
    let combined = headers
        .get(name)
        .and_then(|existing| existing.to_str().ok())
        .map_or_else(
            || value.to_string(),
            |existing| format!("{existing}, {value}"),
        );
    headers.insert(
        name,
        HeaderValue::from_str(&combined).map_err(|_| ProxyError::Internal)?,
    );
    Ok(())
}

fn upstream_url(backend: &BackendRuntime, uri: &Uri) -> Result<url::Url, ProxyError> {
    let path = uri.path().trim_start_matches('/');
    let mut url = backend.endpoint(path).map_err(|_| ProxyError::Internal)?;
    url.set_query(uri.query());
    Ok(url)
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("req_{}", Uuid::new_v4().simple()))
}

fn insert_request_id(headers: &mut HeaderMap, request_id: &str) {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert("x-request-id", value);
    }
}

fn is_inference_request(method: &Method, uri: &Uri, state: &LegacyAppState) -> bool {
    if *method == Method::HEAD || (*method == Method::GET && uri.path() == "/v1/models") {
        return false;
    }
    !state
        .registry
        .all()
        .iter()
        .any(|backend| uri.path() == backend.config.heartbeat_path)
}

fn convert_method(method: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

fn classify_reqwest_error(error: &reqwest::Error) -> FailureKind {
    if error.is_connect() {
        let message = error.to_string().to_ascii_lowercase();
        if message.contains("tls") || message.contains("certificate") {
            FailureKind::Tls
        } else {
            FailureKind::Connect
        }
    } else if error.is_timeout() {
        FailureKind::ResponseHeaderTimeout
    } else {
        FailureKind::Protocol
    }
}

fn record_failure(state: &LegacyAppState, backend: &BackendRuntime, kind: FailureKind) {
    let changed = backend.record_failure(
        kind,
        state.config.cooldown.consecutive_failure_threshold,
        state.config.cooldown.duration,
    );
    if changed && backend.snapshot().health.as_str() == "cooldown" {
        state.metrics.increment(
            "ds4_proxy_cooldown_total",
            &[("backend", &backend.config.id), ("reason", kind.as_str())],
        );
    }
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

fn can_retry(error: &ProxyError, attempt: usize, max_attempts: usize) -> bool {
    attempt < max_attempts
        && matches!(
            error,
            ProxyError::Connect | ProxyError::RetryableUpstreamStatus
        )
}

fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyIngress {
    Public,
    Peer,
}

#[derive(Debug, Clone, Copy)]
pub struct ProxyRequestContext {
    pub ingress: ProxyIngress,
    pub peer: SocketAddr,
    pub hop: u8,
}

#[derive(Debug, Clone, Copy)]
struct FixedTargetState {
    target: ProxyTarget,
    ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeAwareTargetSnapshot {
    pub target: ProxyTarget,
    pub ready: bool,
}

pub struct ModeAwareProxyState {
    client: reqwest::Client,
    local_upstream: url::Url,
    coordinator_upstream: url::Url,
    target: RwLock<FixedTargetState>,
    admission: AdmissionGate,
    request_body_limit_bytes: usize,
    response_header_timeout: std::time::Duration,
    first_body_byte_timeout: std::time::Duration,
    stream_idle_timeout: std::time::Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct ModeAwareProxyOptions {
    pub max_in_flight: usize,
    pub request_body_limit_bytes: usize,
    pub response_header_timeout: std::time::Duration,
    pub first_body_byte_timeout: std::time::Duration,
    pub stream_idle_timeout: std::time::Duration,
    pub connect_timeout: std::time::Duration,
}

impl ModeAwareProxyState {
    pub fn new(
        local_upstream: url::Url,
        coordinator_upstream: url::Url,
        options: ModeAwareProxyOptions,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(options.connect_timeout)
                .build()?,
            local_upstream,
            coordinator_upstream,
            target: RwLock::new(FixedTargetState {
                target: ProxyTarget::Unavailable {
                    reason: crate::target::UnavailableReason::Transition,
                },
                ready: false,
            }),
            admission: AdmissionGate::new(options.max_in_flight),
            request_body_limit_bytes: options.request_body_limit_bytes,
            response_header_timeout: options.response_header_timeout,
            first_body_byte_timeout: options.first_body_byte_timeout,
            stream_idle_timeout: options.stream_idle_timeout,
        })
    }

    pub fn set_target(&self, target: ProxyTarget, ready: bool) {
        *self
            .target
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = FixedTargetState { target, ready };
    }

    pub fn admission(&self) -> &AdmissionGate {
        &self.admission
    }

    pub fn target_snapshot(&self) -> ModeAwareTargetSnapshot {
        let fixed = self.fixed_target();
        ModeAwareTargetSnapshot {
            target: fixed.target,
            ready: fixed.ready,
        }
    }

    fn fixed_target(&self) -> FixedTargetState {
        *self
            .target
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub async fn mode_aware_proxy_handler(
    State(state): State<Arc<ModeAwareProxyState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    forward_mode_aware(
        state,
        ProxyRequestContext {
            ingress: ProxyIngress::Public,
            peer,
            hop: 0,
        },
        request,
    )
    .await
}

pub async fn forward_mode_aware(
    state: Arc<ModeAwareProxyState>,
    context: ProxyRequestContext,
    request: Request<Body>,
) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let request_id = request_id(&parts.headers);
    let declared_length = parts
        .headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > state.request_body_limit_bytes) {
        return ProxyError::BodyTooLarge.response(&request_id);
    }

    let fixed = state.fixed_target();
    let base = match fixed.target {
        ProxyTarget::LocalStandalone => &state.local_upstream,
        ProxyTarget::Coordinator => &state.coordinator_upstream,
        ProxyTarget::Unavailable { .. } => {
            return ProxyError::NoBackendAvailable.response(&request_id);
        }
    };
    let permit = match state.admission.try_acquire(fixed.ready) {
        Ok(permit) => permit,
        Err(_) => return ProxyError::NoBackendAvailable.response(&request_id),
    };
    let url = match fixed_upstream_url(base, &parts.uri) {
        Ok(url) => url,
        Err(error) => return error.response(&request_id),
    };
    let headers = match build_mode_aware_headers(&parts.headers, &parts.uri, &request_id, context) {
        Ok(headers) => headers,
        Err(error) => return error.response(&request_id),
    };
    let method = convert_method(&parts.method);
    let body_limit = state.request_body_limit_bytes;
    let request_stream = stream::unfold(
        (body.into_data_stream(), 0_usize, false),
        move |(mut body, total, exceeded)| async move {
            if exceeded {
                return None;
            }
            match body.next().await {
                Some(Ok(chunk)) => {
                    let new_total = total.saturating_add(chunk.len());
                    if new_total > body_limit {
                        Some((
                            Err(io::Error::other("request body limit exceeded")),
                            (body, new_total, true),
                        ))
                    } else {
                        Some((Ok(chunk), (body, new_total, false)))
                    }
                }
                Some(Err(error)) => Some((
                    Err(io::Error::other(error.to_string())),
                    (body, total, true),
                )),
                None => None,
            }
        },
    );
    let builder = state
        .client
        .request(method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(request_stream));
    let upstream = match tokio::time::timeout(state.response_header_timeout, builder.send()).await {
        Err(_) => return ProxyError::ResponseHeaderTimeout.response(&request_id),
        Ok(Err(error)) => {
            let chain = format_error_chain(&error);
            if chain.contains("request body limit exceeded") {
                return ProxyError::BodyTooLarge.response(&request_id);
            }
            let proxy_error = if error.is_connect() {
                ProxyError::Connect
            } else {
                ProxyError::Protocol
            };
            return proxy_error.response(&request_id);
        }
        Ok(Ok(response)) => response,
    };

    match prepare_mode_aware_response(&state, upstream).await {
        Ok(prepared) => build_mode_aware_response(&state, &request_id, prepared, permit),
        Err(error) => error.response(&request_id),
    }
}

fn fixed_upstream_url(base: &url::Url, uri: &Uri) -> Result<url::Url, ProxyError> {
    let mut url = base.clone();
    let base_path = base.path().trim_end_matches('/');
    let path = if base_path.is_empty() {
        uri.path().to_string()
    } else {
        format!("{base_path}{}", uri.path())
    };
    url.set_path(&path);
    url.set_query(uri.query());
    Ok(url)
}

fn build_mode_aware_headers(
    incoming: &HeaderMap,
    uri: &Uri,
    request_id: &str,
    context: ProxyRequestContext,
) -> Result<HeaderMap, ProxyError> {
    let mut headers = build_upstream_headers(incoming, context.peer, uri, request_id)?;
    headers.remove("x-ds4-peer-proxy-token");
    headers.remove("x-ds4-proxy-hop");
    let cluster_headers = headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-ds4-cluster-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in cluster_headers {
        headers.remove(name);
    }
    // P3のpeer ingress認証でsource/tokenと合わせて検証するため、hopをcontextに保持する。
    let _peer_hop_validation_hook = (context.ingress == ProxyIngress::Peer).then_some(context.hop);
    Ok(headers)
}

struct ModeAwarePreparedResponse {
    status: StatusCode,
    headers: HeaderMap,
    stream: UpstreamStream,
    first_chunk: Option<Bytes>,
}

async fn prepare_mode_aware_response(
    state: &ModeAwareProxyState,
    upstream: reqwest::Response,
) -> Result<ModeAwarePreparedResponse, ProxyError> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let no_body = status == StatusCode::NO_CONTENT || upstream.content_length() == Some(0);
    let mut stream: UpstreamStream = Box::pin(upstream.bytes_stream());
    if no_body {
        return Ok(ModeAwarePreparedResponse {
            status,
            headers,
            stream,
            first_chunk: None,
        });
    }
    match tokio::time::timeout(state.first_body_byte_timeout, stream.next()).await {
        Ok(Some(Ok(first_chunk))) => Ok(ModeAwarePreparedResponse {
            status,
            headers,
            stream,
            first_chunk: Some(first_chunk),
        }),
        Ok(None) => Ok(ModeAwarePreparedResponse {
            status,
            headers,
            stream,
            first_chunk: None,
        }),
        Ok(Some(Err(_))) => Err(ProxyError::Protocol),
        Err(_) => Err(ProxyError::FirstByteTimeout),
    }
}

fn build_mode_aware_response(
    state: &ModeAwareProxyState,
    request_id: &str,
    prepared: ModeAwarePreparedResponse,
    permit: AdmissionPermit,
) -> Response<Body> {
    let status = prepared.status;
    let headers = prepared.headers;
    let idle_timeout = state.stream_idle_timeout;
    let stream = stream::unfold(
        (prepared.first_chunk, prepared.stream, Some(permit)),
        move |(first, mut upstream, permit)| async move {
            if let Some(chunk) = first {
                return Some((Ok(chunk), (None, upstream, permit)));
            }
            match tokio::time::timeout(idle_timeout, upstream.next()).await {
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), (None, upstream, permit))),
                Ok(Some(Err(_))) => {
                    upstream = Box::pin(stream::empty());
                    Some((
                        Err(io::Error::other("upstream stream error")),
                        (None, upstream, None),
                    ))
                }
                Err(_) => {
                    upstream = Box::pin(stream::empty());
                    Some((
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "upstream stream idle timeout",
                        )),
                        (None, upstream, None),
                    ))
                }
                Ok(None) => None,
            }
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    copy_response_headers(&headers, response.headers_mut());
    insert_request_id(response.headers_mut(), request_id);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::{Json, Router, routing::any};
    use futures::stream;
    use serde_json::Value;
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::Duration,
    };
    use tokio::task::JoinHandle;

    #[test]
    fn removes_standard_and_connection_declared_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("x-remove"));
        headers.insert("x-remove", HeaderValue::from_static("secret"));
        headers.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        headers.insert("x-keep", HeaderValue::from_static("yes"));
        let tokens = connection_header_tokens(&headers);
        let mut output = HeaderMap::new();
        copy_response_headers(&headers, &mut output);
        assert!(is_hop_by_hop("x-remove", &tokens));
        assert!(output.get("x-remove").is_none());
        assert_eq!(output.get("x-keep").unwrap(), "yes");
    }

    #[test]
    fn only_gateway_statuses_are_retryable() {
        assert!(retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
    }

    struct TestServer {
        address: SocketAddr,
        task: JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start_server(app: Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestServer { address, task }
    }

    fn test_config(backends: &str) -> Config {
        toml::from_str(&format!(
            r#"
listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"

[routing]
max_attempts = 2

[affinity]
enabled = false

[timeouts]
connect = "200ms"
response_headers = "2s"
first_body_byte = "2s"
stream_idle = "2s"

{backends}
"#
        ))
        .unwrap()
    }

    async fn start_proxy(config: Config) -> (TestServer, Arc<LegacyAppState>) {
        let state = LegacyAppState::from_config(config).unwrap();
        for backend in state.registry.all() {
            backend.record_heartbeat_success();
        }
        let app = Router::new()
            .route("/", any(proxy_handler))
            .route("/{*path}", any(proxy_handler))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        (TestServer { address, task }, state)
    }

    fn mode_aware_state(address: SocketAddr, body_limit: usize) -> Arc<ModeAwareProxyState> {
        let upstream = url::Url::parse(&format!("http://{address}")).unwrap();
        let state = Arc::new(
            ModeAwareProxyState::new(
                upstream.clone(),
                upstream,
                ModeAwareProxyOptions {
                    max_in_flight: 8,
                    request_body_limit_bytes: body_limit,
                    response_header_timeout: Duration::from_secs(2),
                    first_body_byte_timeout: Duration::from_secs(2),
                    stream_idle_timeout: Duration::from_secs(2),
                    connect_timeout: Duration::from_millis(200),
                },
            )
            .unwrap(),
        );
        state.set_target(ProxyTarget::LocalStandalone, true);
        state.admission().start_serving();
        state
    }

    async fn start_mode_aware_proxy(state: Arc<ModeAwareProxyState>) -> TestServer {
        let app = Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        TestServer { address, task }
    }

    #[tokio::test]
    async fn mode_aware_proxy_forwards_unknown_path_query_and_headers() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                Json(serde_json::json!({
                    "uri": request.uri().to_string(),
                    "request_id": request
                        .headers()
                        .get("x-request-id")
                        .and_then(|value| value.to_str().ok()),
                    "forwarded_for": request
                        .headers()
                        .get("x-forwarded-for")
                        .and_then(|value| value.to_str().ok()),
                    "custom": request
                        .headers()
                        .get("x-custom")
                        .and_then(|value| value.to_str().ok()),
                    "internal_headers_removed": request
                        .headers()
                        .get("x-ds4-peer-proxy-token")
                        .is_none()
                        && request.headers().get("x-ds4-proxy-hop").is_none()
                        && request
                            .headers()
                            .get("x-ds4-cluster-generation")
                            .is_none(),
                }))
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state).await;

        let response: Value = reqwest::Client::new()
            .get(format!("http://{}/v1/custom?answer=42", proxy.address))
            .header("x-request-id", "req_mode_aware")
            .header("x-custom", "preserved")
            .header("x-ds4-peer-proxy-token", "untrusted")
            .header("x-ds4-proxy-hop", "99")
            .header("x-ds4-cluster-generation", "99")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(response["uri"], "/v1/custom?answer=42");
        assert_eq!(response["request_id"], "req_mode_aware");
        assert_eq!(response["forwarded_for"], "127.0.0.1");
        assert_eq!(response["custom"], "preserved");
        assert_eq!(response["internal_headers_removed"], true);
    }

    #[tokio::test]
    async fn mode_aware_proxy_streams_sse_without_whole_response_buffering() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    match index {
                        0 => Some((
                            Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n")),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(120)).await;
                            Some((
                                Ok::<_, Infallible>(Bytes::from_static(b"data: second\n\n")),
                                2,
                            ))
                        }
                        _ => None,
                    }
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state.clone()).await;
        let started = Instant::now();

        let response = reqwest::get(format!("http://{}/v1/stream", proxy.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        let first_elapsed = started.elapsed();
        let second = body.next().await.unwrap().unwrap();

        assert_eq!(first, Bytes::from_static(b"data: first\n\n"));
        assert_eq!(second, Bytes::from_static(b"data: second\n\n"));
        assert!(
            first_elapsed < Duration::from_millis(100),
            "first SSE chunk was delayed for {first_elapsed:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(body);
        for _ in 0..20 {
            if state.admission().snapshot().in_flight == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.admission().snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn mode_aware_proxy_returns_503_during_transition() {
        let hits = Arc::new(AtomicUsize::new(0));
        let backend_hits = hits.clone();
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(move || {
                let hits = backend_hits.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::Relaxed);
                    "unexpected"
                }
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        state.set_target(
            ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            false,
        );
        let proxy = start_mode_aware_proxy(state).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "5");
        assert_eq!(hits.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn mode_aware_proxy_maps_connect_failure_to_502_without_retry() {
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable = unused.local_addr().unwrap();
        drop(unused);
        let state = mode_aware_state(unavailable, 4096);
        let proxy = start_mode_aware_proxy(state).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn mode_aware_proxy_does_not_retry_gateway_status() {
        let hits = Arc::new(AtomicUsize::new(0));
        let backend_hits = hits.clone();
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(move || {
                let hits = backend_hits.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::Relaxed);
                    (StatusCode::SERVICE_UNAVAILABLE, "upstream unavailable")
                }
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.text().await.unwrap(), "upstream unavailable");
        assert_eq!(hits.load(AtomicOrdering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mode_aware_proxy_rejects_declared_body_over_limit() {
        let hits = Arc::new(AtomicUsize::new(0));
        let backend_hits = hits.clone();
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(move || {
                let hits = backend_hits.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::Relaxed);
                    "unexpected"
                }
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4);
        let proxy = start_mode_aware_proxy(state).await;

        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/upload", proxy.address))
            .body(vec![0_u8; 5])
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(hits.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn mode_aware_proxy_enforces_body_limit_while_streaming() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                match to_bytes(request.into_body(), 4096).await {
                    Ok(_) => StatusCode::OK,
                    Err(_) => StatusCode::BAD_REQUEST,
                }
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4);
        let proxy = start_mode_aware_proxy(state).await;
        let chunks = stream::iter([
            Ok::<_, io::Error>(Bytes::from_static(b"abc")),
            Ok::<_, io::Error>(Bytes::from_static(b"def")),
        ]);

        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/upload", proxy.address))
            .body(reqwest::Body::wrap_stream(chunks))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn mode_aware_proxy_releases_permit_on_client_cancellation() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0_u64, |index| async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((
                        Ok::<_, Infallible>(Bytes::from(format!("{index}\n"))),
                        index + 1,
                    ))
                });
                Response::new(Body::from_stream(chunks))
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state.clone()).await;
        let response = reqwest::get(format!("http://{}/v1/stream", proxy.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        let _ = body.next().await.unwrap().unwrap();
        drop(body);

        for _ in 0..50 {
            if state.admission().snapshot().in_flight == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.admission().snapshot().in_flight, 0);
    }

    #[test]
    fn peer_context_removes_internal_hop_headers_before_forwarding() {
        let mut incoming = HeaderMap::new();
        incoming.insert("x-ds4-peer-proxy-token", HeaderValue::from_static("secret"));
        incoming.insert("x-ds4-proxy-hop", HeaderValue::from_static("1"));
        incoming.insert("x-ds4-cluster-generation", HeaderValue::from_static("7"));
        incoming.insert("x-keep", HeaderValue::from_static("yes"));
        let headers = build_mode_aware_headers(
            &incoming,
            &Uri::from_static("/v1/test"),
            "req_peer",
            ProxyRequestContext {
                ingress: ProxyIngress::Peer,
                peer: "127.0.0.1:1234".parse().unwrap(),
                hop: 1,
            },
        )
        .unwrap();

        assert!(headers.get("x-ds4-peer-proxy-token").is_none());
        assert!(headers.get("x-ds4-proxy-hop").is_none());
        assert!(headers.get("x-ds4-cluster-generation").is_none());
        assert_eq!(headers.get("x-keep").unwrap(), "yes");
    }

    #[tokio::test]
    async fn forwards_unknown_path_query_and_forwarding_headers() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                let request_id = request
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                let forwarded_for = request
                    .headers()
                    .get("x-forwarded-for")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                Json(serde_json::json!({
                    "uri": request.uri().to_string(),
                    "request_id": request_id,
                    "forwarded_for": forwarded_for,
                }))
            }),
        ))
        .await;
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
"#,
            backend.address
        ));
        let (proxy, _state) = start_proxy(config).await;

        let response: Value = reqwest::Client::new()
            .get(format!("http://{}/v1/custom?answer=42", proxy.address))
            .header("x-request-id", "req_test")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["uri"], "/v1/custom?answer=42");
        assert_eq!(response["request_id"], "req_test");
        assert_eq!(response["forwarded_for"], "127.0.0.1");
    }

    #[tokio::test]
    async fn retries_connect_failure_on_different_backend() {
        let remote =
            start_server(Router::new().route("/{*path}", any(|| async { "remote-ok" }))).await;
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = unused.local_addr().unwrap();
        drop(unused);
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{unavailable_address}"
local = true
priority = 100

[[backends]]
id = "remote"
url = "http://{}"
priority = 50
"#,
            remote.address
        ));
        let (proxy, state) = start_proxy(config).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "remote-ok");
        assert_eq!(
            state.registry.by_id("local").unwrap().snapshot().health,
            crate::backend::BackendHealth::Suspect
        );
    }

    #[tokio::test]
    async fn does_not_retry_http_4xx() {
        let local = start_server(Router::new().route(
            "/{*path}",
            any(|| async { (StatusCode::BAD_REQUEST, "local-bad-request") }),
        ))
        .await;
        let remote_hits = Arc::new(AtomicUsize::new(0));
        let hits = remote_hits.clone();
        let remote = start_server(Router::new().route(
            "/{*path}",
            any(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::Relaxed);
                    "remote"
                }
            }),
        ))
        .await;
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
priority = 100

[[backends]]
id = "remote"
url = "http://{}"
priority = 50
"#,
            local.address, remote.address
        ));
        let (proxy, _state) = start_proxy(config).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.text().await.unwrap(), "local-bad-request");
        assert_eq!(remote_hits.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn streams_chunks_without_buffering_whole_response() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    if index == 3 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let chunk = Bytes::from(format!("data: {index}\n\n"));
                    Some((Ok::<_, Infallible>(chunk), index + 1))
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        ))
        .await;
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
"#,
            backend.address
        ));
        let (proxy, state) = start_proxy(config).await;
        let started = Instant::now();
        let response = reqwest::get(format!("http://{}/v1/stream", proxy.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        let first_elapsed = started.elapsed();
        let mut output = first.to_vec();
        while let Some(chunk) = body.next().await {
            output.extend_from_slice(&chunk.unwrap());
        }
        assert!(
            first_elapsed < Duration::from_millis(200),
            "first chunk was delayed for {first_elapsed:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(220));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "data: 0\n\ndata: 1\n\ndata: 2\n\n"
        );
        assert_eq!(state.registry.by_id("local").unwrap().in_flight(), 0);
    }

    #[tokio::test]
    async fn first_byte_timeout_is_not_retried() {
        let local = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::once(async {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok::<_, Infallible>(Bytes::from_static(b"late"))
                });
                Response::new(Body::from_stream(chunks))
            }),
        ))
        .await;
        let remote_hits = Arc::new(AtomicUsize::new(0));
        let hits = remote_hits.clone();
        let remote = start_server(Router::new().route(
            "/{*path}",
            any(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, AtomicOrdering::Relaxed);
                    "remote"
                }
            }),
        ))
        .await;
        let mut config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
priority = 100

[[backends]]
id = "remote"
url = "http://{}"
priority = 50
"#,
            local.address, remote.address
        ));
        config.timeouts.first_body_byte = Duration::from_millis(50);
        let (proxy, state) = start_proxy(config).await;

        let response = reqwest::get(format!("http://{}/v1/test", proxy.address))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(remote_hits.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(
            state.registry.by_id("local").unwrap().snapshot().health,
            crate::backend::BackendHealth::Cooldown
        );
    }

    #[tokio::test]
    async fn client_cancellation_releases_in_flight_permit() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((
                        Ok::<_, Infallible>(Bytes::from(format!("{index}\n"))),
                        index + 1,
                    ))
                });
                Response::new(Body::from_stream(chunks))
            }),
        ))
        .await;
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
"#,
            backend.address
        ));
        let (proxy, state) = start_proxy(config).await;
        let response = reqwest::get(format!("http://{}/v1/stream", proxy.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        let _ = body.next().await.unwrap().unwrap();
        drop(body);
        for _ in 0..20 {
            if state.registry.by_id("local").unwrap().in_flight() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state.registry.by_id("local").unwrap().in_flight(), 0);
    }

    #[tokio::test]
    async fn forwards_body_larger_than_replay_limit_without_retry_buffer() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                let body = to_bytes(request.into_body(), 4096).await.unwrap();
                body.len().to_string()
            }),
        ))
        .await;
        let mut config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
"#,
            backend.address
        ));
        config.max_replayable_body_bytes = 4;
        config.request_body_limit_bytes = 4096;
        let (proxy, _state) = start_proxy(config).await;
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/upload", proxy.address))
            .body(vec![7_u8; 1024])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "1024");
    }

    #[tokio::test]
    async fn suspect_backend_recovers_via_half_open_real_request() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async { Json(serde_json::json!({"ok": true})) }),
        ))
        .await;
        let config = test_config(&format!(
            r#"
[[backends]]
id = "local"
url = "http://{}"
local = true
"#,
            backend.address
        ));
        let (proxy, state) = start_proxy(config).await;
        let runtime = state.registry.by_id("local").unwrap();
        runtime.record_failure(FailureKind::Heartbeat, 1, Duration::from_secs(300));
        runtime.record_heartbeat_success();
        assert_eq!(
            runtime.snapshot().health,
            crate::backend::BackendHealth::Suspect
        );

        let models = reqwest::get(format!("http://{}/v1/models", proxy.address))
            .await
            .unwrap();
        assert_eq!(models.status(), StatusCode::OK);
        assert_eq!(
            runtime.snapshot().health,
            crate::backend::BackendHealth::Suspect,
            "non-inference success must not promote inference health"
        );

        let completion = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", proxy.address))
            .json(&serde_json::json!({
                "model": "test",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(completion.status(), StatusCode::OK);
        assert_eq!(
            runtime.snapshot().health,
            crate::backend::BackendHealth::Alive
        );
    }
}
