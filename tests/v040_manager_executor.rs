#![cfg(feature = "test-support")]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use siderostat::manager::executor::{
    FixtureManagerBackend, ManagerExecutionBackend, ManagerExecutionError, ManagerExecutionRequest,
    ManagerExecutor, ManagerJobInput, ManagerJobInputResolver, RuntimeManagerBackend,
};
use siderostat::manager::jobs::{JobJournal, JobKind, JobPhase};
use siderostat::manager::{ArtifactRegistry, BuildRequest, DownloadSpec, ManagerRoot};
use siderostat::manager::{JobSubmitRequest, RollbackRequest};

fn request(kind: JobKind, key: &str) -> ManagerExecutionRequest {
    ManagerExecutionRequest {
        id: format!("{key}-0"),
        kind,
        payload_key: key.into(),
        expected_generation: 3,
        runtime_lease: Some("lease-3".into()),
    }
}

#[test]
fn unknown_payload_key_is_rejected_without_domain_call() {
    let backend = FixtureManagerBackend::new();
    let result = backend.execute(
        request(JobKind::Fetch, "unknown"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(
        result,
        Err(ManagerExecutionError::InputRejected(_))
    ));
    assert_eq!(backend.domain_call_count(), 0);
}

#[test]
fn activation_without_generation_or_lease_is_rejected() {
    let backend = FixtureManagerBackend::new();
    let mut req = request(JobKind::Activate, "fixture-activate");
    req.expected_generation = 0;
    req.runtime_lease = None;
    let result = backend.execute(req, Arc::new(AtomicBool::new(false)));
    assert!(matches!(
        result,
        Err(ManagerExecutionError::InputRejected(_))
    ));
    assert_eq!(backend.domain_call_count(), 0);
}

#[test]
fn unavailable_real_model_is_terminal_failure_not_pending_forever() {
    let backend = RuntimeManagerBackend::without_model_catalog();
    let result = backend.execute(
        request(JobKind::Download, "mxfp4-0731"),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(result, Err(ManagerExecutionError::Unavailable)));
}

#[test]
fn fixture_backend_reaches_each_domain_adapter() {
    let backend = FixtureManagerBackend::new();
    for (kind, key) in [
        (JobKind::Fetch, "fixture-fetch"),
        (JobKind::Build, "fixture-build"),
        (JobKind::Download, "fixture-download"),
        (JobKind::Verify, "fixture-verify"),
        (JobKind::Stage, "fixture-stage"),
        (JobKind::Activate, "fixture-activate"),
        (JobKind::Rollback, "fixture-rollback"),
    ] {
        let result = backend.execute(request(kind, key), Arc::new(AtomicBool::new(false)));
        assert!(result.is_ok(), "{kind}: {result:?}");
    }
    assert_eq!(backend.domain_call_count(), 7);
}

#[tokio::test]
async fn seven_fixture_jobs_reach_terminal_success() {
    let journal = Arc::new(Mutex::new(JobJournal::new()));
    let (executor, worker) = ManagerExecutor::start(journal.clone(), FixtureManagerBackend::new());
    let mut ids = Vec::new();
    for kind in [
        JobKind::Fetch,
        JobKind::Build,
        JobKind::Download,
        JobKind::Verify,
        JobKind::Stage,
        JobKind::Activate,
        JobKind::Rollback,
    ] {
        let key = format!("fixture-{kind}");
        let id = journal.lock().unwrap().enqueue(kind, &key).unwrap();
        executor
            .submit(ManagerExecutionRequest {
                id: id.clone(),
                ..request(kind, &key)
            })
            .unwrap();
        ids.push(id);
    }
    executor.shutdown_for_test();
    worker.await.unwrap();
    let journal = journal.lock().unwrap();
    assert!(
        ids.iter()
            .all(|id| journal.get(id).unwrap().phase == JobPhase::Succeeded)
    );
}

#[tokio::test]
async fn rejected_key_reaches_redacted_terminal_failure() {
    let journal = Arc::new(Mutex::new(JobJournal::new()));
    let (executor, worker) = ManagerExecutor::start(journal.clone(), FixtureManagerBackend::new());
    let id = journal
        .lock()
        .unwrap()
        .enqueue(JobKind::Fetch, "unknown")
        .unwrap();
    executor
        .submit(ManagerExecutionRequest {
            id: id.clone(),
            ..request(JobKind::Fetch, "unknown")
        })
        .unwrap();
    executor.shutdown_for_test();
    worker.await.unwrap();
    let job = journal.lock().unwrap().get(&id).unwrap().clone();
    assert_eq!(job.phase, JobPhase::Failed);
    assert_eq!(job.error, "manager job input rejected");
}

#[test]
fn runtime_resolver_requires_checkout_and_catalog_checksum() {
    let mut resolver = ManagerJobInputResolver::new();
    let build = BuildRequest::new("ds4", "build", "commit", "/missing/checkout", "out.bin");
    resolver
        .register(JobKind::Build, "build-key", ManagerJobInput::Build(build))
        .unwrap();
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(
            request(JobKind::Build, "build-key"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));

    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Download,
            "model",
            ManagerJobInput::Download {
                spec: DownloadSpec::new("https://example.invalid/model", 10, ""),
                part_path: std::env::temp_dir().join("model.part"),
            },
        )
        .unwrap();
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(
            request(JobKind::Download, "model"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
}

#[test]
fn runtime_resolver_preserves_activation_context_and_requires_runtime() {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Activate,
            "profile",
            ManagerJobInput::Activate(siderostat::manager::ActivationRequest {
                operation_id: "operation".into(),
                expected_generation: 1,
                runtime_lease: "configured".into(),
                policy_epoch: 7,
                nodes: vec!["local".into(), "peer".into()],
            }),
        )
        .unwrap();
    let input = resolver
        .resolve(&request(JobKind::Activate, "profile"))
        .unwrap();
    let ManagerJobInput::Activate(input) = input else {
        panic!("wrong plan")
    };
    assert_eq!(input.expected_generation, 3);
    assert_eq!(input.runtime_lease, "lease-3");
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(
            request(JobKind::Activate, "profile"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
}

#[test]
fn submit_context_is_preserved_and_blank_lease_is_rejected() {
    let submitted = JobSubmitRequest {
        kind: "rollback".into(),
        payload_key: "previous".into(),
        expected_generation: 42,
        runtime_lease: Some("lease-42".into()),
    };
    let execution = submitted.execution_request("job-42".into()).unwrap();
    assert_eq!(execution.kind, JobKind::Rollback);
    assert_eq!(execution.payload_key, "previous");
    assert_eq!(execution.expected_generation, 42);
    assert_eq!(execution.runtime_lease.as_deref(), Some("lease-42"));

    let backend = FixtureManagerBackend::new();
    let mut invalid = request(JobKind::Rollback, "fixture-rollback");
    invalid.runtime_lease = Some("  ".into());
    assert!(matches!(
        backend.execute(invalid, Arc::new(AtomicBool::new(false))),
        Err(ManagerExecutionError::InputRejected(_))
    ));
    assert_eq!(backend.domain_call_count(), 0);
}

#[test]
fn missing_previous_digest_is_unavailable_before_runtime_call() {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Rollback,
            "previous",
            ManagerJobInput::Rollback(RollbackRequest {
                operation_id: "rollback".into(),
                expected_generation: 1,
                runtime_lease: "configured".into(),
                policy_epoch: 1,
                previous_digest: String::new(),
                nodes: vec!["local".into(), "peer".into()],
            }),
        )
        .unwrap();
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(
            request(JobKind::Rollback, "previous"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
}

#[test]
fn missing_verified_artifact_is_unavailable() {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Verify,
            "artifact",
            ManagerJobInput::Verify {
                registry: Arc::new(Mutex::new(ArtifactRegistry::new(ManagerRoot::explicit(
                    std::env::temp_dir().join("siderostat-missing-artifact"),
                )))),
                artifact_id: "missing".into(),
                expected_sha256: siderostat::manager::hex_sha256(b"fixture"),
            },
        )
        .unwrap();
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(
            request(JobKind::Verify, "artifact"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
}
