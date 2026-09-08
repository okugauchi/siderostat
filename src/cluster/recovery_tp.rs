//! TP（Tensor Parallelism）障害と有限 recovery（C02 障害表・C03 lease）。
//!
//! - worker/coordinator crash、RDMA route loss、role swap、first prefill/bulk timeout、
//!   model mismatch を有限ラベルに分類する。
//! - 同一 incident の再試行は最大 `max_attempts`（既定 2）回。上限到達・検証できない
//!   fallback・identity 不明は ManualIntervention へ。
//! - ForcedStandalone では TP retry を 0 にする（policy latch を尊重）。
//! - 同時の peer-loss と route-loss は単一の recovery owner が直列化する（二重起動なし）。
//! - 即座に新規 route を閉じ、drain 後に owned child を停止し、検証済み fallback へ戻す。
//!
//! 本モジュールは純粋な分類・判定のみ。実際の child 停止・fallback 起動は T11 が所有する。

use crate::cluster::{ClusterFailure, OperationPolicy};

/// TP 障害の有限分類（C02 障害表）。実プロセス観測（child exit / route loss / ログ）に
/// 由来する。固定値禁止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpFailureKind {
    /// worker child crash / early exit。
    WorkerCrash,
    /// coordinator child crash / early exit。
    CoordinatorCrash,
    /// RDMA route loss / DS4 接続断。。
    RouteLoss,
    /// role swap / child 交換による旧セッション。。
    RoleSwap,
    /// first prefill / bulk round timeout。専用 failure（FirstPrefillTimeout）。。
    FirstPrefillTimeout,
    /// model / source 不一致（unsupported 含む）。。
    ModelMismatch,
    /// identity 不明 / PID 再利用。signal 禁止・manual。。
    IdentityUnknown,
    /// drain timeout。既存 stream の所有保持、Failed。強制 kill へ昇格しない。。
    DrainTimeout,
}

impl TpFailureKind {
    pub fn name(self) -> &'static str {
        match self {
            TpFailureKind::WorkerCrash => "worker-crash",
            TpFailureKind::CoordinatorCrash => "coordinator-crash",
            TpFailureKind::RouteLoss => "route-loss",
            TpFailureKind::RoleSwap => "role-swap",
            TpFailureKind::FirstPrefillTimeout => "first-prefill-timeout",
            TpFailureKind::ModelMismatch => "model-mismatch",
            TpFailureKind::IdentityUnknown => "identity-unknown",
            TpFailureKind::DrainTimeout => "drain-timeout",
        }
    }

    /// 対応する ClusterFailure ラベル。診断・persist に使う。。
    pub fn cluster_failure(self) -> ClusterFailure {
        match self {
            TpFailureKind::WorkerCrash
            | TpFailureKind::CoordinatorCrash
            | TpFailureKind::RouteLoss
            | TpFailureKind::RoleSwap => ClusterFailure::RouteIncomplete,
            TpFailureKind::FirstPrefillTimeout => ClusterFailure::FirstPrefillTimeout,
            TpFailureKind::ModelMismatch => ClusterFailure::DeploymentMismatch,
            TpFailureKind::IdentityUnknown => ClusterFailure::ChildIdentityUnknown,
            TpFailureKind::DrainTimeout => ClusterFailure::DrainTimeout,
        }
    }
}

/// TP recovery の判定結果（C02 障害表の動作列）。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpRecoveryDecision {
    /// Automatic + 試行残あり → 新 session で worker 先行再起動（backoff）。。
    Retry,
    /// 試行上限到達 → 検証済み fallback へ（PairedStandalone）。。。
    StopAndFallback,
    /// 検証できない fallback / identity 不明 / 復旧不能 → ManualIntervention。。
    ManualIntervention,
    /// ForcedStandalone → TP retry 0、TP/pair/promote 禁止。。
    Suppressed,
}

/// 有限 recovery の純粋状態機械。同一 incident の再試行上限と policy latch を判定する。
/// 実プロセス操作は行わない（T11 が所有）。単一 owner として直列化する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TpRecoveryTracker {
    /// 同一 incident で試行した回数。
    attempts: u32,
    /// 同一 incident の最大再試行回数（既定 2）。上限到達で StopAndFallback / Manual。。
    max_attempts: u32,
    /// 現在 incident が active か。healthy/fallback で解除。
    incident_active: bool,
}

impl TpRecoveryTracker {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            attempts: 0,
            max_attempts: max_attempts.max(1),
            incident_active: false,
        }
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn incident_active(&self) -> bool {
        self.incident_active
    }

    /// 障害を分類して recovery 判定を返す。policy latch（ForcedStandalone）を尊重する。。
    ///
    /// - ForcedStandalone → Suppressed（TP retry 0、TP spawn 抑止）。。
    /// - model mismatch / identity 不明 → 即 ManualIntervention（signal 禁止）。。
    /// - drain timeout → ManualIntervention（既存 stream の所有保持、強制 kill 昇格なし）。。
    /// - それ以外の transient 障害 → 試行残があれば Retry、なければ StopAndFallback。。
    pub fn classify(&mut self, kind: TpFailureKind, policy: OperationPolicy) -> TpRecoveryDecision {
        if policy == OperationPolicy::ForcedStandalone {
            return TpRecoveryDecision::Suppressed;
        }
        self.incident_active = true;
        match kind {
            TpFailureKind::ModelMismatch
            | TpFailureKind::IdentityUnknown
            | TpFailureKind::DrainTimeout => TpRecoveryDecision::ManualIntervention,
            TpFailureKind::WorkerCrash
            | TpFailureKind::CoordinatorCrash
            | TpFailureKind::RouteLoss
            | TpFailureKind::RoleSwap
            | TpFailureKind::FirstPrefillTimeout => {
                if self.attempts < self.max_attempts {
                    self.attempts += 1;
                    TpRecoveryDecision::Retry
                } else {
                    TpRecoveryDecision::StopAndFallback
                }
            }
        }
    }

    /// 検証済み fallback へ戻ったとき、incident を解除する。。
    pub fn note_healthy_fallback(&mut self) {
        self.attempts = 0;
        self.incident_active = false;
    }

    /// fallback の検証に失敗したとき、ManualIntervention へ移行する。。
    pub fn note_unverifiable_fallback(&mut self) -> TpRecoveryDecision {
        self.attempts = 0;
        self.incident_active = false;
        TpRecoveryDecision::ManualIntervention
    }
}

impl Default for TpRecoveryTracker {
    fn default() -> Self {
        Self::new(2)
    }
}

/// TP recovery owner の直列化。同時の peer-loss と route-loss を単一 owner が処理する。
/// 二重起動（mode 切替と recovery が別 lock で child を二重起動）を防ぐ。
#[derive(Debug, Default)]
pub struct TpRecoveryOwner {
    lock: tokio::sync::Mutex<()>,
}

impl TpRecoveryOwner {
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock.lock().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid_policy() -> OperationPolicy {
        OperationPolicy::Automatic
    }

    #[test]
    fn transient_faults_retry_then_stop_and_fallback() {
        for kind in [
            TpFailureKind::WorkerCrash,
            TpFailureKind::CoordinatorCrash,
            TpFailureKind::RouteLoss,
            TpFailureKind::RoleSwap,
            TpFailureKind::FirstPrefillTimeout,
        ] {
            let mut t = TpRecoveryTracker::new(2);
            assert_eq!(t.classify(kind, sid_policy()), TpRecoveryDecision::Retry);
            assert_eq!(t.attempts(), 1);
            assert_eq!(t.classify(kind, sid_policy()), TpRecoveryDecision::Retry);
            assert_eq!(t.attempts(), 2);
            // 上限到達 → StopAndFallback（検証済み fallback）。。。。
            assert_eq!(
                t.classify(kind, sid_policy()),
                TpRecoveryDecision::StopAndFallback
            );
            assert_eq!(t.attempts(), 2);
        }
    }

    #[test]
    fn model_mismatch_and_identity_unknown_are_manual() {
        let mut tracker = TpRecoveryTracker::new(2);
        assert_eq!(
            tracker.classify(TpFailureKind::ModelMismatch, sid_policy()),
            TpRecoveryDecision::ManualIntervention
        );
        assert_eq!(
            tracker.classify(TpFailureKind::IdentityUnknown, sid_policy()),
            TpRecoveryDecision::ManualIntervention
        );
        assert_eq!(
            tracker.classify(TpFailureKind::DrainTimeout, sid_policy()),
            TpRecoveryDecision::ManualIntervention
        );
    }

    #[test]
    fn forced_standalone_suppresses_tp_retry() {
        let mut tracker = TpRecoveryTracker::new(2);
        assert_eq!(
            tracker.classify(
                TpFailureKind::WorkerCrash,
                OperationPolicy::ForcedStandalone
            ),
            TpRecoveryDecision::Suppressed
        );
        // ForcedStandalone は試行回数を消費しない（TP retry 0）。。。
        assert_eq!(tracker.attempts(), 0);
        assert!(!tracker.incident_active());
    }

    #[test]
    fn healthy_fallback_resets_incident() {
        let mut tracker = TpRecoveryTracker::new(2);
        assert_eq!(
            tracker.classify(TpFailureKind::RouteLoss, sid_policy()),
            TpRecoveryDecision::Retry
        );
        tracker.note_healthy_fallback();
        assert_eq!(tracker.attempts(), 0);
        assert!(!tracker.incident_active());
        // 新 incident は再び retry できる。
        assert_eq!(
            tracker.classify(TpFailureKind::RouteLoss, sid_policy()),
            TpRecoveryDecision::Retry
        );
    }

    #[test]
    fn unverifiable_fallback_is_manual() {
        let mut tracker = TpRecoveryTracker::new(2);
        tracker.classify(TpFailureKind::WorkerCrash, sid_policy());
        assert_eq!(
            tracker.note_unverifiable_fallback(),
            TpRecoveryDecision::ManualIntervention
        );
        assert!(!tracker.incident_active());
    }

    #[test]
    fn failure_kind_labels_are_stable() {
        assert_eq!(TpFailureKind::WorkerCrash.name(), "worker-crash");
        assert_eq!(TpFailureKind::CoordinatorCrash.name(), "coordinator-crash");
        assert_eq!(TpFailureKind::RouteLoss.name(), "route-loss");
        assert_eq!(TpFailureKind::RoleSwap.name(), "role-swap");
        assert_eq!(
            TpFailureKind::FirstPrefillTimeout.name(),
            "first-prefill-timeout"
        );
        assert_eq!(TpFailureKind::ModelMismatch.name(), "model-mismatch");
        assert_eq!(TpFailureKind::IdentityUnknown.name(), "identity-unknown");
        assert_eq!(TpFailureKind::DrainTimeout.name(), "drain-timeout");
    }
}
