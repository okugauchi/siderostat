#![cfg(feature = "test-support")]

//! R0-04 production-equivalent harness smoke tests: Solo startup and a normal first pair,
//! both driven through real control HTTP on separate loopback addresses and state paths.

mod support;

use siderostat::{
    admission::AdmissionState,
    cluster::{DistributedControlPhase, EventOwner},
    target::{ClusterState, LocalRole, ProxyTarget, StableMode},
};
use std::time::Duration;
use support::{Node, TwoNode, wait_until};

/// One-node observation captured from the reconnect diagnostics contract plus the proxy
/// target / admission surface, so every E-01 checkpoint can assert state, stable mode,
/// cluster + control generation, lease, control phase, target, admission, and child identity.
struct NodeObserved {
    state: ClusterState,
    stable_mode: StableMode,
    generation: u64,
    phase: DistributedControlPhase,
    lease_valid: bool,
    standalone_running: bool,
    standalone_ready: Option<bool>,
    standalone_identity: Option<u64>,
    distributed_running: bool,
    distributed_identity: Option<u64>,
    proxy_target: ProxyTarget,
    proxy_ready: bool,
    admission: AdmissionState,
}

async fn observe(node: &Node) -> NodeObserved {
    let diagnostics = node.production.diagnostics().await;
    let snapshot = node.mode.snapshot();
    let proxy = node.proxy.target_snapshot();
    let standalone = diagnostics
        .children
        .standalone
        .as_ref()
        .expect("standalone child");
    let (distributed_running, distributed_identity) = match node.role {
        LocalRole::Coordinator => {
            let child = diagnostics
                .children
                .distributed_coordinator
                .as_ref()
                .expect("coordinator child");
            (child.running, child.generation)
        }
        LocalRole::Worker => {
            let child = diagnostics
                .children
                .distributed_worker
                .as_ref()
                .expect("worker child");
            (child.running, child.generation)
        }
        LocalRole::Unknown => panic!("unknown role"),
    };
    NodeObserved {
        state: snapshot.state,
        stable_mode: snapshot.stable_mode,
        generation: diagnostics.control_session.generation,
        phase: diagnostics.control_session.phase,
        lease_valid: diagnostics.control_session.lease.valid,
        standalone_running: standalone.running,
        standalone_ready: standalone.ready,
        standalone_identity: standalone.generation,
        distributed_running,
        distributed_identity,
        proxy_target: proxy.target,
        proxy_ready: proxy.ready,
        admission: node.proxy.admission().snapshot().state,
    }
}

/// A node that is Solo serving: SoloStandaloneReady, local standalone running and ready,
/// admission serving, and the proxy targeting the local standalone.
async fn assert_solo_serving(node: &Node) {
    let observed = observe(node).await;
    assert_eq!(
        observed.state,
        ClusterState::SoloStandaloneReady,
        "solo serving cluster state"
    );
    assert_eq!(
        observed.stable_mode,
        StableMode::SoloStandalone,
        "solo serving stable mode"
    );
    assert!(
        observed.standalone_running,
        "solo serving standalone running"
    );
    assert_eq!(
        observed.standalone_ready,
        Some(true),
        "solo serving standalone readiness"
    );
    assert!(
        observed.standalone_identity.is_some(),
        "solo serving standalone identity"
    );
    assert!(
        !observed.distributed_running,
        "solo serving must not run a distributed child"
    );
    assert_eq!(
        observed.admission,
        AdmissionState::Serving,
        "solo serving admission"
    );
    assert_eq!(
        observed.proxy_target,
        ProxyTarget::LocalStandalone,
        "solo serving proxy target"
    );
    assert!(observed.proxy_ready, "solo serving proxy ready");
}

/// A node that is Paired serving: PairedStandaloneReady, admission serving, and the proxy
/// target follows the role (coordinator serves the local standalone, worker serves the peer
/// coordinator).
async fn assert_paired_serving(node: &Node) {
    let observed = observe(node).await;
    assert_eq!(
        observed.state,
        ClusterState::PairedStandaloneReady,
        "paired serving cluster state"
    );
    assert_eq!(
        observed.stable_mode,
        StableMode::PairedStandalone,
        "paired serving stable mode"
    );
    assert_eq!(
        observed.admission,
        AdmissionState::Serving,
        "paired serving admission"
    );
    let expected_target = match node.role {
        LocalRole::Coordinator => ProxyTarget::LocalStandalone,
        LocalRole::Worker => ProxyTarget::Coordinator,
        LocalRole::Unknown => panic!("unknown role"),
    };
    assert_eq!(
        observed.proxy_target, expected_target,
        "paired serving proxy target for {:?}",
        node.role
    );
    assert!(
        !observed.distributed_running,
        "paired serving must not run a distributed child"
    );
}

/// Both nodes converged to DistributedReady with matching child identities and no standalone.
async fn assert_distributed_consistent(coordinator: &Node, worker: &Node) {
    let coordinator = observe(coordinator).await;
    let worker = observe(worker).await;

    for observed in [&coordinator, &worker] {
        assert_eq!(
            observed.state,
            ClusterState::DistributedReady,
            "distributed cluster state"
        );
        assert_eq!(
            observed.stable_mode,
            StableMode::DistributedMxfp4,
            "distributed stable mode"
        );
        assert!(observed.distributed_running, "distributed child running");
        assert!(
            observed.distributed_identity.is_some(),
            "distributed child identity"
        );
        assert!(
            !observed.standalone_running,
            "no standalone child in distributed mode"
        );
        assert_eq!(
            observed.admission,
            AdmissionState::Serving,
            "distributed admission"
        );
        assert!(observed.lease_valid, "distributed peer lease valid");
    }

    assert_eq!(
        coordinator.generation, worker.generation,
        "coordinator/worker control generation diverged in distributed"
    );
}

/// Poll until a single node reaches the given cluster state, or return the last snapshot. Used
/// when only one side of a restart is expected to move (the other is being restarted).
async fn wait_until_node_state(node: &Node, state: ClusterState, timeout: Duration) -> bool {
    wait_until(
        timeout,
        || async move { node.mode.snapshot().state == state },
    )
    .await
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

    // Serving surface: admission serving, role-correct proxy target, and a valid peer lease.
    for observed in [&coordinator, &worker] {
        assert_eq!(
            observed.admission,
            AdmissionState::Serving,
            "paired serving admission"
        );
        assert!(observed.lease_valid, "paired peer lease must be valid");
    }
    assert_eq!(
        coordinator.proxy_target,
        ProxyTarget::LocalStandalone,
        "coordinator paired proxy target"
    );
    assert_eq!(
        worker.proxy_target,
        ProxyTarget::Coordinator,
        "worker paired proxy target"
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
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");

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
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    assert!(
        coordinator_child.child().is_running(),
        "coordinator distributed child should be running in DistributedReady"
    );
    assert!(
        worker_child.child().is_running(),
        "worker distributed child should be running in DistributedReady"
    );

    // Expire control/route on both nodes by taking the peer control HTTP servers down.
    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;

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
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;
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
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
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
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    assert!(coordinator_child.child().is_running());
    assert!(worker_child.child().is_running());

    // Peer unreachable and the coordinator DS4 route drops at the same time.
    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;
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
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    worker_child.set_stop_fails(true);
    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;

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
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;
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

// ---- E-01 P0 reconnect matrix acceptance suite --------------------------------
//
// Each P0-C row maps one-to-one to a test below:
//   PairedStandaloneReady + cable blip          -> paired_cable_blip_repairs_to_paired_over_10_cycles
//   DistributedReady + cable blip               -> distributed_cable_blip_rebuilds_new_generation_over_10_cycles
//   coordinator process only restart            -> coordinator_only_restart_converges_generation_without_orphan
//   worker process only restart                 -> worker_only_restart_converges_including_coordinator_low_generation
//   both process restart                        -> both_process_restart_converges_from_persisted_generation
//   pair response delay / duplicate             -> pair_response_delay_and_duplicate_converge_idempotently
//
// Every scenario asserts the common surface through the shared helpers
// (`assert_solo_serving`, `assert_paired_consistent`, `assert_paired_serving`,
// `assert_distributed_consistent`) at the intermediate Solo serving and Paired serving
// checkpoints, not only at the final checkpoint.

/// Re-pair and re-promote both nodes to DistributedReady with full consistency checks. Used by
/// the process-restart scenarios after both nodes have recovered to Solo.
async fn repair_to_distributed(harness: &TwoNode) {
    harness.pair().await.expect("re-pair after restart");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        paired,
        "nodes did not re-pair after restart; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    assert_paired_serving(&harness.coordinator).await;
    assert_paired_serving(&harness.worker).await;
    harness
        .promote_to_distributed()
        .await
        .expect("re-promote to DistributedReady");
    assert_distributed_consistent(&harness.coordinator, &harness.worker).await;
}

/// P0-C row 1: from PairedStandaloneReady, a cable blip drops both nodes to Solo serving, then
/// re-pair restores Paired serving, repeated over 10 cycles. The control session generation
/// never drops below its pre-blip value.
#[tokio::test]
async fn paired_cable_blip_repairs_to_paired_over_10_cycles() {
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("initial pair");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not pair");
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    let initial_generation = harness.worker.production.control_session_generation().await;

    for cycle in 0..10 {
        // Cable blip: the peer becomes unreachable in both directions.
        harness.coordinator.stop_serve().await;
        harness.worker.stop_serve().await;
        harness
            .worker
            .production
            .reconcile()
            .await
            .expect("worker PeerLost reconcile after blip");
        harness
            .coordinator
            .production
            .reconcile()
            .await
            .expect("coordinator PeerLost reconcile after blip");
        let solo = harness
            .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
            .await;
        assert!(
            solo,
            "cycle {cycle}: nodes did not recover to solo; coordinator={:?} worker={:?}",
            harness.coordinator.mode.snapshot().state,
            harness.worker.mode.snapshot().state,
        );
        // Intermediate Solo serving checkpoint.
        assert_solo_serving(&harness.coordinator).await;
        assert_solo_serving(&harness.worker).await;

        // Cable restored: re-serve both and re-pair.
        harness
            .coordinator
            .restart_serve()
            .await
            .expect("coordinator serve restart");
        harness
            .worker
            .restart_serve()
            .await
            .expect("worker serve restart");
        harness.pair().await.expect("re-pair after blip");
        let repaired = harness
            .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
            .await;
        assert!(
            repaired,
            "cycle {cycle}: nodes did not re-pair; coordinator={:?} worker={:?}",
            harness.coordinator.mode.snapshot().state,
            harness.worker.mode.snapshot().state,
        );
        // Intermediate Paired serving checkpoint.
        assert_paired_consistent(&harness.coordinator, &harness.worker).await;
        assert_paired_serving(&harness.coordinator).await;
        assert_paired_serving(&harness.worker).await;

        let generation = harness.worker.production.control_session_generation().await;
        assert!(
            generation >= initial_generation,
            "cycle {cycle}: control session generation dropped below its pre-blip value"
        );
    }
    harness.shutdown().await;
}

/// P0-C row 2: from DistributedReady, a cable blip stops the distributed children and both
/// nodes serve Solo, then re-pair + re-promote rebuilds a new DistributedReady generation over
/// 10 cycles. Child generations are never reused.
#[tokio::test]
async fn distributed_cable_blip_rebuilds_new_generation_over_10_cycles() {
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("initial pair");
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    assert_distributed_consistent(&harness.coordinator, &harness.worker).await;
    let initial_generation = harness.worker.production.control_session_generation().await;

    for cycle in 0..10 {
        let prior_coordinator_gen = coordinator_child
            .child()
            .identity()
            .expect("coordinator identity")
            .generation;
        let prior_worker_gen = worker_child
            .child()
            .identity()
            .expect("worker identity")
            .generation;

        // Cable blip: peer unreachable, both nodes recover to Solo and stop the distributed
        // children.
        harness.coordinator.stop_serve().await;
        harness.worker.stop_serve().await;
        harness
            .worker
            .production
            .reconcile()
            .await
            .expect("worker PeerLost reconcile after blip");
        harness
            .coordinator
            .production
            .reconcile()
            .await
            .expect("coordinator PeerLost reconcile after blip");
        let solo = harness
            .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
            .await;
        assert!(
            solo,
            "cycle {cycle}: nodes did not recover to solo; coordinator={:?} worker={:?}",
            harness.coordinator.mode.snapshot().state,
            harness.worker.mode.snapshot().state,
        );
        assert!(
            !coordinator_child.child().is_running(),
            "cycle {cycle}: coordinator distributed child not orphaned"
        );
        assert!(
            !worker_child.child().is_running(),
            "cycle {cycle}: worker distributed child not orphaned"
        );
        // Intermediate Solo serving checkpoint.
        assert_solo_serving(&harness.coordinator).await;
        assert_solo_serving(&harness.worker).await;

        // Cable restored: re-serve, re-pair, re-promote.
        harness
            .coordinator
            .restart_serve()
            .await
            .expect("coordinator serve restart");
        harness
            .worker
            .restart_serve()
            .await
            .expect("worker serve restart");
        harness.pair().await.expect("re-pair after blip");
        let repaired = harness
            .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
            .await;
        assert!(
            repaired,
            "cycle {cycle}: nodes did not re-pair; coordinator={:?} worker={:?}",
            harness.coordinator.mode.snapshot().state,
            harness.worker.mode.snapshot().state,
        );
        // Intermediate Paired serving checkpoint.
        assert_paired_consistent(&harness.coordinator, &harness.worker).await;
        assert_paired_serving(&harness.coordinator).await;
        assert_paired_serving(&harness.worker).await;

        harness
            .promote_to_distributed()
            .await
            .expect("re-promote to DistributedReady");
        let new_coordinator_gen = coordinator_child
            .child()
            .identity()
            .expect("coordinator identity")
            .generation;
        let new_worker_gen = worker_child
            .child()
            .identity()
            .expect("worker identity")
            .generation;
        assert_ne!(
            new_coordinator_gen, prior_coordinator_gen,
            "cycle {cycle}: coordinator child generation must not be reused"
        );
        assert_ne!(
            new_worker_gen, prior_worker_gen,
            "cycle {cycle}: worker child generation must not be reused"
        );
        assert_distributed_consistent(&harness.coordinator, &harness.worker).await;
        let generation = harness.worker.production.control_session_generation().await;
        assert!(
            generation >= initial_generation,
            "cycle {cycle}: control session generation dropped below its pre-blip value"
        );
    }

    // Stop the fresh distributed children explicitly; the harness ends in DistributedReady.
    coordinator_child.child().stop();
    worker_child.child().stop();
    harness.shutdown().await;
}

/// P0-C row 3: restarting only the coordinator process (from Paired and from Distributed)
/// converges the control session generation with no orphaned distributed child and auto-recovers
/// to Paired / Distributed.
#[tokio::test]
async fn coordinator_only_restart_converges_generation_without_orphan() {
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");

    // --- From PairedStandaloneReady ---
    harness.pair().await.expect("initial pair");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not pair");
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    let persisted = harness
        .coordinator
        .production
        .control_session_generation()
        .await;

    // The coordinator process restarts alone: its control goes down, the worker recovers to
    // Solo, then the coordinator boots fresh at its persisted generation.
    harness.coordinator.stop_serve().await;
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker PeerLost reconcile while coordinator restarts");
    let worker_solo = wait_until_node_state(
        &harness.worker,
        ClusterState::SoloStandaloneReady,
        Duration::from_secs(10),
    )
    .await;
    assert!(worker_solo, "worker did not recover to solo");
    harness
        .coordinator
        .restart_control_process(persisted)
        .await
        .expect("coordinator process restart");
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    harness
        .pair()
        .await
        .expect("coordinator auto-pairs after restart");
    let repaired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        repaired,
        "coordinator-only restart did not re-pair; coordinator={:?} worker={:?}",
        harness.coordinator.mode.snapshot().state,
        harness.worker.mode.snapshot().state,
    );
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    assert_paired_serving(&harness.coordinator).await;
    assert_paired_serving(&harness.worker).await;
    assert!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await
            >= persisted,
        "coordinator session generation must not drop below its persisted value after restart"
    );

    // --- From DistributedReady ---
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    let persisted = harness
        .coordinator
        .production
        .control_session_generation()
        .await;

    // The coordinator process restarts from Distributed: the worker loses it, recovers to Solo
    // and stops its distributed child; the coordinator's own restart terminates its child.
    harness.coordinator.stop_serve().await;
    harness
        .worker
        .production
        .reconcile()
        .await
        .expect("worker PeerLost reconcile while coordinator restarts from distributed");
    let solo = wait_until_node_state(
        &harness.worker,
        ClusterState::SoloStandaloneReady,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        solo,
        "worker did not recover to solo after coordinator restart"
    );
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child must not be orphaned after coordinator restart"
    );
    harness
        .coordinator
        .restart_control_process(persisted)
        .await
        .expect("coordinator process restart from distributed");
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child must not be orphaned after its own restart"
    );
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    repair_to_distributed(&harness).await;
    assert!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await
            >= persisted,
        "coordinator session generation must not drop after distributed recovery"
    );

    coordinator_child.child().stop();
    worker_child.child().stop();
    harness.shutdown().await;
}

/// P0-C row 4: restarting only the worker process (from Paired and from Distributed) converges
/// the control session generation, including the coordinator-low-generation case where the
/// restarted worker carries a higher persistent generation.
#[tokio::test]
async fn worker_only_restart_converges_including_coordinator_low_generation() {
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");

    // --- From PairedStandaloneReady, equal generation ---
    harness.pair().await.expect("initial pair");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not pair");
    let persisted = harness.worker.production.control_session_generation().await;

    harness.worker.stop_serve().await;
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator PeerLost reconcile while worker restarts");
    let coordinator_solo = wait_until_node_state(
        &harness.coordinator,
        ClusterState::SoloStandaloneReady,
        Duration::from_secs(10),
    )
    .await;
    assert!(coordinator_solo, "coordinator did not recover to solo");
    harness
        .worker
        .restart_control_process(persisted)
        .await
        .expect("worker process restart");
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    harness.pair().await.expect("re-pair after worker restart");
    let repaired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(repaired, "nodes did not re-pair after worker restart");
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    assert!(
        harness.worker.production.control_session_generation().await >= persisted,
        "worker session generation must not drop below its persisted value after restart"
    );

    // --- Coordinator low generation: the restarted worker carries a higher generation, and the
    // coordinator (session authority) adopts it on re-pair. ---
    let worker_ahead = harness.worker.production.control_session_generation().await + 100;
    harness.worker.stop_serve().await;
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator PeerLost reconcile before worker low-gen restart");
    let solo = wait_until_node_state(
        &harness.coordinator,
        ClusterState::SoloStandaloneReady,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        solo,
        "coordinator did not recover to solo before low-gen restart"
    );
    harness
        .worker
        .restart_control_process(worker_ahead)
        .await
        .expect("worker restart with higher generation");
    harness
        .pair()
        .await
        .expect("pair adopts higher worker generation");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(
        converged,
        "nodes did not converge after worker low-gen restart"
    );
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    let coordinator_session = harness
        .coordinator
        .production
        .control_session_generation()
        .await;
    let worker_session = harness.worker.production.control_session_generation().await;
    assert_eq!(
        coordinator_session, worker_session,
        "generation must converge after worker low-gen restart"
    );
    assert!(
        coordinator_session >= worker_ahead,
        "coordinator must adopt the higher worker generation"
    );

    // --- From DistributedReady ---
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .clone()
        .expect("coordinator child");
    let persisted = harness.worker.production.control_session_generation().await;

    harness.worker.stop_serve().await;
    harness
        .coordinator
        .production
        .reconcile()
        .await
        .expect("coordinator PeerLost reconcile while worker restarts from distributed");
    let distributed_solo = wait_until_node_state(
        &harness.coordinator,
        ClusterState::SoloStandaloneReady,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        distributed_solo,
        "coordinator did not recover to solo after worker restart from distributed"
    );
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child must not be orphaned after worker restart"
    );
    harness
        .worker
        .restart_control_process(persisted)
        .await
        .expect("worker process restart from distributed");
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child must not be orphaned after its own restart"
    );
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    repair_to_distributed(&harness).await;
    assert!(
        harness.worker.production.control_session_generation().await >= persisted,
        "worker session generation must not drop after distributed recovery"
    );

    worker_child.child().stop();
    coordinator_child.child().stop();
    harness.shutdown().await;
}

/// P0-C row 5: restarting both processes (from Paired and from Distributed) converges from the
/// persisted control session generation with no orphaned distributed child.
#[tokio::test]
async fn both_process_restart_converges_from_persisted_generation() {
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");

    // --- From PairedStandaloneReady ---
    harness.pair().await.expect("initial pair");
    let paired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(paired, "nodes did not pair");
    let coordinator_persisted = harness
        .coordinator
        .production
        .control_session_generation()
        .await;
    let worker_persisted = harness.worker.production.control_session_generation().await;
    assert_eq!(
        coordinator_persisted, worker_persisted,
        "paired nodes share one session generation"
    );

    harness
        .coordinator
        .restart_control_process(coordinator_persisted)
        .await
        .expect("coordinator process restart");
    harness
        .worker
        .restart_control_process(worker_persisted)
        .await
        .expect("worker process restart");
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    harness.pair().await.expect("re-pair after both restart");
    let repaired = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(repaired, "nodes did not re-pair after both restart");
    assert_paired_consistent(&harness.coordinator, &harness.worker).await;
    assert!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await
            >= coordinator_persisted,
        "both-restart must converge from the persisted generation"
    );

    // --- From DistributedReady ---
    harness
        .promote_to_distributed()
        .await
        .expect("promote to DistributedReady");
    let coordinator_child = harness
        .coordinator
        .coordinator_child
        .clone()
        .expect("coordinator child");
    let worker_child = harness.worker.worker_child.clone().expect("worker child");
    let coordinator_persisted = harness
        .coordinator
        .production
        .control_session_generation()
        .await;
    let worker_persisted = harness.worker.production.control_session_generation().await;

    harness
        .coordinator
        .restart_control_process(coordinator_persisted)
        .await
        .expect("coordinator process restart from distributed");
    harness
        .worker
        .restart_control_process(worker_persisted)
        .await
        .expect("worker process restart from distributed");
    assert!(
        !coordinator_child.child().is_running(),
        "coordinator distributed child not orphaned after both restart"
    );
    assert!(
        !worker_child.child().is_running(),
        "worker distributed child not orphaned after both restart"
    );
    assert_solo_serving(&harness.coordinator).await;
    assert_solo_serving(&harness.worker).await;

    repair_to_distributed(&harness).await;
    assert!(
        harness
            .coordinator
            .production
            .control_session_generation()
            .await
            >= coordinator_persisted,
        "both-restart distributed recovery must converge from the persisted generation"
    );

    coordinator_child.child().stop();
    worker_child.child().stop();
    harness.shutdown().await;
}

/// P0-C row 6: delayed and duplicate Pair responses converge idempotently to the same control
/// session generation (starting from a worker-higher baseline).
#[tokio::test]
async fn pair_response_delay_and_duplicate_converge_idempotently() {
    let harness = TwoNode::boot_with_baseline(0, 100)
        .await
        .expect("boot with a worker-higher baseline");
    harness
        .coordinator
        .production
        .pair()
        .await
        .expect("initial pair adopts the worker generation");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(converged, "nodes did not pair");
    let session = harness.worker.production.control_session_generation().await;
    assert!(session >= 100, "session must keep the worker baseline");

    // Simulate delayed peer responses (interleaved reconciles) and duplicate pairs: every round
    // must converge idempotently to the same session generation with both nodes Paired.
    for round in 0..3 {
        let _ = harness.coordinator.production.reconcile().await;
        let _ = harness.worker.production.reconcile().await;
        harness
            .coordinator
            .production
            .pair()
            .await
            .expect("duplicate pair is idempotent");
        let converged = harness
            .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
            .await;
        assert!(
            converged,
            "round {round}: nodes did not converge to PairedStandaloneReady"
        );
        assert_paired_consistent(&harness.coordinator, &harness.worker).await;
        assert_paired_serving(&harness.coordinator).await;
        assert_paired_serving(&harness.worker).await;
        assert_eq!(
            harness
                .coordinator
                .production
                .control_session_generation()
                .await,
            session,
            "round {round}: coordinator session must not change"
        );
        assert_eq!(
            harness.worker.production.control_session_generation().await,
            session,
            "round {round}: worker session must not change"
        );
    }
    harness.shutdown().await;
}

#[tokio::test]
async fn coordinator_backoff_reconcile_recovers_to_paired_after_deadline() {
    // B-02: the coordinator's periodic reconcile drives backoff recovery. Before the deadline
    // reconcile keeps Backoff (and never starts pair/promote concurrently); after the deadline
    // it recovers exactly once to the stable paired state.
    let harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(converged, "nodes did not pair before backoff test");

    let coordinator = harness.node(LocalRole::Coordinator);
    // Drive the coordinator into Backoff. Record with a future timestamp so the retry deadline
    // (now + promotion_backoff = 50ms) sits comfortably ahead of the real clock, giving a
    // robust before/after split even on a slow CI host.
    let real_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    coordinator
        .production
        .record_promotion_failure(
            siderostat::cluster::ClusterFailure::HelloTimeout,
            real_now + 400,
        )
        .await
        .expect("record promotion failure");
    assert_eq!(coordinator.mode.snapshot().state, ClusterState::Backoff);

    // Before the deadline: reconcile keeps Backoff and does not move to pair/promote.
    let before = coordinator
        .production
        .reconcile()
        .await
        .expect("reconcile before deadline");
    assert_eq!(before.state, ClusterState::Backoff);

    // After the deadline: reconcile recovers exactly once to PairedStandaloneReady.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = coordinator
        .production
        .reconcile()
        .await
        .expect("reconcile after deadline");
    assert_eq!(after.state, ClusterState::PairedStandaloneReady);

    // Recovery is one-shot: a further reconcile is a no-op, staying Paired Standalone.
    let again = coordinator
        .production
        .reconcile()
        .await
        .expect("reconcile after recovery");
    assert_eq!(again.state, ClusterState::PairedStandaloneReady);

    harness.shutdown().await;
}

#[tokio::test]
async fn peer_loss_during_backoff_recovers_to_solo_first() {
    // B-02: peer loss is prioritized over the backoff deadline. Even though the coordinator is
    // in Backoff, losing the peer routes through the shared recovery owner to Solo Standalone
    // instead of waiting out the deadline.
    let mut harness = TwoNode::boot().await.expect("boot two-node harness");
    harness.pair().await.expect("coordinator-initiated pair");
    let converged = harness
        .wait_until_both(ClusterState::PairedStandaloneReady, Duration::from_secs(10))
        .await;
    assert!(converged, "nodes did not pair before backoff test");

    {
        let coordinator = harness.node(LocalRole::Coordinator);
        let real_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        coordinator
            .production
            .record_promotion_failure(siderostat::cluster::ClusterFailure::HelloTimeout, real_now)
            .await
            .expect("record promotion failure");
        assert_eq!(coordinator.mode.snapshot().state, ClusterState::Backoff);
    }

    // Stop the worker's control server so the coordinator's reconcile sees the peer as gone
    // and invalidates the route, then recovers to Solo rather than honoring the backoff.
    harness.worker.stop_serve().await;
    let recovered = harness
        .node(LocalRole::Coordinator)
        .production
        .reconcile()
        .await
        .expect("reconcile during backoff with peer loss");
    assert_eq!(recovered.state, ClusterState::SoloStandaloneReady);

    harness.shutdown().await;
}
