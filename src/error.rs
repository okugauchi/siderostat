use axum::{
    body::Body,
    http::{HeaderValue, Response, StatusCode},
};
use serde::Serialize;
use std::error::Error;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    Connect,
    Tls,
    Heartbeat,
    ResponseHeaderTimeout,
    FirstByteTimeout,
    StreamIdleTimeout,
    Http5xx,
    Protocol,
    ClientCancelled,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Heartbeat => "heartbeat",
            Self::ResponseHeaderTimeout => "response_header_timeout",
            Self::FirstByteTimeout => "first_body_byte_timeout",
            Self::StreamIdleTimeout => "stream_idle_timeout",
            Self::Http5xx => "http_5xx",
            Self::Protocol => "protocol",
            Self::ClientCancelled => "client_cancelled",
        }
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("No DS4 backend is currently available")]
    NoBackendAvailable,
    #[error("invalid affinity header")]
    InvalidAffinity,
    #[error("request body is too large")]
    BodyTooLarge,
    #[error("failed to read request body")]
    InvalidBody,
    #[error("upstream connection failed")]
    Connect,
    #[error("upstream DS4 backend timed out before returning response headers")]
    ResponseHeaderTimeout,
    #[error("upstream DS4 backend timed out before producing output")]
    FirstByteTimeout,
    #[error("upstream protocol error")]
    Protocol,
    #[error("upstream returned a retryable gateway status")]
    RetryableUpstreamStatus,
    #[error("internal state error")]
    Internal,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    error_type: &'a str,
    code: &'a str,
    request_id: &'a str,
}

impl ProxyError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidAffinity | Self::InvalidBody => StatusCode::BAD_REQUEST,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NoBackendAvailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Connect | Self::Protocol | Self::RetryableUpstreamStatus => {
                StatusCode::BAD_GATEWAY
            }
            Self::ResponseHeaderTimeout | Self::FirstByteTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NoBackendAvailable => "no_backend_available",
            Self::InvalidAffinity => "invalid_affinity_header",
            Self::BodyTooLarge => "request_body_too_large",
            Self::InvalidBody => "invalid_request_body",
            Self::Connect => "upstream_connect_failed",
            Self::ResponseHeaderTimeout => "response_header_timeout",
            Self::FirstByteTimeout => "first_body_byte_timeout",
            Self::Protocol => "upstream_protocol_error",
            Self::RetryableUpstreamStatus => "upstream_gateway_status",
            Self::Internal => "internal_state_error",
        }
    }

    fn error_type(&self) -> &'static str {
        match self.status() {
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => "invalid_request_error",
            StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
            StatusCode::BAD_GATEWAY => "upstream_error",
            StatusCode::GATEWAY_TIMEOUT => "upstream_timeout",
            _ => "internal_error",
        }
    }

    pub fn response(&self, request_id: &str) -> Response<Body> {
        let body = serde_json::to_vec(&ErrorEnvelope {
            error: ErrorBody {
                message: &self.to_string(),
                error_type: self.error_type(),
                code: self.code(),
                request_id,
            },
        })
        .unwrap_or_else(|_| b"{\"error\":{\"code\":\"serialization_error\"}}".to_vec());
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status();
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static("application/json"));
        if matches!(self, Self::NoBackendAvailable) {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("5"));
        }
        if let Ok(value) = HeaderValue::from_str(request_id) {
            response.headers_mut().insert("x-request-id", value);
        }
        response
    }
}

pub fn format_error_chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}
