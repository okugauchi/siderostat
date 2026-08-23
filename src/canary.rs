use anyhow::{Result, ensure};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    net::{IpAddr, SocketAddr},
    str,
    time::{Duration, Instant},
};

pub const CANARY_PROMPT: &str = "Reply with the single word: OK.";
pub const CANARY_MAX_TOKENS: u64 = 64;
pub const CANARY_DEADLINE: Duration = Duration::from_secs(30);
pub const CANARY_PROGRESS_STALL: Duration = Duration::from_secs(60);
pub const CANARY_LOW_DECODE_TPS: f64 = 5.0;
const CANARY_PATH: &str = "/v1/chat/completions";
const MAX_SSE_BUFFER_BYTES: usize = 128 * 1024;
pub(crate) const RECOVERY_CANARY_HEADER: &str = "x-siderostat-recovery-canary";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanaryPolicy {
    pub deadline: Duration,
    pub progress_stall: Duration,
    pub low_decode_tps: f64,
}

impl Default for CanaryPolicy {
    fn default() -> Self {
        Self {
            deadline: CANARY_DEADLINE,
            progress_stall: CANARY_PROGRESS_STALL,
            low_decode_tps: CANARY_LOW_DECODE_TPS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryReason {
    Healthy,
    Deadline,
    HttpError,
    LowDecodeTps,
    ProgressStall,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CanaryResult {
    pub status: &'static str,
    pub reason: CanaryReason,
    pub elapsed_ms: u64,
    pub ttfb_ms: Option<u64>,
    pub generated_tokens: u64,
    pub chunk_tps: Option<f64>,
    pub http_status: Option<u16>,
}

pub struct CanaryExecutor {
    client: Client,
    public_listen: SocketAddr,
    policy: CanaryPolicy,
}

impl CanaryExecutor {
    pub fn new(public_listen: SocketAddr, policy: CanaryPolicy) -> Result<Self> {
        ensure!(
            !policy.deadline.is_zero(),
            "canary deadline must be positive"
        );
        ensure!(
            !policy.progress_stall.is_zero(),
            "canary progress stall must be positive"
        );
        ensure!(
            policy.low_decode_tps.is_finite() && policy.low_decode_tps >= 0.0,
            "canary low decode TPS must be finite and non-negative"
        );
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            public_listen,
            policy,
        })
    }

    pub fn default_for(public_listen: SocketAddr) -> Result<Self> {
        Self::new(public_listen, CanaryPolicy::default())
    }

    pub async fn execute(&self) -> CanaryResult {
        self.execute_with_permit(None).await
    }

    pub(crate) async fn execute_with_recovery_permit(&self, permit: &str) -> CanaryResult {
        self.execute_with_permit(Some(permit)).await
    }

    async fn execute_with_permit(&self, recovery_permit: Option<&str>) -> CanaryResult {
        let started = Instant::now();
        let deadline = started + self.policy.deadline;
        let mut request = self
            .client
            .post(self.endpoint())
            .json(&canary_request_payload());
        if let Some(permit) = recovery_permit {
            request = request.header(RECOVERY_CANARY_HEADER, permit);
        }

        let response = match tokio::time::timeout(remaining(deadline), request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return self.failure(started, None, CanaryReason::HttpError, 0, None),
            Err(_) => return self.failure(started, None, CanaryReason::Deadline, 0, None),
        };

        let http_status = Some(response.status().as_u16());
        if !response.status().is_success() {
            return self.failure(started, http_status, CanaryReason::HttpError, 0, None);
        }

        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut ttfb = None;
        let mut generated_tokens = 0_u64;
        let mut generation_started = None;
        let mut last_progress = None;
        let mut completed = false;

        loop {
            let now = Instant::now();
            let deadline_remaining = deadline.saturating_duration_since(now);
            let stall_remaining = last_progress.map_or(deadline_remaining, |last| {
                self.policy
                    .progress_stall
                    .saturating_sub(now.saturating_duration_since(last))
            });
            let wait = deadline_remaining.min(stall_remaining);
            if wait.is_zero() {
                let reason = timeout_reason(deadline, last_progress, self.policy.progress_stall);
                return self.failure(started, http_status, reason, generated_tokens, ttfb);
            }

            match tokio::time::timeout(wait, stream.next()).await {
                Ok(Some(Ok(bytes))) => {
                    ttfb.get_or_insert_with(|| elapsed_millis(started));
                    let events = match parser.push(&bytes) {
                        Ok(events) => events,
                        Err(()) => {
                            return self.failure(
                                started,
                                http_status,
                                CanaryReason::HttpError,
                                generated_tokens,
                                ttfb,
                            );
                        }
                    };
                    for event in events {
                        if event.done {
                            completed = true;
                        }
                        if event.generated_tokens > 0 {
                            generated_tokens =
                                generated_tokens.saturating_add(event.generated_tokens);
                            generation_started.get_or_insert_with(Instant::now);
                            last_progress = Some(Instant::now());
                        }
                    }
                    if completed {
                        break;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => {
                    return self.failure(
                        started,
                        http_status,
                        CanaryReason::HttpError,
                        generated_tokens,
                        ttfb,
                    );
                }
                Err(_) => {
                    let reason =
                        timeout_reason(deadline, last_progress, self.policy.progress_stall);
                    return self.failure(started, http_status, reason, generated_tokens, ttfb);
                }
            }
        }

        if generated_tokens == 0 {
            return self.failure(
                started,
                http_status,
                CanaryReason::HttpError,
                generated_tokens,
                ttfb,
            );
        }

        let chunk_tps = generation_started.and_then(|first| {
            let seconds = first.elapsed().as_secs_f64();
            (seconds > 0.0).then_some(generated_tokens as f64 / seconds)
        });
        let reason = if generated_tokens >= 2
            && chunk_tps.is_some_and(|tps| tps < self.policy.low_decode_tps)
        {
            CanaryReason::LowDecodeTps
        } else {
            CanaryReason::Healthy
        };
        CanaryResult {
            status: status_for(reason),
            reason,
            elapsed_ms: elapsed_millis(started),
            ttfb_ms: ttfb,
            generated_tokens,
            chunk_tps,
            http_status,
        }
    }

    fn endpoint(&self) -> String {
        let address = if self.public_listen.ip().is_unspecified() {
            SocketAddr::new(
                loopback_for(self.public_listen.ip()),
                self.public_listen.port(),
            )
        } else {
            self.public_listen
        };
        format!("http://{address}{CANARY_PATH}")
    }

    fn failure(
        &self,
        started: Instant,
        http_status: Option<u16>,
        reason: CanaryReason,
        generated_tokens: u64,
        ttfb_ms: Option<u64>,
    ) -> CanaryResult {
        CanaryResult {
            status: status_for(reason),
            reason,
            elapsed_ms: elapsed_millis(started),
            ttfb_ms,
            generated_tokens,
            chunk_tps: None,
            http_status,
        }
    }
}

fn canary_request_payload() -> Value {
    json!({
        "messages": [{
            "role": "user",
            "content": CANARY_PROMPT,
        }],
        "max_tokens": CANARY_MAX_TOKENS,
        "stream": true,
    })
}

fn loopback_for(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    }
}

fn status_for(reason: CanaryReason) -> &'static str {
    if reason == CanaryReason::Healthy {
        "healthy"
    } else {
        "failed"
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn timeout_reason(
    deadline: Instant,
    first_progress: Option<Instant>,
    progress_stall: Duration,
) -> CanaryReason {
    let now = Instant::now();
    if first_progress
        .is_some_and(|last| now >= last + progress_stall && last + progress_stall < deadline)
    {
        CanaryReason::ProgressStall
    } else {
        CanaryReason::Deadline
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn push(&mut self, bytes: &Bytes) -> Result<Vec<SseEvent>, ()> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_SSE_BUFFER_BYTES {
            return Err(());
        }
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((end, delimiter_len)) = event_boundary(&self.buffer) {
            let event = self.buffer.drain(..end).collect::<Vec<_>>();
            self.buffer.drain(..delimiter_len);
            events.push(parse_event(&event)?);
        }
        Ok(events)
    }
}

fn event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
struct SseEvent {
    done: bool,
    generated_tokens: u64,
}

fn parse_event(bytes: &[u8]) -> Result<SseEvent, ()> {
    let text = str::from_utf8(bytes).map_err(|_| ())?;
    let mut data = String::new();
    for line in text.lines() {
        let Some(value) = line.strip_prefix("data:") else {
            continue;
        };
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(value.strip_prefix(' ').unwrap_or(value));
    }
    if data == "[DONE]" {
        return Ok(SseEvent {
            done: true,
            generated_tokens: 0,
        });
    }
    if data.is_empty() {
        return Ok(SseEvent::default());
    }
    let value: Value = serde_json::from_str(&data).map_err(|_| ())?;
    Ok(SseEvent {
        done: false,
        generated_tokens: generated_tokens(&value),
    })
}

fn generated_tokens(value: &Value) -> u64 {
    if let Some(tokens) = value["usage"]["completion_tokens"].as_u64() {
        return tokens;
    }
    value["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|choice| {
            [choice["delta"]["content"].as_str(), choice["text"].as_str()]
                .into_iter()
                .flatten()
                .any(|content| !content.is_empty())
        })
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_split_sse_frames_and_done() {
        let mut parser = SseParser::default();
        assert!(
            parser
                .push(&Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
                ))
                .is_ok()
        );
        let events = parser
            .push(&Bytes::from_static(b"\ndata: [DONE]\n\n"))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].generated_tokens, 1);
        assert!(events[1].done);
    }

    #[test]
    fn parser_rejects_an_oversized_unterminated_event() {
        let mut parser = SseParser::default();
        let bytes = Bytes::from(vec![b'x'; MAX_SSE_BUFFER_BYTES + 1]);
        assert!(parser.push(&bytes).is_err());
    }

    #[test]
    fn unspecified_public_listen_is_loopback_only() {
        let executor =
            CanaryExecutor::new("0.0.0.0:18080".parse().unwrap(), CanaryPolicy::default()).unwrap();
        assert_eq!(
            executor.endpoint(),
            "http://127.0.0.1:18080/v1/chat/completions"
        );
    }

    #[test]
    fn request_uses_chat_messages_payload() {
        let payload = canary_request_payload();
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"], CANARY_PROMPT);
        assert_eq!(payload["max_tokens"], CANARY_MAX_TOKENS);
        assert_eq!(payload["stream"], true);
        assert!(payload.get("prompt").is_none());
    }
}
