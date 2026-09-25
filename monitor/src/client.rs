//! Admin API polling client for the siderostat `/metrics` endpoint.

use crate::{
    config::MonitorConfig,
    connection_mode::{ConnectionPolicy, NodeResult, PendingJob, PolicyApiError},
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
            admin_token: config.effective_admin_token()?,
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
    /// endpoint (C-04). Returns the HTTP status and response body. The body is
    /// intentionally kept as text because an older runtime may acknowledge a
    /// successful restart with an empty or non-JSON response.
    pub async fn graceful_restart(&self) -> Result<(reqwest::StatusCode, String)> {
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
        let body = response
            .text()
            .await
            .context("read graceful restart response body")?;
        Ok((status, body))
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

    /// Return whether the runtime is ready to serve model traffic. A 503 from
    /// `/readyz` is an expected not-yet-ready result, not a client error.
    pub async fn ready(&self) -> Result<bool> {
        let url = format!("{}/readyz", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        Ok(response.status().is_success())
    }

    /// Fetch the DS4 Manager job status from `/manager/status` (M10 / C04).
    /// Returns `Ok` with the full snapshot on success; a 404 means the runtime
    /// predates the manager API (旧 version) and the caller should mark new
    /// operations disabled (G01)。A network failure is a disconnect to retry.
    pub async fn fetch_manager_jobs(
        &self,
    ) -> Result<siderostat_core::manager::api::ManagerStatusResponse> {
        let url = format!("{}/manager/status", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            // 旧 runtime: /manager API が無い。新操作（submit/cancel）disabled。G01
            return Err(anyhow!("manager API not found (old runtime) at {url}"));
        }
        if !response.status().is_success() {
            return Err(anyhow!(
                "manager/status endpoint returned {}",
                response.status()
            ));
        }
        response
            .json()
            .await
            .context("parse manager/status response")
    }

    /// Fetch the sanitized, node-local Manager inventory. This uses the same
    /// admin bearer token as job polling and never requests peer inventory.
    pub async fn fetch_manager_inventory(
        &self,
    ) -> Result<siderostat_core::manager::api::ManagerInventoryResponse> {
        let url = format!("{}/manager/inventory", self.base_url);
        let mut request = self.http.get(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow!("manager inventory unavailable (old runtime)"));
        }
        if !response.status().is_success() {
            return Err(anyhow!(
                "manager/inventory endpoint returned {}",
                response.status()
            ));
        }
        response
            .json()
            .await
            .context("parse manager/inventory response")
    }

    /// 接続モードを `/cluster/operation-policy`（P06 / C03）へ適用する。
    ///
    /// 選択を直接 `StableMode` へ書換えず、API へ送る（レビュー重点）。
    /// 202 = 新規 job または冪等な既存 job。409 = 別の lifecycle 操作が
    /// 進行中（理由表示）。それ以外は Other。G02。
    pub async fn apply_operation_policy(
        &self,
        policy: ConnectionPolicy,
        expected_generation: u64,
    ) -> Result<PendingJob, PolicyApiError> {
        let url = format!("{}/cluster/operation-policy", self.base_url);
        let body = serde_json::json!({
            "policy": policy.wire_value(),
            "expected_generation": expected_generation,
            "request_id": uuid::Uuid::new_v4().to_string(),
        });
        let mut request = self.http.post(&url).json(&body);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return Err(PolicyApiError::Other(format!("POST {url} failed: {error}")));
            }
        };
        match response.status() {
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::OK => {
                let value: serde_json::Value = match response.json().await {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(PolicyApiError::Other(format!(
                            "parse operation-policy response failed: {error}"
                        )));
                    }
                };
                parse_policy_job(&value)
                    .ok_or_else(|| PolicyApiError::Other("policy job malformed".to_string()))
            }
            reqwest::StatusCode::CONFLICT => {
                // 409: 別の lifecycle 操作が進行中。理由を表示する。G02。
                let reason = response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "another lifecycle operation is in progress".to_string());
                Err(PolicyApiError::Busy(reason))
            }
            other => Err(PolicyApiError::Other(format!(
                "operation-policy endpoint returned {other}"
            ))),
        }
    }

    pub async fn submit_manager_job(
        &self,
        kind: &str,
        payload_key: &str,
    ) -> Result<siderostat_core::manager::api::SubmitResponse> {
        let body = manager_job_body(kind, payload_key, None)?;
        self.submit_manager_job_body(body).await
    }

    /// Submit an activation/rollback job with the observed generation. The
    /// runtime owner resolves and validates its live lease internally.
    pub async fn submit_manager_job_with_generation(
        &self,
        kind: &str,
        payload_key: &str,
        expected_generation: u64,
    ) -> Result<siderostat_core::manager::api::SubmitResponse> {
        let body = manager_job_body(kind, payload_key, Some(expected_generation))?;
        self.submit_manager_job_body(body).await
    }

    async fn submit_manager_job_body(
        &self,
        body: serde_json::Value,
    ) -> Result<siderostat_core::manager::api::SubmitResponse> {
        let url = format!("{}/manager/jobs", self.base_url);
        let mut request = self.http.post(&url).json(&body);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "manager/jobs endpoint returned {}",
                response.status()
            ));
        }
        response
            .json()
            .await
            .context("parse manager/jobs submit response")
    }

    /// `POST /manager/jobs/{id}/cancel`。進行中の job をキャンセルする。
    /// G03。。/
    pub async fn cancel_manager_job(&self, id: &str) -> Result<()> {
        let url = format!("{}/manager/jobs/{id}/cancel", self.base_url);
        let mut request = self.http.post(&url);
        if let Some(token) = &self.admin_token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "manager/jobs/{id}/cancel returned {}",
                response.status()
            ));
        }
        Ok(())
    }
}

fn parse_policy_job(value: &serde_json::Value) -> Option<PendingJob> {
    let job_id = value.get("job_id")?.as_str()?.to_string();
    let policy = match value.get("desired")?.as_str()? {
        "automatic" => ConnectionPolicy::Automatic,
        "forced-standalone" => ConnectionPolicy::ForcedStandalone,
        _ => return None,
    };
    let phase = value
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("running")
        .to_string();
    let node_results = value
        .get("nodes")
        .and_then(|n| n.as_array())
        .map(|nodes| {
            nodes
                .iter()
                .map(|n| NodeResult {
                    node: n
                        .get("node_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("")
                        .to_string(),
                    state: n
                        .get("state")
                        .and_then(|s| s.as_str())
                        .map(|s| if s == "Complete" { "ok" } else { "failed" })
                        .unwrap_or("failed")
                        .to_string(),
                    error: n
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("")
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(PendingJob {
        job_id,
        policy,
        phase,
        node_results,
    })
}

fn manager_job_body(
    kind: &str,
    payload_key: &str,
    expected_generation: Option<u64>,
) -> Result<serde_json::Value> {
    let needs_context = matches!(kind, "activate" | "rollback");
    if needs_context {
        anyhow::ensure!(
            expected_generation.is_some_and(|generation| generation > 0),
            "expected_generation must be greater than zero for {kind}"
        );
    }

    let mut body = serde_json::json!({
        "kind": kind,
        "payload_key": payload_key,
    });
    if let Some(generation) = expected_generation {
        body["expected_generation"] = serde_json::json!(generation);
    }
    Ok(body)
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

    #[test]
    fn ready_endpoint_distinguishes_ready_from_not_ready() {
        assert!(reqwest::StatusCode::OK.is_success());
        assert!(!reqwest::StatusCode::SERVICE_UNAVAILABLE.is_success());
    }

    #[test]
    fn manager_job_body_includes_generation_without_exposing_a_runtime_lease() {
        let activation =
            manager_job_body("activate", "profile-a", Some(3)).expect("activation body");
        assert_eq!(activation["kind"], "activate");
        assert_eq!(activation["payload_key"], "profile-a");
        assert_eq!(activation["expected_generation"], 3);
        assert!(activation.get("runtime_lease").is_none());

        let fetch = manager_job_body("fetch", "official", None).expect("fetch body");
        assert_eq!(fetch["kind"], "fetch");
        assert!(fetch.get("expected_generation").is_none());
        assert!(fetch.get("runtime_lease").is_none());
    }

    #[test]
    fn manager_job_body_rejects_incomplete_activation_context() {
        let error = manager_job_body("activate", "profile-a", Some(0))
            .expect_err("zero generation must be rejected");
        assert!(error.to_string().contains("expected_generation"));
    }
}
