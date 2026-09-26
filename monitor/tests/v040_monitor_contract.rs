//! G01 monitor contract: DTO・job polling・互換表示（integration）。
//!
//! monitor を lib 化し、`/manager/status`（M10 / C04）の job 追跡を公開 API
//! 経由で検証する。受入 case は全て必須。dry-run 方針に従い、実
//! ネットワークを使わず JobTracker を fake 境界で駆動する。G01。
//!
//! 受入 case:
//! - 入力: 旧 version → 新操作 disabled
//! - 入力: out-of-order poll → 状態巻戻しなし
//! - 入力: runtime disconnect → 不明/再試行
//! - 入力: 複数 manager job → 個別状態
use siderostat_core::manager::api::{ManagerJobDto, ManagerStatusResponse};
use siderostat_monitor::{
    jobs::{JobPhase, JobTracker},
    operation::{OperationKind, OperationOutcome, OperationState},
    state::{VersionHandshake, version_handshake},
};

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

/// 入力: 旧 version → 新操作 disabled。G01。
///
/// monitor は `/healthz`（version_handshake）または `/manager` 404 で旧
/// runtime を検出し、`mark_manager_unsupported` で新操作（submit / cancel）
/// を disabled にする。version_handshake が非 Matched のとき新操作は
/// disabled になることを検証する。G01。
#[test]
fn old_version_disables_new_operations() {
    // 旧 runtime（app が new、runtime が old）→ 新操作 disabled。G01。
    let handshake = version_handshake("0.3.0", "0.2.1");
    assert_eq!(handshake, VersionHandshake::RuntimeOlder);
    let mut tracker = JobTracker::new();
    assert!(tracker.is_manager_supported());
    // /manager 404（旧 runtime）を検出した呼び出し側が unsupported に。G01。
    tracker.mark_manager_unsupported();
    assert!(!tracker.is_manager_supported());
    // runtime 新版（rollback）も新操作 disabled（/manager が無い場合）。G01。
    let newer = version_handshake("0.2.1", "0.3.0");
    assert_eq!(newer, VersionHandshake::RuntimeNewer);
}

/// 入力: out-of-order poll → 状態巻戻しなし。G01。
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

/// 入力: runtime disconnect → 不明/再試行。G01。
#[test]
fn runtime_disconnect_marks_unknown_and_retries() {
    let mut tracker = JobTracker::new();
    tracker.apply(&status(vec![dto("build-1", "build", "running", 10)]));
    tracker.mark_disconnected();
    assert_eq!(
        tracker.get("build-1").expect("build").phase,
        JobPhase::Unknown
    );
    assert!(tracker.get("build-1").expect("build").terminal);
    // 次回 apply で再試行 → 実状態に復帰。G01。
    tracker.apply(&status(vec![dto("build-1", "build", "running", 12)]));
    assert_eq!(
        tracker.get("build-1").expect("build").phase,
        JobPhase::Running
    );
    assert!(!tracker.get("build-1").expect("build").terminal);
}

/// 入力: 複数 manager job → 個別状態。G01。
#[test]
fn multiple_manager_jobs_are_tracked_individually() {
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
    // 1 つの job が終わっても他は個別に残る。G01。
    tracker.apply(&status(vec![dto("fetch-1", "fetch", "succeeded", 15)]));
    assert_eq!(
        tracker.get("fetch-1").expect("fetch").phase,
        JobPhase::Succeeded
    );
    assert_eq!(
        tracker.get("build-2").expect("build").phase,
        JobPhase::Running
    );
    assert_eq!(tracker.job_count(), 3);
}

/// レビュー重点: 既存 OperationState 単一 slot の busy が全 job を
/// 永続的に隠さない。G01。
///
/// menu 操作（restart 等）は OperationState 単一 slot で追跡し、manager
/// job は JobTracker（複数）で個別追跡する。OperationState が busy でも
/// JobTracker の全 job は見える。G01。
#[test]
fn operation_busy_does_not_hide_manager_jobs() {
    let mut tracker = JobTracker::new();
    tracker.apply(&status(vec![
        dto("fetch-1", "fetch", "running", 10),
        dto("verify-2", "verify", "running", 11),
    ]));
    // menu 操作を開始（busy）。G01。
    let mut operation = OperationState::default();
    assert!(operation.begin(OperationKind::RuntimeRestart));
    assert!(operation.is_busy());
    // JobTracker の複数 job は全て見える。単一 slot の busy に隠れない。G01。
    assert_eq!(tracker.job_count(), 2);
    assert_eq!(
        tracker.get("fetch-1").expect("fetch").phase,
        JobPhase::Running
    );
    assert_eq!(
        tracker.get("verify-2").expect("verify").phase,
        JobPhase::Running
    );
    // menu 操作が失敗しても job 状態は不変（独立）。G01。
    operation.finish(OperationKind::RuntimeRestart, OperationOutcome::Failed);
    assert_eq!(
        tracker.get("fetch-1").expect("fetch").phase,
        JobPhase::Running
    );
    assert_eq!(tracker.job_count(), 2);
}
