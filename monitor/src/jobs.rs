//! DS4 Manager job tracking for the monitor（G01）。
//!
//! The monitor polls the authenticated `/manager/status` endpoint (M10 / C04)
//! and reflects each manager job individually. This tracker is deliberately
//! independent of the menu `OperationState` single-slot model so a busy menu
//! operation never hides multiple manager jobs（レビュー重点）。G01。
//!
//! 受入 case:
//! - 旧 version → 新操作 disabled（manager_supported=false）G01。
//! - out-of-order poll → 状態巻戻しなし（updated_at 比較）G01。
//! - runtime disconnect → 不明（Unknown）で再試行 G01。
//! - 複数 manager job → 個別状態（BTreeMap で個別追跡）G01。
use siderostat_core::manager::api::ManagerStatusResponse;
use std::collections::BTreeMap;

/// manager job の表示 phase。Unknown は runtime disconnect 中の「不明」。G01。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JobPhase {
    Running,
    Succeeded,
    Failed,
    Cancelling,
    Unknown,
}

/// manager job の表示用ビュー。secret / raw build log を含まない。G01。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub phase: JobPhase,
    pub progress: u8,
    pub updated_at: u64,
    pub cancel: bool,
    pub terminal: bool,
}

/// 複数 manager job を個別に追跡する tracker。OperationState の単一 slot
/// には依存しない（レビュー重点）。G01。
#[derive(Debug, Clone, Default)]
pub struct JobTracker {
    jobs: BTreeMap<String, JobView>,
    /// 旧 runtime で `/manager` API が無いとき false。新操作（submit /
    /// cancel）を disabled 表示する。G01。
    manager_supported: bool,
}

impl JobPhase {
    /// `/manager/status` の phase 文字列（snake_case）から JobPhase へ。
    /// 未知の文字列は Unknown にする（旧 runtime の応答差異で落とさない）。G01。
    pub fn from_wire(value: &str) -> JobPhase {
        match value {
            "running" => JobPhase::Running,
            "succeeded" => JobPhase::Succeeded,
            "failed" => JobPhase::Failed,
            "cancelling" => JobPhase::Cancelling,
            _ => JobPhase::Unknown,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobPhase::Succeeded | JobPhase::Failed | JobPhase::Cancelling | JobPhase::Unknown
        )
    }
}

impl JobView {
    fn from_dto(job: &siderostat_core::manager::api::ManagerJobDto) -> Self {
        let phase = JobPhase::from_wire(&job.phase);
        Self {
            id: job.id.clone(),
            kind: job.kind.clone(),
            phase,
            progress: job.progress,
            updated_at: job.updated_at,
            cancel: job.cancel,
            terminal: phase.is_terminal(),
        }
    }
}

impl JobTracker {
    pub fn new() -> Self {
        Self {
            jobs: BTreeMap::new(),
            manager_supported: true,
        }
    }

    /// `/manager/status` の成功応答を反映する。out-of-order poll は各 job の
    /// updated_at で比較し、古い結果で現在の状態を巻き戻さない。G01。
    pub fn apply(&mut self, status: &ManagerStatusResponse) {
        self.manager_supported = true;
        for job in &status.jobs {
            let incoming = JobView::from_dto(job);
            let replace = match self.jobs.get(&incoming.id) {
                None => true,
                Some(existing) => incoming.updated_at >= existing.updated_at,
            };
            if replace {
                self.jobs.insert(incoming.id.clone(), incoming);
            }
        }
    }

    /// runtime disconnect 時、全 job を不明（Unknown）にして再試行させる。
    /// 次回 `apply` で実状態に復帰する。G01。
    pub fn mark_disconnected(&mut self) {
        for view in self.jobs.values_mut() {
            if !view.terminal {
                view.phase = JobPhase::Unknown;
                view.terminal = true;
            }
        }
    }

    /// `/manager` API が 404 を返す旧 runtime を検出した。新操作を
    /// disabled にする。G01。
    pub fn mark_manager_unsupported(&mut self) {
        self.manager_supported = false;
    }

    /// 新操作（manager job の submit / cancel）が enabled か。旧 runtime
    /// （/manager 無し）では disabled。G01。
    pub fn is_manager_supported(&self) -> bool {
        self.manager_supported
    }

    /// 個別 job を取得。G01。
    pub fn get(&self, id: &str) -> Option<&JobView> {
        self.jobs.get(id)
    }

    /// 全 job を id 順で返す。複数 job を個別に保持する。G01。
    pub fn jobs(&self) -> impl Iterator<Item = &JobView> {
        self.jobs.values()
    }

    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use siderostat_core::manager::api::ManagerJobDto;

    fn dto(id: &str, kind: &str, phase: &str, updated_at: u64) -> ManagerJobDto {
        ManagerJobDto {
            id: id.to_string(),
            kind: kind.to_string(),
            progress: 0,
            phase: phase.to_string(),
            error: String::new(),
            created_at: updated_at,
            updated_at,
            cancel: false,
        }
    }

    fn status(jobs: Vec<ManagerJobDto>) -> ManagerStatusResponse {
        let queue_depth = jobs.len();
        ManagerStatusResponse {
            jobs,
            active_digest: None,
            queue_depth,
        }
    }

    /// 複数 manager job を個別に追跡する（単一 slot の busy に押し込まない）。G01。
    #[test]
    fn multiple_jobs_tracked_individually() {
        let mut tracker = JobTracker::new();
        tracker.apply(&status(vec![
            dto("fetch-1", "fetch", "running", 10),
            dto("build-2", "build", "running", 12),
            dto("download-3", "download", "succeeded", 8),
        ]));
        assert_eq!(tracker.job_count(), 3);
        assert_eq!(
            tracker.get("fetch-1").expect("fetch").phase,
            JobPhase::Running
        );
        assert_eq!(
            tracker.get("build-2").expect("build").phase,
            JobPhase::Running
        );
        assert_eq!(
            tracker.get("download-3").expect("download").phase,
            JobPhase::Succeeded
        );
        // 1 つの job が終わっても他の job の状態は変わらない。G01。
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "succeeded", 15)]));
        assert_eq!(
            tracker.get("fetch-1").expect("fetch").phase,
            JobPhase::Succeeded
        );
        assert_eq!(
            tracker.get("build-2").expect("build").phase,
            JobPhase::Running
        );
    }

    /// out-of-order poll（古い updated_at の結果）で状態を巻き戻さない。G01。
    #[test]
    fn out_of_order_poll_does_not_rewind() {
        let mut tracker = JobTracker::new();
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "running", 20)]));
        // 古いスナップショット（updated_at 15）は無視される。G01。
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "running", 15)]));
        assert_eq!(tracker.get("fetch-1").expect("fetch").updated_at, 20);
        // 新しいスナップショットで進行。G01。
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "succeeded", 25)]));
        assert_eq!(
            tracker.get("fetch-1").expect("fetch").phase,
            JobPhase::Succeeded
        );
        assert_eq!(tracker.get("fetch-1").expect("fetch").updated_at, 25);
    }

    /// runtime disconnect で job は不明（Unknown）になり、次回 poll で
    /// 復帰する。G01。
    #[test]
    fn disconnect_marks_unknown_and_recovers_on_next_poll() {
        let mut tracker = JobTracker::new();
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "running", 10)]));
        tracker.mark_disconnected();
        assert_eq!(
            tracker.get("fetch-1").expect("fetch").phase,
            JobPhase::Unknown
        );
        assert!(tracker.get("fetch-1").expect("fetch").terminal);
        // 次回 apply で実状態に復帰。G01。
        tracker.apply(&status(vec![dto("fetch-1", "fetch", "running", 12)]));
        assert_eq!(
            tracker.get("fetch-1").expect("fetch").phase,
            JobPhase::Running
        );
        assert!(!tracker.get("fetch-1").expect("fetch").terminal);
    }

    /// 旧 runtime（/manager 無し）で新操作（submit/cancel）は disabled。G01。
    #[test]
    fn old_runtime_disables_new_operations() {
        let mut tracker = JobTracker::new();
        assert!(tracker.is_manager_supported());
        tracker.mark_manager_unsupported();
        assert!(!tracker.is_manager_supported());
    }

    /// phase 文字列の未知値は Unknown に落ち、monitor を落とさない。G01。
    #[test]
    fn unknown_wire_phase_falls_back_to_unknown() {
        assert_eq!(JobPhase::from_wire("running"), JobPhase::Running);
        assert_eq!(JobPhase::from_wire("succeeded"), JobPhase::Succeeded);
        assert_eq!(JobPhase::from_wire("weird-old-value"), JobPhase::Unknown);
    }
}
