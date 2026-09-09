//! P04 — ForcedStandalone cluster-wide 適用の受入 case（C03）。
//!
//! 実モデルを使わず、本番 reducer（`spawn_state_machine`）と PolicyJournal（実 StateStore
//! reader）を fake 境界で駆動して ForcedStandalone の cluster-wide 適用を検証する。
//! 実 child 起動・OS 接触・既存 state アクセスは行わない（Recorder で 0 を検証）。
//!
//! 受入 case:
//! 1. TP/LP/Paired/Pairing/Promoting 各状態 → 安全収束（本番 reducer で各状態から
//!    ForcedStandalone 適用 → 安全な Solo/Paired ready へ）。
//! 2. drain timeout → 処理中 request を強制破棄せず Failed。
//! 3. 片 ack 紛失 → Complete でない。
//! 4. 再起動 → policy 維持（journal から復元）。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    ClusterEvent, ClusterEventKind, ForceApplyCoordinator, ForceNodePhase, OperationPolicy,
    PERSISTENT_STATE_SCHEMA_VERSION, PersistentOperationPhase, PolicyJournal, StateStore,
};
use siderostat::target::{ClusterState, LocalRole};
use std::{fs, path::PathBuf};

fn temporary_state_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("ds4-v040-force-test-{}", uuid::Uuid::new_v4()))
        .join("cluster-state.json")
}

/// 受入 case 1: TP/LP/Paired/Pairing/Promoting 各状態 → 安全収束。
/// ForcedStandalone 適用は、どの分散状態からでも安全な local Standalone へ収束する。
/// 本番 reducer の遷移表（BeginTensorParallelDemotion / BeginDemotion / PeerLost →
/// Demoting → Solo/Paired ready）で検証する。実 child 起動・OS 接触は 0。
#[tokio::test]
async fn v040_force_standalone_converges_from_all_states() {
    let h = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);

    // --- TP ready からの収束 ---
    // TP ready まで駆動（solo → BeginTP → worker → coordinator → handshake → warmup）。
    let _ = h
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            g,
            ClusterEventKind::BeginTensorParallel,
            siderostat::cluster::TpSessionId(1),
        ))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            g,
            ClusterEventKind::TensorParallelWorkerPrepared,
            siderostat::cluster::TpSessionId(1),
        ))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            g,
            ClusterEventKind::TensorParallelCoordinatorStarted,
            siderostat::cluster::TpSessionId(1),
        ))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            g,
            ClusterEventKind::TensorParallelHandshakeHttpReady,
            siderostat::cluster::TpSessionId(1),
        ))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::tp(
            g,
            ClusterEventKind::TensorParallelWarmupDone,
            siderostat::cluster::TpSessionId(1),
        ))
        .await;
    assert_eq!(h.handle.snapshot().state, ClusterState::TensorParallelReady);
    // Force 適用: BeginTensorParallelDemotion → Demoting → PairingReady → Paired/Solo ready。
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::new(
            g,
            ClusterEventKind::BeginTensorParallelDemotion,
        ))
        .await;
    let g = h.handle.snapshot().generation;
    let _ = h
        .apply(ClusterEvent::new(g, ClusterEventKind::PairingReady))
        .await;
    assert_eq!(
        h.handle.snapshot().state,
        ClusterState::PairedStandaloneReady,
        "TP ready must converge to Paired ready under Force"
    );
    h._task.abort();

    // --- LP（DistributedReady）からの収束 ---
    let h2 = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    let _ = h2
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::BeginPairing))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::PairingReady))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::BeginPromotion))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::WorkerHelloAccepted))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(
            g,
            ClusterEventKind::DistributedChildStarted,
        ))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(
            g,
            ClusterEventKind::DistributedRouteReady,
        ))
        .await;
    assert_eq!(h2.handle.snapshot().state, ClusterState::DistributedReady);
    // Force 適用: BeginDemotion → Demoting → PairingReady → Paired ready。
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::BeginDemotion))
        .await;
    let g = h2.handle.snapshot().generation;
    let _ = h2
        .apply(ClusterEvent::new(g, ClusterEventKind::PairingReady))
        .await;
    assert_eq!(
        h2.handle.snapshot().state,
        ClusterState::PairedStandaloneReady
    );
    h2._task.abort();

    // --- Paired / Pairing / Promoting からの収束（PeerLost → Solo/Paired ready）---
    // PairedStandaloneReady から Force 適用（PeerLost → SoloStandaloneReady）。。。
    let h3 = support::v040::FakeCluster::automatic_tp(LocalRole::Coordinator);
    let _ = h3
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await;
    let g = h3.handle.snapshot().generation;
    let _ = h3
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await;
    let g = h3.handle.snapshot().generation;
    let _ = h3
        .apply(ClusterEvent::new(g, ClusterEventKind::BeginPairing))
        .await;
    let g = h3.handle.snapshot().generation;
    let _ = h3
        .apply(ClusterEvent::new(g, ClusterEventKind::PairingReady))
        .await;
    assert_eq!(
        h3.handle.snapshot().state,
        ClusterState::PairedStandaloneReady
    );
    let g = h3.handle.snapshot().generation;
    let _ = h3
        .apply(ClusterEvent::new(g, ClusterEventKind::PeerLost))
        .await;
    let g = h3.handle.snapshot().generation;
    let _ = h3
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await;
    assert_eq!(
        h3.handle.snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    // 全ケースで実 child/OS 接触 0。
    assert_eq!(h3.real_process_operations(), 0);
    assert_eq!(h3.recorder.existing_state_accesses(), 0);
    h3._task.abort();
}

/// 受入 case 2: drain timeout → 処理中 request を強制破棄せず Failed。
/// 適用調整（ForceApplyCoordinator）は drain timeout で terminal Failed となり、
/// Automatic へ戻らない。処理中 request の強制破棄は行わない。
#[test]
fn v040_force_standalone_drain_timeout_fails_without_force_dropping() {
    let mut c = ForceApplyCoordinator::begin(1, uuid::Uuid::new_v4());
    c.peer_intent_saved();
    c.begin_drain();
    c.drain_timeout();
    assert!(c.is_failed());
    assert!(!c.complete);
    // Failed 後も両 ack が揃っても Complete にはならない（Automatic へ戻さない）。
    c.note_local_ready();
    c.note_peer_ready();
    c.note_local_applied();
    c.note_peer_applied();
    assert!(!c.complete);
    assert!(c.is_failed());
}

/// 受入 case 3: 片 ack 紛失 → Complete でない。
#[test]
fn v040_force_standalone_one_ack_lost_is_not_complete() {
    let mut c = ForceApplyCoordinator::begin(2, uuid::Uuid::new_v4());
    c.peer_intent_saved();
    c.begin_drain();
    c.note_local_ready();
    c.note_peer_ready();
    c.note_local_applied();
    // peer ack が紛失（LocalReady のまま）。Complete にならない。
    assert!(!c.complete);
    assert_eq!(c.peer, ForceNodePhase::LocalReady);
    // peer ack が届くと Complete。両 ack が揃った時だけ。
    c.note_peer_applied();
    assert!(c.complete);
}

/// 受入 case 4: 再起動 → policy 維持。
/// ForcedStandalone intent は effect 前に永続化され、restart 後も journal の安全ラッチが
/// policy を維持し、TP spawn は発生しない。実 StateStore reader で検証する。
#[tokio::test]
async fn v040_force_standalone_restart_keeps_policy() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&store);
    let epoch = 42;
    let op_id = uuid::Uuid::new_v4();
    journal.persist_force_intent(epoch, op_id).unwrap();
    // crash を模して store を破棄し、同じ path から再開。
    drop(store);
    let reopened = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&reopened);
    let view = journal.load().unwrap();
    assert_eq!(view.operator_policy, OperationPolicy::ForcedStandalone);
    assert_eq!(view.policy_epoch, epoch);
    let pending = view
        .pending_operation
        .expect("pending intent survives restart");
    assert_eq!(pending.id, op_id);
    assert_eq!(pending.desired, OperationPolicy::ForcedStandalone);
    assert_eq!(pending.phase, PersistentOperationPhase::IntentSaved);
    // ForcedStandalone では TP spawn は発生しない（policy 維持）。
    let h =
        support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::ForcedStandalone);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    assert_eq!(h.real_process_operations(), 0);
    // 再起動後の適用調整も policy 維持（両 node ack で Complete、drain なしで）。
    let mut c = ForceApplyCoordinator::begin(epoch, op_id);
    assert_eq!(c.local, ForceNodePhase::IntentSaved);
    c.peer_intent_saved();
    c.note_local_ready();
    c.note_peer_ready();
    c.note_local_applied();
    c.note_peer_applied();
    assert!(c.complete);
    assert_eq!(c.policy_epoch, epoch);
    h._task.abort();
    drop(reopened);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

/// 補足: applied 結果を journal に記録すると Applied に進み、両 node の applied が
/// 揃ったことを表す（cluster-wide 完了後のみ）。
#[test]
fn v040_force_standalone_journal_applied_phase() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&store);
    let epoch = 7;
    journal
        .persist_force_intent(epoch, uuid::Uuid::new_v4())
        .unwrap();
    journal.record_force_applied(epoch).unwrap();
    let view = journal.load().unwrap();
    assert_eq!(view.applied_policy, OperationPolicy::ForcedStandalone);
    let pending = view
        .pending_operation
        .expect("applied intent kept for audit");
    assert_eq!(pending.phase, PersistentOperationPhase::Applied);
    assert!(pending.peer_ack);
    // schema は v2 のまま（未知 version を上書きしない）。
    let raw = fs::read(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(parsed["schema_version"], PERSISTENT_STATE_SCHEMA_VERSION);
    drop(store);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
