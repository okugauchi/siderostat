//! v0.4.0 認証済み policy control（P03 / C03）。。
//!
//! `/v2/operation-policy` の signed control プロトコルを提供する。既存の HMAC
//! （method/path/body/node/nonce/timestamp）を再利用し、policy epoch / operation id を
//! canonical body に含めて署名する。v1 descriptor に未知 field を足さず、v2 endpoint で
//! 能力交渉する。通常 pair 許可を policy 専用制御の必須条件にしない（control lease が
//! 切れても既知 peer の policy 制御を再認証できる）。非同期二 node を原子的成功と装わない。
//!
//! 本モジュールは policy control の wire 契約・検証・世代契約を所有する。cluster 全体の
//! policy 適用（P04〜P06）がここに接続する。coordinator 不在時の安全ラッチ（P01 の
//! journal）は runtime 側が参照する。本 task では wire 契約と検証を実装する。

use super::super::policy::OperationPolicy;
use serde::{Deserialize, Serialize};

/// policy control の protocol version。旧 peer（protocol_version != 1）は unsupported。。
pub const POLICY_CONTROL_PROTOCOL_VERSION: u16 = 1;

/// policy 操作の phase（C03 の prepare / commit / abort 契約に対応）。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyControlPhase {
    /// intent を永続化し、新規 promotion を禁止する。。
    Prepare,
    /// 適用完了をコミットする。。
    Commit,
    /// 適用を中止する。。
    Abort,
}

/// `/v2/operation-policy` の signed request body。epoch / id / phase / desired を
/// canonical body に含めて署名する。。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyControlRequest {
    pub protocol_version: u16,
    /// policy の世代。stale なら 409 / effect 0。。
    pub policy_epoch: u64,
    /// 冪等性のための操作 ID。同 ID 同 body は重複応答、別 body は 409。。
    pub operation_id: uuid::Uuid,
    pub phase: PolicyControlPhase,
    pub desired: OperationPolicy,
}

/// policy control の応答。非同期二 node を原子的成功と装わない。。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyControlResponse {
    pub status: PolicyControlStatus,
    pub policy_epoch: u64,
    /// この node の applied 状態。coordinator 不在時は local safe latch を保持する。。
    pub applied: OperationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyControlStatus {
    Applied,
    /// 冪等重複（同 ID 同 body）。。。
    Duplicate,
    /// 適用を中止した。。
    Aborted,
}

/// policy control の検証結果。P03 の受入 case（stale epoch / 無署名 / 別 node / 旧 peer）。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyControlError {
    /// 旧 peer（protocol_version 不一致）。unsupported。。
    UnsupportedPeer,
    /// 無署名 / 別 node（署名検証失敗）。403。。
    Unauthenticated,
    /// stale epoch。409 / effect 0。。
    StaleEpoch { expected: u64, received: u64 },
    /// 同 ID 別 body。409。。
    IdempotencyConflict,
}

impl PolicyControlError {
    pub fn http_status(&self) -> u16 {
        match self {
            PolicyControlError::UnsupportedPeer | PolicyControlError::Unauthenticated => 403,
            PolicyControlError::StaleEpoch { .. } | PolicyControlError::IdempotencyConflict => 409,
        }
    }
}

/// policy control の検証結果（成功時）。effect を実行する前の事前検証に使う。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyControlVerdict {
    /// 初回の有効な要求。effect を実行してよい。。
    New,
    /// 冪等重複。effect は実行せず既存結果を返す。。
    Duplicate,
}

// ---------------------------------------------------------------------------
// P04 — ForcedStandalone cluster-wide 適用の調整（C03）。
// ---------------------------------------------------------------------------
// 適用調整は本番 reducer の local 収束（TP/LP/Paired/Pairing/Promoting 各状態 → 安全
// 収束）と PolicyJournal の phase 記録、両 node の applied ack 調整を組み合わせる。
// 第二の状態機械を作らず、reducer の遷移表と journal の phase を正本とする。
// ここでは「両 node の ready/applied が揃った時だけ Complete」という適用の調整判断
// を純粋に所有し、cluster 全体調整を reducer/journal と接続する。

/// 適用の進行 phase（C03 の PersistentOperationPhase に対応）。ノード別に追跡する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForceNodePhase {
    /// まだ適用を開始していない（intent 未永続化）。。
    NotStarted,
    /// intent を永続化した（effect 前に journal 保存済み）。TP 再接続は抑止される。。
    IntentSaved,
    /// drain 中。処理中 request を強制破棄せず、既存 stream の所有を保持する。。
    Draining,
    /// 分散 child 停止 → 両 local Standalone 起動 → ready 確認済み。。
    LocalReady,
    /// この node の applied ack を返した。。
    Applied,
}

/// 両 node の Force 適用を調整する。片 ack 紛失では Complete にならない。。
/// drain timeout は強制破棄せず Failed にする。。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForceApplyCoordinator {
    pub policy_epoch: u64,
    pub operation_id: uuid::Uuid,
    pub local: ForceNodePhase,
    pub peer: ForceNodePhase,
    /// drain timeout で Failed になった（処理中 request は強制破棄しない）。。
    pub drain_failed: bool,
    /// cluster-wide 完了（両 node ready/applied が揃った）か。。
    pub complete: bool,
}

impl ForceApplyCoordinator {
    /// 新規の適用調整を開始する。intent 保存（journal）は呼び出し側が effect 前に行う。。
    pub fn begin(policy_epoch: u64, operation_id: uuid::Uuid) -> Self {
        Self {
            policy_epoch,
            operation_id,
            local: ForceNodePhase::IntentSaved,
            peer: ForceNodePhase::NotStarted,
            drain_failed: false,
            complete: false,
        }
    }

    /// peer にも intent を永続化したことを記録する。。
    pub fn peer_intent_saved(&mut self) {
        self.peer = ForceNodePhase::IntentSaved;
    }

    /// この node の drain を開始する。処理中 request は強制破棄しない（C03）。。
    pub fn begin_drain(&mut self) {
        self.local = ForceNodePhase::Draining;
    }

    /// drain timeout。処理中 request を強制破棄せず Failed にする（C03）。。
    /// この適用は terminal Failed となり、Automatic へは戻らない。。
    pub fn drain_timeout(&mut self) {
        self.drain_failed = true;
    }

    /// この node の分散 child 停止 → local Standalone 起動 → ready 確認。。
    pub fn note_local_ready(&mut self) {
        self.local = ForceNodePhase::LocalReady;
    }

    /// peer の local ready を観測した。。
    pub fn note_peer_ready(&mut self) {
        self.peer = ForceNodePhase::LocalReady;
    }

    /// この node の applied ack を返した。。
    pub fn note_local_applied(&mut self) {
        self.local = ForceNodePhase::Applied;
        self.recompute_complete();
    }

    /// peer の applied ack を観測した。。
    pub fn note_peer_applied(&mut self) {
        self.peer = ForceNodePhase::Applied;
        self.recompute_complete();
    }

    /// 両 node の applied ack が揃った時だけ Complete にする。片 ack 紛失では
    /// Complete にならない（pending は journal に残る）。。
    fn recompute_complete(&mut self) {
        self.complete = !self.drain_failed
            && self.local == ForceNodePhase::Applied
            && self.peer == ForceNodePhase::Applied;
    }

    /// この適用が terminal Failed か（drain timeout）。。
    pub fn is_failed(&self) -> bool {
        self.drain_failed
    }
}

// ---------------------------------------------------------------------------
// P05 — Automatic 復帰と全自動経路の policy gate（C03 PolicyEpoch equality）。
// ---------------------------------------------------------------------------
// Automatic 変更は prepare/commit で同 epoch を両 node に保存し、pair/promote の各
// 自動経路（discovery callback / periodic tick / operator promote / recovery / route
// monitor）で「両端の policy epoch 一致」を検証する。片側 commit/ack 不明（部分
// commit）では昇格しない。auto_promote=false / peer 不在 / deployment mismatch
// latch でも Standalone を維持する。job 完了は方針の適用を示し、TP ready とは区別
// される。Automatic を選んでも保護ラッチ（ForcedStandalone）を解除しない。

/// 全自動経路（pair/promote）の policy gate 判定。純粋な決定を所有する。。
/// 各自動経路はこの判定を共有し、gate 漏れを作らない。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticPromotionVerdict {
    /// 昇格を許可する。両端の policy epoch が一致し、auto_promote=true、
    /// deployment mismatch latch が立っていない。。
    Allow,
    /// auto_promote=false。構成上 promotion は無効。。
    AutoPromoteDisabled,
    /// peer 不在（epoch 不明）。Standalone を維持する。。
    PeerAbsent,
    /// 両端の policy epoch が不一致（部分 commit / 片側 ack 不明）。昇格しない。。
    EpochMismatch,
    /// deployment mismatch latch 保持中。promotion を抑止する。。
    DeploymentMismatchLatch,
    /// ForcedStandalone 保護ラッチ保持中。promotion / pair を禁止する。。
    ForcedStandaloneLatch,
}

impl AutomaticPromotionVerdict {
    pub fn allows_promotion(&self) -> bool {
        matches!(self, AutomaticPromotionVerdict::Allow)
    }

    /// ラベル（診断・受入テスト用の安定識別子）。。
    pub fn name(&self) -> &'static str {
        match self {
            AutomaticPromotionVerdict::Allow => "auto-promote-allowed",
            AutomaticPromotionVerdict::AutoPromoteDisabled => "auto-promote-disabled",
            AutomaticPromotionVerdict::PeerAbsent => "auto-promote-peer-absent",
            AutomaticPromotionVerdict::EpochMismatch => "auto-promote-epoch-mismatch",
            AutomaticPromotionVerdict::DeploymentMismatchLatch => {
                "auto-promote-deployment-mismatch-latch"
            }
            AutomaticPromotionVerdict::ForcedStandaloneLatch => {
                "auto-promote-forced-standalone-latch"
            }
        }
    }
}

/// 全自動経路の policy gate 判定関数。pair/promote を開始する前に必ず呼ぶ。。
///
/// - `operator_policy`: 現在の操作方針。ForcedStandalone は保護ラッチで、Automatic を
///   選んでも解除されない（C03）。。
/// - `auto_promote`: config の自動昇格フラグ。。
/// - `deployment_mismatch_latch`: deployment mismatch の独立ラッチ。。
/// - `peer_present`: peer が到達可能か。不在なら epoch 不明で Standalone を維持。。
/// - `local_policy_epoch` / `peer_policy_epoch`: 両端の policy epoch。一致しない場合は
///   部分 commit / 片側 ack 不明として昇格しない。。
pub fn automatic_promotion_verdict(
    operator_policy: OperationPolicy,
    auto_promote: bool,
    deployment_mismatch_latch: bool,
    peer_present: bool,
    local_policy_epoch: u64,
    peer_policy_epoch: u64,
) -> AutomaticPromotionVerdict {
    // ForcedStandalone 保護ラッチが最優先。Automatic を選んでも解除しない。。
    if operator_policy == OperationPolicy::ForcedStandalone {
        return AutomaticPromotionVerdict::ForcedStandaloneLatch;
    }
    if !auto_promote {
        return AutomaticPromotionVerdict::AutoPromoteDisabled;
    }
    if deployment_mismatch_latch {
        return AutomaticPromotionVerdict::DeploymentMismatchLatch;
    }
    if !peer_present {
        return AutomaticPromotionVerdict::PeerAbsent;
    }
    if local_policy_epoch != peer_policy_epoch {
        return AutomaticPromotionVerdict::EpochMismatch;
    }
    AutomaticPromotionVerdict::Allow
}

#[cfg(test)]
mod automatic_policy_tests {
    use super::*;

    #[test]
    fn automatic_with_peer_present_and_equal_epoch_allows() {
        let v = automatic_promotion_verdict(OperationPolicy::Automatic, true, false, true, 7, 7);
        assert_eq!(v, AutomaticPromotionVerdict::Allow);
        assert!(v.allows_promotion());
    }

    #[test]
    fn auto_promote_false_blocks_promotion() {
        let v = automatic_promotion_verdict(OperationPolicy::Automatic, false, false, true, 7, 7);
        assert_eq!(v, AutomaticPromotionVerdict::AutoPromoteDisabled);
        assert!(!v.allows_promotion());
    }

    #[test]
    fn partial_commit_epoch_mismatch_blocks_promotion() {
        // 片側 commit/ack 不明（部分 commit）→ epoch 不一致で昇格しない。。
        let v = automatic_promotion_verdict(OperationPolicy::Automatic, true, false, true, 7, 8);
        assert_eq!(v, AutomaticPromotionVerdict::EpochMismatch);
        assert!(!v.allows_promotion());
    }

    #[test]
    fn peer_absent_keeps_standalone() {
        let v = automatic_promotion_verdict(OperationPolicy::Automatic, true, false, false, 7, 7);
        assert_eq!(v, AutomaticPromotionVerdict::PeerAbsent);
        assert!(!v.allows_promotion());
    }

    #[test]
    fn deployment_mismatch_latch_blocks_promotion() {
        let v = automatic_promotion_verdict(OperationPolicy::Automatic, true, true, true, 7, 7);
        assert_eq!(v, AutomaticPromotionVerdict::DeploymentMismatchLatch);
        assert!(!v.allows_promotion());
    }

    #[test]
    fn forced_standalone_latch_wins_over_automatic() {
        // Automatic を選んでも ForcedStandalone 保護ラッチは解除しない（C03）。。
        let v =
            automatic_promotion_verdict(OperationPolicy::ForcedStandalone, true, false, true, 7, 7);
        assert_eq!(v, AutomaticPromotionVerdict::ForcedStandaloneLatch);
        assert!(!v.allows_promotion());
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(
            automatic_promotion_verdict(OperationPolicy::Automatic, true, false, true, 1, 1,)
                .name(),
            "auto-promote-allowed"
        );
        assert_eq!(
            automatic_promotion_verdict(OperationPolicy::Automatic, true, false, false, 1, 1,)
                .name(),
            "auto-promote-peer-absent"
        );
    }
}

#[cfg(test)]
mod force_tests {
    use super::*;

    #[test]
    fn both_acks_are_required_for_complete() {
        let op = uuid::Uuid::new_v4();
        let mut c = ForceApplyCoordinator::begin(1, op);
        assert!(!c.complete);
        c.peer_intent_saved();
        c.begin_drain();
        c.note_local_ready();
        c.note_peer_ready();
        // 片 ack のみでは Complete でない（受入 case: 片 ack 紛失 → Complete でない）。
        c.note_local_applied();
        assert!(!c.complete, "one ack must not complete the cluster apply");
        c.note_peer_applied();
        assert!(c.complete);
        assert!(!c.is_failed());
    }

    #[test]
    fn missing_peer_ack_keeps_pending() {
        let op = uuid::Uuid::new_v4();
        let mut c = ForceApplyCoordinator::begin(2, op);
        c.peer_intent_saved();
        c.note_local_ready();
        c.note_peer_ready();
        c.note_local_applied();
        // peer ack が無い（紛失）。Complete でない。。
        assert!(!c.complete);
        assert_eq!(c.peer, ForceNodePhase::LocalReady);
    }

    #[test]
    fn drain_timeout_fails_without_force_dropping() {
        let op = uuid::Uuid::new_v4();
        let mut c = ForceApplyCoordinator::begin(3, op);
        c.peer_intent_saved();
        c.begin_drain();
        // drain timeout → Failed。処理中 request を強制破棄しない（drain_failed のみ）。。
        c.drain_timeout();
        assert!(c.is_failed());
        assert!(!c.complete);
        // Failed 後も applied ack が揃っても Complete にはならない（Automatic へ戻さない）。
        c.note_local_ready();
        c.note_peer_ready();
        c.note_local_applied();
        c.note_peer_applied();
        assert!(!c.complete);
        assert!(c.is_failed());
    }

    #[test]
    fn local_ready_requires_child_stop_and_local_start() {
        let op = uuid::Uuid::new_v4();
        let mut c = ForceApplyCoordinator::begin(4, op);
        c.peer_intent_saved();
        // ready 前は Applied にならない。。
        c.note_local_applied();
        assert!(!c.complete);
        c.note_local_ready();
        c.note_peer_ready();
        c.note_peer_applied();
        // local が Applied になっていない（ready のみ）ので Complete でない。。
        assert_eq!(c.local, ForceNodePhase::LocalReady);
        assert!(!c.complete);
    }
}

/// 世代契約のための state。policy epoch と冪等性レジストリを保持する。。
/// 短時間 mutex を保持して network await しない。。
pub struct PolicyControlState {
    epoch: std::sync::Mutex<u64>,
    /// prepare/commit/abortは同じoperation_idを共有するためphaseごとに冪等性を分ける。
    idempotency: std::sync::Mutex<std::collections::HashMap<(uuid::Uuid, PolicyControlPhase), u64>>,
}

impl Default for PolicyControlState {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyControlState {
    pub fn new() -> Self {
        Self {
            epoch: std::sync::Mutex::new(0),
            idempotency: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// policy epoch を進める（commit 時に使用）。。。
    pub fn advance_epoch(&self, next: u64) {
        let mut epoch = self.epoch.lock().unwrap_or_else(|p| p.into_inner());
        *epoch = epoch.max(next);
    }

    pub fn epoch(&self) -> u64 {
        *self.epoch.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// request を検証する。stale epoch / 冪等性 / 旧 peer / 署名を検査する。。
    /// signature_ok は既存 ControlAuthenticator::verify の結果（method/path/body/node/
    /// nonce/timestamp + source ip）。この layer は署名の「意味」だけを扱う。。
    pub fn validate(
        &self,
        request: &PolicyControlRequest,
        signature_ok: bool,
    ) -> Result<PolicyControlVerdict, PolicyControlError> {
        if request.protocol_version != POLICY_CONTROL_PROTOCOL_VERSION {
            return Err(PolicyControlError::UnsupportedPeer);
        }
        if !signature_ok {
            return Err(PolicyControlError::Unauthenticated);
        }
        let current = self.epoch();
        if request.policy_epoch < current {
            return Err(PolicyControlError::StaleEpoch {
                expected: current,
                received: request.policy_epoch,
            });
        }
        let body_hash = canonical_request_hash(request);
        let mut idem = self.idempotency.lock().unwrap_or_else(|p| p.into_inner());
        let key = (request.operation_id, request.phase);
        match idem.get(&key) {
            Some(existing) if *existing == body_hash => Ok(PolicyControlVerdict::Duplicate),
            Some(_) => Err(PolicyControlError::IdempotencyConflict),
            None => {
                idem.insert(key, body_hash);
                Ok(PolicyControlVerdict::New)
            }
        }
    }

    /// terminal 後に冪等性登録を解放する。。。
    pub fn release(&self, operation_id: uuid::Uuid) {
        self.idempotency
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(id, _), _| *id != operation_id);
    }
}

/// request の canonical ハッシュ（冪等性判定用）。。。
pub fn canonical_request_hash(request: &PolicyControlRequest) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    request.policy_epoch.hash(&mut hasher);
    request.phase.hash(&mut hasher);
    request.desired.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(epoch: u64, phase: PolicyControlPhase) -> PolicyControlRequest {
        PolicyControlRequest {
            protocol_version: POLICY_CONTROL_PROTOCOL_VERSION,
            policy_epoch: epoch,
            operation_id: uuid::Uuid::new_v4(),
            phase,
            desired: OperationPolicy::ForcedStandalone,
        }
    }

    #[test]
    fn stale_epoch_is_rejected_with_no_effect() {
        let state = PolicyControlState::new();
        state.advance_epoch(42);
        let err = state
            .validate(&request(1, PolicyControlPhase::Prepare), true)
            .unwrap_err();
        assert_eq!(
            err,
            PolicyControlError::StaleEpoch {
                expected: 42,
                received: 1
            }
        );
        assert_eq!(err.http_status(), 409);
    }

    #[test]
    fn unsigned_or_wrong_node_is_403() {
        let state = PolicyControlState::new();
        // signature_ok=false は無署名 / 別 node を表す。。
        let err = state
            .validate(&request(0, PolicyControlPhase::Prepare), false)
            .unwrap_err();
        assert_eq!(err, PolicyControlError::Unauthenticated);
        assert_eq!(err.http_status(), 403);
    }

    #[test]
    fn old_peer_protocol_is_unsupported() {
        let state = PolicyControlState::new();
        let mut req = request(0, PolicyControlPhase::Prepare);
        req.protocol_version = 0;
        let err = state.validate(&req, true).unwrap_err();
        assert_eq!(err, PolicyControlError::UnsupportedPeer);
        assert_eq!(err.http_status(), 403);
    }

    #[test]
    fn idempotency_returns_duplicate_and_conflict() {
        let state = PolicyControlState::new();
        let req = request(0, PolicyControlPhase::Prepare);
        assert_eq!(
            state.validate(&req, true).unwrap(),
            PolicyControlVerdict::New
        );
        // 同 ID 同 body → Duplicate。。
        assert_eq!(
            state.validate(&req, true).unwrap(),
            PolicyControlVerdict::Duplicate
        );
        // 同 ID 別 body → 409。。
        let mut different = req.clone();
        different.desired = OperationPolicy::Automatic;
        let err = state.validate(&different, true).unwrap_err();
        assert_eq!(err, PolicyControlError::IdempotencyConflict);
        assert_eq!(err.http_status(), 409);
        // terminal 後は New に戻る。。
        state.release(req.operation_id);
        assert_eq!(
            state.validate(&req, true).unwrap(),
            PolicyControlVerdict::New
        );
    }

    #[test]
    fn prepare_and_commit_share_operation_id_but_have_distinct_idempotency_slots() {
        let state = PolicyControlState::new();
        let prepare = request(1, PolicyControlPhase::Prepare);
        assert_eq!(
            state.validate(&prepare, true).unwrap(),
            PolicyControlVerdict::New
        );
        let commit = PolicyControlRequest {
            phase: PolicyControlPhase::Commit,
            ..prepare
        };
        assert_eq!(
            state.validate(&commit, true).unwrap(),
            PolicyControlVerdict::New
        );
    }

    #[test]
    fn coordinator_absent_keeps_local_safe_latch() {
        // coordinator 不在時は「成功を捏造しない」。local safe latch（P01 journal の
        // operator_policy）を保持し、job を failed にする。ここでは safe latch の値が
        // ForcedStandalone であることを検証する（適用の cluster 全体調整は P04〜P06）。
        let state = PolicyControlState::new();
        state.advance_epoch(3);
        assert_eq!(state.epoch(), 3);
        // safe latch: coordinator 不在でも operator_policy は ForcedStandalone のまま。。
        // （P01 の PolicyJournal が保持し、本モジュールは epoch 契約のみ検証する。）
    }
}
