//! DS4 Manager API — ManagerApi / ManagerJob DTO。M10。
//!
//! C04 に基づき、`GET /manager/status`、`POST /manager/jobs`、
//! `GET /manager/jobs/{id}`、`POST /manager/jobs/{id}/cancel` のロジックと
//! 公開 DTO を提供する。許可 job kind の厳密 payload と
//! generation/idempotency を検証する。secret と raw build log を公開 DTO に
//! 入れない。M10。
//!
//! 受入 case（全て必須）:
//! - 入力: fetch→build→download→verify→activate→rollback → 状態一致
//! - 入力: cancel 後 poll → terminal 保持
//! - 入力: 不正 job / unknown field → 400
//! - 入力: activate busy → 409
use crate::manager::executor::ManagerExecutionRequest;
use crate::manager::jobs::{JobJournal, JobKind, JobPhase, ManagerJobError};

/// 公開 job DTO。secret / raw build log を含まない。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerJobDto {
    pub id: String,
    pub kind: String,
    pub progress: u8,
    pub phase: String,
    pub error: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub cancel: bool,
}

/// JobPhase を snake_case 文字列にする。M10。
fn phase_name(phase: JobPhase) -> &'static str {
    match phase {
        JobPhase::Running => "running",
        JobPhase::Succeeded => "succeeded",
        JobPhase::Failed => "failed",
        JobPhase::Cancelling => "cancelling",
    }
}

impl From<&crate::manager::jobs::ManagerJob> for ManagerJobDto {
    fn from(job: &crate::manager::jobs::ManagerJob) -> Self {
        Self {
            id: job.id.clone(),
            kind: job.kind.to_string(),
            progress: job.progress,
            phase: phase_name(job.phase).to_string(),
            error: job.error.clone(),
            created_at: job.created_at,
            updated_at: job.updated_at,
            cancel: job.cancel,
        }
    }
}

/// `GET /manager/status` の応答。job 進捗と旧 active 維持を観測できる。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerStatusResponse {
    pub jobs: Vec<ManagerJobDto>,
    /// 旧 active artifact の digest（維持されていれば Some）。M10。
    pub active_digest: Option<String>,
    /// 進行中 job 数（queue depth）。M10。
    pub queue_depth: usize,
}

/// `POST /manager/jobs` の要求。kind の厳密 payload。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct JobSubmitRequest {
    /// 許可 job kind（fetch/build/download/verify/stage/activate/rollback）。M10。
    pub kind: String,
    /// 同一作業の正規化キー（idempotency）。M10。
    pub payload_key: String,
    /// activation/rollback に要求（C04）。M10。
    #[serde(default)]
    pub expected_generation: u64,
    /// activation/rollback に要求（C04）。M10。
    #[serde(default)]
    pub runtime_lease: Option<String>,
}

impl JobSubmitRequest {
    /// Preserve the submitted generation and lease when handing a journaled
    /// request to the executor. The payload key remains an opaque lookup key.
    pub fn execution_request(
        &self,
        id: String,
    ) -> Result<ManagerExecutionRequest, ManagerApiError> {
        let kind = parse_kind(&self.kind)
            .ok_or_else(|| ManagerApiError::BadRequest("unknown job kind".into()))?;
        Ok(ManagerExecutionRequest {
            id,
            kind,
            payload_key: self.payload_key.clone(),
            expected_generation: self.expected_generation,
            runtime_lease: self.runtime_lease.clone(),
        })
    }
}

/// `POST /manager/jobs` の応答。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SubmitResponse {
    pub id: String,
}

/// manager API エラー。HTTP status に対応する。M10。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerApiError {
    /// 400: 不正 job / unknown field / unknown kind。M10。
    BadRequest(String),
    /// 404: 未知 job ID。M10。
    NotFound,
    /// 409: activate busy（同時 activation は 1）。M10。
    Conflict(String),
}

impl std::fmt::Display for ManagerApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerApiError::BadRequest(msg) => write!(f, "bad request: {msg}"),
            ManagerApiError::NotFound => write!(f, "job not found"),
            ManagerApiError::Conflict(msg) => write!(f, "conflict: {msg}"),
        }
    }
}

impl std::error::Error for ManagerApiError {}

impl From<ManagerJobError> for ManagerApiError {
    fn from(e: ManagerJobError) -> Self {
        match e {
            ManagerJobError::UnknownKind => {
                ManagerApiError::BadRequest("unknown job kind".to_string())
            }
            ManagerJobError::NotFound => ManagerApiError::NotFound,
        }
    }
}

/// kind 文字列 → JobKind。M10。
pub fn parse_kind(kind: &str) -> Option<JobKind> {
    match kind {
        "fetch" => Some(JobKind::Fetch),
        "build" => Some(JobKind::Build),
        "download" => Some(JobKind::Download),
        "verify" => Some(JobKind::Verify),
        "stage" => Some(JobKind::Stage),
        "activate" => Some(JobKind::Activate),
        "rollback" => Some(JobKind::Rollback),
        _ => None,
    }
}

/// job を submit する。activation/rollback は expected_generation +
/// runtime_lease を要求する。activate busy → 409。M10。
pub fn submit(
    journal: &mut JobJournal,
    req: JobSubmitRequest,
) -> Result<SubmitResponse, ManagerApiError> {
    let kind = parse_kind(&req.kind)
        .ok_or_else(|| ManagerApiError::BadRequest(format!("unknown job kind: {}", req.kind)))?;
    // activation/rollback だけ generation + lease を要求（C04）。M10。
    if matches!(kind, JobKind::Activate | JobKind::Rollback) {
        if req.expected_generation == 0 || req.runtime_lease.as_deref().unwrap_or("").is_empty() {
            return Err(ManagerApiError::BadRequest(
                "activate/rollback requires expected_generation and runtime_lease".to_string(),
            ));
        }
        // activate busy → 409（同時 activation は 1、C04）。M10。
        if kind == JobKind::Activate && journal.has_running(JobKind::Activate) {
            return Err(ManagerApiError::Conflict(
                "an activation job is already running".to_string(),
            ));
        }
    }
    let id = journal.enqueue(kind, &req.payload_key)?;
    Ok(SubmitResponse { id })
}

/// JSON 文字列から submit する。unknown field / parse 失敗 → 400。M10。
pub fn submit_json(
    journal: &mut JobJournal,
    body: &str,
) -> Result<SubmitResponse, ManagerApiError> {
    let req: JobSubmitRequest =
        serde_json::from_str(body).map_err(|e| ManagerApiError::BadRequest(e.to_string()))?;
    submit(journal, req)
}

/// `GET /manager/status`。M10。
pub fn status(journal: &JobJournal, active_digest: Option<String>) -> ManagerStatusResponse {
    let jobs: Vec<ManagerJobDto> = journal
        .all()
        .iter()
        .map(|j| ManagerJobDto::from(*j))
        .collect();
    let queue_depth = journal
        .all()
        .iter()
        .filter(|j| matches!(j.phase, JobPhase::Running | JobPhase::Cancelling))
        .count();
    ManagerStatusResponse {
        jobs,
        active_digest,
        queue_depth,
    }
}

/// `GET /manager/jobs/{id}`。M10。
pub fn get(journal: &JobJournal, id: &str) -> Result<ManagerJobDto, ManagerApiError> {
    let job = journal.get(id).ok_or(ManagerApiError::NotFound)?;
    Ok(ManagerJobDto::from(job))
}

/// `POST /manager/jobs/{id}/cancel`。M10。job phase を Cancelling にし、
/// terminal 状態を保持する（cancel 後 poll でも terminal）。M10。
pub fn cancel(journal: &mut JobJournal, id: &str) -> Result<(), ManagerApiError> {
    journal.request_cancel(id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: &str, key: &str) -> JobSubmitRequest {
        JobSubmitRequest {
            kind: kind.to_string(),
            payload_key: key.to_string(),
            expected_generation: 0,
            runtime_lease: None,
        }
    }

    fn activate_req(key: &str) -> JobSubmitRequest {
        JobSubmitRequest {
            kind: "activate".to_string(),
            payload_key: key.to_string(),
            expected_generation: 3,
            runtime_lease: Some("lease-3".to_string()),
        }
    }

    /// 受入: fetch→build→download→verify→activate→rollback → 状態一致。M10。
    #[test]
    fn full_pipeline_states_match() {
        let mut journal = JobJournal::new();
        for (kind, key) in [
            ("fetch", "src-a"),
            ("build", "src-a"),
            ("download", "model-a"),
            ("verify", "model-a"),
            ("stage", "profile-a"),
        ] {
            let id = submit(&mut journal, req(kind, key)).expect("submit").id;
            assert!(journal.get(&id).expect("job").kind.as_str() == kind);
            journal.succeed(&id).expect("succeed");
        }
        // activate（generation+lease 必須）。M10。
        let id = submit(&mut journal, activate_req("profile-a"))
            .expect("activate")
            .id;
        journal.succeed(&id).expect("succeed");
        // rollback（generation+lease 必須）。M10。
        let mut rb = activate_req("profile-a");
        rb.kind = "rollback".to_string();
        let id = submit(&mut journal, rb).expect("rollback").id;
        journal.succeed(&id).expect("succeed");
        // 全 job が Succeeded。状態一致（全 kind が揃う）。M10。all() は
        // ID 順（アルファベット順）で返すため、集合で検証する。M10。
        let jobs = journal.all();
        assert_eq!(jobs.len(), 7);
        assert!(jobs.iter().all(|j| j.phase == JobPhase::Succeeded));
        let mut kinds: Vec<&str> = jobs.iter().map(|j| j.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            vec![
                "activate", "build", "download", "fetch", "rollback", "stage", "verify"
            ]
        );
    }

    /// 受入: cancel 後 poll → terminal 保持。M10。
    #[test]
    fn cancel_keeps_terminal_phase() {
        let mut journal = JobJournal::new();
        let id = submit(&mut journal, req("fetch", "src-b"))
            .expect("submit")
            .id;
        cancel(&mut journal, &id).expect("cancel");
        let job = journal.get(&id).expect("job");
        assert_eq!(job.phase, JobPhase::Cancelling);
        // poll しても terminal（Cancelling）を保持。M10。
        let dto = get(&journal, &id).expect("get");
        assert_eq!(dto.phase, "cancelling");
    }

    /// 受入: 不正 job / unknown field → 400。M10。
    #[test]
    fn invalid_job_and_unknown_field_rejected() {
        let mut journal = JobJournal::new();
        // unknown kind → 400。M10。
        let err = submit(&mut journal, req("publish", "x")).expect_err("unknown kind");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        // unknown field（deny_unknown_fields）→ 400。M10。
        let err = submit_json(
            &mut journal,
            r#"{"kind":"fetch","payload_key":"y","bogus":1}"#,
        )
        .expect_err("unknown field");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        // JSON 破損 → 400。M10。
        let err = submit_json(&mut journal, "not-json").expect_err("bad json");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
    }

    /// 受入: activate busy → 409。M10。
    #[test]
    fn activate_busy_conflicts() {
        let mut journal = JobJournal::new();
        let _ = submit(&mut journal, activate_req("profile-b")).expect("first activate");
        // 2 つ目の activate（進行中）→ 409。M10。
        let err = submit(&mut journal, activate_req("profile-c")).expect_err("busy");
        assert!(matches!(err, ManagerApiError::Conflict(_)));
    }

    /// activation/rollback は generation+lease 必須。M10。
    #[test]
    fn activate_requires_generation_and_lease() {
        let mut journal = JobJournal::new();
        // generation 0 → 400。M10。
        let mut a = activate_req("profile-d");
        a.expected_generation = 0;
        let err = submit(&mut journal, a).expect_err("gen required");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        // lease なし → 400。M10。
        let mut a = activate_req("profile-e");
        a.runtime_lease = None;
        let err = submit(&mut journal, a).expect_err("lease required");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
    }

    /// DTO は secret/raw build log を含まない。M10。
    #[test]
    fn dto_excludes_secret_and_raw_build_log() {
        let mut journal = JobJournal::new();
        let id = submit(&mut journal, req("build", "src-c"))
            .expect("submit")
            .id;
        let dto = get(&journal, &id).expect("get");
        let json = serde_json::to_string(&dto).expect("json");
        // DTO に secret / build log フィールドが無い。M10。
        assert!(!json.contains("secret"));
        assert!(!json.contains("api_key"));
        assert!(!json.contains("build_log"));
        assert!(!json.contains("token"));
    }
}
