use crate::error::format_error_chain;
use crate::state::AppState;
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

const PROBE_BODY: &[u8] = br#"{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"Reply with exactly OK."}],"reasoning_effort":"none","temperature":0,"max_tokens":4,"stream":false}"#;

async fn probe_backend(
    name: &str,
    url: &str,
    client: &reqwest::Client,
    timeout: std::time::Duration,
) -> Result<u64, String> {
    let probe_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let start = Instant::now();

    let resp = client
        .post(&probe_url)
        .header("content-type", "application/json")
        .body(PROBE_BODY)
        .timeout(timeout)
        .send()
        .await;

    match resp {
        Ok(r) if r.status() == 200 => {
            let latency = start.elapsed().as_millis() as u64;
            info!(backend = %name, latency_ms = latency, "probe ok");
            Ok(latency)
        }
        Ok(r) => {
            warn!(backend = %name, status = %r.status(), "probe unhealthy");
            Err(format!("status {}", r.status()))
        }
        Err(e) => {
            let error = format_error_chain(&e);
            warn!(backend = %name, error = %error, "probe failed");
            Err(error)
        }
    }
}

pub async fn probe_loop(state: Arc<AppState>) {
    loop {
        tokio::time::sleep(state.probe_interval).await;

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
            let result = probe_backend(name, url, &state.client, state.probe_timeout).await;
            let mut states = state.states.lock().unwrap();
            if let Some(entry) = states.get_mut(name) {
                match result {
                    Ok(latency) => {
                        entry.healthy = true;
                        entry.average_latency_ms = latency;
                        entry.last_probe = Instant::now();
                    }
                    Err(_) => {
                        entry.healthy = false;
                    }
                }
            }
        }
    }
}
