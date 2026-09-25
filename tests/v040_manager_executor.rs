#![cfg(feature = "test-support")]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use siderostat::manager::executor::{
    FixtureManagerBackend, ManagerExecutionBackend, ManagerExecutionError, ManagerExecutionRequest,
    ManagerExecutor, ManagerJobInput, ManagerJobInputResolver, RuntimeManagerBackend,
};
use siderostat::manager::jobs::{JobJournal, JobKind, JobPhase};
use siderostat::manager::registry::ArtifactRecord;
use siderostat::manager::{
    ArtifactRegistry, ArtifactState, BuildRequest, DownloadSpec, HttpError, HttpResponse,
    HttpTransport, ManagerRoot, ModelCatalogEntry, SourceRecord, StageRequest,
};
use siderostat::manager::{JobSubmitRequest, RollbackRequest};

fn request(kind: JobKind, key: &str) -> ManagerExecutionRequest {
    ManagerExecutionRequest {
        id: format!("{key}-0"),
        kind,
        payload_key: key.into(),
        expected_generation: 3,
    }
}

fn model_entry(id: &str, bytes: &[u8]) -> ModelCatalogEntry {
    ModelCatalogEntry {
        catalog_id: id.into(),
        url: "https://example.invalid/model".into(),
        redirect_allowlist: vec![],
        size: bytes.len() as u64,
        sha256: siderostat::manager::hex_sha256(bytes),
        license: "fixture".into(),
        family: "ds4".into(),
        quantization: "Q4".into(),
        encoder: None,
        support: None,
        prefix_file: None,
        reference: None,
        main_integrated: true,
        ram_reference: None,
        compatibility: vec![],
        status: Default::default(),
    }
}

fn source_record(commit: &str) -> SourceRecord {
    SourceRecord {
        remote: "fixture://source".into(),
        full_commit: commit.into(),
        main_proof: "refs/heads/main".into(),
        fetched_at: 1,
    }
}

fn scratch(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("siderostat-task3-{tag}-{}", std::process::id()))
}

struct CountingTransport(Arc<AtomicUsize>);

impl HttpTransport for CountingTransport {
    fn get_range(
        &self,
        _spec: &DownloadSpec,
        _start: u64,
        _etag: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(HttpError::Transport("unexpected fixture HTTP call".into()))
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
fn activation_without_generation_is_rejected() {
    let backend = FixtureManagerBackend::new();
    let mut req = request(JobKind::Activate, "fixture-activate");
    req.expected_generation = 0;
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
    ] {
        let result = backend.execute(request(kind, key), Arc::new(AtomicBool::new(false)));
        assert!(result.is_ok(), "{kind}: {result:?}");
    }
    let profile_id = format!("profile-{}", "b".repeat(64));
    assert!(matches!(
        backend.execute(
            request(JobKind::Activate, &profile_id),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
    assert!(matches!(
        backend.execute(
            request(JobKind::Rollback, "previous"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
    assert_eq!(backend.domain_call_count(), 5);
}

#[tokio::test]
async fn preparation_fixture_jobs_succeed_but_activation_needs_runtime() {
    let journal = Arc::new(Mutex::new(JobJournal::new()));
    let (executor, worker) = ManagerExecutor::start(journal.clone(), FixtureManagerBackend::new());
    let mut ids = Vec::new();
    for kind in [
        JobKind::Fetch,
        JobKind::Build,
        JobKind::Download,
        JobKind::Verify,
        JobKind::Stage,
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
    let mut rejected_ids = Vec::new();
    for kind in [JobKind::Activate, JobKind::Rollback] {
        let key = if kind == JobKind::Activate {
            format!("profile-{}", "c".repeat(64))
        } else {
            "previous".into()
        };
        let id = journal.lock().unwrap().enqueue(kind, &key).unwrap();
        executor
            .submit(ManagerExecutionRequest {
                id: id.clone(),
                kind,
                payload_key: key,
                expected_generation: 3,
            })
            .unwrap();
        rejected_ids.push(id);
    }
    executor.shutdown_for_test();
    worker.await.unwrap();
    let journal = journal.lock().unwrap();
    assert!(
        ids.iter()
            .all(|id| journal.get(id).unwrap().phase == JobPhase::Succeeded)
    );
    for id in &rejected_ids {
        assert_eq!(journal.get(id).unwrap().phase, JobPhase::Failed);
        assert_eq!(
            journal.get(id).unwrap().error,
            "manager backend unavailable"
        );
    }
    assert_eq!(journal.all().len(), 7);
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
        .register(
            JobKind::Build,
            "build-key",
            ManagerJobInput::Build {
                request: Box::new(build),
                source: source_record("a".repeat(40).as_str()),
            },
        )
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
    assert!(
        resolver
            .register_catalog_entry(ModelCatalogEntry {
                sha256: String::new(),
                ..model_entry("model", b"1234567890")
            })
            .is_err()
    );
    resolver
        .register(
            JobKind::Download,
            "model",
            ManagerJobInput::Download {
                spec: DownloadSpec::new("https://example.invalid/model", 10, ""),
                catalog_id: "model".into(),
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
fn registered_download_url_without_catalog_never_reaches_transport() {
    let mut resolver = ManagerJobInputResolver::new();
    let model = model_entry("model", b"model bytes");
    resolver
        .register(
            JobKind::Download,
            "model",
            ManagerJobInput::Download {
                spec: DownloadSpec::new(model.url.clone(), model.size, model.sha256.clone()),
                catalog_id: "model".into(),
                part_path: std::env::temp_dir().join("model.part"),
            },
        )
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let backend = RuntimeManagerBackend::new(resolver)
        .with_transport(Arc::new(CountingTransport(calls.clone())));
    assert!(matches!(
        backend.execute(
            request(JobKind::Download, "model"),
            Arc::new(AtomicBool::new(false))
        ),
        Err(ManagerExecutionError::Unavailable)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn catalog_identity_mismatch_rejects_download() {
    let mut resolver = ManagerJobInputResolver::new();
    let model = model_entry("model", b"model bytes");
    resolver.register_catalog_entry(model.clone()).unwrap();
    resolver
        .register(
            JobKind::Download,
            "model",
            ManagerJobInput::Download {
                spec: DownloadSpec::new(
                    "https://other.invalid/model",
                    model.size,
                    model.sha256.clone(),
                ),
                catalog_id: "model".into(),
                part_path: std::env::temp_dir().join("model.part"),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Download, "model")),
        Err(siderostat::manager::executor::ManagerInputError::Rejected)
    ));
}

#[test]
fn empty_or_mismatched_checkout_is_unavailable() {
    let root = scratch("checkout");
    std::fs::create_dir_all(&root).unwrap();
    let commit = "a".repeat(40);
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Build,
            "empty",
            ManagerJobInput::Build {
                request: Box::new(BuildRequest::new("ds4", "build", &commit, &root, "out.bin")),
                source: source_record(&commit),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Build, "empty")),
        Err(siderostat::manager::executor::ManagerInputError::Unavailable)
    ));
    resolver
        .register(
            JobKind::Build,
            "mismatch",
            ManagerJobInput::Build {
                request: Box::new(BuildRequest::new(
                    "ds4",
                    "build",
                    "b".repeat(40),
                    &root,
                    "out.bin",
                )),
                source: source_record(&commit),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Build, "mismatch")),
        Err(siderostat::manager::executor::ManagerInputError::Unavailable)
    ));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn staged_model_binds_catalog_to_distinct_verified_registry_id() {
    let root = scratch("stage-model");
    std::fs::create_dir_all(&root).unwrap();
    let role_path = root.join("role.bin");
    std::fs::write(&role_path, b"role").unwrap();
    let mut registry = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
    registry.put(ArtifactRecord {
        id: "role".into(),
        kind: "build".into(),
        rel_path: "role.bin".into(),
        sha256: siderostat::manager::hex_sha256(b"role"),
        state: ArtifactState::Verified,
    });
    let model = model_entry("catalog-entry", b"real model");
    let mut resolver = ManagerJobInputResolver::new();
    resolver.register_catalog_entry(model.clone()).unwrap();
    let registry = Arc::new(Mutex::new(registry));
    let mut fake_model = model.clone();
    fake_model.sha256 = siderostat::manager::hex_sha256(b"fake model");
    resolver
        .register(
            JobKind::Stage,
            "fake-profile",
            ManagerJobInput::Stage {
                request: Box::new(StageRequest {
                    profile_id: "fake-profile".into(),
                    role_artifacts: vec![role_path.clone()],
                    model: fake_model,
                    expected_family: "ds4".into(),
                    context_size: 4096,
                    expected_prefix_digest: None,
                    ram_confirmed: true,
                }),
                registry: registry.clone(),
                artifact_ids: vec!["role".into()],
                model_artifact_id: "registry-model-1".into(),
                model_artifact_path: root.join("model.bin"),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Stage, "fake-profile")),
        Err(siderostat::manager::executor::ManagerInputError::Rejected)
    ));
    resolver
        .register(
            JobKind::Stage,
            "profile",
            ManagerJobInput::Stage {
                request: Box::new(StageRequest {
                    profile_id: "profile".into(),
                    role_artifacts: vec![role_path],
                    model,
                    expected_family: "ds4".into(),
                    context_size: 4096,
                    expected_prefix_digest: None,
                    ram_confirmed: true,
                }),
                registry: registry.clone(),
                artifact_ids: vec!["role".into()],
                model_artifact_id: "registry-model-1".into(),
                model_artifact_path: root.join("model.bin"),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Stage, "profile")),
        Err(siderostat::manager::executor::ManagerInputError::Unavailable)
    ));
    std::fs::write(root.join("model.bin"), b"tampered model").unwrap();
    registry.lock().unwrap().put(ArtifactRecord {
        id: "registry-model-1".into(),
        kind: "model".into(),
        rel_path: "model.bin".into(),
        sha256: model_entry("catalog-entry", b"real model").sha256,
        state: ArtifactState::Verified,
    });
    assert!(matches!(
        resolver.resolve(&request(JobKind::Stage, "profile")),
        Err(siderostat::manager::executor::ManagerInputError::Unavailable)
    ));
    std::fs::write(root.join("model.bin"), b"real model").unwrap();
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(
        backend
            .execute(
                request(JobKind::Stage, "profile"),
                Arc::new(AtomicBool::new(false))
            )
            .is_ok()
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn registered_verify_record_with_missing_file_is_unavailable() {
    let root = scratch("verify-file");
    let mut registry = ArtifactRegistry::new(ManagerRoot::explicit(root));
    registry.put(ArtifactRecord {
        id: "artifact".into(),
        kind: "model".into(),
        rel_path: "missing.bin".into(),
        sha256: siderostat::manager::hex_sha256(b"model"),
        state: ArtifactState::Verified,
    });
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Verify,
            "artifact",
            ManagerJobInput::Verify {
                registry: Arc::new(Mutex::new(registry)),
                artifact_id: "artifact".into(),
                expected_sha256: siderostat::manager::hex_sha256(b"model"),
            },
        )
        .unwrap();
    assert!(matches!(
        resolver.resolve(&request(JobKind::Verify, "artifact")),
        Err(siderostat::manager::executor::ManagerInputError::Unavailable)
    ));
}

#[test]
fn runtime_resolver_preserves_activation_context_and_requires_runtime() {
    let resolver = ManagerJobInputResolver::new();
    let profile_id = format!("profile-{}", "a".repeat(64));
    let mut activation = request(JobKind::Activate, &profile_id);
    activation.id = "operation".into();
    activation.expected_generation = 42;
    let input = resolver.resolve(&activation).unwrap();
    let ManagerJobInput::Activate(input) = input else {
        panic!("wrong plan")
    };
    assert_eq!(input.operation_id, "operation");
    assert_eq!(input.profile_id, profile_id);
    assert_eq!(input.expected_generation, 42);
    let backend = RuntimeManagerBackend::new(resolver);
    assert!(matches!(
        backend.execute(activation, Arc::new(AtomicBool::new(false))),
        Err(ManagerExecutionError::Unavailable)
    ));
}

#[test]
fn submit_context_preserves_generation_without_caller_lease() {
    let submitted = JobSubmitRequest {
        kind: "rollback".into(),
        payload_key: "previous".into(),
        expected_generation: 42,
    };
    let execution = submitted.execution_request("job-42".into()).unwrap();
    assert_eq!(execution.kind, JobKind::Rollback);
    assert_eq!(execution.payload_key, "previous");
    assert_eq!(execution.expected_generation, 42);
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
