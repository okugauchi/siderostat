//! T06 — TPセッションと純粋状態機械の受入 case。
//!
//! C02 の状態表・セッション照合・route 公開 gate を、本番 reducer
//! （`spawn_state_machine`）を fake 境界経由で駆動して検証する。fake mode では実
//! PID 生成・既存 state アクセスを行わない（Recorder で確認）。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    ClusterEvent, ClusterEventKind, OperationPolicy, TpSessionId, TransitionError,
};
use siderostat::target::{ClusterState, LocalRole};
use support::v040::FakeCluster;

const SID: TpSessionId = TpSessionId(1);

fn event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::new(generation, kind)
}

fn tp_event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::tp(generation, kind, SID)
}

/// Solo ready まで進める。戻り値は現在の generation。
async fn to_solo_ready(h: &FakeCluster) -> u64 {
    h.apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("BeginSoloStandalone accepted");
    let ready = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("LocalStandaloneReady accepted");
    ready.generation
}

/// 受入 case 1: 各 ready 要素が一つ欠落 → 非公開。
#[tokio::test]
async fn v040_tp_route_unpublished_until_all_readiness_collected() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    let gen0 = to_solo_ready(&h).await;

    // worker Prepared のみ → AwaitingTensorParallelWorkerHello。route 非公開。
    let starting = h
        .apply(tp_event(gen0, ClusterEventKind::BeginTensorParallel))
        .await
        .expect("BeginTensorParallel accepted");
    assert_eq!(starting.state, ClusterState::TensorParallelStarting);
    let hello = h
        .apply(tp_event(
            starting.generation,
            ClusterEventKind::TensorParallelWorkerPrepared,
        ))
        .await
        .expect("worker prepared accepted");
    assert_eq!(hello.state, ClusterState::AwaitingTensorParallelWorkerHello);
    assert!(!h.route_is_published());

    // coordinator started → まだ非公開。
    let _ = h
        .apply(tp_event(
            hello.generation,
            ClusterEventKind::TensorParallelCoordinatorStarted,
        ))
        .await
        .expect("coordinator started accepted");
    assert!(!h.route_is_published());

    // handshake+HTTP ready → まだ非公開（warm-up 未完了）。
    let _ = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelHandshakeHttpReady,
        ))
        .await
        .expect("handshake+http accepted");
    assert!(!h.route_is_published());

    // warm-up 完了 → route 公開。TensorParallelReady。
    let ready = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelWarmupDone,
        ))
        .await
        .expect("warmup accepted");
    assert_eq!(ready.state, ClusterState::TensorParallelReady);
    assert!(h.route_is_published());
    assert_eq!(h.recorder.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    h._task.abort();
}

/// 受入 case 2: 旧 session Ready → 無視（世代・状態不変）。
#[tokio::test]
async fn v040_tp_old_session_ready_is_ignored() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    let gen0 = to_solo_ready(&h).await;

    let starting = h
        .apply(tp_event(gen0, ClusterEventKind::BeginTensorParallel))
        .await
        .expect("BeginTensorParallel accepted");

    // 旧セッション（SID=2）の warm-up 完了 → 受理されない。
    let old = h
        .apply(ClusterEvent::tp(
            starting.generation,
            ClusterEventKind::TensorParallelWarmupDone,
            TpSessionId(2),
        ))
        .await
        .expect("old session event is ignored, not errored");
    assert_eq!(old.state, starting.state);
    assert_eq!(old.generation, starting.generation);
    assert!(!h.route_is_published());
    h._task.abort();
}

/// 受入 case 3: LP Ready in TP → 拒否（InvalidTransition）。
#[tokio::test]
async fn v040_tp_rejects_lp_ready_event() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    let gen0 = to_solo_ready(&h).await;

    let starting = h
        .apply(tp_event(gen0, ClusterEventKind::BeginTensorParallel))
        .await
        .expect("BeginTensorParallel accepted");

    // TP 中に LP の DistributedRouteReady → InvalidTransition。
    let err = h
        .apply(event(
            starting.generation,
            ClusterEventKind::DistributedRouteReady,
        ))
        .await
        .expect_err("LP ready in TP must be rejected");
    assert!(matches!(err, TransitionError::InvalidTransition { .. }));
    // 状態・generation 不変。
    assert_eq!(h.snapshot().state, ClusterState::TensorParallelStarting);
    h._task.abort();
}

/// 受入 case 4: 強制 policy → 開始 effect なし（spawn 抑止）。
#[tokio::test]
async fn v040_tp_forced_policy_has_no_start_effect() {
    let h = FakeCluster::new(LocalRole::Coordinator, OperationPolicy::ForcedStandalone);
    let gen0 = to_solo_ready(&h).await;
    assert_eq!(h.policy(), OperationPolicy::ForcedStandalone);

    // prepare_worker は ForcedStandalone では TP spawn を抑止する。
    h.prepare_worker().await;
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    // 状態も TP に進まない（Solo ready のまま）。
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneReady);
    assert_eq!(h.recorder.real_process_operations(), 0);
    let _ = gen0;
    h._task.abort();
}
