#![cfg(feature = "test-support")]

mod support;

use siderostat::{
    admission::{AdmissionError, AdmissionGate, AdmissionState, DrainError},
    canary::CanaryReason,
    recovery::{
        RecoveryDetector, RecoveryFailureReason, RecoveryObservation, RecoveryPhase,
        RecoveryReason, RecoveryRequest, RecoveryService, RecoveryStart, RecoveryState,
        RecoveryTrigger,
    },
    target::{ClusterState, LocalRole, StableMode},
};
use std::time::Duration;
use support::{TwoNode, inject_fake_worker_hello};

fn active(
    first_progress_observed: bool,
    active_age_millis: u64,
    progress_age_millis: Option<u64>,
    chunk_tps: Option<f64>,
) -> RecoveryObservation {
    RecoveryObservation::Active {
        first_progress_observed,
        active_age_millis,
        progress_age_millis,
        chunk_tps,
    }
}

fn distributed_gate(generation: u64) -> siderostat::recovery::RecoveryGate {
    siderostat::recovery::RecoveryGate {
        role: LocalRole::Coordinator,
        mode: StableMode::DistributedLayerParallel,
        state: ClusterState::DistributedReady,
        generation,
    }
}

fn recovery_request(key: &str) -> RecoveryRequest {
    RecoveryRequest::new(
        RecoveryReason::ThroughputDegraded,
        RecoveryTrigger::ManualCanaryFailure,
        key,
    )
    .expect("bounded recovery request")
}

async fn assert_recovery_result(
    harness: &TwoNode,
    before_coordinator: &siderostat::cluster::ChildDiagnostics,
    before_worker: &siderostat::cluster::ChildDiagnostics,
    service: &RecoveryService,
    recovery_id: &str,
    new_generation: u64,
) {
    let after_coordinator = harness.coordinator.production.diagnostics().await;
    let after_worker = harness.worker.production.diagnostics().await;
    let coordinator_child = after_coordinator
        .children
        .distributed_coordinator
        .as_ref()
        .expect("coordinator child diagnostics after recovery");
    let worker_child = after_worker
        .children
        .distributed_worker
        .as_ref()
        .expect("worker child diagnostics after recovery");

    assert_ne!(before_coordinator.generation, coordinator_child.generation);
    assert_ne!(before_coordinator.pid, coordinator_child.pid);
    assert_ne!(before_worker.generation, worker_child.generation);
    assert_ne!(before_worker.pid, worker_child.pid);
    assert_eq!(
        harness.coordinator.proxy.admission().snapshot().state,
        AdmissionState::Blocked
    );

    let job = service
        .status(recovery_id)
        .expect("recovery job status after canary");
    assert_eq!(job.state, RecoveryState::Succeeded);
    assert_eq!(job.phase, RecoveryPhase::Completed);
    assert_eq!(job.post_recovery_canary, Some(CanaryReason::Healthy));
    assert_eq!(job.new_cluster_generation, Some(new_generation));
}

#[test]
fn normal_short_request_is_not_degraded() {
    let mut detector = RecoveryDetector::default();

    assert_eq!(
        detector.observe_at(0, active(true, 500, Some(10), Some(24.0))),
        None
    );
    assert_eq!(
        detector.observe_at(1_000, active(true, 1_500, Some(20), Some(18.0))),
        None
    );
    assert_eq!(
        detector.observe_canary_at(2_000, CanaryReason::Healthy),
        None
    );
}

#[test]
fn long_normal_prefill_does_not_trigger_decode_recovery() {
    let mut detector = RecoveryDetector::default();

    assert_eq!(
        detector.observe_at(0, active(true, 300_000, Some(1_000), Some(7.0))),
        None
    );
    assert_eq!(
        detector.observe_at(30_000, active(true, 330_000, Some(2_000), Some(7.0))),
        None
    );
}

#[test]
fn low_tps_requires_a_sustained_window() {
    let mut detector = RecoveryDetector::default();
    let low = active(true, 10_000, Some(100), Some(4.9));

    assert_eq!(detector.observe_at(0, low), None);
    assert_eq!(detector.observe_at(29_999, low), None);
    assert_eq!(
        detector.observe_at(30_000, low),
        Some(RecoveryTrigger::LowDecodeTps)
    );
}

#[test]
fn first_token_stall_is_classified_before_progress_stall() {
    let mut detector = RecoveryDetector::default();

    assert_eq!(
        detector.observe_at(0, active(false, 29_999, None, None),),
        None
    );
    assert_eq!(
        detector.observe_at(30_000, active(false, 30_000, None, None),),
        Some(RecoveryTrigger::FirstTokenTimeout)
    );
}

#[test]
fn progress_stall_is_classified_after_a_progress_event() {
    let mut detector = RecoveryDetector::default();

    assert_eq!(
        detector.observe_at(0, active(true, 90_000, Some(59_999), Some(4.0))),
        None
    );
    assert_eq!(
        detector.observe_at(1, active(true, 90_001, Some(60_000), Some(4.0))),
        Some(RecoveryTrigger::ProgressStall)
    );
}

#[tokio::test]
async fn active_request_drain_preserves_order_and_timeout_without_kill() {
    let gate = AdmissionGate::new(1);
    gate.start_serving();
    let mut events = vec!["request-started"];
    let permit = gate.try_acquire(true).expect("active request permit");

    let drain = gate.drain(7, Duration::from_millis(10)).await;
    events.push("drain-timeout");
    assert_eq!(drain, Err(DrainError::Timeout));
    assert_eq!(gate.snapshot().state, AdmissionState::Draining);
    assert_eq!(gate.snapshot().in_flight, 1);
    assert!(matches!(
        gate.try_acquire(true),
        Err(AdmissionError::NotServing)
    ));

    drop(permit);
    events.push("request-finished");
    gate.drain(7, Duration::from_secs(1)).await.unwrap();
    events.push("drain-completed");
    assert_eq!(
        events,
        vec![
            "request-started",
            "drain-timeout",
            "request-finished",
            "drain-completed"
        ]
    );
    assert_eq!(gate.snapshot().state, AdmissionState::Blocked);
}

#[test]
fn repeated_canary_failure_has_no_automatic_retry() {
    let mut detector = RecoveryDetector::default();
    let service = RecoveryService::new();

    assert_eq!(
        detector.observe_canary_at(0, CanaryReason::HttpError),
        Some(RecoveryTrigger::ManualCanaryFailure)
    );
    assert_eq!(detector.observe_canary_at(1, CanaryReason::HttpError), None);
    let RecoveryStart::Created(job) = service.begin_at(
        distributed_gate(12),
        recovery_request("canary-failure"),
        100,
    ) else {
        panic!("first canary failure must create one recovery job");
    };
    assert_eq!(
        service.begin_at(distributed_gate(12), recovery_request("retry"), 101),
        RecoveryStart::Existing(job)
    );
}

#[test]
fn promotion_failure_stays_safe_and_suppresses_retry_loop() {
    let service = RecoveryService::new();
    let RecoveryStart::Created(job) = service.begin_at(
        distributed_gate(12),
        recovery_request("promotion-failure"),
        100,
    ) else {
        panic!("promotion failure test must create a recovery job");
    };
    service.mark_phase(
        &job.recovery_id,
        RecoveryPhase::Promoting,
        Some("blocked".into()),
    );
    let failed = service
        .mark_failed(
            &job.recovery_id,
            101,
            RecoveryFailureReason::PromotionFailed,
        )
        .expect("promotion failure must close the job");

    assert_eq!(failed.state, RecoveryState::Failed);
    assert_eq!(failed.phase, RecoveryPhase::Failed);
    assert_eq!(
        failed.failure_reason,
        Some(RecoveryFailureReason::PromotionFailed)
    );
    let RecoveryStart::Suppressed(retry) = service.begin_at(
        distributed_gate(12),
        recovery_request("retry-after-failure"),
        102,
    ) else {
        panic!("failed recovery must not immediately retry");
    };
    assert_eq!(retry.failure_reason, Some(RecoveryFailureReason::Cooldown));
}

#[tokio::test]
async fn control_unavailable_converges_to_standalone_without_orphans() {
    let mut harness = TwoNode::boot().await.unwrap();
    harness.pair().await.unwrap();
    harness.promote_to_distributed().await.unwrap();
    let coordinator_child = harness.coordinator.coordinator_child.clone().unwrap();
    let worker_child = harness.worker.worker_child.clone().unwrap();

    harness.coordinator.stop_serve().await;
    harness.worker.stop_serve().await;
    harness.worker.production.reconcile().await.unwrap();
    harness.coordinator.production.reconcile().await.unwrap();

    assert!(
        harness
            .wait_until_both(ClusterState::SoloStandaloneReady, Duration::from_secs(10))
            .await
    );
    assert!(!coordinator_child.child().is_running());
    assert!(!worker_child.child().is_running());
    assert!(harness.coordinator.standalone.child().is_running());
    assert!(harness.worker.standalone.child().is_running());
    harness.shutdown().await;
}

#[tokio::test]
async fn two_node_recovery_replaces_children_and_keeps_canary_gate_blocked() {
    let harness = TwoNode::boot().await.unwrap();
    harness.pair().await.unwrap();
    harness.promote_to_distributed().await.unwrap();

    let before_coordinator = harness.coordinator.production.diagnostics().await;
    let before_worker = harness.worker.production.diagnostics().await;
    let old_generation = harness.coordinator.mode.snapshot().generation;
    assert!(harness.coordinator.production.try_claim_recovery_owner());
    let service = RecoveryService::new();
    let RecoveryStart::Created(job) = service.begin_at(
        distributed_gate(old_generation),
        recovery_request("two-node"),
        100,
    ) else {
        panic!("two-node recovery must create a recovery job");
    };
    service.mark_phase(
        &job.recovery_id,
        RecoveryPhase::AdmissionBlocked,
        Some("blocked".into()),
    );

    let paired = harness
        .coordinator
        .production
        .demote_for_recovery(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
    assert_eq!(
        harness.coordinator.proxy.admission().snapshot().state,
        AdmissionState::Blocked
    );

    let hello = inject_fake_worker_hello(harness.coordinator.ds4_distributed_port);
    let distributed = harness
        .coordinator
        .production
        .promote_for_recovery()
        .await
        .unwrap();
    hello.await.unwrap();
    assert!(distributed.generation > old_generation);
    assert!(
        harness
            .wait_until_both(ClusterState::DistributedReady, Duration::from_secs(10))
            .await
    );
    service.mark_phase(
        &job.recovery_id,
        RecoveryPhase::PostRecoveryCanary,
        Some("blocked".into()),
    );
    service.mark_post_recovery_canary(&job.recovery_id, CanaryReason::Healthy);
    service.mark_succeeded(&job.recovery_id, 101, distributed.generation);
    assert_recovery_result(
        &harness,
        &before_coordinator
            .children
            .distributed_coordinator
            .clone()
            .unwrap(),
        &before_worker.children.distributed_worker.clone().unwrap(),
        &service,
        &job.recovery_id,
        distributed.generation,
    )
    .await;
    assert_eq!(
        harness.coordinator.proxy.admission().snapshot().state,
        AdmissionState::Blocked
    );
    assert!(
        harness
            .coordinator
            .coordinator_child
            .as_ref()
            .unwrap()
            .child()
            .is_running()
    );
    assert!(
        harness
            .worker
            .worker_child
            .as_ref()
            .unwrap()
            .child()
            .is_running()
    );

    harness
        .coordinator
        .production
        .demote_for_recovery(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        !harness
            .coordinator
            .coordinator_child
            .as_ref()
            .unwrap()
            .child()
            .is_running()
    );
    assert!(
        !harness
            .worker
            .worker_child
            .as_ref()
            .unwrap()
            .child()
            .is_running()
    );

    harness.coordinator.production.release_recovery_owner();
    harness.shutdown().await;
}
