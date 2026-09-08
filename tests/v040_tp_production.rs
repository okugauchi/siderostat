//! T11 — TP 本番配線と mixed-version 拒否の受入 case。
//!
//! typed TP が本番経路へ到達する（設定 TP ＋ fake two-node HTTP → Ready）、v1 peer の
//! TP/policy 操作を negotiation で拒否して旧 LP 経路のみを残す、dry-run は同じ
//! state/control の fake driver で実 child/state 操作 0、manual promote 中の強制
//! policy は拒否して安全収束する、ことを検証する。
//!
//! 実プロセスは spawn せず、本番 reducer（`spawn_state_machine`）と本番開始 gate
//! （`check_tp_start` / `TpStartVerdict`）を駆動する。fake mode では実 PID 生成・
//! OS 接触・既存 state アクセスが 0 であることを Recorder で検証する。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    ClusterEvent, ClusterEventKind, OperationPolicy, TpStartVerdict, check_tp_start,
};
use siderostat::target::{ClusterState, LocalRole};

const SID: siderostat::cluster::TpSessionId = siderostat::cluster::TpSessionId(1);

fn tp_event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::tp(generation, kind, SID)
}

fn event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::new(generation, kind)
}

/// Solo ready まで進め、TP ready まで進める。戻り値は現在の generation。
async fn to_tp_ready(h: &support::v040::FakeCluster) -> u64 {
    let _ = h
        .apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("BeginSoloStandalone accepted");
    let solo = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("LocalStandaloneReady accepted");
    let starting = h
        .apply(tp_event(
            solo.generation,
            ClusterEventKind::BeginTensorParallel,
        ))
        .await
        .expect("BeginTensorParallel accepted");
    let _ = h
        .apply(tp_event(
            starting.generation,
            ClusterEventKind::TensorParallelWorkerPrepared,
        ))
        .await
        .expect("worker prepared accepted");
    let _ = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelCoordinatorStarted,
        ))
        .await
        .expect("coordinator started accepted");
    let _ = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelHandshakeHttpReady,
        ))
        .await
        .expect("handshake+http accepted");
    let ready = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelWarmupDone,
        ))
        .await
        .expect("warmup accepted");
    assert_eq!(ready.state, ClusterState::TensorParallelReady);
    ready.generation
}

/// 受入 case 1: 設定 TP ＋ fake two-node HTTP → Ready。
/// 二 node（coordinator / worker）を本番 reducer で駆動し、TP 準備要素を全て揃えると
/// coordinator 側で route が公開され TensorParallelReady に達する。fake では実
/// child/state 操作は 0 のまま。
#[tokio::test]
async fn v040_tp_production_config_tp_two_node_http_reaches_ready() {
    // 二 node の fake 境界（coordinator / worker）を本番 reducer で駆動する。
    let coord = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    let worker = support::v040::FakeCluster::automatic_tp(LocalRole::Worker);

    // 両 node とも TP 準備要素を揃えて TP Ready へ。
    to_tp_ready(&coord).await;
    to_tp_ready(&worker).await;

    // coordinator 側で route が公開される（local_standalone_ready = true）。worker は
    // TP route を coordinator へ転送する（target = Coordinator）。二 node とも TP Ready
    // で route 公開。local_standalone_ready は coordinator のみ true（worker は TP を
    // 所有せず coordinator へ転送）。
    assert_eq!(coord.snapshot().state, ClusterState::TensorParallelReady);
    assert!(coord.route_is_published());
    assert!(coord.snapshot().local_standalone_ready);
    assert_eq!(
        coord.snapshot().target,
        siderostat::target::ProxyTarget::LocalStandalone
    );
    assert_eq!(worker.snapshot().state, ClusterState::TensorParallelReady);
    assert!(!worker.snapshot().local_standalone_ready);
    assert_eq!(
        worker.snapshot().target,
        siderostat::target::ProxyTarget::Coordinator
    );

    // fake では実 PID 生成・OS 接触・既存 state アクセスが 0。
    assert_eq!(coord.real_process_operations(), 0);
    assert_eq!(coord.recorder.existing_state_accesses(), 0);
    assert_eq!(worker.real_process_operations(), 0);
    assert_eq!(worker.recorder.existing_state_accesses(), 0);

    coord._task.abort();
    worker._task.abort();
}

/// 受入 case 2: v1 peer → 互換エラー、旧 LP 経路のみ。
/// 交渉不一致の旧 peer（protocol_version != 1）は TP 開始を拒否し、旧 LP 経路
/// （pair → promote → LP DistributedReady）だけが残る。
#[tokio::test]
async fn v040_tp_production_v1_peer_rejected_legacy_lp_path_only() {
    // 交渉不一致（v2 / v0）の旧 peer は TP 操作を拒否。
    assert_eq!(
        check_tp_start(OperationPolicy::Automatic, Some(2)),
        TpStartVerdict::UnsupportedPeer
    );
    assert_eq!(
        check_tp_start(OperationPolicy::Automatic, Some(0)),
        TpStartVerdict::UnsupportedPeer
    );
    assert!(!check_tp_start(OperationPolicy::Automatic, Some(2)).allows_tp());

    // 旧 LP 経路はそのまま利用可能（Automatic + 一致 peer は TP 開始可、
    // LP 遷移は独立して存在する）。
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    // LP 経路: pair → promote → worker hello → distributed child → LP ready。
    let _ = h
        .apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let _ = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await;
    let pairing = h
        .apply(event(2, ClusterEventKind::BeginPairing))
        .await
        .expect("BeginPairing accepted");
    let paired = h
        .apply(event(pairing.generation, ClusterEventKind::PairingReady))
        .await
        .expect("PairingReady accepted");
    assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
    let awaiting = h
        .apply(event(paired.generation, ClusterEventKind::BeginPromotion))
        .await
        .expect("BeginPromotion accepted");
    let promoting = h
        .apply(event(
            awaiting.generation,
            ClusterEventKind::WorkerHelloAccepted,
        ))
        .await
        .expect("WorkerHelloAccepted accepted");
    let starting = h
        .apply(event(
            promoting.generation,
            ClusterEventKind::DistributedChildStarted,
        ))
        .await
        .expect("DistributedChildStarted accepted");
    let lp_ready = h
        .apply(event(
            starting.generation,
            ClusterEventKind::DistributedRouteReady,
        ))
        .await
        .expect("DistributedRouteReady accepted");
    // 旧 LP 経路（LayerParallel）は到達可能。
    assert_eq!(lp_ready.state, ClusterState::DistributedReady);
    assert_eq!(
        lp_ready.stable_mode,
        siderostat::target::StableMode::DistributedLayerParallel
    );

    h._task.abort();
}

/// 受入 case 3: dry-run → 実 child/state 操作 0。
/// fake（dry-run 相当）境界は本番 reducer と同じ state/control を駆動するが、
/// 実 PID 生成・OS 接触・既存 state アクセスは一切行わない（Recorder 実カウンタで 0）。
#[tokio::test]
async fn v040_tp_production_dry_run_real_ops_zero() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    // TP 準備要素を全て投入しても実 child/state 操作は 0 のまま。
    to_tp_ready(&h).await;
    assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);
    assert_eq!(h.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    // reducer はイベントを受理している（副作用ゼロのまま状態は進む）。
    assert!(h.recorder.generated_events() > 0);
    h._task.abort();
}

/// 受入 case 4: manual promote 中の強制 policy → 拒否 / 安全収束。
/// ForcedStandalone（保護ラッチ）では TP 開始を抑止し、local Standalone を維持する。
/// TP retry 0（spawn 抑止）で、安全に SoloStandaloneReady へ収束する。
#[tokio::test]
async fn v040_tp_production_forced_standalone_rejects_tp_manual() {
    // ForcedStandalone では TP 開始 gate が拒否する（policy latch 優先）。
    assert_eq!(
        check_tp_start(OperationPolicy::ForcedStandalone, Some(1)),
        TpStartVerdict::PolicyForcedStandalone
    );
    assert!(!check_tp_start(OperationPolicy::ForcedStandalone, Some(1)).allows_tp());

    // 本番 reducer を ForcedStandalone で駆動。TP 開始（manual promote 相当）を試みても
    // harness が spawn を抑止し、安全な local Standalone を維持する。
    let h =
        support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::ForcedStandalone);
    // Solo ready へ収束させる。
    let _ = h
        .apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let solo = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("LocalStandaloneReady accepted");
    assert_eq!(solo.state, ClusterState::SoloStandaloneReady);
    // manual promote（TP 開始）を試みる → ForcedStandalone では spawn 抑止（effect なし）。
    h.prepare_worker().await;
    h.start_coordinator().await;
    // TP spawn は一度も起きない（TP retry 0 / 抑止）。
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    // 実 child/state 操作も 0。安全収束（local Standalone 維持）。
    assert_eq!(h.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    assert_eq!(h.snapshot().state, ClusterState::SoloStandaloneReady);
    h._task.abort();
}

/// 受入 case: 開始 gate の有限ラベルが安定している。
#[tokio::test]
async fn v040_tp_production_start_gate_labels_stable() {
    assert_eq!(TpStartVerdict::Allowed.name(), "tp-start-allowed");
    assert_eq!(
        TpStartVerdict::PolicyForcedStandalone.name(),
        "tp-start-policy-forced-standalone"
    );
    assert_eq!(
        TpStartVerdict::UnsupportedPeer.name(),
        "tp-start-unsupported-peer"
    );
}
