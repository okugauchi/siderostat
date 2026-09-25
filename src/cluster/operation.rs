//! v0.4.0 共通操作型（C01〜C04 の newtype）。。
//!
//! 型の定義のみここで行う。policy の永続動作は P01、capability 判定は T01、
//! TP lifecycle は T06〜T08 が所有する。consumer で再宣言しない。。
//!
//! P02 では lifecycle 操作の単一オーナー（OperationLease）と冪等性（idempotency）を追加する。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

/// 単一の lifecycle 操作を識別する ID。重複抑止・冪等性のために用いる。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub Uuid);

/// 操作方針（policy）の世代。stale な control/peer を拒否するために用いる。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PolicyEpoch(pub u64);

/// TP（Tensor Parallelism）セッションを識別する ID。role swap / child 交換後に
/// 旧セッションの成功を受理しないために用いる。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TpSessionId(pub u64);

/// lifecycle 操作の種別。promotion/demotion/restart/recovery/activation/policy に
/// 同じ OperationLease を適用する（C03）。データ取得 job は lease 不要。。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationKind {
    Promotion,
    Demotion,
    Restart,
    Recovery,
    Activation,
    Policy,
}

impl OperationKind {
    pub fn name(self) -> &'static str {
        match self {
            OperationKind::Promotion => "promotion",
            OperationKind::Demotion => "demotion",
            OperationKind::Restart => "restart",
            OperationKind::Recovery => "recovery",
            OperationKind::Activation => "activation",
            OperationKind::Policy => "policy",
        }
    }
}

/// 冪等性判定の結果。C03: 同 ID 同 canonical body は既存 job、別内容は 409。。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// 初回。job を新規作成できる。。
    New,
    /// 同 ID 同 body。既存 job を返す。。
    Existing,
    /// 同 ID 別 body。409 として拒否する。。
    Conflict,
}

/// 冪等性のための job 登録。再起動後は永続 journal（P01）から status を復元する。。
#[derive(Debug, Clone)]
pub struct OperationEnvelope {
    pub id: OperationId,
    pub body_hash: u64,
}

/// 同一種別に単一オーナーのみを許す lease。C03 の「promotion/demotion/restart/
/// recovery/activation/policy に一つ」を満たす。。
///
/// 短時間 state mutex を保持して network await しない。Force intent が立つと
/// 新規 promotion を禁止し、現 owner は安全な drain/child 切替境界で yield する。。
#[derive(Clone)]
pub struct OperationLease {
    /// kind → 現在のオーナー OperationId。単一オーナーのみ保持。。
    owners: Arc<Mutex<HashMap<OperationKind, OperationId>>>,
    /// 冪等性レジストリ: id → body_hash。。
    idempotency: Arc<Mutex<HashMap<OperationId, u64>>>,
    /// ForcedStandalone intent が立ったことを示す。新規 promotion を禁止する。。
    promotion_blocked: Arc<AtomicBool>,
}

impl Default for OperationLease {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationLease {
    pub fn new() -> Self {
        Self {
            owners: Arc::new(Mutex::new(HashMap::new())),
            idempotency: Arc::new(Mutex::new(HashMap::new())),
            promotion_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 冪等性を登録する。同 ID 同 body → Existing、同 ID 別 body → Conflict。。
    pub fn register_idempotent(&self, envelope: &OperationEnvelope) -> IdempotencyOutcome {
        let mut idem = self
            .idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match idem.get(&envelope.id) {
            Some(existing) if *existing == envelope.body_hash => IdempotencyOutcome::Existing,
            Some(_) => IdempotencyOutcome::Conflict,
            None => {
                idem.insert(envelope.id, envelope.body_hash);
                IdempotencyOutcome::New
            }
        }
    }

    /// 冪等性登録を解除する（terminal job 後）。。。
    pub fn release_idempotent(&self, id: OperationId) {
        self.idempotency
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    /// 指定 kind の lease を取得する。既に同 kind のオーナーが居れば失敗。Activation は
    /// 全 lifecycle kind と相互排他。。
    /// promotion は Force intent によりブロックされている場合失敗する。。
    pub fn try_acquire(
        &self,
        kind: OperationKind,
        id: OperationId,
    ) -> Result<(), OperationLeaseError> {
        if kind == OperationKind::Promotion && self.promotion_blocked.load(Ordering::SeqCst) {
            return Err(OperationLeaseError::PromotionBlocked);
        }
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Activation is the one lifecycle operation that mutates the executable behind
        // supervisors. It must exclude every other lifecycle owner, while preserving the
        // established concurrency rules among ordinary policy/recovery/pair/restart work.
        if kind == OperationKind::Activation {
            for owner_kind in [
                OperationKind::Promotion,
                OperationKind::Demotion,
                OperationKind::Restart,
                OperationKind::Recovery,
                OperationKind::Policy,
                OperationKind::Activation,
            ] {
                if owners.contains_key(&owner_kind) {
                    return Err(OperationLeaseError::Busy(owner_kind));
                }
            }
        } else if owners.contains_key(&OperationKind::Activation) {
            return Err(OperationLeaseError::Busy(OperationKind::Activation));
        }
        if owners.contains_key(&kind) {
            return Err(OperationLeaseError::Busy(kind));
        }
        owners.insert(kind, id);
        Ok(())
    }

    /// Acquire a lifecycle lease whose lifetime is tied to a guard. Drop releases only the
    /// matching owner ID, so cancellation and early returns cannot strand the gate.
    pub fn claim(
        &self,
        kind: OperationKind,
        id: OperationId,
    ) -> Result<OperationLeaseGuard, OperationLeaseError> {
        self.try_acquire(kind, id)?;
        Ok(OperationLeaseGuard {
            lease: self.clone(),
            kind,
            id,
        })
    }

    /// lease を解放する。オーナーが id と一致する場合のみ解放する。。
    pub fn release(&self, kind: OperationKind, id: OperationId) {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(&kind) == Some(&id) {
            owners.remove(&kind);
        }
    }

    /// 現在のオーナー ID（あれば）。。。
    pub fn owner(&self, kind: OperationKind) -> Option<OperationId> {
        self.owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&kind)
            .copied()
    }

    /// Force intent を立て、新規 promotion を禁止する。現 owner は安全境界で yield。。
    pub fn block_promotion(&self) {
        self.promotion_blocked.store(true, Ordering::SeqCst);
    }

    /// Force 適用完了後に promotion 禁止を解除する。。
    pub fn unblock_promotion(&self) {
        self.promotion_blocked.store(false, Ordering::SeqCst);
    }

    /// 新規 promotion が許可されるか。。
    pub fn promotion_allowed(&self) -> bool {
        !self.promotion_blocked.load(Ordering::SeqCst)
    }
}

pub struct OperationLeaseGuard {
    lease: OperationLease,
    kind: OperationKind,
    id: OperationId,
}

impl Drop for OperationLeaseGuard {
    fn drop(&mut self) {
        self.lease.release(self.kind, self.id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationLeaseError {
    /// 同 kind の操作が進行中。。。
    Busy(OperationKind),
    /// Force intent により新規 promotion が禁止されている。。。
    PromotionBlocked,
}

impl std::fmt::Display for OperationLeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OperationLeaseError::Busy(kind) => {
                write!(f, "another {} operation is in progress", kind.name())
            }
            OperationLeaseError::PromotionBlocked => {
                write!(
                    f,
                    "promotion is blocked while forced standalone is in effect"
                )
            }
        }
    }
}

impl std::error::Error for OperationLeaseError {}

/// canonical body hash（冪等性用）。body の正規形から計算する。。
pub fn canonical_body_hash(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_is_opaque_but_comparable() {
        let a = OperationId(Uuid::new_v4());
        let b = OperationId(a.0);
        assert_eq!(a, b);
        assert_ne!(a, OperationId(Uuid::new_v4()));
    }

    #[test]
    fn policy_epoch_orders_so_stale_is_detectable() {
        assert!(PolicyEpoch(1) > PolicyEpoch(0));
        assert!(PolicyEpoch(0) < PolicyEpoch(42));
        assert_eq!(PolicyEpoch(7), PolicyEpoch(7));
    }

    #[test]
    fn tp_session_id_orders_so_old_session_is_rejected() {
        assert!(TpSessionId(2) > TpSessionId(1));
        assert_eq!(TpSessionId(5), TpSessionId(5));
    }

    #[test]
    fn lease_allows_single_owner_per_kind() {
        let lease = OperationLease::new();
        let a = OperationId(Uuid::new_v4());
        let b = OperationId(Uuid::new_v4());
        assert!(lease.try_acquire(OperationKind::Promotion, a).is_ok());
        // 同 kind の 2 番目は拒否。。
        assert_eq!(
            lease.try_acquire(OperationKind::Promotion, b),
            Err(OperationLeaseError::Busy(OperationKind::Promotion))
        );
        assert_eq!(lease.owner(OperationKind::Promotion), Some(a));
        lease.release(OperationKind::Promotion, a);
        assert!(lease.try_acquire(OperationKind::Promotion, b).is_ok());
    }

    #[test]
    fn different_kinds_are_independent() {
        let lease = OperationLease::new();
        let a = OperationId(Uuid::new_v4());
        let b = OperationId(Uuid::new_v4());
        assert!(lease.try_acquire(OperationKind::Restart, a).is_ok());
        assert!(lease.try_acquire(OperationKind::Recovery, b).is_ok());
        lease.release(OperationKind::Restart, a);
        lease.release(OperationKind::Recovery, b);
    }

    #[test]
    fn activation_is_exclusive_with_every_lifecycle_mutation() {
        let lease = OperationLease::new();
        let activation = OperationId(Uuid::new_v4());

        for kind in [
            OperationKind::Promotion,
            OperationKind::Demotion,
            OperationKind::Restart,
            OperationKind::Recovery,
            OperationKind::Policy,
        ] {
            let owner = OperationId(Uuid::new_v4());
            assert!(lease.try_acquire(kind, owner).is_ok());
            assert_eq!(
                lease.try_acquire(OperationKind::Activation, activation),
                Err(OperationLeaseError::Busy(kind))
            );
            lease.release(kind, owner);
        }

        assert!(
            lease
                .try_acquire(OperationKind::Activation, activation)
                .is_ok()
        );
        for kind in [
            OperationKind::Promotion,
            OperationKind::Demotion,
            OperationKind::Restart,
            OperationKind::Recovery,
            OperationKind::Policy,
        ] {
            assert_eq!(
                lease.try_acquire(kind, OperationId(Uuid::new_v4())),
                Err(OperationLeaseError::Busy(OperationKind::Activation))
            );
        }
        lease.release(OperationKind::Activation, activation);
    }

    #[test]
    fn lifecycle_guard_releases_only_its_owner_when_dropped() {
        let lease = OperationLease::new();
        let id = OperationId(Uuid::new_v4());
        let guard = lease.claim(OperationKind::Activation, id).unwrap();
        assert_eq!(lease.owner(OperationKind::Activation), Some(id));
        drop(guard);
        assert_eq!(lease.owner(OperationKind::Activation), None);
    }

    #[test]
    fn force_intent_blocks_new_promotion() {
        let lease = OperationLease::new();
        let a = OperationId(Uuid::new_v4());
        let b = OperationId(Uuid::new_v4());
        let activation = OperationId(Uuid::new_v4());
        assert!(lease.promotion_allowed());
        lease.block_promotion();
        assert!(!lease.promotion_allowed());
        assert_eq!(
            lease.try_acquire(OperationKind::Promotion, a),
            Err(OperationLeaseError::PromotionBlocked)
        );
        // Manager activation does not clear the durable ForcedStandalone latch.
        assert!(
            lease
                .try_acquire(OperationKind::Activation, activation)
                .is_ok()
        );
        lease.release(OperationKind::Activation, activation);
        assert!(!lease.promotion_allowed());
        assert_eq!(
            lease.try_acquire(OperationKind::Promotion, a),
            Err(OperationLeaseError::PromotionBlocked)
        );
        // 非 promotion 操作は Force 中も許可される。。
        assert!(lease.try_acquire(OperationKind::Restart, b).is_ok());
        lease.unblock_promotion();
        assert!(lease.promotion_allowed());
        assert!(lease.try_acquire(OperationKind::Promotion, a).is_ok());
    }

    #[test]
    fn idempotency_dedups_by_id_and_body() {
        let lease = OperationLease::new();
        let id = OperationId(Uuid::new_v4());
        let same = OperationEnvelope { id, body_hash: 42 };
        assert_eq!(lease.register_idempotent(&same), IdempotencyOutcome::New);
        assert_eq!(
            lease.register_idempotent(&same),
            IdempotencyOutcome::Existing
        );
        let different = OperationEnvelope { id, body_hash: 43 };
        assert_eq!(
            lease.register_idempotent(&different),
            IdempotencyOutcome::Conflict
        );
        lease.release_idempotent(id);
        assert_eq!(lease.register_idempotent(&same), IdempotencyOutcome::New);
    }
}
