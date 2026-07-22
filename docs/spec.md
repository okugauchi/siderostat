# DS4 Smart Proxy

## Overview

DS4 Smart Proxy is a lightweight OpenAI-compatible reverse proxy written in Rust.

Its purpose is to intelligently route OpenAI-compatible API requests between multiple standalone DwarfStar 4 (DS4) servers in a small homelab environment.

The primary deployment target is a small number of Apple Silicon Macs, where each machine acts as both:

- an OpenAI API client (Codex, Hermes Agent, OpenAI SDKs, etc.)
- a DS4 inference server

The proxy always prefers the local DS4 instance whenever possible and transparently falls back to a remote DS4 instance when the local instance is unavailable or already processing another request.

Clients must never know which backend actually processed the request.

---

# Goals

- OpenAI-compatible reverse proxy
- Rust implementation
- Very low overhead
- Streaming-first design
- Zero client configuration changes
- Automatic backend selection
- Local-first routing
- Infrastructure agnostic
- Single executable
- Minimal dependencies

---

# Non-goals

The following features are intentionally out of scope.

- Distributed inference
- Multi-GPU inference
- Request batching
- Authentication
- User management
- Billing
- GPU scheduling
- Kubernetes integration
- Service discovery

---

# Design Principles

The proxy must remain infrastructure agnostic.

It must not depend on:

- DNS implementation
- mDNS / Bonjour
- Reverse proxies
- Cloud providers
- Kubernetes
- Docker
- Redis
- Prometheus

Backend discovery is entirely configuration-driven.

Any backend reachable over HTTP(S) may participate.

---

# Target Environment

Typical deployment consists of two or more Apple Silicon Macs.

Each machine runs:

- DwarfStar 4
- DS4 Smart Proxy
- Codex / Hermes / OpenAI SDK clients

Each machine behaves simultaneously as:

- API client
- inference server

Every client communicates only with its own local proxy.

---

# High-Level Architecture

```
        +---------------------------+
        | Local AI Client           |
        |---------------------------|
        | Codex                     |
        | Hermes                    |
        | curl                      |
        | OpenAI SDK                |
        +-------------+-------------+
                      |
                localhost:18080
                      |
             DS4 Smart Proxy
                      |
          +-----------+-----------+
          |                       |
          |                       |
      Local DS4              Remote DS4
```

Clients always communicate with the local proxy.

The proxy is responsible for backend selection.

---

# Routing Policy

Backend selection priority:

1. Local backend
2. Remote backend
3. Return HTTP 503 (or optionally queue)

The local backend should always be preferred whenever it is available.

---

# Backend Selection Policy

Backend selection MUST NOT rely on:

- GPU utilization
- CPU utilization
- Unified memory utilization

Reason:

DS4 already serializes inference internally.

The most reliable scheduling signal is actual request occupancy.

---

# Backend State

Each backend maintains runtime state.

```rust
struct BackendState {
    healthy: bool,
    in_flight: usize,
    average_latency: Duration,
    last_heartbeat: Instant,
    last_failure: Option<Instant>,
}
```

---

# Busy Definition

Initially:

```
busy = in_flight >= max_in_flight
```

Default:

```
max_in_flight = 1
```

The implementation should allow this to become configurable later.

---

# Health Monitoring

## Heartbeat

Backends are periodically monitored using a lightweight heartbeat.

Heartbeat endpoint:

```
GET /v1/models
```

The heartbeat path should be configurable.
The default is `/v1/models`.

Purpose:

- verify HTTP connectivity
- verify that the backend process is reachable
- avoid unnecessary inference

Heartbeat interval:

```
5 seconds
```

Heartbeat timeout:

```
3 seconds
```

Heartbeat MUST NOT execute inference.

A successful heartbeat does not by itself prove that inference can be served.
It only means the backend may be considered as a routing candidate.

---

## Active Probe

Inference probes are expensive and use the same DS4 worker and GPU capacity as a
real request.

Therefore:

**Inference probes MUST NOT execute periodically or before every normal request.**

An active inference probe may execute only when:

- a real client request has arrived
- the backend is a routing candidate
- the backend state is uncertain or recovering from a recent failure
- no recent successful real inference is available
- the probe is rate-limited

Normal routing should use heartbeat reachability, request occupancy and recent
real-request success without adding a separate inference request.

Probe success updates:

```
healthy = true
```

Probe failure updates:

```
last_failure = now
healthy = false
```

Probe request:

```
POST /v1/chat/completions
```

```json
{
  "model": "deepseek-v4-flash",
  "messages": [
    {
      "role": "user",
      "content": "Reply with exactly OK."
    }
  ],
  "reasoning_effort": "none",
  "temperature": 0,
  "max_tokens": 4,
  "stream": false
}
```

Requirements:

- timeout ≤ 3 seconds
- never executed on a timer
- execute at most once per uncertain routing candidate per client request
- never execute on the normal path for an already available backend

---

# Backend Selection Algorithm

Preferred routing order:

```
Local backend

if
    heartbeat-reachable or healthy
&&  in_flight < max_in_flight
&&  no unresolved cooldown or suspect state

↓

Remote backend

if
    heartbeat-reachable or healthy
&&  in_flight < max_in_flight
&&  no unresolved cooldown or suspect state

↓

503 Service Unavailable
(or optional request queue)
```

An uncertain candidate may require an active probe before it becomes available.
A recent successful real request is a stronger inference-health signal than a
heartbeat or synthetic probe.

---

# Request Lifecycle

Upon backend selection:

```
in_flight += 1
```

Forward request immediately.

Responses must be streamed directly.

After completion:

```
in_flight -= 1
```

Decrement MUST happen regardless of:

- timeout
- cancellation
- panic
- broken TCP connection
- client disconnect

RAII / Drop guard is strongly recommended.

---

# Retry Policy

Connection failure:

- retry another eligible backend after its active probe succeeds

HTTP 5xx:

- retry once on another eligible backend after its active probe succeeds

HTTP 4xx:

- never retry

Streaming already started:

- never retry

---

# OpenAI Compatibility

The proxy must transparently forward every OpenAI-compatible endpoint.

Known endpoints include:

```
/v1/chat/completions
/v1/responses
/v1/models
```

Unknown paths should also be proxied.

The proxy should avoid inspecting request bodies unless routing requires it.

---

# Streaming

Streaming is a first-class requirement.

Requirements:

- do not buffer response bodies
- preserve ordering
- preserve timing
- preserve streaming semantics
- support SSE
- support chunked transfer encoding

The proxy should behave as transparently as possible.
Exact HTTP chunk boundaries are not guaranteed by the proxy implementation.

---

# Configuration

Configuration format:

```
TOML
```

Example:

```toml
listen = "127.0.0.1:18080"

self_name = "macbook"

heartbeat_interval = "5s"
heartbeat_timeout = "2s"
heartbeat_path = "/v1/models"

active_probe_timeout = "3s"
log_timezone = "Asia/Tokyo"

[[backends]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backends]]
name = "macstudio"
url = "https://macstudio.local"
max_in_flight = 1
```

The same executable should run on every machine.

Only the configuration file changes.

The canonical backend array key is `[[backends]]`.
Implementations may accept the legacy `[[backend]]` spelling for migration, but new configurations should use `[[backends]]`.

---

# Health Endpoints

The proxy exposes:

```
GET /healthz
GET /backends
GET /metrics
```

`/metrics` may return a lightweight implementation-defined text format.
Prometheus-compatible metrics are a future extension, not a requirement for the initial implementation.

Example response:

```json
[
  {
    "name":"macbook",
    "healthy":true,
    "busy":false,
    "in_flight":0,
    "latency_ms":83,
    "last_heartbeat":"2026-07-05T12:34:56Z",
    "last_failure":null
  },
  {
    "name":"macstudio",
    "healthy":true,
    "busy":true,
    "in_flight":1,
    "latency_ms":78,
    "last_heartbeat":"2026-07-05T12:34:48Z",
    "last_failure":null
  }
]
```

---

# Logging

Use structured logging.

Recommended crate:

```
tracing
```

Each request should record:

- request ID
- selected backend
- routing reason
- latency
- response status
- retry count

Heartbeat failures should be logged only when backend state changes, to avoid excessive log noise.

Log timestamps default to GMT when no timezone is configured. Operators may set
`log_timezone` in the TOML configuration to a fixed timezone name or offset such
as `Asia/Tokyo`, `JST`, `UTC`, `GMT`, or `+09:00`.

---

# Recommended Rust Crates

- tokio
- axum
- hyper
- hyper-util
- tower
- tower-http
- reqwest
- serde
- serde_json
- toml
- dashmap
- arc-swap
- tracing
- tracing-subscriber
- anyhow
- thiserror
- clap
- uuid

---

# Future Extensions

Possible future improvements include:

- Request queue
- Weighted routing
- Latency-aware routing
- Sticky sessions
- Backend draining
- Adaptive max_in_flight
- Circuit breaker
- Prometheus exporter
- Unified memory awareness
- Apple GPU utilization
- More than two backends

These features should not complicate the initial implementation.

---

# Implementation Guidelines

The implementation should prioritize:

1. Correctness
2. Simplicity
3. Predictable routing
4. Low runtime overhead
5. Streaming transparency

Avoid premature optimization.

A lightweight and maintainable implementation is preferred over a feature-rich design.

The proxy should remain transparent, reliable, and easy to reason about.
