//! Admin API polling client for the siderostat `/metrics` endpoint and the
//! authenticated `/cluster/restart` mutation.

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

    /// Request a restart of the siderostat runtime through the admin API.
    /// The admin token (hex-encoded) is required because `/cluster/restart`
    /// is an authenticated mutation, unlike the public `/metrics` endpoint.
    /// Returns the accepted admin job JSON on success.
    pub async fn request_restart(&self) -> Result<serde_json::Value> {
        let url = format!("{}/cluster/restart", self.base_url);
        let mut request = self.http.post(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("restart endpoint returned {}", response.status()));
        }
        response.json().await.context("read restart response body")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MonitorConfig;
    use std::io::{Read, Write};

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

    #[tokio::test]
    async fn restart_posts_to_cluster_restart_and_parses_job() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                request.starts_with("POST /cluster/restart HTTP/1.1"),
                "unexpected request: {request}"
            );
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer deadbeef"),
                "restart must be authenticated: {request}"
            );
            let body = r#"{"job_id":"abc","operation":"restart","state":"running"}"#;
            let response = format!(
                "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).unwrap();
        });

        let config = MonitorConfig {
            admin_listen: format!("http://{addr}"),
            admin_token: Some("deadbeef".into()),
            ..MonitorConfig::default()
        };
        let client = MetricsClient::new(&config).unwrap();
        let job = client.request_restart().await.unwrap();
        assert_eq!(job["job_id"], "abc");
        server.join().unwrap();
    }
}
