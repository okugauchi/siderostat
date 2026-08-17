//! Admin API polling client for the siderostat `/metrics` endpoint.

use crate::{
    config::MonitorConfig,
    metrics::{MetricsSnapshot, parse_metrics},
};
use anyhow::{Context, Result, anyhow};
use std::time::Duration;

#[derive(Clone)]
pub struct MetricsClient {
    http: reqwest::Client,
    base_url: String,
    admin_token: Option<String>,
    poll_interval: Duration,
    offline_backoff: Duration,
}

impl MetricsClient {
    pub fn new(config: &MonitorConfig) -> Result<Self> {
        let base_url = config.admin_listen.trim_end_matches('/').to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(anyhow!("admin_listen must be an http(s) URL"));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()
                .context("build admin API client")?,
            base_url,
            admin_token: config.admin_token.clone(),
            poll_interval: config.poll_interval(),
            offline_backoff: config.offline_backoff(),
        })
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub fn offline_backoff(&self) -> Duration {
        self.offline_backoff
    }

    /// Fetch and parse the current metrics. Returns `None` when the endpoint
    /// is unreachable or malformed; the caller treats that as offline.
    pub async fn fetch_metrics(&self) -> Result<MetricsSnapshot> {
        let url = format!("{}/metrics", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("metrics endpoint returned {}", response.status()));
        }
        let text = response
            .text()
            .await
            .context("read metrics response body")?;
        Ok(parse_metrics(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MonitorConfig;
    #[test]
    fn builds_client_from_config() {
        let config = MonitorConfig::default();
        let client = MetricsClient::new(&config).unwrap();
        assert_eq!(client.poll_interval, Duration::from_secs(2));
        assert_eq!(client.offline_backoff, Duration::from_secs(5));
    }

    #[test]
    fn rejects_non_http_admin_listen() {
        let config = MonitorConfig {
            admin_listen: "127.0.0.1:18081".into(),
            ..MonitorConfig::default()
        };
        assert!(MetricsClient::new(&config).is_err());
    }
}
