# Hermes 600-second stall and delayed failover

## Status

Investigation and implementation task.

## Context

Hermes Agent sometimes reports a message similar to:

```text
⏳ waiting on deepseek-v4-flash — 542s with no output yet
(provider may be slow or overloaded, or the model is thinking)
```

In the observed two-node deployment, one DS4 node remains at 0% GPU usage while
the other node handles the request. Around 600 seconds after the request starts,
the previously idle node begins working and Hermes starts receiving output.

The leading hypothesis is:

1. Hermes sends one request to DS4 Smart Proxy.
2. The proxy selects one backend.
3. The selected backend accepts the HTTP request but does not produce a response
   body chunk for about 600 seconds.
4. A timeout or disconnect outside the currently visible proxy logic occurs.
5. The request is retried and routed to the other backend.
6. The second backend produces output successfully.

The exact layer that owns the approximately 600-second timeout is not yet known.

## Findings in the current implementation

### No 600-second timeout exists in the proxy configuration

The shared `reqwest::Client` currently has only a 10-second connection timeout.
The real proxied request has no total request timeout, response-header timeout,
first-body-byte timeout, or stream-idle timeout.

Therefore, the proxy cannot currently attribute a 600-second transition to one
of its own timers. Runtime configuration, Hermes, DS4, Caddy or another network
layer must be inspected before changing Hermes timeout values.

### Request completion is logged before a streaming response completes

`build_response` logs `request completed` immediately after response headers are
received. For SSE, this happens before the first body chunk and before the stream
finishes. The logged latency is therefore time-to-headers rather than total
request duration.

This prevents the current logs from distinguishing:

- upstream connection time;
- time to response headers;
- time to first body chunk;
- stream idle time;
- total request duration.

### Successful headers mark a backend healthy too early

An HTTP success status marks the backend healthy before the first streaming body
chunk arrives. A DS4 process whose HTTP frontend responds but whose inference
worker is stalled may therefore be treated as healthy.

### Heartbeat cannot prove inference availability

The periodic `GET /v1/models` heartbeat correctly avoids GPU inference, but it
only proves HTTP reachability. It cannot detect an inference worker that is hung
or occupied by an abandoned request.

### A failed backend is re-admitted too quickly

`heartbeat_after_failure` makes a backend eligible again after the next successful
heartbeat. Because `/v1/models` does not exercise inference, an inference-stalled
backend can re-enter routing after one heartbeat interval.

### Active probes add inference work to every real request

The current implementation executes a short completion probe before every real
request. This consumes the single DS4 inference worker and GPU capacity. It should
be reserved for recovery from an uncertain or suspect state, not used on the
normal routing path.

### Retry count is effectively two attempts, but is implicit

The current code attempts one selected backend and then one different backend for
connection errors or HTTP 5xx responses. This already excludes the first backend,
but the policy is not represented by an explicit `max_attempts = 2` setting and
does not cover first-body-byte or stream-idle timeouts.

### Per-proxy `in_flight` state is not globally authoritative

Each Mac runs its own proxy and maintains independent backend state. A request
sent through one proxy is not reflected in the other proxy's `in_flight` counter.
This can cause both proxies to select the same DS4 node concurrently. This is a
separate scheduling limitation and must be documented when interpreting logs.

## Investigation plan

### 1. Capture runtime configuration

Record the exact production configuration and service definitions on both Macs:

- DS4 Smart Proxy configuration;
- Hermes provider timeout and retry settings;
- DS4 server arguments;
- Caddy or other reverse-proxy timeouts;
- LaunchAgent environment variables.

Do not assume the repository defaults match the running binaries.

### 2. Correlate one request across all layers

Use a stable request ID. Accept `x-request-id` from the client when present;
otherwise generate one. Forward it upstream as `x-request-id` and include it in
every proxy log event.

For one reproduced stall, correlate:

- Hermes logs;
- DS4 Smart Proxy logs on the client machine;
- DS4 logs on both backend machines;
- Caddy access logs, if present;
- GPU activity timestamps.

### 3. Identify the owner of the 600-second boundary

Look for the first event around 600 seconds:

- client cancellation;
- `ReadTimeout` or stale-call detection in Hermes;
- upstream body read error in the proxy;
- HTTP 502/504 from another proxy;
- TCP FIN/RST;
- DS4 request cancellation or completion;
- a new request ID or a second attempt using the same logical Hermes task.

The current source code has no 600-second proxy timer. This must be treated as a
key finding unless the deployed binary differs from the current commit.

## Required structured log events

Every event must include `request_id`, `attempt`, and `backend` where applicable.

Required events and fields:

| Event | Required fields |
|---|---|
| `request_received` | method, path |
| `backend_selected` | selected_backend, routing_reason, in_flight_before |
| `upstream_connect_started` | backend |
| `upstream_connected` | connect_elapsed_ms |
| `response_headers_received` | status, headers_elapsed_ms |
| `first_body_chunk` | first_body_chunk_elapsed_ms |
| `stream_idle_timeout` | idle_elapsed_ms |
| `first_body_byte_timeout` | elapsed_ms |
| `retry_started` | from_backend, to_backend, retry_reason |
| `request_finished` | status, total_elapsed_ms, bytes_forwarded, outcome |
| `client_disconnected` | elapsed_ms, stream_started |
| `backend_state_changed` | old_state, new_state, reason, cooldown_until |

Do not use `request completed` when only response headers have arrived.

## Timeout model

The following timeout phases must be separate:

```toml
connect_timeout = "5s"
response_headers_timeout = "60s"
first_body_byte_timeout = "300s"
stream_idle_timeout = "300s"
max_attempts = 2
failure_cooldown = "300s"
```

These values are initial defaults for investigation, not final tuning. A large
uncached context may legitimately require a long prefill, so first-body-byte
timeout must be configurable and validated against measured DS4 prefill times.

Definitions:

- `connect_timeout`: TCP/TLS connection establishment.
- `response_headers_timeout`: request sent until upstream response headers.
- `first_body_byte_timeout`: response headers received until the first body chunk.
- `stream_idle_timeout`: maximum gap between body chunks after streaming starts.

For non-streaming responses, body completion still needs a bounded or explicitly
unbounded policy. It must not accidentally inherit the stream-idle policy.

## Retry policy

1. `max_attempts` defaults to 2.
2. Attempt 2 must always use a backend different from attempt 1.
3. Retry before any downstream response body has been sent only.
4. Never retry HTTP 4xx.
5. Retry connection failures and clearly pre-response HTTP 5xx failures.
6. Treat first-body-byte timeout as an investigation-gated retry condition.
7. Never retry a stream after the first downstream body chunk.
8. Log `retry_started` before the second attempt.

Important DS4 caveat: disconnecting a timed-out request may not cancel backend
prefill immediately. Retrying on the other node can temporarily make both GPUs
process the same logical request. The implementation must expose this risk in
logs and must not perform more than one failover attempt.

## Backend state model

Replace the overloaded `healthy: bool` interpretation with explicit state:

```rust
enum BackendStatus {
    Unknown,
    Available,
    Busy,
    Suspect,
    Offline,
    Cooldown,
}
```

Suggested transitions:

- heartbeat success: `Offline -> Unknown`, not directly `Available`;
- heartbeat failure: any non-busy state -> `Offline`;
- successful real inference first chunk: `Unknown/Suspect -> Available`;
- connection failure: -> `Suspect` or `Offline` depending on error;
- first-body-byte or stream-idle timeout: -> `Cooldown`;
- cooldown expiry plus heartbeat success: -> `Unknown`;
- successful recovery probe: `Unknown -> Available`.

`/v1/models` must never clear `Suspect` or `Cooldown` by itself.

## Active probe policy

Remove the active inference probe from the normal request path.

Use it only when all of the following are true:

- a real client request is waiting;
- the candidate is `Unknown` or `Suspect` after cooldown;
- no recent real inference success is available;
- the probe is rate-limited;
- the backend has no known in-flight request.

Normal routing should use heartbeat reachability, backend status, occupancy and
recent real-request success without adding a separate GPU-consuming request.

## Implementation candidates

### Phase 1: observability only

- Add the required lifecycle log events.
- Preserve or forward `x-request-id`.
- Record response-header, first-chunk and total timings separately.
- Record bytes forwarded and client disconnects.
- Add the running configuration values to the startup log.

This phase should not change failover behavior. Use it to locate the current
approximately 600-second timeout owner.

### Phase 2: explicit timeout phases

- Add configuration keys for the four timeout phases.
- Apply connection and response-header timeouts before constructing the Axum
  response.
- Wrap the upstream body stream with first-body-byte and idle timers.
- Return an explicit 504 if no downstream body has started and the applicable
  timeout expires.
- Do not attempt to synthesize a second response after downstream streaming starts.

### Phase 3: cooldown and bounded failover

- Add explicit backend status and `cooldown_until`.
- Add `max_attempts`, default 2.
- Ensure retry selection excludes every previously attempted backend.
- Add cooldown after first-byte or stream-idle timeout.
- Prevent heartbeat success from immediately clearing cooldown.

### Phase 4: scheduling correctness across two proxies

Evaluate one of these designs separately:

1. Route remote traffic through the remote machine's proxy so that each proxy is
   authoritative for its local DS4 occupancy.
2. Add a small backend-local lease endpoint.
3. Accept occasional distributed `in_flight` races and document the limitation.

Do not introduce Redis or another central dependency without a demonstrated need.

## Acceptance criteria

- A single request produces a traceable lifecycle from receipt to final stream
  completion or failure.
- Logs identify whether a 600-second stall occurs before headers, before the first
  body chunk, or between body chunks.
- `request_finished` is emitted only after the body stream ends or is dropped.
- First-body-byte and stream-idle timeouts are separately configurable.
- At most two backends are attempted, and the second differs from the first.
- A timeout places the failed backend into cooldown/suspect state.
- `/v1/models` success does not immediately clear inference-related suspicion.
- Periodic health checks never execute model inference.
- Active inference probes do not run before every normal client request.
- Hermes timeout settings are not changed until proxy logs locate the current
  600-second boundary.

