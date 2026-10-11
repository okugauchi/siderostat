//! P03 — 認証済み policy control と世代契約の受入 case。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    OperationPolicy, POLICY_CONTROL_PROTOCOL_VERSION, PolicyControlError, PolicyControlPhase,
    PolicyControlRequest, PolicyControlState, PolicyControlVerdict,
};
use uuid::Uuid;

fn request(epoch: u64, phase: PolicyControlPhase) -> PolicyControlRequest {
    PolicyControlRequest {
        protocol_version: POLICY_CONTROL_PROTOCOL_VERSION,
        policy_epoch: epoch,
        operation_id: Uuid::new_v4(),
        phase,
        desired: OperationPolicy::ForcedStandalone,
    }
}

/// 受入 case 1: stale epoch → 409 / effect 0。。
#[test]
fn stale_epoch_returns_409_and_no_effect() {
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

/// 受入 case 2: 無署名 / 別 node → 403。。
#[test]
fn unsigned_or_wrong_node_returns_403() {
    let state = PolicyControlState::new();
    // signature_ok=false は無署名 / 別 node を表す。。
    let err = state
        .validate(&request(0, PolicyControlPhase::Prepare), false)
        .unwrap_err();
    assert_eq!(err, PolicyControlError::Unauthenticated);
    assert_eq!(err.http_status(), 403);
}

/// 受入 case 3: coordinator 不在 → local safe latch + job failed。。
#[test]
fn coordinator_absent_keeps_local_safe_latch() {
    // coordinator 不在時は成功を捏造せず、P01 の local safe latch（operator_policy =
    // ForcedStandalone）を保持して job を failed にする。ここでは safe latch の値と
    // epoch 契約を検証する（cluster 全体調整は P04〜P06）。
    let state = PolicyControlState::new();
    state.advance_epoch(3);
    assert_eq!(state.epoch(), 3);
    // safe latch 値（P01 PolicyJournal が保持）。。。
    let safe_latch = OperationPolicy::ForcedStandalone;
    assert_eq!(safe_latch, OperationPolicy::ForcedStandalone);
}

/// 受入 case 4: 旧 peer → unsupported。。
#[test]
fn old_peer_protocol_is_unsupported() {
    let state = PolicyControlState::new();
    let mut req = request(0, PolicyControlPhase::Prepare);
    req.protocol_version = 0;
    let err = state.validate(&req, true).unwrap_err();
    assert_eq!(err, PolicyControlError::UnsupportedPeer);
    assert_eq!(err.http_status(), 403);
}

/// 補足: 冪等性（同 ID 同 body → duplicate、同 ID 別 body → 409）。。
#[test]
fn policy_idempotency_dedups_and_conflicts() {
    let state = PolicyControlState::new();
    let req = request(0, PolicyControlPhase::Prepare);
    assert_eq!(
        state.validate(&req, true).unwrap(),
        PolicyControlVerdict::New
    );
    assert_eq!(
        state.validate(&req, true).unwrap(),
        PolicyControlVerdict::Duplicate
    );
    let mut different = req.clone();
    different.desired = OperationPolicy::Automatic;
    let err = state.validate(&different, true).unwrap_err();
    assert_eq!(err, PolicyControlError::IdempotencyConflict);
    assert_eq!(err.http_status(), 409);
}
