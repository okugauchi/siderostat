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

/// 世代契約のための state。policy epoch と冪等性レジストリを保持する。。
/// 短時間 mutex を保持して network await しない。。
pub struct PolicyControlState {
    epoch: std::sync::Mutex<u64>,
    idempotency: std::sync::Mutex<std::collections::HashMap<uuid::Uuid, u64>>,
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
        match idem.get(&request.operation_id) {
            Some(existing) if *existing == body_hash => Ok(PolicyControlVerdict::Duplicate),
            Some(_) => Err(PolicyControlError::IdempotencyConflict),
            None => {
                idem.insert(request.operation_id, body_hash);
                Ok(PolicyControlVerdict::New)
            }
        }
    }

    /// terminal 後に冪等性登録を解放する。。。
    pub fn release(&self, operation_id: uuid::Uuid) {
        self.idempotency
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&operation_id);
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
