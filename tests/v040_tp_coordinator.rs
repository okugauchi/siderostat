//! T08 — TP coordinator 起動・handshake・HTTP ready・warm-up の受入 case。
//!
//! C02 の harness API（prepare_worker → start_coordinator → observe_handshake_and_http_ready
//! → finish_warmup）を駆動し、route 公開が bounded warm-up 完了でのみ起きること、
//! 順序（worker 先行 → coordinator）が守られること、実プロセス操作がゼロであることを
//! 検証する。実プロセスは spawn せず、本番 reducer を本番と同じ経路で駆動する。

#![cfg(feature = "test-support")]

mod support;

use siderostat::target::{ClusterState, LocalRole};
use support::v040::FakeCluster;

/// C02 harness の固定 API をそのまま受入 case 1 として再現する。
#[tokio::test]
async fn v040_tp_requires_warmup() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    h.prepare_worker().await;
    h.start_coordinator().await;
    h.observe_handshake_and_http_ready().await;
    // warm-up 完了前は route 非公開。
    assert!(!h.route_is_published());
    h.finish_warmup().await;
    assert!(h.route_is_published());
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}

/// 受入 case: worker 待機 → coordinator 開始の順序が守られる（worker 先行）。
/// worker Prepared の後に coordinator ChildStarted を投入し、両者とも受理される。
#[tokio::test]
async fn v040_tp_coordinator_starts_after_worker_prepared() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    h.prepare_worker().await;
    h.start_coordinator().await;
    h.observe_handshake_and_http_ready().await;
    assert!(!h.route_is_published());
    h.finish_warmup().await;
    assert!(h.route_is_published());
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}

/// 受入 case: HTTP ready / handshake だけでは route 非公開。
/// handshake+HTTP ready 観測後に assert。warm-up 前は公開されない。
#[tokio::test]
async fn v040_tp_http_ready_alone_does_not_publish_route() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    h.prepare_worker().await;
    h.start_coordinator().await;
    h.observe_handshake_and_http_ready().await;
    assert!(!h.route_is_published());
    // 状態は AwaitingTensorParallelWorkerHello のまま（warm-up 待ち）。
    assert_eq!(
        h.snapshot().state,
        ClusterState::AwaitingTensorParallelWorkerHello
    );
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}

/// 受入 case: warm-up 完了（canary 成功）でのみ route 公開。
#[tokio::test]
async fn v040_tp_warmup_done_publishes_route() {
    let h = FakeCluster::automatic_tp(LocalRole::Coordinator);
    h.prepare_worker().await;
    h.start_coordinator().await;
    h.observe_handshake_and_http_ready().await;
    assert!(!h.route_is_published());
    h.finish_warmup().await;
    assert!(h.route_is_published());
    assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}

/// ForcedStandalone では TP coordinator も起動されない（spawn 抑止、route 非公開維持）。。
#[tokio::test]
async fn v040_tp_coordinator_suppressed_in_forced_standalone() {
    use siderostat::cluster::OperationPolicy;
    let h = FakeCluster::new(LocalRole::Coordinator, OperationPolicy::ForcedStandalone);
    let spawns_before = h.recorder.tp_spawn_count();
    h.prepare_worker().await;
    h.start_coordinator().await;
    h.observe_handshake_and_http_ready().await;
    h.finish_warmup().await;
    // TP spawn は一度も起きていない。
    assert_eq!(h.recorder.tp_spawn_count(), spawns_before);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    // route は公開されない。
    assert!(!h.route_is_published());
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}
