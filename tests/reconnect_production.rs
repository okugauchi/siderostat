#![cfg(feature = "test-support")]

//! R0-04 production-equivalent harness smoke tests: Solo startup and a normal first pair,
//! both driven through real control HTTP on separate loopback addresses and state paths.

mod support;

use siderostat::{
    cluster::{DistributedControlPhase, EventOwner},
    target::{ClusterState, LocalRole},
};
use std::time::Duration;
use support::{Node, TwoNode, wait_until};

/// One-node observation captured from the reconnect diagnostics contract.
struct NodeObserved {
    generation: u64,
    phase: DistributedControlPhase,
    standalone_running: bool,
    standalone_ready: Option<bool>,
    standalone_identity: Option<u64>,
    distributed_running: bool,
}

async fn observe(node: &Node) -> NodeObserved {
    let diagnostics = node.production.diagnostics().await;
    let standalone = diagnostics
        .children
        .standalone
        .as_ref()
        .expect("standalone child");
    let distributed_running = match node.role {
        LocalRole::Coordinator => {
            diagnostics
                .children
                .distributed_coordinator
                .as_ref()
                .expect("coordinator child")
                .running
        }
        LocalRole::Worker => {
            diagnostics
                .children
                .distributed_worker
                .as_ref()
                .expect("worker child")
                .running
        }
        LocalRole::Unknown => panic!("unknown role"),
    };
    NodeObserved {
        generation: diagnostics.control_session.generation,
        phase: diagnostics.control_session.phase,
        standalone_running: standalone.running,
        standalone_ready: standalone.ready,
        standalone_identity: standalone.generation,
        distributed_running,
    }
}

/// Compare both nodes' state and child identity in a single assertion helper.
async fn assert_paired_consistent(coordinator: &Node, worker: &Node) {
    let coordinator = observe(coordinator).await;
    let worker = observe(worker).await;

    // Shared control session: same generation, both Paired.
    assert_eq!(
        coordinator.generation, worker.generation,
        "coordinator/worker control generation diverged"
    );
    assert_eq!(
        coordinator.phase,
        DistributedControlPhase::Paired,
        "coordinator control phase"
    );
    assert_eq!(
        worker.phase,
        DistributedControlPhase::Paired,
        "worker control phase"
    );

    // Coordinator keeps its standalone child running with a recorded identity; no distributed
    // coordinator child was started.
    assert!(
        coordinator.standalone_running,
        "coordinator standalone child should be running"
    );
    assert_eq!(
        coordinator.standalone_ready,
        Some(true),
        "coordinator standalone readiness"
    );
    assert!(
        coordinator.standalone_identity.is_some(),
        "coordinator standalone child must record a generation"
    );
    assert!(
        !coordinator.distributed_running,
        "coordinator distributed child must not be running in paired standalone"
    );

    // Worker drains and stops its standalone child; no distributed worker child started.
    assert!(
        !worker.standalone_running,
        "worker standalone child should be stopped after pairing"
    );
    assert_eq!(
        worker.standalone_ready,
        Some(false),
        "worker standalone readiness after pairing"
    );
    assert!(
        worker.standalone_identity.is_none(),
        "stopped worker standalone must expose no child identity"
    );
    assert!(
        !worker.distributed_running,
        "worker distributed child must not be running in paired standalone"
    );
}

#[tokio::test]
async fn solo_startup_readies_both_nodes_on_separate_state() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    assert_eq!(
        harness.coordinator.mode.snapshot().state,
        ClusterState::SoloStandaloneReady,
        "coordinator solo startup"
    );
    assert_eq!(
        harness.worker.mode.snapshot().state,
        ClusterState::SoloStandaloneReady,
        "worker solo startup"
    );

    // Each node uses a distinct persistent state path.
    assert_ne!(
        harness.coordinator.config.cluster.state_path,
        harness.worker.config.cluster.state_path
    );
    assert_ne!(
        harness.coordinator.config.cluster.control_port,
        harness.worker.config.cluster.control_port
    );

    // Both standalone children were started with a recorded identity.
    assert!(
        harness.coordinator.standalone.child().is_running(),
        "coordinator standalone running"
    );
    assert!(
        harness.worker.standalone.child().is_running(),
        "worker standalone running"
    );
    assert_eq!(harness.coordinator.standalone.child().starts(), 1);
    assert_eq!(harness.worker.standalone.child().starts(), 1);

    harness.shutdown().await;
}

#[tokio::test]
async fn normal_first_pair_converges_over_control_http() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");

    harness.pair().await.expect("coordinator-initiated pair");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge to PairedStandaloneReady; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    assert_paired_consistent(&harness.coordinator, &harness.worker).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn wait_until_reports_last_snapshot_on_timeout() {
    // Sanity check the deadline-based helper: a predicate that never becomes true returns
    // false, and the caller can still read the final snapshot afterwards.
    let probe = false;
    let reached = wait_until(Duration::from_millis(80), || async { probe }).await;
    assert!(!reached);
}

#[tokio::test]
async fn peer_lost_from_distributed_ready_orphans_distributed_children() {
    // P0-A: reaching DistributedReady, then losing the peer, must fully recover to a
    // solo standalone with no distributed child left behind. The single recovery owner stops
    // both the coordinator and worker distributed children and restarts each local standalone.
    let harness = TwoNode::boot().await.expect("boot two-node harness");

    harness.pair().await.expect("coordinator-initiated pair");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not pair");

    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");

    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .as_ref()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.as_ref().expect("worker child");
    assert!(
        coordinator_child.child().is_running(),
        "coordinator distributed child should be running in DistributedReady"
    );
    assert!(
        worker_child.child().is_running(),
        "worker distributed child should be running in DistributedReady"
    );

    // Expire control/route on both nodes by taking the peer control HTTP servers down.
    harness.coordinator.serve.abort();
    harness.worker.serve.abort();

    // Each node observes the peer is unreachable and falls back to a solo standalone.
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker PeerLost reconcile");
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator PeerLost reconcile");

    let recovered = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        recovered,
        "nodes did not return to solo; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    // P0-A lifecycle consistency: after PeerLost no distributed child may remain, and the
    // worker must never run standalone and distributed at the same time.
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child must be stopped after PeerLost (P0-A: orphaned)"
    );
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child must be stopped after PeerLost (P0-A: orphaned)"
    );
    assert!(
        harness.worker.standalone.child().is_running(),
        "worker standalone must be running after PeerLost recovery"
    );
    assert!(
        !(worker_child.child().is_running() && harness.worker.standalone.child().is_running()),
        "worker must not run standalone and distributed simultaneously (P0-A: coexistence)"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn coordinator_adopts_higher_worker_generation_on_pair() {
    // P0-B: when only the worker carries a higher persistent control session generation, the
    // coordinator (session authority) adopts that higher generation via the /v1/node candidate
    // and the pair converges direction-independently.
    let worker_baseline = 100;
    let coordinator_baseline = 0;
    let harness = TwoNode::boot_with_baseline(coordinator_baseline, worker_baseline)
        .await
        .expect("boot two-node harness");

    harness.coordinator.production.pair().await.expect(
        "coordinator Pair should adopt the worker's higher control generation and succeed \
             (P0-B: rejected with 409 GenerationMismatch instead)",
    );
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge to PairedStandaloneReady; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn higher_coordinator_baseline_generation_is_followed_by_worker() {
    // Reverse direction, fixing the direction-dependence: when the coordinator carries the
    // higher control session generation, the worker follows it and the pair converges.
    let harness = TwoNode::boot_with_baseline(100, 0)
        .await
        .expect("boot two-node harness");

    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("coordinator Pair must succeed when the coordinator generation is higher");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    harness.shutdown().await;
}

// ---- G-05 control session generation negotiation -----------------------------

/// After a worker-higher pair, both nodes converge on the worker's control session
/// generation: the coordinator (session authority) adopts it via the /v1/node candidate, and
/// a subsequent duplicate pair stays on the same generation without lowering it.
#[tokio::test]
async fn worker_higher_pair_converges_on_the_worker_control_session_generation() {
    let harness = TwoNode::boot_with_baseline(0, 100)
        .await
        .expect("boot two-node harness");

    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("coordinator Pair adopts the worker's higher control session generation");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    let worker_session = harness.worker.production.control_session_generation().await;
    assert!(
        worker_session >= 100,
        "worker control session generation must not drop below its persistent value; got {worker_session}"
    );
    let coordinator_session = harness
        .coordinator
        .production
        .control_session_generation()
        .await;
    assert_eq!(
        coordinator_session, worker_session,
        "coordinator must adopt the worker's higher control session generation"
    );

    // A duplicate pair after convergence must not lower either node's session generation.
    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("duplicate pair stays on the same session generation");
    assert_eq!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await,
        coordinator_session,
        "duplicate pair must not lower the coordinator session generation"
    );
    assert_eq!(
        harness.worker.production.control_session_generation().await,
        worker_session,
        "duplicate pair must not lower the worker session generation"
    );
    harness.shutdown().await;
}

/// A coordinator-higher pair keeps the coordinator's higher control session generation, and a
/// repeated pair stays on it (direction matrix, design §8).
#[tokio::test]
async fn coordinator_higher_pair_keeps_the_coordinator_control_session_generation() {
    let harness = TwoNode::boot_with_baseline(100, 0)
        .await
        .expect("boot two-node harness");

    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("coordinator Pair with the higher generation succeeds");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );

    let coordinator_session = harness
        .coordinator
        .production
        .control_session_generation()
        .await;
    assert!(
        coordinator_session >= 100,
        "coordinator session generation must not drop below its persistent value"
    );
    let worker_session = harness.worker.production.control_session_generation().await;
    assert_eq!(
        worker_session, coordinator_session,
        "worker follows the coordinator's higher control session generation"
    );

    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("duplicate pair stays on the coordinator-higher session generation");
    assert_eq!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await,
        coordinator_session,
        "duplicate pair must not lower the coordinator session generation"
    );
    assert_eq!(
        harness.worker.production.control_session_generation().await,
        worker_session,
        "duplicate pair must not lower the worker session generation"
    );
    harness.shutdown().await;
}

// ---- A-04 PeerLost recovery races and failures --------------------------------

/// Duplicate PeerLost reconcile after a completed recovery is a no-op: state stays Solo and
/// the standalone is not started again.
#[tokio::test]
async fn peer_loss_recovery_is_idempotent_on_duplicate_reconcile() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    harness.coordinator.serve.abort();
    harness.worker.serve.abort();
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker PeerLost reconcile");
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator PeerLost reconcile");
    let solo = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(solo, "nodes did not recover to solo");

    let coordinator_starts = harness.coordinator.standalone.child().starts();
    let worker_starts = harness.worker.standalone.child().starts();

    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("duplicate worker reconcile is a no-op");
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("duplicate coordinator reconcile is a no-op");

    assert_eq!(
        harness.coordinator.mode.snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert_eq!(
        harness.worker.mode.snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert_eq!(
        harness.coordinator.standalone.child().starts(),
        coordinator_starts,
        "duplicate recovery must not restart the coordinator standalone"
    );
    assert_eq!(
        harness.worker.standalone.child().starts(),
        worker_starts,
        "duplicate recovery must not restart the worker standalone"
    );
    harness.shutdown().await;
}

/// Recovery then re-promotion must start fresh distributed children with new identities; the
/// old child generation is never reused.
#[tokio::test]
async fn recovery_then_repromotion_uses_new_child_generation() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .as_ref()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.as_ref().expect("worker child");
    let first_coordinator_gen = coordinator_child
        .child()
        .identity()
        .expect("coordinator identity")
        .generation;
    let first_worker_gen = worker_child
        .child()
        .identity()
        .expect("worker identity")
        .generation;

    // Recover both nodes to Solo through the single recovery owner while control HTTP stays up.
    harness
        .coordinator
        .production
        .recover_from_peer_loss(EventOwner::Control)
        .await
        .expect("coordinator recovery");
    harness
        .worker
        .production
        .recover_from_peer_loss(EventOwner::Control)
        .await
        .expect("worker recovery");
    let solo = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(solo, "nodes did not recover to solo");
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child stopped after recovery"
    );
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child stopped after recovery"
    );

    // Re-pair and re-promote; the new children carry new generations.
    harness.pair().await.expect("re-pair after recovery");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not re-pair after recovery");
    harness
        .promote_to_distributed()
        .await
        .expect("re-promote to DistributedReady");
    let second_coordinator_gen = coordinator_child
        .child()
        .identity()
        .expect("coordinator identity")
        .generation;
    let second_worker_gen = worker_child
        .child()
        .identity()
        .expect("worker identity")
        .generation;
    assert_ne!(
        second_coordinator_gen, first_coordinator_gen,
        "coordinator child generation must not be reused after recovery"
    );
    assert_ne!(
        second_worker_gen, first_worker_gen,
        "worker child generation must not be reused after recovery"
    );
    // Stop the fresh distributed children explicitly; the harness ends in DistributedReady.
    coordinator_child.child().stop();
    worker_child.child().stop();
    harness.shutdown().await;
}

/// Control lease loss and the route-loss monitor firing together must converge to a single
/// Solo recovery with no orphaned distributed child.
#[tokio::test]
async fn route_loss_monitor_and_peer_loss_reconcile_race_converge_to_solo() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .as_ref()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.as_ref().expect("worker child");
    assert!(coordinator_child.child().is_running());
    assert!(worker_child.child().is_running());

    // Peer unreachable and the coordinator DS4 route drops at the same time.
    harness.coordinator.serve.abort();
    harness.worker.serve.abort();
    coordinator_child.lose_route();

    let coordinator = harness.coordinator.production.clone();
    let worker = harness.worker.production.clone();
    let coordinator_reconcile = tokio::spawn(async move {
        let _ = coordinator.reconcile().await;
    });
    let worker_reconcile = tokio::spawn(async move {
        let _ = worker.reconcile().await;
    });
    coordinator_reconcile
        .await
        .expect("coordinator reconcile task");
    worker_reconcile.await.expect("worker reconcile task");

    let solo = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        solo,
        "nodes did not converge to solo after route-loss + reconcile race"
    );
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child must not be orphaned by the race"
    );
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child must not be orphaned by the race"
    );

    // Give the route-loss monitor time to run; it must be a stale no-op against the already
    // recovered solo state.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        harness.coordinator.mode.snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert!(!coordinator_child.child().is_running());
    harness.shutdown().await;
}

/// A distributed-worker stop failure must keep the node in SoloStandaloneStarting + Unavailable
/// (never faking Ready) and retry on the next reconcile.
#[tokio::test]
async fn worker_stop_failure_keeps_recovery_from_faking_ready() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let worker_child = harness.worker.worker_child.as_ref().expect("worker child");
    worker_child.set_stop_fails(true);
    harness.coordinator.serve.abort();
    harness.worker.serve.abort();

    harness
        .worker
        .production
        .reconcile()
        .await
        .expect_err("worker recovery must fail while distributed stop fails");
    assert_ne!(
        harness.worker.mode.snapshot().state,
        ClusterState::SoloStandaloneReady,
        "stop failure must not fake SoloReady"
    );
    assert!(
        worker_child.child().is_running(),
        "distributed worker child stays running while stop fails"
    );

    // Lift the failure; the next reconcile retries to a clean Solo recovery.
    worker_child.set_stop_fails(false);
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker recovery retry");
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator recovery");
    let solo = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(solo, "nodes did not recover after stop-failure retry");
    assert!(!worker_child.child().is_running());
    harness.shutdown().await;
}

/// A standalone-start failure during recovery must keep the node from faking Ready and retry.
#[tokio::test]
async fn standalone_start_failure_keeps_recovery_from_faking_ready() {
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let worker_child = harness.worker.worker_child.as_ref().expect("worker child");
    harness.coordinator.serve.abort();
    harness.worker.serve.abort();
    harness.worker.standalone.set_start_fails(true);

    harness
        .worker
        .production
        .reconcile()
        .await
        .expect_err("worker recovery must fail while standalone start fails");
    assert_ne!(
        harness.worker.mode.snapshot().state,
        ClusterState::SoloStandaloneReady,
        "standalone-start failure must not fake SoloReady"
    );
    assert!(
        !worker_child.child().is_running(),
        "distributed worker child was stopped even though standalone start failed"
    );

    harness.worker.standalone.set_start_fails(false);
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker recovery retry");
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator recovery");
    let solo = harness
        .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(solo, "nodes did not recover after standalone-start retry");
    harness.shutdown().await;
}
