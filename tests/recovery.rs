use siderostat::{canary::CanaryReason, recovery::*, target::*};

fn distributed_gate() -> RecoveryGate {
    RecoveryGate {
        role: LocalRole::Coordinator,
        mode: StableMode::DistributedLayerParallel,
        state: ClusterState::DistributedReady,
        generation: 42,
    }
}

fn request(key: &str) -> RecoveryRequest {
    RecoveryRequest::new(
        RecoveryReason::ThroughputDegraded,
        RecoveryTrigger::ManualCanaryFailure,
        key,
    )
    .unwrap()
}

#[test]
fn recovery_service_has_one_active_owner_and_idempotent_history() {
    let service = RecoveryService::new();
    let first = service.begin_at(distributed_gate(), request("first"), 1_000);
    let RecoveryStart::Created(first_job) = first else {
        panic!("first request must create a job");
    };

    let duplicate = service.begin_at(distributed_gate(), request("second"), 1_001);
    let RecoveryStart::Existing(duplicate_job) = duplicate else {
        panic!("duplicate must return the active job");
    };
    assert_eq!(duplicate_job.recovery_id, first_job.recovery_id);

    service
        .mark_snapshot_failed(
            &first_job.recovery_id,
            RecoveryFailureReason::SnapshotWrite,
            1_002,
        )
        .unwrap();
    let replay = service.begin_at(distributed_gate(), request("first"), 1_003);
    let RecoveryStart::Existing(replayed_job) = replay else {
        panic!("completed idempotency key must not create a new job");
    };
    assert_eq!(replayed_job.recovery_id, first_job.recovery_id);
    assert_eq!(
        service.status(&first_job.recovery_id).unwrap().state,
        RecoveryState::Failed
    );
    let serialized = serde_json::to_value(service.status(&first_job.recovery_id).unwrap()).unwrap();
    assert_eq!(serialized["reason"], "throughput-degraded");
    assert_eq!(serialized["started_at"], "1970-01-01T00:00:01.000Z");
    assert_eq!(serialized["finished_at"], "1970-01-01T00:00:01.002Z");
    assert_eq!(serialized["old_cluster_generation"], 42);
    assert!(serialized.get("started_at_millis").is_none());
}

#[test]
fn recovery_gate_rejects_worker_without_creating_an_active_job() {
    let service = RecoveryService::new();
    let mut gate = distributed_gate();
    gate.role = LocalRole::Worker;

    let result = service.begin_at(gate, request("worker"), 1_000);
    let RecoveryStart::Suppressed(job) = result else {
        panic!("worker must be suppressed");
    };
    assert_eq!(job.state, RecoveryState::Suppressed);
    assert_eq!(job.phase, RecoveryPhase::Suppressed);
    assert!(!service.has_active_job());
}

#[test]
fn snapshot_failure_is_terminal_without_lifecycle_mutation() {
    let service = RecoveryService::new();
    let RecoveryStart::Created(job) =
        service.begin_at(distributed_gate(), request("failure"), 1_000)
    else {
        panic!("request must create a job");
    };
    service
        .mark_snapshot_failed(
            &job.recovery_id,
            RecoveryFailureReason::SnapshotWrite,
            1_001,
        )
        .unwrap();

    let status = service.status(&job.recovery_id).unwrap();
    assert_eq!(status.state, RecoveryState::Failed);
    assert_eq!(status.phase, RecoveryPhase::Failed);
    assert!(!service.has_active_job());
}

#[test]
fn history_is_bounded_without_discarding_the_newest_job() {
    let service = RecoveryService::with_policy(RecoveryPolicy {
        history_limit: 3,
        cooldown: 0,
        window: 1_000_000,
        max_attempts: 100,
    });
    let mut ids = Vec::new();
    for index in 0..5 {
        let key = format!("history-{index}");
        let RecoveryStart::Created(job) =
            service.begin_at(distributed_gate(), request(&key), 1_000 + index)
        else {
            panic!("history request must create a job");
        };
        ids.push(job.recovery_id.clone());
        service
            .mark_snapshot_failed(
                &job.recovery_id,
                RecoveryFailureReason::SnapshotWrite,
                1_000 + index + 1,
            )
            .unwrap();
    }

    assert_eq!(service.history_len(), 3);
    assert!(service.status(&ids[0]).is_none());
    assert!(service.status(ids.last().unwrap()).is_some());
}

#[test]
fn stale_recovery_id_is_not_reused() {
    assert!(
        RecoveryService::new()
            .status("00000000-0000-4000-8000-000000000000")
            .is_none()
    );
}

#[test]
fn recovery_phase_and_canary_result_are_recorded_before_completion() {
    let service = RecoveryService::new();
    let RecoveryStart::Created(job) = service.begin_at(distributed_gate(), request("phase"), 1_000)
    else {
        panic!("request must create a job");
    };
    service
        .mark_phase(
            &job.recovery_id,
            RecoveryPhase::PostRecoveryCanary,
            Some("blocked".into()),
        )
        .unwrap();
    service
        .mark_post_recovery_canary(&job.recovery_id, CanaryReason::Healthy)
        .unwrap();
    let status = service.status(&job.recovery_id).unwrap();
    assert_eq!(status.state, RecoveryState::Running);
    assert_eq!(status.phase, RecoveryPhase::PostRecoveryCanary);
    assert_eq!(status.post_recovery_canary, Some(CanaryReason::Healthy));
    assert_eq!(status.admission.as_deref(), Some("blocked"));
}

#[test]
fn automatic_detector_starts_one_bounded_recovery_for_a_sustained_low_sample() {
    let mut detector = RecoveryDetector::default();
    let service = RecoveryService::new();
    let observation = RecoveryObservation::Active {
        first_progress_observed: true,
        active_age_millis: 10_000,
        progress_age_millis: Some(100),
        chunk_tps: Some(4.0),
    };

    assert_eq!(detector.observe_at(0, observation), None);
    let trigger = detector
        .observe_at(30_000, observation)
        .expect("sustained low TPS must produce one trigger");
    let request = RecoveryRequest::new(
        RecoveryReason::ThroughputDegraded,
        trigger,
        "automatic-test-event",
    )
    .unwrap();
    let RecoveryStart::Created(job) = service.begin_at(distributed_gate(), request, 1_000) else {
        panic!("the first automatic event must claim the recovery owner");
    };

    assert_eq!(detector.observe_at(31_000, observation), None);
    assert!(service.has_active_job());
    assert_eq!(job.trigger, RecoveryTrigger::LowDecodeTps);
}
