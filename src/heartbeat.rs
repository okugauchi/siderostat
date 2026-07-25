use crate::{
    backend::{BackendHealth, BackendRegistry, BackendRuntime},
    config::{ActiveProbeConfig, CooldownConfig, HeartbeatConfig},
    error::{FailureKind, format_error_chain},
    metrics::Metrics,
};
use reqwest::StatusCode;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tracing::{debug, info, warn};

pub fn spawn_heartbeat_tasks(
    registry: Arc<BackendRegistry>,
    heartbeat: HeartbeatConfig,
    cooldown: CooldownConfig,
    metrics: Arc<Metrics>,
) {
    for (index, backend) in registry.all().iter().enumerate() {
        let backend = backend.clone();
        let heartbeat = heartbeat.clone();
        let cooldown = cooldown.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            heartbeat_loop(backend, heartbeat, cooldown, metrics, index as u64).await;
        });
    }
}

async fn heartbeat_loop(
    backend: Arc<BackendRuntime>,
    config: HeartbeatConfig,
    cooldown: CooldownConfig,
    metrics: Arc<Metrics>,
    seed: u64,
) {
    tokio::time::sleep(Duration::from_millis(seed.saturating_mul(50))).await;
    loop {
        let Some(_permit) = backend.try_acquire(false) else {
            tokio::time::sleep(jittered(
                config.interval,
                config.jitter_ratio,
                seed.wrapping_add(unix_seconds()),
            ))
            .await;
            continue;
        };
        let before = backend.snapshot().health;
        let url = match backend.endpoint(&backend.config.heartbeat_path) {
            Ok(url) => url,
            Err(error) => {
                warn!(backend = %backend.config.id, error = %error, "invalid heartbeat URL");
                return;
            }
        };
        let result = backend.client.get(url).timeout(config.timeout).send().await;
        match result {
            Ok(response) if response.status().is_success() => {
                let changed = backend.record_heartbeat_success();
                metrics.increment(
                    "ds4_proxy_heartbeat_total",
                    &[("backend", &backend.config.id), ("result", "success")],
                );
                if changed {
                    info!(
                        backend = %backend.config.id,
                        from = before.as_str(),
                        to = backend.snapshot().health.as_str(),
                        "backend health changed"
                    );
                } else {
                    debug!(backend = %backend.config.id, "heartbeat ok");
                }
            }
            Ok(response) => {
                heartbeat_failed(
                    &backend,
                    &config,
                    &cooldown,
                    &metrics,
                    before,
                    &format!("status {}", response.status()),
                );
            }
            Err(error) => {
                heartbeat_failed(
                    &backend,
                    &config,
                    &cooldown,
                    &metrics,
                    before,
                    &format_error_chain(&error),
                );
            }
        }
        drop(_permit);
        tokio::time::sleep(jittered(
            config.interval,
            config.jitter_ratio,
            seed.wrapping_add(unix_seconds()),
        ))
        .await;
    }
}

fn heartbeat_failed(
    backend: &BackendRuntime,
    config: &HeartbeatConfig,
    cooldown: &CooldownConfig,
    metrics: &Metrics,
    before: BackendHealth,
    error: &str,
) {
    let changed = backend.record_failure(
        FailureKind::Heartbeat,
        config.failure_threshold,
        cooldown.duration,
    );
    metrics.increment(
        "ds4_proxy_heartbeat_total",
        &[("backend", &backend.config.id), ("result", "failure")],
    );
    if changed {
        warn!(
            backend = %backend.config.id,
            from = before.as_str(),
            to = backend.snapshot().health.as_str(),
            error = %error,
            "backend health changed"
        );
    } else {
        debug!(backend = %backend.config.id, error = %error, "heartbeat failed");
    }
}

pub async fn try_active_probe(
    registry: &BackendRegistry,
    config: &ActiveProbeConfig,
    cooldown: &CooldownConfig,
    metrics: &Metrics,
) -> bool {
    if !config.enabled || registry.all().iter().any(|backend| backend.is_available()) {
        return false;
    }
    for backend in registry
        .all()
        .iter()
        .filter(|backend| backend.is_probe_candidate())
    {
        let Some(_probe_guard) = backend.start_probe(config.minimum_interval) else {
            continue;
        };
        let Some(_permit) = backend.try_acquire(true) else {
            continue;
        };
        let url = match backend.endpoint("/v1/chat/completions") {
            Ok(url) => url,
            Err(_) => continue,
        };
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(config.timeout, async {
            let response = backend
                .client
                .post(url)
                .json(&json!({
                    "model": config.model,
                    "messages": [{"role": "user", "content": "Reply with exactly OK."}],
                    "reasoning_effort": "none",
                    "temperature": 0,
                    "max_tokens": 4,
                    "stream": false
                }))
                .send()
                .await?;
            let status = response.status();
            let _ = response.bytes().await?;
            Ok::<StatusCode, reqwest::Error>(status)
        })
        .await;
        match response {
            Ok(Ok(status)) if status.is_success() => {
                backend.record_success(started.elapsed());
                metrics.increment(
                    "ds4_proxy_active_probe_total",
                    &[("backend", &backend.config.id), ("result", "success")],
                );
                info!(backend = %backend.config.id, "active probe restored backend");
                return true;
            }
            _ => {
                backend.record_failure(
                    FailureKind::Protocol,
                    cooldown.consecutive_failure_threshold,
                    cooldown.duration,
                );
                metrics.increment(
                    "ds4_proxy_active_probe_total",
                    &[("backend", &backend.config.id), ("result", "failure")],
                );
            }
        }
    }
    false
}

fn jittered(duration: Duration, ratio: f64, seed: u64) -> Duration {
    if ratio == 0.0 {
        return duration;
    }
    let mixed = seed.wrapping_mul(6_364_136_223_846_793_005).rotate_left(17);
    let unit = (mixed as f64 / u64::MAX as f64) * 2.0 - 1.0;
    duration.mul_f64((1.0 + unit * ratio).max(0.01))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
