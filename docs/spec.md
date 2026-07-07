# DS4 Smart Proxy

## Overview
DS4 Smart Proxy is an OpenAI-compatible reverse proxy written in Rust. It is deployed locally alongside OpenAI API clients and transparently forwards requests to one of multiple DS4 backends. It is not an HTTP forward proxy in the conventional networking sense.

Its purpose is to intelligently route OpenAI-compatible API requests between multiple standalone DwarfStar 4 (DS4) servers in a small homelab environment.

The primary design target is a two-node deployment where each Apple Silicon machine hosts both:

- a DS4 server
- local AI clients (Codex, Hermes Agent, OpenAI SDKs, etc.)

The proxy should always prefer the local DS4 instance whenever possible and transparently fall back to a remote DS4 instance when the local instance is busy or unavailable.

Clients must never know which backend actually processed a request.

---

## Goals

- OpenAI API compatible
- Rust implementation
- Extremely low overhead
- Streaming compatible
- Zero client configuration changes
- Automatic backend selection
- Health-aware routing
- Infrastructure agnostic
- Single executable
- Minimal dependencies

---

## Non-goals

The following are explicitly outside the scope of this project.

- Distributed inference
- Multi-GPU inference
- Request batching
- Authentication provider
- User management
- Billing
- GPU scheduling
- Kubernetes integration
- Service discovery

---

## Design Principles

The proxy must remain infrastructure agnostic.

- It must NOT depend on:
    - DNS implementation
    - mDNS / Bonjour
    - Cloud providers
    - Kubernetes
    - Docker
    - Prometheus
    - Redis

Backend discovery is entirely configuration-driven. Any backend reachable over HTTP(S) may participate.

---

## Target Environment

- Typical deployment: **Two Apple Silicon Macs.**
- Each machine runs:
    - DwarfStar 4
    - DS4 Smart Proxy
    - Codex / Hermes / OpenAI SDK clients
- Each machine behaves as both
    - client
    - inference server

There is no dedicated load balancer machine. Each client communicates only with its own local proxy.

---

## High-Level Architecture

```
          +--------------------------+
          |      Local Client        |
          |--------------------------|
          | Codex                    |
          | Hermes                   |
          | curl                     |
          | OpenAI SDK               |
          +------------+-------------+
                       |
                       |
                localhost:18080
                       |
                DS4 Smart Proxy
                       |
          +------------+------------+
          |                         |
          |                         |
      Local DS4               Remote DS4
```

The proxy always exposes a single OpenAI-compatible endpoint.

Clients never communicate directly with backend servers.

---

## Routing Policy

Priority order:

1. Local backend
2. Remote backend
3. Return HTTP 503

The proxy MUST always prefer the local backend whenever it is available.

---

## Backend Selection

Backend selection must NOT rely on:

- GPU utilization
- CPU utilization
- Unified memory usage

Reason:

DS4 serializes inference internally. The best scheduling signal is actual request occupancy.

---

## Backend State

Each backend maintains:

```rust
struct BackendState {
    healthy: bool,
    in_flight: usize,
    average_latency: Duration,
    last_probe: Instant,
}
```

---

## Busy Definition

Initially

```
busy = in_flight >= max_in_flight
```

Default

```
max_in_flight = 1
```

Future versions may allow higher concurrency.

---

## Health Monitoring

Every backend is periodically probed.

### Probe interval

```
5 seconds
```

### Probe timeout

```
5 seconds
```

### Probe request

`POST /v1/chat/completions`

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

### Healthy if

- HTTP 200
- response received
- timeout not exceeded

---

## Request Routing

```
Incoming request
  ↓
Choose backend
  ↓
Increment: in_flight += 1
  ↓
Forward request
  ↓
Stream response directly
  ↓
Finally: in_flight -= 1
```

- The decrement MUST happen even when:
    - timeout
    - cancellation
    - panic
    - broken connection

**RAII / Drop guard is strongly recommended.**

---

## Retry Policy

- Connection failure: **Retry another healthy backend.**
- HTTP 5xx: **Retry once on another backend.**
- HTTP 4xx: **Never retry.**
- Streaming already started: **Never retry.**

---

## OpenAI Compatibility

The proxy MUST transparently support every OpenAI endpoint. Known endpoints include:

```
/v1/chat/completions
/v1/responses
/v1/models
```

Unknown paths should also be proxied. The proxy must never inspect request bodies except when required for routing.

---

## Streaming

Streaming is a first-class requirement.

- Requirements
    - no buffering
    - no event aggregation
    - preserve chunk ordering
    - preserve chunk timing
    - support Server Sent Events
    - support HTTP chunked transfer

The proxy should behave as transparently as possible.

---

## Configuration

Configuration format

```
TOML
```

### Example

```toml
listen = "127.0.0.1:18080"

self = "macbook"
tls_accept_invalid_certs = false

probe_interval = "5s"
probe_timeout = "5s"

[[backend]]
name = "macbook"
url = "http://127.0.0.1:8000"
max_in_flight = 1

[[backend]]
name = "macstudio"
url = "https://macstudio.example.internal"
max_in_flight = 1
```

The `self` field determines which backend should be preferred. The same binary must run on every machine. Only the configuration file changes.

---

## Health Endpoints

The proxy exposes

```
GET /healthz
GET /backends
GET /metrics
```

### Example

```json
[
  {
    "name":"macbook",
    "healthy":true,
    "busy":false,
    "in_flight":0,
    "latency_ms":84
  },
  {
    "name":"macstudio",
    "healthy":true,
    "busy":true,
    "in_flight":1,
    "latency_ms":76
  }
]
```

---

## Logging

Use structured logging. 

Recommended crate

```
tracing
```

Every request should emit

- request id
- selected backend
- latency
- response status
- retry count

---

## Recommended Crates

- tokio
- axum
- hyper
- hyper-util
- tower
- tower-http
- reqwest
- serde
- serde_json
- dashmap
- arc-swap
- tracing
- tracing-subscriber
- anyhow
- thiserror
- toml

---

## Future Extensions

Possible future improvements include

- Weighted routing
- Latency-aware routing
- Sticky sessions
- Request queue
- Backend draining
- Dynamic max_in_flight
- Adaptive routing
- Circuit breaker
- Prometheus exporter
- Unified memory monitoring
- Apple GPU utilization
- More than two backends

These features should not complicate the initial implementation.
