//! DS4 Manager — job journal。M01。
//!
//! C04 の `ManagerJob = id/kind/progress/phase/error/time/cancel` に基づき、
//! managed job を記録・管理する。同一 payload の進行中 job は同 ID を返す
//! （受入 case: 重複 job → 同 ID）。kind は fetch/build/download/verify/
//! stage/activate/rollback。M01。
//!
//! 受入 case（全て必須）:
//! - 入力: 重複 job → 同 ID
//!
//! 契約: CONTRACTS.md C04 / ManagerJob。M01。

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// job の種類。C04: fetch/build/download/verify/stage/activate/rollback。M01。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// 公式 source の fetch。
    Fetch,
    /// 隔離 build。
    Build,
    /// bounded download。
    Download,
    /// full hash / compatibility 検証。
    Verify,
    /// stage 準備。
    Stage,
    /// 両 node digest/epoch prepare 後の activation。
    Activate,
    /// previous への rollback。
    Rollback,
}

impl JobKind {
    /// kind 名（snake_case）。C04 の payload kind と一致。M01。
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Fetch => "fetch",
            JobKind::Build => "build",
            JobKind::Download => "download",
            JobKind::Verify => "verify",
            JobKind::Stage => "stage",
            JobKind::Activate => "activate",
            JobKind::Rollback => "rollback",
        }
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// job の phase。M01。progress と併せて進捗を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    /// 実行待ち / 実行中。
    Running,
    /// 成功。
    Succeeded,
    /// 失敗。
    Failed,
    /// キャンセル要求済み。
    Cancelling,
}

/// managed job。C04 の `ManagerJob`。M01。
///
/// payload の同一性で重複を判定する。同一 payload の進行中 job は同 ID を
/// 返す（受入 case: 重複 job → 同 ID）。M01。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagerJob {
    /// job ID（決定論的）。M01。
    pub id: String,
    /// job の種類。M01。
    pub kind: JobKind,
    /// 進捗（0..=100）。M01。
    pub progress: u8,
    /// 現在の phase。M01。
    pub phase: JobPhase,
    /// 失敗時の error message（成功時は空）。M01。
    pub error: String,
    /// 生成時刻（epoch secs）。M01。
    pub created_at: u64,
    /// 最終更新時刻（epoch secs）。M01。
    pub updated_at: u64,
    /// キャンセル要求フラグ。M01。
    pub cancel: bool,
}

impl ManagerJob {
    /// 新しい進行中 job を作る。M01。
    pub fn new(id: String, kind: JobKind) -> Self {
        let now = now_secs();
        Self {
            id,
            kind,
            progress: 0,
            phase: JobPhase::Running,
            error: String::new(),
            created_at: now,
            updated_at: now,
            cancel: false,
        }
    }

    /// progress を更新する。M01。
    pub fn set_progress(&mut self, progress: u8) {
        self.progress = progress.min(100);
        self.updated_at = now_secs();
    }

    /// 成功へ遷移する。M01。
    pub fn succeed(&mut self) {
        self.phase = JobPhase::Succeeded;
        self.progress = 100;
        self.updated_at = now_secs();
    }

    /// 失敗へ遷移する。M01。
    pub fn fail(&mut self, error: impl Into<String>) {
        self.phase = JobPhase::Failed;
        self.error = error.into();
        self.updated_at = now_secs();
    }

    /// キャンセル要求する。M01。
    pub fn request_cancel(&mut self) {
        self.cancel = true;
        self.phase = JobPhase::Cancelling;
        self.updated_at = now_secs();
    }
}

/// 現在の epoch secs。テストでは時刻を注入できるよう分離する。M01。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// job journal エラー。M01。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerJobError {
    /// 不明な kind。
    UnknownKind,
    /// 進行中 job が無い。
    NotFound,
}

impl std::fmt::Display for ManagerJobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerJobError::UnknownKind => write!(f, "unknown job kind"),
            ManagerJobError::NotFound => write!(f, "job not found"),
        }
    }
}

impl std::error::Error for ManagerJobError {}

/// job journal。同一 payload の進行中 job を同 ID で返す。M01。
///
/// 重複判定（受入 case: 重複 job → 同 ID）: `kind` + payload の正規化
/// 文字列が同一の進行中 job が既にあれば、その job の ID を返す。
/// payload は fetch/build/download/verify/stage/activate/rollback の各
/// 入力（URL・source ref・artifact ID 等）。M01。
#[derive(Debug, Default)]
pub struct JobJournal {
    jobs: HashMap<String, ManagerJob>,
    /// 進行中（Running/Cancelling）の重複キー → job ID。M01。
    running_by_key: HashMap<(JobKind, String), String>,
    /// 次 ID カウンタ（決定論的 ID のための連番）。M01。
    next_id: u64,
}

impl JobJournal {
    /// 新しい journal。M01。
    pub fn new() -> Self {
        Self::default()
    }

    /// job を作成または既存の進行中 job を返す。M01。
    ///
    /// `payload_key` は同一作業を表す正規化キー。同一キーの進行中 job が
    /// 既にあれば同 ID を返す（重複 job → 同 ID）。M01。
    pub fn enqueue(&mut self, kind: JobKind, payload_key: &str) -> Result<String, ManagerJobError> {
        let key = (kind, payload_key.to_string());
        if let Some(id) = self.running_by_key.get(&key) {
            // 進行中 job が既にある → 同 ID を返す。M01。
            return Ok(id.clone());
        }
        let id = format!("{}-{}", kind.as_str(), self.next_id);
        self.next_id += 1;
        let job = ManagerJob::new(id.clone(), kind);
        self.running_by_key.insert(key, job.id.clone());
        self.jobs.insert(id.clone(), job);
        Ok(id)
    }

    /// 指定 ID の job を可変参照で取得する。M01。
    pub fn get_mut(&mut self, id: &str) -> Option<&mut ManagerJob> {
        self.jobs.get_mut(id)
    }

    /// 指定 ID の job を取得する。M01。
    pub fn get(&self, id: &str) -> Option<&ManagerJob> {
        self.jobs.get(id)
    }

    /// ID が journal に登録された kind と payload key の組に一致するか。
    pub fn matches_request(&self, id: &str, kind: JobKind, payload_key: &str) -> bool {
        self.jobs.get(id).is_some_and(|job| job.kind == kind)
            && self
                .running_by_key
                .get(&(kind, payload_key.to_string()))
                .is_some_and(|registered_id| registered_id == id)
    }

    /// 全 job を ID 順で返す。M01。
    pub fn all(&self) -> Vec<&ManagerJob> {
        let mut v: Vec<&ManagerJob> = self.jobs.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// job を成功で閉じる。重複キーを解放する。M01。
    pub fn succeed(&mut self, id: &str) -> Result<(), ManagerJobError> {
        let kind = {
            let job = self.jobs.get_mut(id).ok_or(ManagerJobError::NotFound)?;
            job.succeed();
            job.kind
        };
        self.release_key(kind, id);
        Ok(())
    }

    /// job を失敗で閉じる。重複キーを解放する。M01。
    pub fn fail(&mut self, id: &str, error: impl Into<String>) -> Result<(), ManagerJobError> {
        let kind = {
            let job = self.jobs.get_mut(id).ok_or(ManagerJobError::NotFound)?;
            let error = error.into();
            job.fail(error);
            job.kind
        };
        self.release_key(kind, id);
        Ok(())
    }

    /// 指定 job がキャンセル中かを返す。
    pub fn is_cancelling(&self, id: &str) -> Result<bool, ManagerJobError> {
        let job = self.jobs.get(id).ok_or(ManagerJobError::NotFound)?;
        Ok(job.phase == JobPhase::Cancelling)
    }

    /// 実行中の job だけを成功で閉じる。遅れて届いた成功は状態を変えない。
    pub fn succeed_if_running(&mut self, id: &str) -> Result<bool, ManagerJobError> {
        let job = self.jobs.get(id).ok_or(ManagerJobError::NotFound)?;
        if job.phase != JobPhase::Running {
            return Ok(false);
        }
        self.succeed(id)?;
        Ok(true)
    }

    /// 実行中またはキャンセル中の job だけを失敗で閉じる。
    pub fn fail_if_running(
        &mut self,
        id: &str,
        error: impl Into<String>,
    ) -> Result<bool, ManagerJobError> {
        let job = self.jobs.get(id).ok_or(ManagerJobError::NotFound)?;
        if !matches!(job.phase, JobPhase::Running | JobPhase::Cancelling) {
            return Ok(false);
        }
        self.fail(id, error)?;
        Ok(true)
    }

    /// キャンセル要求する。重複キーは保持（キャンセル中も進行扱い）。M01。
    pub fn request_cancel(&mut self, id: &str) -> Result<(), ManagerJobError> {
        let job = self.jobs.get_mut(id).ok_or(ManagerJobError::NotFound)?;
        job.request_cancel();
        Ok(())
    }

    /// 指定 kind の進行中（Running/Cancelling）job が存在するか。M01。
    pub fn has_running(&self, kind: JobKind) -> bool {
        self.jobs
            .values()
            .any(|j| j.kind == kind && matches!(j.phase, JobPhase::Running | JobPhase::Cancelling))
    }

    /// job 完了時に重複キーを解放する。M01。
    fn release_key(&mut self, kind: JobKind, id: &str) {
        // kind が一致し、ID が一致する進行中エントリだけを除去する。
        // （終了した job が別 payload の新規 job の重複判定を誤らないように）
        let stale: Vec<(JobKind, String)> = self
            .running_by_key
            .iter()
            .filter(|(_, v)| **v == id)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(existing_id) = self.running_by_key.get(&k) {
                if existing_id == id {
                    self.running_by_key.remove(&k);
                }
            }
        }
        let _ = kind;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_job_returns_same_id() {
        let mut journal = JobJournal::new();
        let first_id = journal
            .enqueue(JobKind::Fetch, "repo@commit")
            .expect("first");
        // 同一 payload の進行中 job → 同 ID。M01。
        let dup_id = journal.enqueue(JobKind::Fetch, "repo@commit").expect("dup");
        assert_eq!(dup_id, first_id);
    }

    #[test]
    fn completed_job_frees_duplicate_key() {
        let mut journal = JobJournal::new();
        let first_id = journal.enqueue(JobKind::Build, "src@flags").expect("first");
        journal.succeed(&first_id).expect("succeed");
        // 完了後は同じ payload で新しい job が作れる（別 ID）。M01。
        let second_id = journal
            .enqueue(JobKind::Build, "src@flags")
            .expect("second");
        assert_ne!(second_id, first_id);
    }

    #[test]
    fn different_payload_same_kind_distinct() {
        let mut journal = JobJournal::new();
        let a = journal.enqueue(JobKind::Download, "model-a").expect("a");
        let b = journal.enqueue(JobKind::Download, "model-b").expect("b");
        assert_ne!(a, b);
    }

    #[test]
    fn same_payload_different_kind_has_distinct_jobs() {
        let mut journal = JobJournal::new();
        let build = journal
            .enqueue(JobKind::Build, "shared-key")
            .expect("build");
        let verify = journal
            .enqueue(JobKind::Verify, "shared-key")
            .expect("verify");
        assert_ne!(build, verify);
        assert_eq!(journal.get(&build).unwrap().kind, JobKind::Build);
        assert_eq!(journal.get(&verify).unwrap().kind, JobKind::Verify);
        assert!(journal.matches_request(&build, JobKind::Build, "shared-key"));
        assert!(!journal.matches_request(&build, JobKind::Verify, "shared-key"));
        assert!(!journal.matches_request(&build, JobKind::Build, "other-key"));
        journal.succeed(&build).expect("complete build");
        assert_eq!(
            journal
                .enqueue(JobKind::Verify, "shared-key")
                .expect("duplicate verify"),
            verify
        );
        assert_ne!(
            journal
                .enqueue(JobKind::Build, "shared-key")
                .expect("new build"),
            build
        );
    }

    #[test]
    fn job_lifecycle_transitions() {
        let mut journal = JobJournal::new();
        let id = journal.enqueue(JobKind::Stage, "artifact").expect("job");
        journal.get_mut(&id).expect("job").set_progress(50);
        assert_eq!(journal.get(&id).expect("job").progress, 50);
        journal.succeed(&id).expect("succeed");
        let job = journal.get(&id).expect("job");
        assert_eq!(job.phase, JobPhase::Succeeded);
        assert_eq!(job.progress, 100);
    }

    #[test]
    fn terminal_guard_rejects_late_success_after_cancel() {
        let mut journal = JobJournal::new();
        let id = journal
            .enqueue(JobKind::Build, "build-key")
            .expect("enqueue");
        journal.request_cancel(&id).expect("cancel");
        assert!(journal.is_cancelling(&id).expect("lookup"));
        assert!(!journal.succeed_if_running(&id).expect("lookup"));
        assert_eq!(journal.get(&id).expect("job").phase, JobPhase::Cancelling);
    }

    #[test]
    fn terminal_guard_closes_cancelling_job_as_failure_once() {
        let mut journal = JobJournal::new();
        let id = journal
            .enqueue(JobKind::Build, "build-key")
            .expect("enqueue");
        journal.request_cancel(&id).expect("cancel");
        assert!(journal.fail_if_running(&id, "canceled").expect("lookup"));
        assert!(!journal.fail_if_running(&id, "late error").expect("lookup"));
        let job = journal.get(&id).expect("job");
        assert_eq!(job.phase, JobPhase::Failed);
        assert_eq!(job.error, "canceled");
        assert_ne!(
            journal
                .enqueue(JobKind::Build, "build-key")
                .expect("enqueue"),
            id
        );
    }

    #[test]
    fn terminal_guard_closes_running_job_as_success_once() {
        let mut journal = JobJournal::new();
        let id = journal
            .enqueue(JobKind::Build, "build-key")
            .expect("enqueue");
        assert!(!journal.is_cancelling(&id).expect("lookup"));
        assert!(journal.succeed_if_running(&id).expect("lookup"));
        assert!(!journal.fail_if_running(&id, "late error").expect("lookup"));
        let job = journal.get(&id).expect("job");
        assert_eq!(job.phase, JobPhase::Succeeded);
        assert_eq!(job.progress, 100);
    }
}
