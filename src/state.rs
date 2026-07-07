use crate::config::{BackendConfig, Config};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone)]
pub struct BackendState {
    pub config: BackendConfig,
    pub healthy: bool,
    pub in_flight: usize,
    pub average_latency_ms: u64,
    pub last_heartbeat: Option<SystemTime>,
    pub last_failure: Option<SystemTime>,
}

impl BackendState {
    pub fn new(config: BackendConfig) -> Self {
        Self {
            healthy: true,
            in_flight: 0,
            average_latency_ms: 0,
            last_heartbeat: None,
            last_failure: None,
            config,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.in_flight >= self.config.max_in_flight
    }
}

pub struct AppState {
    pub self_name: String,
    pub backends: Vec<BackendConfig>,
    pub states: Mutex<HashMap<String, BackendState>>,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub heartbeat_path: String,
    pub active_probe_timeout: Duration,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn from_config(config: &Config) -> Self {
        let states: HashMap<String, BackendState> = config
            .backends
            .iter()
            .map(|b| (b.name.clone(), BackendState::new(b.clone())))
            .collect();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(config.tls_accept_invalid_certs)
            .build()
            .expect("failed to build HTTP client");

        Self {
            self_name: config.self_name.clone(),
            backends: config.backends.clone(),
            states: Mutex::new(states),
            heartbeat_interval: config.heartbeat_interval,
            heartbeat_timeout: config.heartbeat_timeout,
            heartbeat_path: config.heartbeat_path.clone(),
            active_probe_timeout: config.active_probe_timeout,
            client,
        }
    }
}
