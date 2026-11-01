use crate::{
    admission::{AdmissionGate, AdmissionPermit},
    canary::RECOVERY_CANARY_HEADER,
    error::{ProxyError, format_error_chain},
    metrics::{Metrics, RequestMetricGuard},
    target::ProxyTarget,
};
use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri},
};
use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, stream};
use std::{
    collections::HashSet,
    fmt, io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

type UpstreamStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;
type ResponseStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + 'static>>;

// Keep an incomplete event private: once sent, its partial JSON cannot be retracted
// before a later failure event. Limits apply to one event, not the transport chunk.
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct SseFrameBuffer {
    bytes: BytesMut,
    cursor: usize,
    line_start: usize,
    pending_lf: bool,
}

impl SseFrameBuffer {
    fn take_event(&mut self, eof: bool) -> Option<Bytes> {
        if self.pending_lf {
            if self.bytes.is_empty() && !eof {
                return None;
            }
            self.pending_lf = false;
            if self.bytes.first() == Some(&b'\n') {
                return Some(self.bytes.split_to(1).freeze());
            }
        }
        while self.cursor < self.bytes.len() {
            let byte = self.bytes[self.cursor];
            if byte != b'\n' && byte != b'\r' {
                self.cursor += 1;
                continue;
            }
            if byte == b'\r'
                && self.cursor + 1 == self.bytes.len()
                && !eof
                && self.cursor != self.line_start
            {
                return None;
            }
            let split_crlf = byte == b'\r' && self.cursor + 1 == self.bytes.len() && !eof;
            let end = self.cursor
                + if byte == b'\r' && self.bytes.get(self.cursor + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            let empty = self.cursor == self.line_start;
            self.cursor = end;
            self.line_start = end;
            if empty {
                self.pending_lf = split_crlf;
                let event = self.bytes.split_to(end).freeze();
                self.cursor = 0;
                self.line_start = 0;
                return Some(event);
            }
        }
        None
    }
}

fn is_sse_terminal_event(event: &[u8]) -> bool {
    let text = String::from_utf8_lossy(event);
    let text = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let data = text
        .lines()
        .filter_map(|line| {
            if line == "data" {
                Some("")
            } else {
                line.strip_prefix("data:")
                    .map(|value| value.strip_prefix(' ').unwrap_or(value))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    data == "[DONE]"
        || serde_json::from_str::<serde_json::Value>(&data)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .is_some_and(|error| !error.is_null())
}

fn frame_sse_stream(source: ResponseStream, require_terminal: bool) -> ResponseStream {
    Box::pin(stream::unfold(
        (
            source,
            SseFrameBuffer::default(),
            false,
            false,
            None::<io::Error>,
        ),
        move |(mut source, mut buffer, mut eof, done, mut pending_error)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = buffer.take_event(eof) {
                    if event.len() > MAX_SSE_EVENT_BYTES {
                        return Some((
                            Err(io::Error::other("SSE event exceeds size limit")),
                            (source, buffer, eof, true, pending_error),
                        ));
                    }
                    let terminal = require_terminal && is_sse_terminal_event(&event);
                    return Some((Ok(event), (source, buffer, eof, terminal, pending_error)));
                }
                if let Some(error) = pending_error.take() {
                    return Some((Err(error), (source, buffer, eof, true, None)));
                }
                if buffer.bytes.len() > MAX_SSE_EVENT_BYTES || (eof && !buffer.bytes.is_empty()) {
                    return Some((
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "incomplete or oversized SSE event",
                        )),
                        (source, buffer, eof, true, None),
                    ));
                }
                if eof {
                    return if require_terminal {
                        Some((
                            Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "SSE completion marker missing",
                            )),
                            (source, buffer, eof, true, None),
                        ))
                    } else {
                        None
                    };
                }
                match source.next().await {
                    Some(Ok(chunk)) => buffer.bytes.extend_from_slice(&chunk),
                    Some(Err(error)) => {
                        eof = true;
                        pending_error = Some(error);
                    }
                    None => eof = true,
                }
            }
        },
    ))
}

fn with_idle_timeout(source: ResponseStream, timeout: Duration) -> ResponseStream {
    Box::pin(stream::unfold(
        (source, false),
        move |(mut source, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(timeout, source.next()).await {
                Ok(Some(result)) => Some((result, (source, false))),
                Ok(None) => None,
                Err(_) => Some((
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "upstream stream idle timeout",
                    )),
                    (source, true),
                )),
            }
        },
    ))
}

struct RecoveryCanaryPermit {
    recovery_id: String,
    token: String,
    expires_at: Instant,
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
    append_forwarded(&mut output, "via", "1.1 siderostat")?;
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

fn convert_method(method: &Method) -> reqwest::Method {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyIngress {
    Public,
    Peer,
}

pub struct PeerProxyToken(Vec<u8>);

impl PeerProxyToken {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PeerIngressError> {
        let mut bytes = bytes.into();
        if bytes.len() < 32 {
            return Err(PeerIngressError::InvalidTokenConfiguration);
        }
        let mut encoded = Vec::with_capacity(bytes.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for &byte in &bytes {
            encoded.push(HEX[usize::from(byte >> 4)]);
            encoded.push(HEX[usize::from(byte & 0x0f)]);
        }
        bytes.fill(0);
        Ok(Self(encoded))
    }
}

impl fmt::Debug for PeerProxyToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PeerProxyToken([REDACTED])")
    }
}

impl Drop for PeerProxyToken {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerIngressError {
    InvalidTokenConfiguration,
    MissingSecurityConfiguration,
    WrongSource,
    InvalidToken,
    InvalidHop,
}

#[derive(Debug)]
struct PeerProxySecurity {
    token: PeerProxyToken,
    expected_source: IpAddr,
}

impl PeerProxySecurity {
    fn verify(&self, source: IpAddr, headers: &HeaderMap) -> Result<(), PeerIngressError> {
        if source != self.expected_source {
            return Err(PeerIngressError::WrongSource);
        }
        let hop = headers
            .get("x-ds4-proxy-hop")
            .and_then(|value| value.to_str().ok());
        if hop != Some("1") {
            return Err(PeerIngressError::InvalidHop);
        }
        let supplied = headers
            .get("x-ds4-peer-proxy-token")
            .map(HeaderValue::as_bytes)
            .ok_or(PeerIngressError::InvalidToken)?;
        if supplied.len() != self.token.0.len()
            || !bool::from(supplied.ct_eq(self.token.0.as_slice()))
        {
            return Err(PeerIngressError::InvalidToken);
        }
        Ok(())
    }
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
    peer_security: RwLock<Option<Arc<PeerProxySecurity>>>,
    admission: AdmissionGate,
    request_body_limit_bytes: usize,
    response_header_timeout: std::time::Duration,
    first_body_byte_timeout: std::time::Duration,
    stream_idle_timeout: std::time::Duration,
    metrics: RwLock<Option<Arc<Metrics>>>,
    recovery_canary: Mutex<Option<RecoveryCanaryPermit>>,
    stream_interrupt: tokio::sync::watch::Sender<bool>,
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
            peer_security: RwLock::new(None),
            admission: AdmissionGate::new(options.max_in_flight),
            request_body_limit_bytes: options.request_body_limit_bytes,
            response_header_timeout: options.response_header_timeout,
            first_body_byte_timeout: options.first_body_byte_timeout,
            stream_idle_timeout: options.stream_idle_timeout,
            metrics: RwLock::new(None),
            recovery_canary: Mutex::new(None),
            stream_interrupt: tokio::sync::watch::channel(false).0,
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

    pub(crate) fn interrupt_in_flight(&self) {
        self.admission.close();
        self.stream_interrupt.send_replace(true);
    }

    /// Issue a short-lived, one-use permit for the recovery owner's fixed canary. The token is
    /// never forwarded upstream and is consumed before admission acquisition.
    pub(crate) fn issue_recovery_canary_permit(
        &self,
        recovery_id: &str,
        lifetime: Duration,
    ) -> String {
        let token = Uuid::new_v4().simple().to_string();
        *self
            .recovery_canary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RecoveryCanaryPermit {
            recovery_id: recovery_id.to_owned(),
            token: token.clone(),
            expires_at: Instant::now() + lifetime,
        });
        token
    }

    fn consume_recovery_canary_permit(&self, token: &str) -> bool {
        let mut permit = self
            .recovery_canary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let valid = permit.as_ref().is_some_and(|permit| {
            !permit.recovery_id.is_empty()
                && permit.expires_at > now
                && permit.token.as_bytes().ct_eq(token.as_bytes()).into()
        });
        if valid
            || permit
                .as_ref()
                .is_some_and(|permit| permit.expires_at <= now)
        {
            permit.take();
        }
        valid
    }

    pub fn configure_peer_proxy(&self, token: PeerProxyToken, expected_source: IpAddr) {
        *self
            .peer_security
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(PeerProxySecurity {
            token,
            expected_source,
        }));
    }

    pub(crate) fn configure_metrics(&self, metrics: Arc<Metrics>) {
        *self
            .metrics
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(metrics);
    }

    fn begin_request_metrics(
        &self,
        ingress: ProxyIngress,
        target: ProxyTarget,
        request_id: String,
        method: String,
        path_template: &'static str,
    ) -> Option<RequestMetricGuard> {
        self.metrics
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|metrics| {
                metrics.begin_request(
                    match ingress {
                        ProxyIngress::Public => "public",
                        ProxyIngress::Peer => "peer",
                    },
                    crate::app::target_name(target),
                    request_id,
                    method,
                    path_template,
                )
            })
    }

    fn peer_security(&self) -> Option<Arc<PeerProxySecurity>> {
        self.peer_security
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

pub async fn peer_ingress_handler(
    State(state): State<Arc<ModeAwareProxyState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    let request_id = request_id(request.headers());
    let Some(security) = state.peer_security() else {
        return ProxyError::NoBackendAvailable.response(&request_id);
    };
    if security.verify(peer.ip(), request.headers()).is_err() {
        return peer_ingress_rejection(&request_id);
    }
    forward_mode_aware(
        state,
        ProxyRequestContext {
            ingress: ProxyIngress::Peer,
            peer,
            hop: 1,
        },
        request,
    )
    .await
}

fn peer_ingress_rejection(request_id: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(
        b"{\"error\":{\"message\":\"peer ingress authentication failed\",\"type\":\"authentication_error\",\"code\":\"peer_authentication_failed\"}}"
            .as_slice(),
    ));
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    insert_request_id(response.headers_mut(), request_id);
    response
}

pub async fn forward_mode_aware(
    state: Arc<ModeAwareProxyState>,
    context: ProxyRequestContext,
    request: Request<Body>,
) -> Response<Body> {
    let mut interrupt = state.stream_interrupt.subscribe();
    let started = std::time::Instant::now();
    let (mut parts, body) = request.into_parts();
    let recovery_canary = parts
        .headers
        .remove(RECOVERY_CANARY_HEADER)
        .and_then(|value| {
            (context.ingress == ProxyIngress::Public)
                .then(|| value.to_str().ok().map(ToOwned::to_owned))
                .flatten()
        });
    let request_id = request_id(&parts.headers);
    let fixed = state.fixed_target();
    let mut observation = state.begin_request_metrics(
        context.ingress,
        fixed.target,
        request_id.clone(),
        parts.method.as_str().to_owned(),
        safe_path_template(parts.uri.path()),
    );
    let declared_length = parts
        .headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared_length.is_some_and(|length| length > state.request_body_limit_bytes) {
        return observed_error(ProxyError::BodyTooLarge, &request_id, &mut observation);
    }

    let base = match (context.ingress, fixed.target) {
        (ProxyIngress::Public | ProxyIngress::Peer, ProxyTarget::LocalStandalone) => {
            &state.local_upstream
        }
        (ProxyIngress::Public, ProxyTarget::Coordinator) => &state.coordinator_upstream,
        (ProxyIngress::Peer, ProxyTarget::Coordinator | ProxyTarget::Unavailable { .. })
        | (ProxyIngress::Public, ProxyTarget::Unavailable { .. }) => {
            return observed_error(
                ProxyError::NoBackendAvailable,
                &request_id,
                &mut observation,
            );
        }
    };
    let permit_result = match recovery_canary {
        Some(token) if state.consume_recovery_canary_permit(&token) => {
            state.admission.try_acquire_recovery(fixed.ready)
        }
        _ => state.admission.try_acquire(fixed.ready),
    };
    let permit = match permit_result {
        Ok(permit) => permit,
        Err(_) => {
            return observed_error(
                ProxyError::NoBackendAvailable,
                &request_id,
                &mut observation,
            );
        }
    };
    let peer_security = if fixed.target == ProxyTarget::Coordinator {
        match state.peer_security() {
            Some(security) => Some(security),
            None => {
                return observed_error(
                    ProxyError::NoBackendAvailable,
                    &request_id,
                    &mut observation,
                );
            }
        }
    } else {
        None
    };
    let url = match fixed_upstream_url(base, &parts.uri) {
        Ok(url) => url,
        Err(error) => return observed_error(error, &request_id, &mut observation),
    };
    let headers = match build_mode_aware_headers(
        &parts.headers,
        &parts.uri,
        &request_id,
        context,
        peer_security.as_deref(),
    ) {
        Ok(headers) => headers,
        Err(error) => return observed_error(error, &request_id, &mut observation),
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
    let upstream_result = tokio::select! {
        biased;
        _ = wait_for_interrupt(&mut interrupt) => return observed_error(ProxyError::NoBackendAvailable, &request_id, &mut observation),
        result = tokio::time::timeout(state.response_header_timeout, builder.send()) => result,
    };
    let upstream = match upstream_result {
        Err(_) => {
            return observed_error(
                ProxyError::ResponseHeaderTimeout,
                &request_id,
                &mut observation,
            );
        }
        Ok(Err(error)) => {
            let chain = format_error_chain(&error);
            if chain.contains("request body limit exceeded") {
                return observed_error(ProxyError::BodyTooLarge, &request_id, &mut observation);
            }
            let proxy_error = if error.is_connect() {
                ProxyError::Connect
            } else {
                ProxyError::Protocol
            };
            return observed_error(proxy_error, &request_id, &mut observation);
        }
        Ok(Ok(response)) => response,
    };

    let prepared = tokio::select! {
        biased;
        _ = wait_for_interrupt(&mut interrupt) => return observed_error(ProxyError::NoBackendAvailable, &request_id, &mut observation),
        result = prepare_mode_aware_response(&state, upstream) => result,
    };
    match prepared {
        Ok(prepared) => {
            if let Some(observation) = observation.as_mut() {
                observation.set_status(prepared.status.as_u16());
                observation.set_ttfb(started.elapsed().as_secs_f64());
            }
            let completion = parts.method != Method::HEAD
                && matches!(parts.uri.path(), "/v1/chat/completions" | "/v1/completions");
            build_mode_aware_response(
                &state,
                &request_id,
                prepared,
                permit,
                observation,
                completion,
            )
        }
        Err(error) => observed_error(error, &request_id, &mut observation),
    }
}

async fn wait_for_interrupt(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    if *receiver.borrow_and_update() {
        return;
    }
    let _ = receiver.changed().await;
}

fn safe_path_template(path: &str) -> &'static str {
    match path {
        "/v1/chat/completions" => "/v1/chat/completions",
        "/v1/completions" => "/v1/completions",
        "/v1/models" => "/v1/models",
        "/health" => "/health",
        _ => "/*",
    }
}

fn observed_error(
    error: ProxyError,
    request_id: &str,
    observation: &mut Option<RequestMetricGuard>,
) -> Response<Body> {
    if let Some(observation) = observation {
        observation.set_status(error.status().as_u16());
        if matches!(
            error,
            ProxyError::Connect
                | ProxyError::ResponseHeaderTimeout
                | ProxyError::FirstByteTimeout
                | ProxyError::Protocol
        ) {
            observation.set_failure(error.code());
        }
    }
    error.response(request_id)
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
    peer_security: Option<&PeerProxySecurity>,
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
    if context.ingress == ProxyIngress::Public
        && let Some(security) = peer_security
    {
        headers.insert(
            "x-ds4-peer-proxy-token",
            HeaderValue::from_bytes(&security.token.0).map_err(|_| ProxyError::Internal)?,
        );
        headers.insert("x-ds4-proxy-hop", HeaderValue::from_static("1"));
    }
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
    observation: Option<RequestMetricGuard>,
    completion: bool,
) -> Response<Body> {
    let status = prepared.status;
    let headers = prepared.headers;
    let is_sse = completion
        && !matches!(
            status,
            StatusCode::NO_CONTENT | StatusCode::RESET_CONTENT | StatusCode::NOT_MODIFIED
        )
        && headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
        && headers.get_all("content-encoding").iter().all(|value| {
            value
                .to_str()
                .is_ok_and(|encoding| encoding.trim().eq_ignore_ascii_case("identity"))
        });
    let idle_timeout = state.stream_idle_timeout;
    let interrupt = state.stream_interrupt.subscribe();
    let first = stream::iter(prepared.first_chunk.map(Ok::<Bytes, io::Error>));
    let source: ResponseStream = Box::pin(
        first.chain(
            prepared
                .stream
                .map(|result| result.map_err(io::Error::other)),
        ),
    );
    let source = with_idle_timeout(source, idle_timeout);
    let source = if is_sse {
        frame_sse_stream(source, completion && status.is_success())
    } else {
        source
    };
    let stream = stream::unfold(
        (source, Some(permit), observation, interrupt, false),
        move |(mut upstream, permit, mut observation, mut interrupt, done)| async move {
            if done {
                return None;
            }
            if *interrupt.borrow() {
                if let Some(observation) = observation.as_mut() {
                    observation.set_failure("stream-interrupted");
                }
                upstream = Box::pin(stream::empty());
                let result = if is_sse {
                    Ok(Bytes::from_static(b"\n\ndata: {\"error\":{\"message\":\"Stream interrupted by runtime restart\",\"type\":\"service_unavailable\",\"code\":\"stream_interrupted\"}}\n\n"))
                } else {
                    Err(io::Error::other("stream interrupted by runtime restart"))
                };
                return Some((result, (upstream, None, observation, interrupt, true)));
            }
            let next = tokio::select! {
                biased;
                _ = interrupt.changed() => {
                    if let Some(observation) = observation.as_mut() { observation.set_failure("stream-interrupted"); }
                    upstream = Box::pin(stream::empty());
                    let result = if is_sse {
                        Ok(Bytes::from_static(b"\n\ndata: {\"error\":{\"message\":\"Stream interrupted by runtime restart\",\"type\":\"service_unavailable\",\"code\":\"stream_interrupted\"}}\n\n"))
                    } else { Err(io::Error::other("stream interrupted by runtime restart")) };
                    return Some((result, (upstream, None, observation, interrupt, true)));
                }
                next = upstream.next() => next,
            };
            match next {
                Some(Ok(chunk)) => {
                    Some((Ok(chunk), (upstream, permit, observation, interrupt, false)))
                }
                Some(Err(error)) if error.kind() == io::ErrorKind::TimedOut => {
                    if let Some(observation) = observation.as_mut() {
                        observation.set_failure("stream-idle-timeout");
                    }
                    upstream = Box::pin(stream::empty());
                    if is_sse {
                        return Some((
                            Ok(Bytes::from_static(b"\n\ndata: {\"error\":{\"message\":\"Upstream stream timed out\",\"type\":\"upstream_timeout\",\"code\":\"upstream_stream_timeout\"}}\n\n")),
                            (upstream, None, observation, interrupt, true),
                        ));
                    }
                    Some((Err(error), (upstream, None, observation, interrupt, true)))
                }
                Some(Err(error)) => {
                    if let Some(observation) = observation.as_mut() {
                        observation.set_failure("stream-error");
                    }
                    upstream = Box::pin(stream::empty());
                    if is_sse {
                        let payload = if error.kind() == io::ErrorKind::UnexpectedEof {
                            b"\n\ndata: {\"error\":{\"message\":\"Upstream stream ended without completion\",\"type\":\"upstream_error\",\"code\":\"upstream_stream_incomplete\"}}\n\n".as_slice()
                        } else {
                            b"\n\ndata: {\"error\":{\"message\":\"Upstream stream interrupted\",\"type\":\"upstream_error\",\"code\":\"upstream_stream_interrupted\"}}\n\n".as_slice()
                        };
                        return Some((
                            Ok(Bytes::from_static(payload)),
                            (upstream, None, observation, interrupt, true),
                        ));
                    }
                    Some((
                        Err(io::Error::other("upstream stream error")),
                        (upstream, None, observation, interrupt, true),
                    ))
                }
                None => None,
            }
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    copy_response_headers(&headers, response.headers_mut());
    if is_sse {
        response.headers_mut().remove("content-length");
    }
    insert_request_id(response.headers_mut(), request_id);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, body::to_bytes, routing::any};
    use futures::stream;
    use serde_json::Value;
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::{Duration, Instant},
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
    fn sse_frames_survive_every_chunk_boundary_and_mixed_line_endings() {
        let sample = b"\xef\xbb\xbf: comment\r\ndata: first\r\n\ndata: second\n\r\ndata: third\r\r";
        for split in 0..=sample.len() {
            let mut buffer = SseFrameBuffer::default();
            let mut received = Vec::new();
            for chunk in [&sample[..split], &sample[split..]] {
                buffer.bytes.extend_from_slice(chunk);
                while let Some(event) = buffer.take_event(false) {
                    received.extend_from_slice(&event);
                }
            }
            while let Some(event) = buffer.take_event(true) {
                received.extend_from_slice(&event);
            }
            assert_eq!(received, sample, "split={split}");
            assert!(buffer.bytes.is_empty());
        }
    }

    #[test]
    fn completion_terminal_recognizes_done_and_multiline_error() {
        assert!(is_sse_terminal_event(
            b"\xef\xbb\xbf: comment\r\ndata: [DONE]\r\n\r\n"
        ));
        assert!(is_sse_terminal_event(
            b"data: {\"error\":\r\ndata: {\"code\":\"failed\"}}\r\n\r\n"
        ));
        assert!(!is_sse_terminal_event(b"data: {\"choices\":[]}\n\n"));
    }

    #[tokio::test]
    async fn sse_size_limit_applies_per_event_not_per_transport_chunk() {
        let event = b"data: x\n\n";
        let many = event.repeat(MAX_SSE_EVENT_BYTES / event.len() + 1);
        let source: ResponseStream = Box::pin(stream::iter([Ok(Bytes::from(many.clone()))]));
        let mut framed = frame_sse_stream(source, false);
        let mut received = Vec::new();
        while let Some(part) = framed.next().await {
            received.extend_from_slice(&part.unwrap());
        }
        assert_eq!(received, many);
        let source: ResponseStream = Box::pin(stream::iter([Ok(Bytes::from(vec![
            b'x';
            MAX_SSE_EVENT_BYTES
                + 1
        ]))]));
        let mut framed = frame_sse_stream(source, false);
        assert!(framed.next().await.unwrap().is_err());
    }

    #[tokio::test]
    async fn partial_sse_bytes_refresh_idle_timeout() {
        let source: ResponseStream = Box::pin(stream::unfold(0, |step| async move {
            if step >= 4 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
            let byte = b"d\n\n\n"[step];
            Some((Ok(Bytes::copy_from_slice(&[byte])), step + 1))
        }));
        let mut framed =
            frame_sse_stream(with_idle_timeout(source, Duration::from_millis(150)), false);
        let first = tokio::time::timeout(Duration::from_secs(1), framed.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first, Bytes::from_static(b"d\n\n"));
    }

    #[tokio::test]
    async fn complete_cr_terminated_events_are_not_lost_on_transport_error() {
        for (payload, terminal) in [
            (b"data: [DONE]\r\r".as_slice(), true),
            (b"data: first\r\r".as_slice(), false),
        ] {
            let source: ResponseStream = Box::pin(stream::iter([
                Ok(Bytes::copy_from_slice(payload)),
                Err(io::Error::other("upstream lost")),
            ]));
            let mut framed = frame_sse_stream(source, terminal);
            assert_eq!(
                framed.next().await.unwrap().unwrap(),
                Bytes::copy_from_slice(payload)
            );
            if terminal {
                assert!(framed.next().await.is_none());
            } else {
                assert!(framed.next().await.unwrap().is_err());
            }
        }
    }

    #[tokio::test]
    async fn cr_terminated_event_does_not_wait_for_another_chunk() {
        let source: ResponseStream = Box::pin(
            stream::once(async { Ok(Bytes::from_static(b"data: [DONE]\r\r")) })
                .chain(stream::pending()),
        );
        let mut framed = frame_sse_stream(source, true);
        let event = tokio::time::timeout(Duration::from_millis(200), framed.next())
            .await
            .expect("completed event must not wait for the next transport chunk")
            .unwrap()
            .unwrap();
        assert_eq!(event, Bytes::from_static(b"data: [DONE]\r\r"));
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

    async fn start_peer_ingress(state: Arc<ModeAwareProxyState>) -> TestServer {
        let app = Router::new()
            .route("/", any(peer_ingress_handler))
            .route("/{*path}", any(peer_ingress_handler))
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
    async fn recovery_canary_permit_is_consumed_once_without_serving_public_admission() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async { Response::new(Body::from("canary")) }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        state.admission().block();
        let proxy = start_mode_aware_proxy(state.clone()).await;
        let token = state.issue_recovery_canary_permit("recovery-test", Duration::from_secs(5));
        let client = reqwest::Client::new();

        let first = client
            .get(format!("http://{}/v1/canary", proxy.address))
            .header(RECOVERY_CANARY_HEADER, &token)
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.text().await.unwrap(), "canary");

        let second = client
            .get(format!("http://{}/v1/canary", proxy.address))
            .header(RECOVERY_CANARY_HEADER, token)
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            state.admission().snapshot().state,
            crate::admission::AdmissionState::Blocked
        );
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
    async fn sse_upstream_disconnect_reports_failure_without_success_marker() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    match index {
                        0 => Some((
                            Ok::<_, io::Error>(Bytes::from_static(
                                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
                            )),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(io::Error::other("simulated disconnect")), 2))
                        }
                        _ => None,
                    }
                });
                let mut response = Response::new(Body::from_stream(chunks));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state.clone()).await;
        let response = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap();
        let body = response
            .text()
            .await
            .expect("SSE failure must end as a complete HTTP response");
        assert!(body.contains("\"content\":\"hi\""));
        assert!(body.contains("\"error\""));
        assert!(!body.contains("[DONE]"));
        assert_eq!(state.admission().snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn restart_interrupts_requests_waiting_for_headers_or_first_byte() {
        use tokio::io::AsyncWriteExt;
        for send_headers in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (ready, started) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                if send_headers {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                }
                let _ = ready.send(());
                std::future::pending::<()>().await;
            });
            let state = mode_aware_state(address, 4096);
            let proxy = start_mode_aware_proxy(state.clone()).await;
            let request = tokio::spawn(async move {
                reqwest::get(format!("http://{}/v1/chat/completions", proxy.address)).await
            });
            started.await.unwrap();
            state.interrupt_in_flight();
            let response = tokio::time::timeout(Duration::from_millis(500), request)
                .await
                .expect("restart must stop pending request")
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(state.admission().snapshot().in_flight, 0);
            server.abort();
        }
    }

    #[test]
    fn interrupted_admission_cannot_be_reopened_by_stale_recovery() {
        let state = mode_aware_state("127.0.0.1:1".parse().unwrap(), 4096);
        state.interrupt_in_flight();
        state.admission().start_serving();
        assert!(state.admission().try_acquire(true).is_err());
        assert!(state.admission().try_acquire_recovery(true).is_err());
    }

    #[tokio::test]
    async fn sse_disconnect_mid_event_emits_only_complete_events_and_error() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    match index {
                        0 => Some((
                            Ok::<_, io::Error>(Bytes::from_static(b"data: {\"choices\":[]}")),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(io::Error::other("disconnect")), 2))
                        }
                        _ => None,
                    }
                });
                let mut response = Response::new(Body::from_stream(chunks));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state).await;
        let body = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let events: Vec<_> = body
            .split("\n\n")
            .filter_map(|event| event.strip_prefix("data: "))
            .collect();
        assert_eq!(
            events.len(),
            1,
            "partial upstream event must be withheld: {body:?}"
        );
        let error: serde_json::Value = serde_json::from_str(events[0]).unwrap();
        assert_eq!(error["error"]["code"], "upstream_stream_interrupted");
    }

    #[tokio::test]
    async fn compressed_sse_disconnect_does_not_append_uncompressed_error() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::unfold(0, |index| async move {
                    match index {
                        0 => Some((
                            Ok::<_, io::Error>(Bytes::from_static(b"compressed-bytes")),
                            1,
                        )),
                        1 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            Some((Err(io::Error::other("disconnect")), 2))
                        }
                        _ => None,
                    }
                });
                let mut response = Response::new(Body::from_stream(chunks));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                response
                    .headers_mut()
                    .insert("content-encoding", HeaderValue::from_static("gzip"));
                response
            }),
        ))
        .await;
        let proxy = start_mode_aware_proxy(mode_aware_state(backend.address, 4096)).await;
        let response = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap();
        assert!(
            response.bytes().await.is_err(),
            "encoded SSE must keep a transport error rather than mix encodings"
        );
    }

    #[tokio::test]
    async fn completion_eof_without_done_reports_error_but_generic_sse_can_end() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from("data: {\"choices\":[]}\n\n"))
                    .unwrap()
            }),
        ))
        .await;
        let proxy = start_mode_aware_proxy(mode_aware_state(backend.address, 4096)).await;
        let chat = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(chat.contains("\"code\":\"upstream_stream_incomplete\""));
        assert!(!chat.contains("[DONE]"));
        let other = reqwest::get(format!("http://{}/v1/events", proxy.address))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(other, "data: {\"choices\":[]}\n\n");
    }

    #[tokio::test]
    async fn bodyless_completion_responses_do_not_require_sse_terminal() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                let status = if request.uri().query() == Some("empty") {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::OK
                };
                Response::builder()
                    .status(status)
                    .header("content-type", "text/event-stream")
                    .body(Body::empty())
                    .unwrap()
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        for request in [
            Request::head("/v1/chat/completions"),
            Request::get("/v1/chat/completions?empty"),
        ] {
            let response = forward_mode_aware(
                state.clone(),
                ProxyRequestContext {
                    ingress: ProxyIngress::Public,
                    peer: "127.0.0.1:1234".parse().unwrap(),
                    hop: 0,
                },
                request.body(Body::empty()).unwrap(),
            )
            .await;
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert!(
                body.is_empty(),
                "bodyless response must not synthesize an SSE error: {body:?}"
            );
        }
    }

    #[tokio::test]
    async fn upstream_error_event_is_forwarded_without_duplicate_error() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from("data: {\"error\":{\"code\":\"original\"}}\n\n"))
                    .unwrap()
            }),
        ))
        .await;
        let proxy = start_mode_aware_proxy(mode_aware_state(backend.address, 4096)).await;
        let body = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "data: {\"error\":{\"code\":\"original\"}}\n\n");
    }

    #[tokio::test]
    async fn stalled_sse_reports_timeout_without_success_marker() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::once(async {
                    Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n"))
                })
                .chain(stream::pending());
                let mut response = Response::new(Body::from_stream(chunks));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream; charset=utf-8"),
                );
                response
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        let proxy = start_mode_aware_proxy(state.clone()).await;
        let response = reqwest::get(format!("http://{}/v1/chat/completions", proxy.address))
            .await
            .unwrap();
        let body = tokio::time::timeout(Duration::from_secs(4), response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(body.contains("\"code\":\"upstream_stream_timeout\""));
        assert!(!body.contains("[DONE]"));
        assert_eq!(state.admission().snapshot().in_flight, 0);
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
            None,
        )
        .unwrap();

        assert!(headers.get("x-ds4-peer-proxy-token").is_none());
        assert!(headers.get("x-ds4-proxy-hop").is_none());
        assert!(headers.get("x-ds4-cluster-generation").is_none());
        assert_eq!(headers.get("x-keep").unwrap(), "yes");
    }

    #[test]
    fn request_log_path_template_never_exposes_arbitrary_path_segments() {
        assert_eq!(
            safe_path_template("/v1/chat/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(safe_path_template("/sessions/private-session-id"), "/*");
        assert_eq!(safe_path_template("/prompt/do-not-log"), "/*");
    }

    #[test]
    fn peer_security_rejects_wrong_source_token_and_hop() {
        let security = PeerProxySecurity {
            token: PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            expected_source: IpAddr::from([10, 99, 0, 2]),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-ds4-peer-proxy-token",
            HeaderValue::from_static(
                "7474747474747474747474747474747474747474747474747474747474747474",
            ),
        );
        headers.insert("x-ds4-proxy-hop", HeaderValue::from_static("1"));
        assert_eq!(
            security.verify(IpAddr::from([192, 168, 1, 2]), &headers),
            Err(PeerIngressError::WrongSource)
        );
        headers.insert(
            "x-ds4-peer-proxy-token",
            HeaderValue::from_static(
                "7878787878787878787878787878787878787878787878787878787878787878",
            ),
        );
        assert_eq!(
            security.verify(IpAddr::from([10, 99, 0, 2]), &headers),
            Err(PeerIngressError::InvalidToken)
        );
        headers.insert(
            "x-ds4-peer-proxy-token",
            HeaderValue::from_static(
                "7474747474747474747474747474747474747474747474747474747474747474",
            ),
        );
        headers.insert("x-ds4-proxy-hop", HeaderValue::from_static("2"));
        assert_eq!(
            security.verify(IpAddr::from([10, 99, 0, 2]), &headers),
            Err(PeerIngressError::InvalidHop)
        );
    }

    #[tokio::test]
    async fn public_coordinator_target_replaces_untrusted_internal_headers() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|request: Request<Body>| async move {
                Json(serde_json::json!({
                    "token": request.headers().get("x-ds4-peer-proxy-token").and_then(|v| v.to_str().ok()),
                    "hop": request.headers().get("x-ds4-proxy-hop").and_then(|v| v.to_str().ok()),
                }))
            }),
        ))
        .await;
        let state = mode_aware_state(backend.address, 4096);
        state.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        state.set_target(ProxyTarget::Coordinator, true);
        let proxy = start_mode_aware_proxy(state).await;
        let response: Value = reqwest::Client::new()
            .get(format!("http://{}/v1/test", proxy.address))
            .header("x-ds4-peer-proxy-token", "untrusted")
            .header("x-ds4-proxy-hop", "99")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            response["token"],
            "7474747474747474747474747474747474747474747474747474747474747474"
        );
        assert_eq!(response["hop"], "1");
    }

    #[tokio::test]
    async fn peer_ingress_rejects_invalid_auth_before_backend() {
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
        state.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        let peer = start_peer_ingress(state).await;
        let response = reqwest::Client::new()
            .get(format!("http://{}/v1/test", peer.address))
            .header(
                "x-ds4-peer-proxy-token",
                "7878787878787878787878787878787878787878787878787878787878787878",
            )
            .header("x-ds4-proxy-hop", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(hits.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn peer_ingress_never_forwards_to_another_peer_hop() {
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
        state.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        state.set_target(ProxyTarget::Coordinator, true);
        let peer = start_peer_ingress(state).await;
        let response = reqwest::Client::new()
            .get(format!("http://{}/v1/test", peer.address))
            .header(
                "x-ds4-peer-proxy-token",
                "7474747474747474747474747474747474747474747474747474747474747474",
            )
            .header("x-ds4-proxy-hop", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(hits.load(AtomicOrdering::Relaxed), 0);
    }

    #[tokio::test]
    async fn two_hop_peer_proxy_streams_sse_and_uses_coordinator_admission() {
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
                            tokio::time::sleep(Duration::from_millis(80)).await;
                            Some((
                                Ok::<_, Infallible>(Bytes::from_static(b"data: second\n\n")),
                                2,
                            ))
                        }
                        _ => None,
                    }
                });
                Response::new(Body::from_stream(chunks))
            }),
        ))
        .await;
        let coordinator = mode_aware_state(backend.address, 4096);
        coordinator.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        let peer = start_peer_ingress(coordinator.clone()).await;

        let worker = Arc::new(
            ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:1").unwrap(),
                url::Url::parse(&format!("http://{}", peer.address)).unwrap(),
                ModeAwareProxyOptions {
                    max_in_flight: 8,
                    request_body_limit_bytes: 4096,
                    response_header_timeout: Duration::from_secs(2),
                    first_body_byte_timeout: Duration::from_secs(2),
                    stream_idle_timeout: Duration::from_secs(2),
                    connect_timeout: Duration::from_millis(200),
                },
            )
            .unwrap(),
        );
        worker.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        worker.set_target(ProxyTarget::Coordinator, true);
        worker.admission().start_serving();
        let public = start_mode_aware_proxy(worker.clone()).await;

        let response = reqwest::get(format!("http://{}/v1/stream", public.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(first, Bytes::from_static(b"data: first\n\n"));
        assert_eq!(coordinator.admission().snapshot().in_flight, 1);
        let second = body.next().await.unwrap().unwrap();
        assert_eq!(second, Bytes::from_static(b"data: second\n\n"));
        drop(body);
        for _ in 0..30 {
            if coordinator.admission().snapshot().in_flight == 0
                && worker.admission().snapshot().in_flight == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(coordinator.admission().snapshot().in_flight, 0);
        assert_eq!(worker.admission().snapshot().in_flight, 0);
    }

    #[tokio::test]
    async fn two_hop_restart_interrupts_stalled_sse_and_releases_both_permits() {
        let backend = start_server(Router::new().route(
            "/{*path}",
            any(|| async {
                let chunks = stream::once(async {
                    Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n"))
                })
                .chain(stream::pending());
                let mut response = Response::new(Body::from_stream(chunks));
                response.headers_mut().insert(
                    "content-type",
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        ))
        .await;
        let coordinator = mode_aware_state(backend.address, 4096);
        coordinator.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        let peer = start_peer_ingress(coordinator.clone()).await;
        let worker = mode_aware_state(peer.address, 4096);
        worker.configure_peer_proxy(
            PeerProxyToken::new(vec![b't'; 32]).unwrap(),
            IpAddr::from([127, 0, 0, 1]),
        );
        worker.set_target(ProxyTarget::Coordinator, true);
        let public = start_mode_aware_proxy(worker.clone()).await;
        let response = reqwest::get(format!("http://{}/v1/chat/completions", public.address))
            .await
            .unwrap();
        let mut body = response.bytes_stream();
        assert_eq!(
            body.next().await.unwrap().unwrap(),
            Bytes::from_static(b"data: first\n\n")
        );
        assert_eq!(coordinator.admission().snapshot().in_flight, 1);
        worker.interrupt_in_flight();
        tokio::time::timeout(Duration::from_secs(1), async {
            while coordinator.admission().snapshot().in_flight != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("upstream permit must be released without waiting for the client");
        let event = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&event).contains("\"code\":\"stream_interrupted\""));
        assert!(body.next().await.is_none());
        tokio::time::timeout(Duration::from_secs(1), async {
            while coordinator.admission().snapshot().in_flight != 0
                || worker.admission().snapshot().in_flight != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both admission permits must be released");
    }
}
