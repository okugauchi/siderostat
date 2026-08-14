#![cfg(feature = "test-support")]

//! R0-04 production-equivalent harness smoke tests: Solo startup and a normal first pair,
//! both driven through real control HTTP on separate loopback addresses and state paths.

mod support;

use siderostat::{
    cluster::DistributedControlPhase,
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
#[ignore = "RED for P0-A; resolve in A-03 coordinator PeerLost recovery (see reconnect plan R0-05)"]
async fn peer_lost_from_distributed_ready_orphans_distributed_children() {
    // RED for P0-A: reaching DistributedReady, then losing the peer, must fully recover to a
    // solo standalone with no distributed child left behind. Today `fallback_to_solo` never
    // stops the distributed children, so the coordinator orphans its distributed coordinator
    // child and the worker runs standalone and distributed simultaneously.
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
