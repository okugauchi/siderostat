//! P02 — lifecycle 操作の排他・優先順位・job 継続の受入 case。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    IdempotencyOutcome, OperationEnvelope, OperationId, OperationKind, OperationLease,
};
use uuid::Uuid;

/// 受入 case 1: Force と promotion 同時 → 新規 promotion 0。。
#[test]
fn force_blocks_new_promotion() {
    let lease = OperationLease::new();
    let promote_id = OperationId(Uuid::new_v4());
    let force_id = OperationId(Uuid::new_v4());

    // Force intent が立つと新規 promotion は 0。。
    lease.block_promotion();
    assert!(!lease.promotion_allowed());
    assert!(
        lease
            .try_acquire(OperationKind::Promotion, promote_id)
            .is_err()
    );
    // Force の policy 操作自体は許可される。。
    assert!(lease.try_acquire(OperationKind::Policy, force_id).is_ok());
    lease.release(OperationKind::Policy, force_id);

    // Force 適用完了で promotion 禁止が解除され、promotion が可能になる。。
    lease.unblock_promotion();
    assert!(lease.promotion_allowed());
    assert!(
        lease
            .try_acquire(OperationKind::Promotion, promote_id)
            .is_ok()
    );
    lease.release(OperationKind::Promotion, promote_id);
}

/// 受入 case 2: restart / recovery 同時 → オーナー 1。。
#[test]
fn restart_and_recovery_are_single_owner() {
    let lease = OperationLease::new();
    let restart = OperationId(Uuid::new_v4());
    let recovery = OperationId(Uuid::new_v4());
    let other = OperationId(Uuid::new_v4());

    // restart を取得。同種別の 2 番目は拒否。。
    assert!(lease.try_acquire(OperationKind::Restart, restart).is_ok());
    assert_eq!(
        lease.try_acquire(OperationKind::Restart, other),
        Err(siderostat::cluster::OperationLeaseError::Busy(
            OperationKind::Restart
        ))
    );
    assert_eq!(lease.owner(OperationKind::Restart), Some(restart));

    // recovery は別種別なので同時に 1 オーナー。。
    assert!(lease.try_acquire(OperationKind::Recovery, recovery).is_ok());
    assert_eq!(lease.owner(OperationKind::Recovery), Some(recovery));

    lease.release(OperationKind::Restart, restart);
    lease.release(OperationKind::Recovery, recovery);
    assert_eq!(lease.owner(OperationKind::Restart), None);
    assert_eq!(lease.owner(OperationKind::Recovery), None);
}

/// 受入 case 3: 同 ID 同 body → 既存 job。。
#[test]
fn same_id_same_body_returns_existing_job() {
    let lease = OperationLease::new();
    let id = OperationId(Uuid::new_v4());
    let body = "{\"policy\":\"forced-standalone\",\"expected_generation\":42}";
    let envelope = OperationEnvelope {
        id,
        body_hash: siderostat::cluster::canonical_body_hash(body),
    };
    // 初回は New。。
    assert_eq!(
        lease.register_idempotent(&envelope),
        IdempotencyOutcome::New
    );
    // 同 ID 同 body は Existing（既存 job を返す）。。
    assert_eq!(
        lease.register_idempotent(&envelope),
        IdempotencyOutcome::Existing
    );
}

/// 受入 case 4: 同 ID 別 body → 409。。
#[test]
fn same_id_different_body_conflicts() {
    let lease = OperationLease::new();
    let id = OperationId(Uuid::new_v4());
    let body_a = "{\"policy\":\"forced-standalone\"}";
    let body_b = "{\"policy\":\"automatic\"}";
    let a = OperationEnvelope {
        id,
        body_hash: siderostat::cluster::canonical_body_hash(body_a),
    };
    let b = OperationEnvelope {
        id,
        body_hash: siderostat::cluster::canonical_body_hash(body_b),
    };
    assert_eq!(lease.register_idempotent(&a), IdempotencyOutcome::New);
    // 同 ID 別 body は Conflict（409 相当）。。
    assert_eq!(lease.register_idempotent(&b), IdempotencyOutcome::Conflict);
    // terminal 後の再登録は New に戻る。。
    lease.release_idempotent(id);
    assert_eq!(lease.register_idempotent(&a), IdempotencyOutcome::New);
}
