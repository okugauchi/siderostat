use crate::error::format_error_chain;
use crate::state::AppState;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tracing::{debug, info, warn};

const PROBE_BODY: &[u8] = br#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Reply with exactly OK."}],"reasoning_effort":"none","temperature":0,"max_tokens":4,"stream":false}"#;

async fn heartbeat_backend(
    name: &str,
    url: &str,
    path: &str,
    client: &reqwest::Client,
    timeout: std::time::Duration,
) -> Result<u64, String> {
    let heartbeat_url = format!(
        "{}{}",
        url.trim_end_matches('/'),
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    );
    let start = Instant::now();

    let resp = client.get(&heartbeat_url).timeout(timeout).send().await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let latency = start.elapsed().as_millis() as u64;
            debug!(backend = %name, latency_ms = latency, "heartbeat ok");
            Ok(latency)
        }
        Ok(r) => Err(format!("status {}", r.status())),
        Err(e) => {
            let error = format_error_chain(&e);
            Err(error)
        }
    }
}

pub async fn active_probe_backend(state: &Arc<AppState>, name: &str, url: &str) -> bool {
    let probe_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let start = Instant::now();

    let result = state
        .client
        .post(&probe_url)
        .header("content-type", "application/json")
        .body(PROBE_BODY)
        .timeout(state.active_probe_timeout)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            let latency = start.elapsed().as_millis() as u64;
            if let Ok(mut states) = state.states.lock()
                && let Some(entry) = states.get_mut(name)
            {
                entry.healthy = true;
                entry.average_latency_ms = latency;
            }
            info!(backend = %name, latency_ms = latency, "active probe ok");
            true
        }
        Ok(resp) => {
            mark_failure(state, name);
            warn!(backend = %name, status = %resp.status(), "active probe failed");
            false
        }
        Err(error) => {
            mark_failure(state, name);
            let error = format_error_chain(&error);
            warn!(backend = %name, error = %error, "active probe failed");
            false
        }
    }
}

pub fn mark_failure(state: &Arc<AppState>, name: &str) {
    if let Ok(mut states) = state.states.lock()
        && let Some(entry) = states.get_mut(name)
    {
        entry.healthy = false;
        entry.last_failure = Some(SystemTime::now());
    }
}

pub fn mark_success(state: &Arc<AppState>, name: &str, latency_ms: u64) {
    if let Ok(mut states) = state.states.lock()
        && let Some(entry) = states.get_mut(name)
    {
        entry.healthy = true;
        entry.average_latency_ms = latency_ms;
    }
}

pub async fn heartbeat_loop(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(state.heartbeat_interval).await;

        // Collect backend list under lock
        let backends: Vec<(String, String)> = {
            let states = state.states.lock().unwrap();
            state
                .backends
                .iter()
                .filter_map(|b| states.get(&b.name).map(|_| (b.name.clone(), b.url.clone())))
                .collect()
        };

        for (name, url) in &backends {
            let result = heartbeat_backend(
                name,
                url,
                &state.heartbeat_path,
                &state.client,
                state.heartbeat_timeout,
            )
            .await;
            let mut states = state.states.lock().unwrap();
            if let Some(entry) = states.get_mut(name) {
                match result {
                    Ok(latency) => {
                        entry.average_latency_ms = latency;
                        entry.last_heartbeat = Some(SystemTime::now());
                    }
                    Err(error) => {
                        let was_available = entry.healthy
                            || match (entry.last_heartbeat, entry.last_failure) {
                                (Some(last_heartbeat), Some(last_failure)) => {
                                    last_heartbeat > last_failure
                                }
                                (Some(_), None) => true,
                                _ => false,
                            };
                        entry.healthy = false;
                        entry.last_failure = Some(SystemTime::now());
                        if was_available {
                            warn!(backend = %name, error = %error, "heartbeat failed");
                        } else {
                            debug!(backend = %name, error = %error, "heartbeat still failing");
                        }
                    }
                }
            }
        }
    }
}
