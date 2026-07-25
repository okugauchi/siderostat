use crate::{
    config::{BackendConfig, Config},
    error::FailureKind,
};
use reqwest::Client;
use std::{
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendHealth {
    Unknown,
    Alive,
    Suspect,
    Offline,
    Cooldown,
    Disabled,
}

impl BackendHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Alive => "alive",
            Self::Suspect => "suspect",
            Self::Offline => "offline",
            Self::Cooldown => "cooldown",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendState {
    pub health: BackendHealth,
    pub ewma_latency_ms: Option<f64>,
    pub last_heartbeat_at: Option<SystemTime>,
    pub last_heartbeat_ok_at: Option<SystemTime>,
    pub last_success_at: Option<SystemTime>,
    pub last_failure_at: Option<SystemTime>,
    pub last_failure_kind: Option<FailureKind>,
    pub cooldown_until: Option<Instant>,
    pub consecutive_failures: u32,
}

pub struct BackendRuntime {
    pub config: BackendConfig,
    state: RwLock<BackendState>,
    inference_slots: Arc<Semaphore>,
    non_inference_slots: Arc<Semaphore>,
    probe_lock: Mutex<ProbeState>,
    pub client: Client,
}

#[derive(Debug, Default)]
struct ProbeState {
    running: bool,
    last_started: Option<Instant>,
}

pub struct ProbeGuard {
    backend: Arc<BackendRuntime>,
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.backend.probe_lock.lock() {
            state.running = false;
        }
    }
}

impl BackendRuntime {
    fn new(config: BackendConfig, connect_timeout: Duration) -> anyhow::Result<Self> {
        let health = if config.enabled {
            BackendHealth::Unknown
        } else {
            BackendHealth::Disabled
        };
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .danger_accept_invalid_certs(config.danger_accept_invalid_certs)
            .build()?;
        Ok(Self {
            inference_slots: Arc::new(Semaphore::new(config.max_in_flight)),
            non_inference_slots: Arc::new(Semaphore::new(8)),
            state: RwLock::new(BackendState {
                health,
                ewma_latency_ms: None,
                last_heartbeat_at: None,
                last_heartbeat_ok_at: None,
                last_success_at: None,
                last_failure_at: None,
                last_failure_kind: None,
                cooldown_until: None,
                consecutive_failures: 0,
            }),
            probe_lock: Mutex::new(ProbeState::default()),
            client,
            config,
        })
    }

    pub fn snapshot(&self) -> BackendState {
        self.refresh_cooldown();
        self.state.read().expect("backend state poisoned").clone()
    }

    pub fn endpoint(&self, path: &str) -> Result<Url, url::ParseError> {
        let mut base = self.config.url.clone();
        if !base.path().ends_with('/') {
            let normalized = format!("{}/", base.path());
            base.set_path(&normalized);
        }
        base.join(path.trim_start_matches('/'))
    }

    pub fn in_flight(&self) -> usize {
        self.config
            .max_in_flight
            .saturating_sub(self.inference_slots.available_permits())
    }

    pub fn load_ratio(&self) -> f64 {
        self.in_flight() as f64 / self.config.max_in_flight as f64
    }

    pub fn is_available(&self) -> bool {
        self.refresh_cooldown();
        self.is_alive() && self.inference_slots.available_permits() > 0
    }

    pub fn is_alive(&self) -> bool {
        self.refresh_cooldown();
        self.state
            .read()
            .is_ok_and(|state| state.health == BackendHealth::Alive)
    }

    pub fn is_probe_candidate(&self) -> bool {
        self.refresh_cooldown();
        self.state.read().is_ok_and(|state| {
            matches!(
                state.health,
                BackendHealth::Unknown | BackendHealth::Suspect
            )
        }) && self.inference_slots.available_permits() > 0
    }

    pub fn try_acquire(self: &Arc<Self>, inference: bool) -> Option<OwnedSemaphorePermit> {
        let semaphore = if inference {
            self.inference_slots.clone()
        } else {
            self.non_inference_slots.clone()
        };
        semaphore.try_acquire_owned().ok()
    }

    pub async fn acquire_affinity_slot(
        self: &Arc<Self>,
        grace: Duration,
    ) -> Option<OwnedSemaphorePermit> {
        tokio::time::timeout(grace, self.inference_slots.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
    }

    pub fn record_success(&self, latency: Duration) -> bool {
        let mut state = self.state.write().expect("backend state poisoned");
        let changed = state.health != BackendHealth::Alive;
        state.health = BackendHealth::Alive;
        state.last_success_at = Some(SystemTime::now());
        state.consecutive_failures = 0;
        state.cooldown_until = None;
        let value = latency.as_secs_f64() * 1_000.0;
        state.ewma_latency_ms = Some(match state.ewma_latency_ms {
            Some(previous) => 0.2 * value + 0.8 * previous,
            None => value,
        });
        changed
    }

    pub fn record_heartbeat_success(&self) -> bool {
        let mut state = self.state.write().expect("backend state poisoned");
        let previous = state.health;
        let now = SystemTime::now();
        state.last_heartbeat_at = Some(now);
        state.last_heartbeat_ok_at = Some(now);
        state.consecutive_failures = 0;
        state.health = match state.health {
            BackendHealth::Unknown => BackendHealth::Alive,
            BackendHealth::Offline => BackendHealth::Suspect,
            BackendHealth::Cooldown | BackendHealth::Suspect => state.health,
            BackendHealth::Disabled => BackendHealth::Disabled,
            BackendHealth::Alive => BackendHealth::Alive,
        };
        previous != state.health
    }

    pub fn record_failure(&self, kind: FailureKind, threshold: u32, cooldown: Duration) -> bool {
        if kind == FailureKind::ClientCancelled {
            return false;
        }
        let mut state = self.state.write().expect("backend state poisoned");
        let previous = state.health;
        state.last_failure_at = Some(SystemTime::now());
        state.last_failure_kind = Some(kind);
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);

        if kind == FailureKind::Heartbeat {
            if state.consecutive_failures >= threshold {
                state.health = BackendHealth::Offline;
                state.cooldown_until = None;
            }
        } else if kind == FailureKind::FirstByteTimeout || state.consecutive_failures >= threshold {
            state.health = BackendHealth::Cooldown;
            state.cooldown_until = Some(Instant::now() + cooldown);
        } else {
            state.health = BackendHealth::Suspect;
        }
        previous != state.health
    }

    pub fn start_probe(self: &Arc<Self>, minimum_interval: Duration) -> Option<ProbeGuard> {
        let mut state = self.probe_lock.lock().ok()?;
        if state.running
            || state
                .last_started
                .is_some_and(|last| last.elapsed() < minimum_interval)
        {
            return None;
        }
        state.running = true;
        state.last_started = Some(Instant::now());
        Some(ProbeGuard {
            backend: self.clone(),
        })
    }

    fn refresh_cooldown(&self) {
        let should_refresh = self.state.read().is_ok_and(|state| {
            state.health == BackendHealth::Cooldown
                && state
                    .cooldown_until
                    .is_some_and(|until| until <= Instant::now())
        });
        if should_refresh && let Ok(mut state) = self.state.write() {
            state.health = BackendHealth::Suspect;
            state.cooldown_until = None;
        }
    }
}

pub struct BackendRegistry {
    backends: Vec<Arc<BackendRuntime>>,
}

impl BackendRegistry {
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let backends = config
            .backends
            .iter()
            .cloned()
            .map(|backend| BackendRuntime::new(backend, config.timeouts.connect).map(Arc::new))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self { backends })
    }

    pub fn all(&self) -> &[Arc<BackendRuntime>] {
        &self.backends
    }

    pub fn by_id(&self, id: &str) -> Option<Arc<BackendRuntime>> {
        self.backends
            .iter()
            .find(|backend| backend.config.id == id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendKind;
    use std::collections::HashMap;
    use url::Url;

    fn runtime(max_in_flight: usize) -> Arc<BackendRuntime> {
        Arc::new(
            BackendRuntime::new(
                BackendConfig {
                    id: "test".into(),
                    url: Url::parse("http://127.0.0.1:8000").unwrap(),
                    kind: BackendKind::Standalone,
                    local: true,
                    enabled: true,
                    priority: 1,
                    max_in_flight,
                    heartbeat_path: "/v1/models".into(),
                    static_headers: HashMap::new(),
                    tags: Vec::new(),
                    danger_accept_invalid_certs: false,
                },
                Duration::from_secs(1),
            )
            .unwrap(),
        )
    }

    #[test]
    fn semaphore_never_exceeds_limit() {
        let backend = runtime(1);
        let permit = backend.try_acquire(true).unwrap();
        assert!(backend.try_acquire(true).is_none());
        drop(permit);
        assert!(backend.try_acquire(true).is_some());
    }

    #[test]
    fn suspect_heartbeat_success_does_not_restore_alive() {
        let backend = runtime(1);
        backend.record_failure(FailureKind::Connect, 2, Duration::from_secs(300));
        assert_eq!(backend.snapshot().health, BackendHealth::Suspect);
        backend.record_heartbeat_success();
        assert_eq!(backend.snapshot().health, BackendHealth::Suspect);
    }

    #[tokio::test]
    async fn cooldown_expires_to_suspect() {
        let backend = runtime(1);
        backend.record_failure(FailureKind::FirstByteTimeout, 2, Duration::from_millis(1));
        assert_eq!(backend.snapshot().health, BackendHealth::Cooldown);
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(backend.snapshot().health, BackendHealth::Suspect);
    }

    #[test]
    fn heartbeat_uses_offline_state_after_threshold() {
        let backend = runtime(1);
        backend.record_heartbeat_success();
        backend.record_failure(FailureKind::Heartbeat, 2, Duration::from_secs(300));
        assert_eq!(backend.snapshot().health, BackendHealth::Alive);
        backend.record_failure(FailureKind::Heartbeat, 2, Duration::from_secs(300));
        assert_eq!(backend.snapshot().health, BackendHealth::Offline);
    }

    #[test]
    fn endpoint_preserves_backend_base_path() {
        let mut backend = runtime(1);
        Arc::get_mut(&mut backend).unwrap().config.url =
            Url::parse("http://127.0.0.1:8000/base").unwrap();
        assert_eq!(
            backend.endpoint("/v1/models").unwrap().as_str(),
            "http://127.0.0.1:8000/base/v1/models"
        );
    }
}
