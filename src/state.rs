use crate::config::{BackendConfig, Config};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct BackendState {
    pub config: BackendConfig,
    pub healthy: bool,
    pub in_flight: usize,
    pub average_latency_ms: u64,
    pub last_probe: Instant,
}

impl BackendState {
    pub fn new(config: BackendConfig) -> Self {
        Self {
            healthy: true,
            in_flight: 0,
            average_latency_ms: 0,
            last_probe: Instant::now(),
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
    pub probe_interval: Duration,
    pub probe_timeout: Duration,
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
            .build()
            .expect("failed to build HTTP client");

        Self {
            self_name: config.self_name.clone(),
            backends: config.backends.clone(),
            states: Mutex::new(states),
            probe_interval: config.probe_interval,
            probe_timeout: config.probe_timeout,
            client,
        }
    }
}
