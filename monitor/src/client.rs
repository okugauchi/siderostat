//! Admin API polling client for the siderostat `/metrics` endpoint.

use crate::{
    config::MonitorConfig,
    metrics::{MetricsSnapshot, parse_metrics},
};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct ClusterRoutingState {
    role: String,
    target: String,
}

/// Runtime build metadata from the read-only `/healthz` admin endpoint
/// (B-01 / D-03).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimeVersion {
    pub version: String,
    pub git_commit: String,
    pub build_number: String,
}

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

    /// Fetch and parse the metrics source for the current serving node.
    ///
    /// A worker serving through the coordinator reads the coordinator snapshot through the
    /// worker's loopback `/metrics/coordinator` endpoint. The siderostat worker authenticates
    /// that hop over the existing control plane, so the coordinator's admin listener remains
    /// loopback-only.
    pub async fn fetch_metrics(&self) -> Result<MetricsSnapshot> {
        let routing = self.fetch_cluster_routing().await?;
        let path = metrics_path(&routing);
        self.fetch_metrics_at(path).await
    }

    async fn fetch_cluster_routing(&self) -> Result<ClusterRoutingState> {
        let url = format!("{}/cluster", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("cluster endpoint returned {}", response.status()));
        }
        response.json().await.context("parse cluster response")
    }

    async fn fetch_metrics_at(&self, path: &str) -> Result<MetricsSnapshot> {
        let url = format!("{}{path}", self.base_url);
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

    /// Request a graceful runtime restart via the authenticated `/admin/restart`
    /// endpoint (C-04). Returns the HTTP status and the JSON response body.
    pub async fn graceful_restart(&self) -> Result<(reqwest::StatusCode, serde_json::Value)> {
        let url = format!("{}/admin/restart", self.base_url);
        let mut request = self.http.post(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let value: serde_json::Value = response
            .json()
            .await
            .context("parse graceful restart response")?;
        Ok((status, value))
    }

    /// Fetch the runtime build metadata from the read-only `/healthz` endpoint
    /// (B-01 / D-03). This is a non-mutating read and is used to compare the
    /// app version against the running runtime's version.
    pub async fn health(&self) -> Result<RuntimeVersion> {
        let url = format!("{}/healthz", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("healthz endpoint returned {}", response.status()));
        }
        let version: RuntimeVersion = response.json().await.context("parse healthz response")?;
        Ok(version)
    }
}

fn metrics_path(routing: &ClusterRoutingState) -> &'static str {
    if routing.role == "worker" && routing.target == "coordinator" {
        "/metrics/coordinator"
    } else {
        "/metrics"
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

    #[test]
    fn selects_coordinator_metrics_when_worker_targets_coordinator() {
        let routing = ClusterRoutingState {
            role: "worker".into(),
            target: "coordinator".into(),
        };
        assert_eq!(metrics_path(&routing), "/metrics/coordinator");
    }

    #[test]
    fn keeps_local_metrics_for_worker_solo_mode() {
        let routing = ClusterRoutingState {
            role: "worker".into(),
            target: "local-standalone".into(),
        };
        assert_eq!(metrics_path(&routing), "/metrics");
    }
}
