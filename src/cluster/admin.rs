use super::operation::{OperationId, OperationKind, OperationLease};
use serde::Serialize;
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
    Demote { reason: Option<String> },
    Restart,
    Fingerprint { profile: FingerprintProfile },
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

#[derive(Clone)]
pub struct AdminController {
    token: Arc<Vec<u8>>,
    executor: Arc<dyn AdminExecutor>,
    jobs: Arc<Mutex<HashMap<String, AdminJob>>>,
    active_fingerprints: Arc<Mutex<HashSet<FingerprintProfile>>>,
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
    }
}
