use super::operation::{OperationId, OperationKind, OperationLease};
use super::policy::OperationPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

pub type AdminFuture = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;

/// AdminController::start の開始失敗理由。P02 / C03 の排他・冪等性違反。。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminStartError {
    /// 同一 fingerprint profile の job が進行中。。
    FingerprintBusy(FingerprintProfile),
    /// lifecycle lease が他操作に占有されている（または Force による promotion 禁止）。。
    LeaseBusy(super::operation::OperationLeaseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAction {
    Reconcile,
    Pair,
    Promote,
    Demote {
        reason: Option<String>,
    },
    Restart,
    Fingerprint {
        profile: FingerprintProfile,
    },
    /// v0.4.0 操作方針の適用（P06 / C03）。desired / expected_generation / request_id を
    /// 受け、両 node の ready/applied を調整して Complete にする。job 完了は方針の適用を
    /// 示し、TP ready とは区別される（CLI が混同しない）。。
    SetPolicy {
        desired: OperationPolicy,
        expected_generation: u64,
        request_id: uuid::Uuid,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FingerprintProfile {
    Standalone,
    Distributed,
}

impl FingerprintProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Distributed => "distributed",
        }
    }
}

pub trait AdminExecutor: Send + Sync + 'static {
    fn execute(&self, action: AdminAction) -> AdminFuture;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminJobState {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminJob {
    pub job_id: String,
    pub operation: String,
    pub state: AdminJobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// v0.4.0 policy 適用 job の状態（P06 / C03）。GET /cluster/jobs/{id} が返す。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyJobState {
    Running,
    Complete,
    Failed,
}

/// node 別の適用結果。partial（片側失敗）では失敗 node の error を保持し、
/// 成功 node の applied は維持する。。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyNodeResult {
    pub node_id: String,
    pub state: PolicyJobState,
    pub applied: OperationPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// policy 適用 job。kind=operation-policy、desired/applied/node 別結果/error を返す。。
#[derive(Debug, Clone, Serialize)]
pub struct PolicyJob {
    pub job_id: String,
    pub kind: &'static str,
    pub state: PolicyJobState,
    pub desired: OperationPolicy,
    pub applied: OperationPolicy,
    pub nodes: Vec<PolicyNodeResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// policy 適用 job の開始結果（P06 / C03）。。
#[derive(Debug, Clone)]
pub enum PolicyStart {
    /// 新規作成。202 + job。。
    Created(PolicyJob),
    /// 冪等重複（同 ID 同 body）。既存 job を返す（同時 GUI 二箇所 → 一 job）。。
    Existing(PolicyJob),
}

/// policy 適用 job の開始失敗理由（P06 / C03）。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyStartError {
    /// 同 ID 別 body。409。。
    IdempotencyConflict,
    /// policy lease が進行中の他操作に占有されている。409。。
    LeaseBusy(super::operation::OperationLeaseError),
}

/// 冪等性のための canonical body hash。request の正規形から計算する（C03: 同 ID 同
/// canonical body は同 job、別内容は 409）。。
pub fn policy_body_hash(desired: OperationPolicy, expected_generation: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    desired.hash(&mut hasher);
    expected_generation.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone)]
pub struct AdminController {
    token: Arc<Vec<u8>>,
    executor: Arc<dyn AdminExecutor>,
    jobs: Arc<Mutex<HashMap<String, AdminJob>>>,
    active_fingerprints: Arc<Mutex<HashSet<FingerprintProfile>>>,
    /// policy 適用 job の store。GET /cluster/jobs/{id} が読む。。
    policy_jobs: Arc<Mutex<HashMap<uuid::Uuid, PolicyJob>>>,
    /// policy 冪等性（request_id → body_hash）。同 ID 同 body は同 job、別 body は 409。。
    policy_idempotency: Arc<Mutex<HashMap<uuid::Uuid, u64>>>,
    /// 全 lifecycle 操作の単一オーナー lease（P02 / C03）。データ取得 job（fingerprint）は
    /// 不要で、activation のみ排他。fingerprint 専用 HashSet だけで済ませない。
    lease: OperationLease,
}

impl AdminController {
    pub fn new(token: Vec<u8>, executor: Arc<dyn AdminExecutor>) -> anyhow::Result<Self> {
        anyhow::ensure!(!token.is_empty(), "admin token must not be empty");
        Ok(Self {
            token: Arc::new(token),
            executor,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            active_fingerprints: Arc::new(Mutex::new(HashSet::new())),
            policy_jobs: Arc::new(Mutex::new(HashMap::new())),
            policy_idempotency: Arc::new(Mutex::new(HashMap::new())),
            lease: OperationLease::new(),
        })
    }

    pub fn authorize(&self, authorization: Option<&str>) -> bool {
        let Some(value) = authorization.and_then(|value| value.strip_prefix("Bearer ")) else {
            return false;
        };
        let Ok(supplied) = decode_hex(value) else {
            return false;
        };
        supplied.len() == self.token.len() && supplied.ct_eq(self.token.as_slice()).into()
    }

    pub fn start(&self, action: AdminAction) -> Result<AdminJob, AdminStartError> {
        let fingerprint_profile = match action {
            AdminAction::Fingerprint { profile } => Some(profile),
            _ => None,
        };
        if let Some(profile) = fingerprint_profile {
            let mut active = self
                .active_fingerprints
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !active.insert(profile) {
                return Err(AdminStartError::FingerprintBusy(profile));
            }
        }
        // P02 / C03: lifecycle 操作は OperationLease で単一オーナーを確保する。
        // データ取得 job（fingerprint）は lease 不要。pair/promote は同一 Promotion lease。
        let lease_kind = operation_kind(&action);
        let operation_id = OperationId(uuid::Uuid::new_v4());
        if let Some(kind) = lease_kind {
            if let Err(error) = self.lease.try_acquire(kind, operation_id) {
                if let Some(profile) = fingerprint_profile {
                    self.active_fingerprints
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&profile);
                }
                return Err(AdminStartError::LeaseBusy(error));
            }
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let operation = action_name(&action).to_owned();
        let job = AdminJob {
            job_id: job_id.clone(),
            operation,
            state: AdminJobState::Running,
            result: None,
            error: None,
        };
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(job_id.clone(), job.clone());

        let executor = self.executor.clone();
        let jobs = self.jobs.clone();
        let active_fingerprints = self.active_fingerprints.clone();
        let lease = self.lease.clone();
        tokio::spawn(async move {
            let result = executor.execute(action).await;
            // 完了時に lease を解放する（future drop を cancel に使わない）。。。
            if let Some(kind) = lease_kind {
                lease.release(kind, operation_id);
            }
            let mut jobs = jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(job) = jobs.get_mut(&job_id) else {
                return;
            };
            match result {
                Ok(value) => {
                    job.state = AdminJobState::Complete;
                    job.result = Some(value);
                }
                Err(error) => {
                    job.state = AdminJobState::Failed;
                    job.error = Some(error.to_string());
                }
            }
            if let Some(profile) = fingerprint_profile {
                active_fingerprints
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&profile);
            }
        });
        Ok(job)
    }

    pub fn job(&self, job_id: &str) -> Option<AdminJob> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(job_id)
            .cloned()
    }

    /// policy 適用 job を開始する（P06 / C03）。冪等性（request_id → body_hash）と
    /// 単一オーナー（OperationKind::Policy lease）を検査し、同 ID 同 body は既存 job、
    /// 別 body は Conflict、policy lease 進行中は Busy を返す。。
    pub fn start_policy(
        &self,
        request_id: uuid::Uuid,
        body_hash: u64,
        desired: OperationPolicy,
        expected_generation: u64,
        local_node_id: String,
    ) -> Result<PolicyStart, PolicyStartError> {
        let mut idem = self
            .policy_idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match idem.get(&request_id) {
            // 同 ID 同 body → 既存 job を返す（同時 GUI 二箇所 → 一 job）。。
            Some(existing) if *existing == body_hash => {
                let job_id = self
                    .policy_jobs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&request_id)
                    .cloned();
                match job_id {
                    Some(job) => Ok(PolicyStart::Existing(job)),
                    None => Err(PolicyStartError::IdempotencyConflict),
                }
            }
            // 同 ID 別 body → 409。。
            Some(_) => Err(PolicyStartError::IdempotencyConflict),
            None => {
                idem.insert(request_id, body_hash);
                let operation_id = OperationId(uuid::Uuid::new_v4());
                if let Err(error) = self.lease.try_acquire(OperationKind::Policy, operation_id) {
                    idem.remove(&request_id);
                    return Err(PolicyStartError::LeaseBusy(error));
                }
                let job_id = uuid::Uuid::new_v4();
                let job = PolicyJob {
                    job_id: job_id.to_string(),
                    kind: "operation-policy",
                    state: PolicyJobState::Running,
                    desired,
                    applied: OperationPolicy::Automatic,
                    nodes: vec![PolicyNodeResult {
                        node_id: local_node_id.clone(),
                        state: PolicyJobState::Running,
                        applied: OperationPolicy::Automatic,
                        error: None,
                    }],
                    error: None,
                };
                self.policy_jobs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(request_id, job.clone());
                drop(idem);
                let executor = self.executor.clone();
                let policy_jobs = self.policy_jobs.clone();
                let lease = self.lease.clone();
                tokio::spawn(async move {
                    let result = executor
                        .execute(AdminAction::SetPolicy {
                            desired,
                            expected_generation,
                            request_id,
                        })
                        .await;
                    lease.release(OperationKind::Policy, operation_id);
                    let mut store = policy_jobs
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let Some(entry) = store.get_mut(&request_id) else {
                        return;
                    };
                    match result {
                        Ok(value) => {
                            // executor は必ず node 別結果と cluster-wide 完了判定を返す。
                            // 旧単一node executorとの互換性のため、明示値が無い場合だけ
                            // 1 node 全て complete を成功とみなす。複数nodeで明示的な
                            // `cluster_complete=false` を返した場合は、local nodeが
                            // completeでも job 全体を成功扱いしない（C03）。。
                            let nodes =
                                value
                                    .get("nodes")
                                    .and_then(Value::as_array)
                                    .and_then(|nodes| {
                                        serde_json::from_value::<Vec<PolicyNodeResult>>(
                                            serde_json::Value::Array(nodes.clone()),
                                        )
                                        .ok()
                                    });
                            if let Some(nodes) = nodes {
                                entry.nodes = nodes;
                            }
                            let cluster_complete = value
                                .get("cluster_complete")
                                .and_then(Value::as_bool)
                                .unwrap_or_else(|| {
                                    entry.nodes.len() == 1
                                        && entry
                                            .nodes
                                            .iter()
                                            .all(|node| node.state == PolicyJobState::Complete)
                                });
                            if cluster_complete
                                && entry
                                    .nodes
                                    .iter()
                                    .all(|node| node.state == PolicyJobState::Complete)
                            {
                                entry.state = PolicyJobState::Complete;
                                entry.applied = desired;
                            } else {
                                entry.state = PolicyJobState::Failed;
                                entry.error = Some(
                                    "cluster-wide policy apply did not complete on every node"
                                        .into(),
                                );
                            }
                        }
                        Err(error) => {
                            entry.state = PolicyJobState::Failed;
                            entry.error = Some(error.to_string());
                            if let Some(node) = entry.nodes.first_mut() {
                                node.state = PolicyJobState::Failed;
                                node.error = Some(error.to_string());
                            }
                        }
                    }
                });
                Ok(PolicyStart::Created(job))
            }
        }
    }

    /// policy 適用 job を返す（GET /cluster/jobs/{id}）。未知 ID は None。。
    pub fn policy_job(&self, request_id: &uuid::Uuid) -> Option<PolicyJob> {
        self.policy_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(request_id)
            .cloned()
    }
}

pub fn encode_token(token: &[u8]) -> String {
    token.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    if value.len() % 2 != 0 {
        return Err(());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).map_err(|_| ())?;
            u8::from_str_radix(text, 16).map_err(|_| ())
        })
        .collect()
}

fn action_name(action: &AdminAction) -> &'static str {
    match action {
        AdminAction::Reconcile => "reconcile",
        AdminAction::Pair => "pair",
        AdminAction::Promote => "promote",
        AdminAction::Demote { .. } => "demote",
        AdminAction::Restart => "restart",
        AdminAction::Fingerprint { .. } => "fingerprint",
        AdminAction::SetPolicy { .. } => "operation-policy",
    }
}

/// lifecycle 操作を OperationKind へ対応付ける。データ取得（fingerprint）は None。。。
/// pair/promote は同一 Promotion lease を共有する（同時進行しない）。。。。
fn operation_kind(action: &AdminAction) -> Option<OperationKind> {
    match action {
        AdminAction::Reconcile => Some(OperationKind::Restart),
        AdminAction::Pair | AdminAction::Promote => Some(OperationKind::Promotion),
        AdminAction::Demote { .. } => Some(OperationKind::Demotion),
        AdminAction::Restart => Some(OperationKind::Restart),
        AdminAction::Fingerprint { .. } => None,
        AdminAction::SetPolicy { .. } => Some(OperationKind::Policy),
    }
}
