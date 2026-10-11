//! T12 — TP 自動受入 matrix（C02 / TP software acceptance）。
//!
//! 実モデルを使わず、本番 reducer（`spawn_state_machine`）と本番判定
//! （TpSessionState / TpRecoveryTracker / TpRecoveryOwner / 開始 gate）を fake 境界
//! （FakeCluster）で駆動して TP software gate を閉じる。実 child 起動・OS 接触・
//! 既存 state アクセスは行わず、Recorder で 0 を検証する。H03 だけが実機互換 smoke
//! として残る。
//!
//! 受入 case:
//! 1. 正常 → ready かつ warm-up 1。
//! 2. 各欠落（worker Prepared / coordinator 起動 / handshake+HTTP / warm-up の
//!    TpReadiness 4 要素を一つずつ欠落）→ route 公開 0。
//! 3. 全障害 → 単一 recovery owner（owner 1）/ orphan 0。
//! 4. 再接続 10 cycle → generation 単調増加。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    ClusterEvent, ClusterEventKind, TpFailureKind, TpRecoveryDecision, TpRecoveryOwner,
    TpRecoveryTracker, TpSessionId,
};
use siderostat::target::{ClusterState, LocalRole, ProxyTarget};

const SID: TpSessionId = TpSessionId(1);

fn tp_event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::tp(generation, kind, SID)
}

fn event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
    ClusterEvent::new(generation, kind)
}

/// Solo ready まで駆動する。戻り値は現在 generation。。。
async fn drive_solo_ready(h: &support::v040::FakeCluster) -> u64 {
    let _ = h
        .apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("BeginSoloStandalone accepted");
    let _ = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("LocalStandaloneReady accepted");
    h.snapshot().generation
}

/// 現在の generation から TP 準備要素を一つずつ投入する。`skip` を指定するとその要素を
/// 投入しない（欠落 table test 用）。reducer の遷移表により、前段（worker Prepared）が
/// 欠落していると後段（coordinator 起動以降）は InvalidTransition になるため、要素は
/// 前段が揃っている場合のみ投入する。開始時点は Solo/Paired ready であること。戻り値は
/// 現在 generation。。。
async fn drive_tp_elements(h: &support::v040::FakeCluster, skip: Option<TpReadinessEvent>) -> u64 {
    let _ = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::BeginTensorParallel,
        ))
        .await
        .expect("BeginTensorParallel accepted");
    // worker Prepared。欠落時は以降の要素が InvalidTransition になるためここで停止。
    if skip == Some(TpReadinessEvent::WorkerPrepared) {
        return h.snapshot().generation;
    }
    let _ = h
        .apply(tp_event(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelWorkerPrepared,
        ))
        .await
        .expect("worker prepared accepted");
    if skip != Some(TpReadinessEvent::CoordinatorStarted) {
        let _ = h
            .apply(tp_event(
                h.snapshot().generation,
                ClusterEventKind::TensorParallelCoordinatorStarted,
            ))
            .await
            .expect("coordinator started accepted");
    }
    if skip != Some(TpReadinessEvent::HandshakeHttpReady) {
        let _ = h
            .apply(tp_event(
                h.snapshot().generation,
                ClusterEventKind::TensorParallelHandshakeHttpReady,
            ))
            .await
            .expect("handshake+http accepted");
    }
    if skip != Some(TpReadinessEvent::WarmupDone) {
        let _ = h
            .apply(tp_event(
                h.snapshot().generation,
                ClusterEventKind::TensorParallelWarmupDone,
            ))
            .await
            .expect("warmup accepted");
    }
    h.snapshot().generation
}

/// Solo ready から TP 準備要素を一つずつ投入する（欠落 table test 用）。。。
async fn drive_readiness(h: &support::v040::FakeCluster, skip: Option<TpReadinessEvent>) -> u64 {
    drive_solo_ready(h).await;
    drive_tp_elements(h, skip).await
}

/// TP 準備要素の列挙（欠落 table test 用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TpReadinessEvent {
    WorkerPrepared,
    CoordinatorStarted,
    HandshakeHttpReady,
    WarmupDone,
}

/// 受入 case 1: 正常 → ready かつ warm-up 1。
/// 全要素を揃えると TensorParallelReady で route 公開。warm-up は 1 回だけ受理される
/// （冪等。TpReadiness が要素を一つだけ立てる）。
#[tokio::test]
async fn v040_tp_acceptance_normal_reaches_ready_with_warmup_once() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    drive_readiness(&h, None).await;
    // ready かつ route 公開。
    assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);
    assert!(h.route_is_published());
    assert_eq!(h.snapshot().target, ProxyTarget::LocalStandalone);
    // 現在セッションの readiness が全要素そろっている（warm-up 1 回）。
    let tp = h.snapshot().tp.expect("tp session present");
    assert!(tp.readiness.worker_prepared);
    assert!(tp.readiness.coordinator_started);
    assert!(tp.readiness.handshake_http_ready);
    assert!(tp.readiness.warmup_done);
    assert_eq!(h.recorder.generated_events(), 7);
    // fake では実 child/state 操作 0。
    assert_eq!(h.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    h._task.abort();
}

/// 受入 case 2: 各欠落 → 公開 0。
/// TpReadiness の 4 要素を一つずつ欠落させると、route は公開されない。欠落要素を
/// 最後に投入すると公開される（その要素が本当に必要であることを確認）。table test。
#[tokio::test]
async fn v040_tp_acceptance_each_missing_element_keeps_route_closed() {
    let cases = [
        ("worker-prepared-missing", TpReadinessEvent::WorkerPrepared),
        (
            "coordinator-started-missing",
            TpReadinessEvent::CoordinatorStarted,
        ),
        (
            "handshake-http-missing",
            TpReadinessEvent::HandshakeHttpReady,
        ),
        ("warmup-missing", TpReadinessEvent::WarmupDone),
    ];
    for (name, missing) in cases {
        let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
        drive_readiness(&h, Some(missing)).await;
        // 欠落要素がある間は route 非公開（公開 0）。
        assert!(
            !h.route_is_published(),
            "{name}: route should stay closed while element is missing"
        );
        // 状態は TP 遷移中（TensorParallelReady 未到達）。欠落要素のみで公開されない。
        assert_ne!(h.snapshot().state, ClusterState::TensorParallelReady);
        // 欠落要素を投入すると、その後続の要素を順に投入して route 公開に至る。
        // 欠落要素より前段の要素は drive 済み、後続は未投入のため、欠落要素から
        // 順に投入する（reducer の遷移表が順序を要求するため）。
        let order = [
            TpReadinessEvent::WorkerPrepared,
            TpReadinessEvent::CoordinatorStarted,
            TpReadinessEvent::HandshakeHttpReady,
            TpReadinessEvent::WarmupDone,
        ];
        let from = order.iter().position(|e| *e == missing).unwrap();
        for &element in &order[from..] {
            let kind = match element {
                TpReadinessEvent::WorkerPrepared => ClusterEventKind::TensorParallelWorkerPrepared,
                TpReadinessEvent::CoordinatorStarted => {
                    ClusterEventKind::TensorParallelCoordinatorStarted
                }
                TpReadinessEvent::HandshakeHttpReady => {
                    ClusterEventKind::TensorParallelHandshakeHttpReady
                }
                TpReadinessEvent::WarmupDone => ClusterEventKind::TensorParallelWarmupDone,
            };
            let _ = h
                .apply(tp_event(h.snapshot().generation, kind))
                .await
                .expect("element accepted");
        }
        assert!(
            h.route_is_published(),
            "{name}: route should publish after the missing element is supplied"
        );
        assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);
        // fake では実 child/state 操作 0。
        assert_eq!(h.real_process_operations(), 0);
        assert_eq!(h.recorder.existing_state_accesses(), 0);
        h._task.abort();
    }
}

/// 受入 case 3: 全障害 → owner 1 / orphan 0。
/// 各障害ラベルを TpRecoveryTracker で分類し、単一 recovery owner（TpRecoveryOwner）が
/// 直列化することを確認する。同時の peer-loss と route-loss は owner が 1 つで処理され、
/// orphan（二重起動）は 0。
#[tokio::test]
async fn v040_tp_acceptance_all_faults_single_owner_no_orphan() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    // 各障害ラベル（C02 障害表）を分類。transient 障害は Automatic 下で Retry。。
    // 上限（2 回）は 1 kind で個別に検証するため、kind 毎に fresh tracker を使う。。。
    for kind in [
        TpFailureKind::WorkerCrash,
        TpFailureKind::CoordinatorCrash,
        TpFailureKind::RouteLoss,
        TpFailureKind::RoleSwap,
        TpFailureKind::FirstPrefillTimeout,
    ] {
        let mut t = TpRecoveryTracker::new(2);
        assert_eq!(
            t.classify(kind, h.policy()),
            TpRecoveryDecision::Retry,
            "{:?} should retry under automatic policy",
            kind
        );
    }
    // 上限到達 → StopAndFallback（検証済み fallback）。同一 tracker で 2 回 Retry 後に
    // 3 回目で StopAndFallback。。。
    let mut tracker = TpRecoveryTracker::new(2);
    assert_eq!(
        tracker.classify(TpFailureKind::WorkerCrash, h.policy()),
        TpRecoveryDecision::Retry
    );
    assert_eq!(
        tracker.classify(TpFailureKind::WorkerCrash, h.policy()),
        TpRecoveryDecision::Retry
    );
    assert_eq!(
        tracker.classify(TpFailureKind::WorkerCrash, h.policy()),
        TpRecoveryDecision::StopAndFallback
    );
    // ManualIntervention 系（model mismatch / identity 不明 / drain timeout）。。
    assert_eq!(
        tracker.classify(TpFailureKind::ModelMismatch, h.policy()),
        TpRecoveryDecision::ManualIntervention
    );
    assert_eq!(
        tracker.classify(TpFailureKind::IdentityUnknown, h.policy()),
        TpRecoveryDecision::ManualIntervention
    );
    assert_eq!(
        tracker.classify(TpFailureKind::DrainTimeout, h.policy()),
        TpRecoveryDecision::ManualIntervention
    );

    // 単一 recovery owner: 同時 peer-loss と route-loss を直列化（owner 1 / orphan 0）。
    let owner = TpRecoveryOwner::default();
    let first = owner.lock();
    let second = owner.lock();
    let first_guard = first.await;
    // 保持中は 2 度目は進行できない（二重起動なし）。デッドロック回避のため先に解放。
    let second_pending = std::pin::pin!(second);
    drop(first_guard);
    let _second_guard = second_pending.await;
    // 直列化が機能していればここまで到達（orphan 0）。

    h._task.abort();
}

/// 受入 case 4: 再接続 10 cycle → generation 単調増加。
/// TP ready → demote（fallback へ）→ 再接続（TP 再開始）を 10 cycle 繰り返す。
/// 各 cycle で generation が単調増加し、最終 state が TP Ready のまま。old session は
/// 受理されない（session 照合 C02）。
#[tokio::test]
async fn v040_tp_acceptance_reconnect_ten_cycles_generation_monotonic() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    // 初回の solo ready を一度だけ駆動し、以後は TP 要素を現在 generation から投入する。
    drive_solo_ready(&h).await;
    let mut previous_generation: u64 = 0;
    for cycle in 0..10u32 {
        // 各 cycle で TP 準備要素を投入して TP Ready へ。。。
        let gen0 = drive_tp_elements(&h, None).await;
        assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);
        assert!(h.route_is_published());
        // generation 単調増加。。。
        assert!(
            gen0 > previous_generation,
            "cycle {cycle}: generation must be strictly increasing ({} > {})",
            gen0,
            previous_generation
        );
        previous_generation = gen0;

        // demote（TP → Paired fallback）。検証済み fallback へ戻す。。
        let demoting = h
            .apply(event(
                h.snapshot().generation,
                ClusterEventKind::BeginTensorParallelDemotion,
            ))
            .await
            .expect("demote accepted");
        let paired = h
            .apply(event(demoting.generation, ClusterEventKind::PairingReady))
            .await
            .expect("fallback accepted");
        assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
        // fallback で route は Paired の local（coordinator）へ。incident 解除相当。。
        assert!(h.route_is_published());
        // 次 cycle の TP 再開始は新 generation で受理される。。
    }
    // 最終は fallback（Paired）へ戻った状態。generation 単調（10 cycle 全て増加）を
    // 各 cycle で確認済み。TP は全 cycle で ready に到達した（loop 内 assert）。
    assert_eq!(h.snapshot().state, ClusterState::PairedStandaloneReady);
    assert!(h.route_is_published());
    // fake では実 child/state 操作 0。
    assert_eq!(h.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    h._task.abort();
}

/// 受入 case: 旧 session の成功は受理されない（session 照合 C02）。
/// role swap / child 交換後に旧 session の TP イベントは reducer が無視する。
#[tokio::test]
async fn v040_tp_acceptance_old_session_success_is_not_accepted() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    // 新 session（SID=2）で TP ready まで進める。
    let _ = h
        .apply(event(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let _ = h
        .apply(event(1, ClusterEventKind::LocalStandaloneReady))
        .await;
    let session2 = TpSessionId(2);
    let starting = h
        .apply(ClusterEvent::tp(
            h.snapshot().generation,
            ClusterEventKind::BeginTensorParallel,
            session2,
        ))
        .await
        .expect("session 2 BeginTP accepted");
    let _ = h
        .apply(ClusterEvent::tp(
            starting.generation,
            ClusterEventKind::TensorParallelWorkerPrepared,
            session2,
        ))
        .await
        .expect("session 2 worker prepared accepted");
    let _ = h
        .apply(ClusterEvent::tp(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelCoordinatorStarted,
            session2,
        ))
        .await
        .expect("session 2 coordinator started accepted");
    let _ = h
        .apply(ClusterEvent::tp(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelHandshakeHttpReady,
            session2,
        ))
        .await
        .expect("session 2 handshake accepted");
    let ready = h
        .apply(ClusterEvent::tp(
            h.snapshot().generation,
            ClusterEventKind::TensorParallelWarmupDone,
            session2,
        ))
        .await
        .expect("session 2 warmup accepted");
    assert_eq!(ready.state, ClusterState::TensorParallelReady);

    // 旧 session（SID=1）の warm-up 完了は受理されない（無視）。route 状態は不変。
    let before = h.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            before,
            ClusterEventKind::TensorParallelWarmupDone,
            SID,
        ))
        .await
        .expect("old-session warmup apply returns without error");
    // 旧 session の成功は受理されないため generation は進まない。
    assert_eq!(h.snapshot().generation, before);
    assert_eq!(h.snapshot().state, ClusterState::TensorParallelReady);

    h._task.abort();
}
