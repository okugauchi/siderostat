//! A04 — 共通型・操作契約・テスト用境界の受入 case。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{ClusterEvent, ClusterEventKind, OperationPolicy};
use siderostat::target::{ClusterState, LocalRole};
use support::v040::FakeCluster;

/// 生成イベントを generation 付きで構築するヘルパ。
fn event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::new(generation, kind)
}

/// 受入 case 1: fake mode → 実 PID 生成/既存 state アクセス 0。
#[tokio::test]
async fn v040_fake_mode_has_zero_real_process_and_state_access() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    // Booting → SoloStandaloneStarting（本番 reducer 経由で受理）
    h.apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("BeginSoloStandalone should be accepted");
    assert_eq!(h.recorder.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    assert_eq!(h.recorder.generated_events(), 1);
    assert_eq!(h.policy(), OperationPolicy::Automatic);
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneStarting);
    h._task.abort();
}

/// 受入 case 2: 同時 test → root/port 非共有。
#[tokio::test]
async fn v040_concurrent_instances_do_not_share_root_or_port() {
    let a = FakeCluster::automatic_tp(LocalRole::Coordinator);
    let b = FakeCluster::automatic_tp(LocalRole::Worker);
    assert_ne!(a.recorder.root, b.recorder.root);
    assert_ne!(a.recorder.control_port, b.recorder.control_port);
    assert_ne!(a.recorder.state_path, b.recorder.state_path);
    assert_ne!(a.role, b.role);
    assert_eq!(a.snapshot().state, ClusterState::Booting);
    assert_eq!(b.snapshot().state, ClusterState::Booting);
    a._task.abort();
    b._task.abort();
}

/// 受入 case 3: stale generation → effect 数 0（拒否・副作用なし）。
#[tokio::test]
async fn v040_stale_generation_produces_no_effect() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    // 有効遷移 1 回 → generation 0 → 1
    h.apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("BeginSoloStandalone should be accepted");
    assert_eq!(h.recorder.generated_events(), 1);

    // stale generation 999 → 拒否、effect 増えない、snapshot 不変
    let err = h
        .apply(event(999, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect_err("stale generation must be rejected");
    assert!(matches!(
        err,
        siderostat::cluster::TransitionError::StaleGeneration {
            expected: 999,
            current: 1
        }
    ));
    assert_eq!(h.recorder.generated_events(), 1);
    assert_eq!(h.recorder.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneStarting);
    h._task.abort();
}

/// 受入 case 4: 本番 reducer の有効遷移（受理・effect 記録）と不正遷移の拒否。
#[tokio::test]
async fn v040_production_reducer_accepts_valid_and_rejects_invalid() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    // 有効: Booting → SoloStandaloneStarting
    h.apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .unwrap();
    assert_eq!(h.recorder.generated_events(), 1);
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneStarting);

    // 不正: SoloStandaloneStarting から BeginPairing は無効遷移
    let err = h
        .apply(event(1, ClusterEventKind::BeginPairing))
        .await
        .expect_err("invalid transition must be rejected");
    assert!(matches!(
        err,
        siderostat::cluster::TransitionError::InvalidTransition { .. }
    ));
    // effect は増えない
    assert_eq!(h.recorder.generated_events(), 1);
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneStarting);
    h._task.abort();
}
