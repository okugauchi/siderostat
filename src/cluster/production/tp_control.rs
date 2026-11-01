//! Signed TP control is independent of the legacy LP protocol.
use super::*;
use crate::cluster::TpLocalStatus;
use serde::{Deserialize, Serialize};
pub(super) const TP_PATH: &str = "/v2/tp";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum TpCommand {
    Node,
    Pair,
    Prepare,
    Status,
    Ready,
    Stop,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TpRequest {
    pub version: u16,
    pub deployment: String,
    pub epoch: u64,
    pub session: u64,
    pub operation: uuid::Uuid,
    pub coordinator_address: IpAddr,
    pub command: TpCommand,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TpResponse {
    pub version: u16,
    pub deployment: String,
    pub epoch: u64,
    pub session: u64,
    pub operation: uuid::Uuid,
    pub address: IpAddr,
    pub status: TpLocalStatus,
}
impl ProductionControlClient {
    pub(super) async fn tp(&self, request: &TpRequest) -> anyhow::Result<TpResponse> {
        let body = serde_json::to_vec(request)?;
        let signed = self.inner.authenticator.sign(
            self.inner.local_node_id.clone(),
            "POST",
            TP_PATH,
            now_millis(),
            uuid::Uuid::new_v4().simple().to_string(),
            &body,
        )?;
        let mut response = self
            .inner
            .client
            .post(self.inner.base.join(TP_PATH.trim_start_matches('/'))?)
            .header(HEADER_NODE, signed.node_id())
            .header(HEADER_TIMESTAMP, signed.timestamp_millis())
            .header(HEADER_NONCE, signed.nonce())
            .header(HEADER_SIGNATURE, signed.signature())
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(
                if matches!(request.command, TpCommand::Node | TpCommand::Status) {
                    self.inner.request_timeout
                } else {
                    self.inner.lifecycle_timeout
                },
            )
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "peer TP control rejected request: {}",
            response.status()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= 16384, "oversized TP response");
            bytes.extend(chunk);
        }
        let reply: TpResponse = serde_json::from_slice(&bytes)?;
        ensure!(
            reply.version == 1
                && reply.deployment == request.deployment
                && reply.epoch == request.epoch
                && reply.operation == request.operation,
            "TP peer capability or operation mismatch"
        );
        if !matches!(request.command, TpCommand::Node | TpCommand::Pair) {
            ensure!(reply.session == request.session, "stale TP peer session");
        }
        Ok(reply)
    }
}
pub(super) async fn handle(
    State(runtime): State<ProductionClusterRuntime>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = async {
        runtime
            .authenticate("POST", TP_PATH, &body, source, &headers)
            .await
            .map_err(|_| anyhow::anyhow!("TP authentication rejected"))?;
        let request: TpRequest = serde_json::from_slice(&body)?;
        // The owned task survives cancellation of the initiating HTTP request.
        tokio::spawn(async move { runtime.handle_tp(request).await }).await?
    }
    .await;
    match result {
        Ok(reply) => Json(reply).into_response(),
        Err(error) => {
            tracing::warn!(error=%error,"TP control rejected");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"TP control rejected"})),
            )
                .into_response()
        }
    }
}
