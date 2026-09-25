#![cfg(feature = "test-support")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use siderostat::manager::catalog::{CapabilityStatus, ModelCatalogEntry};
use siderostat::manager::download::{HttpError, HttpResponse, HttpTransport};
use siderostat::manager::executor::{
    ManagerExecutionBackend, ManagerExecutionRequest, ManagerJobInput, ManagerJobInputResolver,
    RuntimeManagerBackend,
};
use siderostat::manager::jobs::{JobJournal, JobKind, JobPhase};
use siderostat::manager::registry::{BuildRecord, ManagerRoot, SourceRecord};
use siderostat::manager::stage::StageRuntimeConfig;
use siderostat::manager::store::{
    ArtifactDraft, ArtifactKind, ArtifactProvenance, HardwareReadiness, ManagerReleaseStore,
    ProfileCompatibility, ReleaseIdentity,
};
use siderostat::manager::{GitRunner, ManagerExecutor, OfficialRemote};

const OFFICIAL_REMOTE: &str = "https://github.com/antirez/ds4.git";

struct ModelHttp {
    responses: Mutex<std::collections::VecDeque<Result<HttpResponse, HttpError>>>,
    calls: AtomicUsize,
}

impl ModelHttp {
    fn new(responses: Vec<Result<HttpResponse, HttpError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl HttpTransport for ModelHttp {
    fn get_range(
        &self,
        _spec: &siderostat::manager::DownloadSpec,
        _start: u64,
        _etag: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.responses
            .lock()
            .expect("model HTTP lock")
            .pop_front()
            .unwrap_or_else(|| Err(HttpError::InvalidResponse("no fixture response".into())))
    }
}

fn model_entry(bytes: &[u8], sha256: Option<&str>, size: Option<u64>) -> ModelCatalogEntry {
    ModelCatalogEntry {
        catalog_id: "fixture-model-v1".into(),
        url: "https://models.example.com/fixture-model-v1.gguf".into(),
        redirect_allowlist: vec!["https://cdn.example.com/".into()],
        size: size.unwrap_or(bytes.len() as u64),
        sha256: sha256
            .map(str::to_owned)
            .unwrap_or_else(|| siderostat::manager::hex_sha256(bytes)),
        license: "fixture-license".into(),
        family: "ds4".into(),
        quantization: "q4".into(),
        encoder: None,
        support: None,
        prefix_file: None,
        reference: Some("https://github.com/example/ds4/releases".into()),
        main_integrated: true,
        ram_reference: None,
        compatibility: vec![],
        status: CapabilityStatus::Candidate,
    }
}

fn model_backend(
    store: Arc<Mutex<ManagerReleaseStore>>,
    entry: ModelCatalogEntry,
    transport: Arc<dyn HttpTransport + Send + Sync>,
) -> RuntimeManagerBackend {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register_catalog_entry(entry)
        .expect("register fixture catalog entry");
    RuntimeManagerBackend::new(resolver)
        .with_manager_store(store)
        .with_transport(transport)
}

fn publish_stage_artifacts(
    manager_root: &Path,
    store: &Arc<Mutex<ManagerReleaseStore>>,
    build_role: &str,
    model_bytes: &[u8],
    entry: &ModelCatalogEntry,
) -> (String, String) {
    let source_commit = "a".repeat(40);
    let source_receipt_id = store
        .lock()
        .expect("store lock")
        .record_source(SourceRecord {
            remote: OFFICIAL_REMOTE.into(),
            full_commit: source_commit.clone(),
            main_proof: source_commit.clone(),
            fetched_at: 1,
        })
        .expect("record source receipt");

    let role_bytes = format!("fixture executable for {build_role}").into_bytes();
    let role_sha256 = siderostat::manager::hex_sha256(&role_bytes);
    let role_path = manager_root.join("stage-build-input.bin");
    std::fs::write(&role_path, &role_bytes).expect("write build input");
    let build_record = BuildRecord {
        source: source_commit,
        flags: "default".into(),
        toolchain: "rustc fixture".into(),
        arch: std::env::consts::ARCH.into(),
        role: build_role.into(),
        target: build_role.into(),
        digest: role_sha256.clone(),
        help_digest: "b".repeat(64),
    };
    let role_id = store
        .lock()
        .expect("store lock")
        .publish_artifact(
            &role_path,
            ArtifactDraft {
                kind: ArtifactKind::Build,
                expected_sha256: role_sha256,
                expected_size: role_bytes.len() as u64,
                provenance: ArtifactProvenance::Build {
                    source_receipt_id,
                    record: build_record,
                },
            },
        )
        .expect("publish build artifact")
        .id;

    let model_sha256 = siderostat::manager::hex_sha256(model_bytes);
    let model_path = manager_root.join("stage-model-input.bin");
    std::fs::write(&model_path, model_bytes).expect("write model input");
    let model_id = store
        .lock()
        .expect("store lock")
        .publish_artifact(
            &model_path,
            ArtifactDraft {
                kind: ArtifactKind::Model,
                expected_sha256: model_sha256,
                expected_size: model_bytes.len() as u64,
                provenance: ArtifactProvenance::Model {
                    catalog_id: entry.catalog_id.clone(),
                },
            },
        )
        .expect("publish model artifact")
        .id;
    (role_id, model_id)
}

fn stage_backend(
    store: Arc<Mutex<ManagerReleaseStore>>,
    entry: ModelCatalogEntry,
    config: StageRuntimeConfig,
) -> RuntimeManagerBackend {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register_catalog_entry(entry)
        .expect("register fixture catalog entry");
    RuntimeManagerBackend::new(resolver)
        .with_manager_store(store)
        .with_stage_runtime_config(config)
}

#[test]
fn stage_payload_key_accepts_only_generated_local_artifact_ids() {
    let build_id = format!("build-{}", "a".repeat(64));
    let model_id = format!("model-{}", "b".repeat(64));
    let expected = format!("{build_id}:{model_id}");
    assert_eq!(
        siderostat::manager::manager_stage_payload_key(&build_id, &model_id).as_deref(),
        Some(expected.as_str())
    );
    assert!(siderostat::manager::manager_stage_payload_key("/tmp/build.bin", &model_id).is_none());
    assert!(
        siderostat::manager::manager_stage_payload_key(&build_id, "https://models.example/x")
            .is_none()
    );
}

struct CancelModelHttp {
    response: HttpResponse,
}

impl HttpTransport for CancelModelHttp {
    fn get_range(
        &self,
        _spec: &siderostat::manager::DownloadSpec,
        _start: u64,
        _etag: Option<&str>,
    ) -> Result<HttpResponse, HttpError> {
        Ok(self.response.clone())
    }

    fn get_range_to_file(
        &self,
        _spec: &siderostat::manager::DownloadSpec,
        _start: u64,
        _etag: Option<&str>,
        destination: &Path,
        _max_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<siderostat::manager::HttpResponseMetadata, HttpError> {
        std::fs::write(destination, &self.response.body).expect("write response fixture");
        cancel.store(true, Ordering::SeqCst);
        Ok(siderostat::manager::HttpResponseMetadata {
            status: self.response.status,
            etag: self.response.etag.clone(),
            content_range: self.response.content_range.clone(),
            final_url: self.response.final_url.clone(),
            body_size: self.response.body.len() as u64,
        })
    }
}

fn model_response(entry: &ModelCatalogEntry, body: &[u8], final_url: Option<&str>) -> HttpResponse {
    HttpResponse {
        status: 200,
        etag: Some("fixture-etag".into()),
        content_range: None,
        final_url: final_url.unwrap_or(&entry.url).into(),
        body: body.into(),
    }
}

struct Fixture {
    base: PathBuf,
    remote: PathBuf,
    main_commit: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn fixture() -> Fixture {
    let base = std::env::temp_dir().join(format!(
        "siderostat-v040-manager-pipeline-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&base).expect("create fixture root");
    let work = base.join("work");
    let remote = base.join("remote.git");
    std::fs::create_dir(&work).expect("create worktree");
    let git = GitRunner::default();
    let work_text = work.to_str().expect("UTF-8 fixture path");
    git.run(["-C", work_text, "init", "-b", "main"])
        .expect("init");
    git.run(["-C", work_text, "config", "user.email", "test@example.com"])
        .expect("set email");
    git.run(["-C", work_text, "config", "user.name", "fixture"])
        .expect("set name");
    std::fs::write(work.join("main.txt"), "main").expect("write main");
    std::fs::write(
        work.join("Makefile"),
        ".PHONY: ds4-server ds4-agent\nds4-server:\n\t@pwd > ds4-server\n\t@printf 'fixture help\\n' > help.txt\nds4-agent:\n\t@printf 'fixture help\\n' > help.txt\n",
    )
    .expect("write fixture Makefile");
    git.run(["-C", work_text, "add", "."]).expect("add main");
    git.run(["-C", work_text, "commit", "-m", "main"])
        .expect("commit main");
    git.run(["-C", work_text, "checkout", "-b", "feature"])
        .expect("create feature");
    std::fs::write(work.join("feature.txt"), "feature").expect("write feature");
    git.run(["-C", work_text, "add", "."]).expect("add feature");
    git.run(["-C", work_text, "commit", "-m", "feature"])
        .expect("commit feature");
    git.run(["-C", work_text, "checkout", "main"])
        .expect("return to main");
    let main_commit = git
        .run(["-C", work_text, "rev-parse", "HEAD^{commit}"])
        .expect("resolve main commit");
    let remote_text = remote.to_str().expect("UTF-8 remote path");
    git.run(["-C", work_text, "clone", "--bare", ".", remote_text])
        .expect("create bare remote");
    Fixture {
        base,
        remote,
        main_commit,
    }
}

fn store(root: &Path) -> Arc<Mutex<ManagerReleaseStore>> {
    Arc::new(Mutex::new(
        ManagerReleaseStore::open(ManagerRoot::explicit(root.to_path_buf()), "node-a")
            .expect("open store"),
    ))
}

fn backend(
    root: &Path,
    remote: &str,
    revision: &str,
    store: Arc<Mutex<ManagerReleaseStore>>,
) -> RuntimeManagerBackend {
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Fetch,
            "official",
            ManagerJobInput::Fetch {
                cache: root.join("ds4/sources/official.git"),
                official: OfficialRemote::new(remote),
                remote: remote.into(),
                revision: revision.into(),
                main_ref: "refs/heads/main".into(),
            },
        )
        .expect("register fixture fetch");
    RuntimeManagerBackend::new(resolver).with_manager_store(store)
}

fn fetch_request(id: &str) -> ManagerExecutionRequest {
    ManagerExecutionRequest {
        id: id.into(),
        kind: JobKind::Fetch,
        payload_key: "official".into(),
        expected_generation: 0,
        runtime_lease: None,
    }
}

async fn run_job(
    backend: RuntimeManagerBackend,
    kind: JobKind,
    payload_key: &str,
) -> (JobPhase, Arc<Mutex<JobJournal>>) {
    let journal = Arc::new(Mutex::new(JobJournal::new()));
    let id = journal
        .lock()
        .expect("journal lock")
        .enqueue(kind, payload_key)
        .expect("enqueue manager job");
    let (executor, worker) = ManagerExecutor::start(journal.clone(), backend);
    executor
        .submit(ManagerExecutionRequest {
            id: id.clone(),
            kind,
            payload_key: payload_key.into(),
            expected_generation: 0,
            runtime_lease: None,
        })
        .expect("submit manager job");
    executor.shutdown_for_test();
    worker.await.expect("manager worker");
    let phase = journal
        .lock()
        .expect("journal lock")
        .get(&id)
        .expect("fetch job")
        .phase;
    (phase, journal)
}

async fn run_fetch(backend: RuntimeManagerBackend) -> (JobPhase, Arc<Mutex<JobJournal>>) {
    run_job(backend, JobKind::Fetch, "official").await
}

fn build_key(source_receipt_id: &str, role: &str) -> String {
    format!("{source_receipt_id}:{role}")
}

#[tokio::test]
async fn fetch_main_persists_full_commit_and_main_proof_across_reopen() {
    let fixture = fixture();
    let root = fixture.base.join("manager");
    let store = store(&root);
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();
    let backend = backend(&root, &remote, "main", store.clone());

    let (phase, journal) = run_fetch(backend).await;

    assert_eq!(phase, JobPhase::Succeeded);
    let id = format!("source-{}", fixture.main_commit);
    let job_id = journal.lock().unwrap().all()[0].id.clone();
    assert_ne!(
        id, job_id,
        "domain receipt identity is separate from job identity"
    );
    {
        let store = store.lock().expect("store lock");
        let receipt = store
            .snapshot()
            .source_receipts
            .get(&id)
            .expect("durable source receipt");
        assert_eq!(receipt.full_commit, fixture.main_commit);
        assert_eq!(receipt.main_proof, fixture.main_commit);
        assert_eq!(receipt.remote, remote);
        assert_eq!(store.snapshot().release_pointers.active, None);
    }

    let reopened =
        ManagerReleaseStore::open(ManagerRoot::explicit(root), "node-a").expect("reopen store");
    let receipt = reopened
        .snapshot()
        .source_receipts
        .get(&id)
        .expect("same source receipt after restart");
    assert_eq!(receipt.full_commit, fixture.main_commit);
    assert_eq!(receipt.main_proof, fixture.main_commit);
}

#[tokio::test]
async fn fetch_rejects_non_main_and_remote_mismatch_without_changing_inventory() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();

    let non_main_store = store(&manager_root);
    let (phase, journal) = run_fetch(backend(
        &manager_root,
        &remote,
        "refs/heads/feature",
        non_main_store.clone(),
    ))
    .await;
    assert_eq!(phase, JobPhase::Failed);
    assert_eq!(
        journal.lock().unwrap().all()[0].error,
        "manager backend failed"
    );
    {
        let store = non_main_store.lock().expect("store lock");
        assert!(store.snapshot().source_receipts.is_empty());
        assert_eq!(store.snapshot().release_pointers.active, None);
    }

    let mismatch_store = store(&manager_root.join("mismatch"));
    let mut resolver = ManagerJobInputResolver::new();
    resolver
        .register(
            JobKind::Fetch,
            "official",
            ManagerJobInput::Fetch {
                cache: manager_root.join("mismatch/ds4/sources/official.git"),
                official: OfficialRemote::new(OFFICIAL_REMOTE),
                remote: remote.clone(),
                revision: "main".into(),
                main_ref: "refs/heads/main".into(),
            },
        )
        .expect("register mismatched fetch");
    let mismatch_backend =
        RuntimeManagerBackend::new(resolver).with_manager_store(mismatch_store.clone());
    let (phase, journal) = run_fetch(mismatch_backend).await;
    assert_eq!(phase, JobPhase::Failed);
    let error = journal.lock().unwrap().all()[0].error.clone();
    assert_eq!(error, "manager backend failed");
    assert!(!error.contains(&remote));
    let store = mismatch_store.lock().expect("store lock");
    assert!(store.snapshot().source_receipts.is_empty());
    assert_eq!(store.snapshot().release_pointers.active, None);
}

#[tokio::test]
async fn fetch_persistence_failure_never_marks_job_succeeded() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let operations = manager_root.join("ds4/operations");
    std::fs::remove_dir_all(&operations).expect("remove store operations directory");
    std::fs::write(&operations, "blocked").expect("replace operations directory");
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();

    let (phase, _) = run_fetch(backend(&manager_root, &remote, "main", store.clone())).await;

    assert_eq!(phase, JobPhase::Failed);
    let store = store.lock().expect("store lock");
    assert!(store.snapshot().source_receipts.is_empty());
    assert_eq!(store.snapshot().release_pointers.active, None);
}

#[cfg(unix)]
#[tokio::test]
async fn fetch_refuses_a_cache_symlink_outside_the_managed_source_directory() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let outside = fixture.base.join("outside");
    std::fs::create_dir(&outside).expect("create outside target");
    let cache = manager_root.join("ds4/sources/official.git");
    std::os::unix::fs::symlink(&outside, &cache).expect("symlink cache outside root");
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();

    let (phase, _) = run_fetch(backend(&manager_root, &remote, "main", store.clone())).await;

    assert_eq!(phase, JobPhase::Failed);
    assert!(
        !outside.join("HEAD").exists(),
        "external path was not initialized"
    );
    let store = store.lock().expect("store lock");
    assert!(store.snapshot().source_receipts.is_empty());
    assert_eq!(store.snapshot().release_pointers.active, None);
}

#[cfg(unix)]
#[tokio::test]
async fn fetch_refuses_symlinked_git_object_storage() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let cache = manager_root.join("ds4/sources/official.git");
    GitRunner::default()
        .run(["init", "--bare", cache.to_str().unwrap()])
        .expect("initialize managed bare cache");
    let outside = fixture.base.join("outside-objects");
    std::fs::create_dir(&outside).expect("create outside objects");
    std::fs::remove_dir_all(cache.join("objects")).expect("remove cache objects");
    std::os::unix::fs::symlink(&outside, cache.join("objects")).expect("symlink objects");
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();

    let (phase, _) = run_fetch(backend(&manager_root, &remote, "main", store.clone())).await;

    assert_eq!(phase, JobPhase::Failed);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    let store = store.lock().expect("store lock");
    assert!(store.snapshot().source_receipts.is_empty());
    assert_eq!(store.snapshot().release_pointers.active, None);
}

#[tokio::test]
async fn build_from_receipt_uses_unique_pinned_worktrees_and_reopens_artifacts() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();
    let (fetch_phase, _) = run_fetch(backend(&manager_root, &remote, "main", store.clone())).await;
    assert_eq!(fetch_phase, JobPhase::Succeeded);
    let source_receipt_id = format!("source-{}", fixture.main_commit);

    let (first_phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&source_receipt_id, "ds4-server"),
    )
    .await;
    assert_eq!(first_phase, JobPhase::Succeeded);
    let first_artifact = {
        let store = store.lock().expect("store lock");
        assert_eq!(store.snapshot().artifacts.len(), 1);
        assert_eq!(store.snapshot().release_pointers.active, None);
        let artifact = store
            .snapshot()
            .artifacts
            .values()
            .next()
            .expect("first build artifact");
        let ArtifactProvenance::Build {
            source_receipt_id: provenance_id,
            record,
        } = &artifact.provenance
        else {
            panic!("build provenance");
        };
        assert_eq!(provenance_id, &source_receipt_id);
        assert_eq!(record.source, fixture.main_commit);
        assert_eq!(record.role, "ds4-server");
        assert_eq!(record.target, "ds4-server");
        assert_eq!(record.arch, std::env::consts::ARCH);
        assert_eq!(record.flags, "default");
        assert!(!record.toolchain.is_empty());
        assert!(!record.help_digest.is_empty());
        std::fs::read(manager_root.join(&artifact.rel_path)).expect("published build bytes")
    };

    let (second_phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&source_receipt_id, "ds4-server"),
    )
    .await;
    assert_eq!(second_phase, JobPhase::Succeeded);
    let store_guard = store.lock().expect("store lock");
    assert_eq!(store_guard.snapshot().artifacts.len(), 2);
    let artifact_bytes = store_guard
        .snapshot()
        .artifacts
        .values()
        .map(|artifact| {
            std::fs::read(manager_root.join(&artifact.rel_path)).expect("build artifact bytes")
        })
        .collect::<Vec<_>>();
    assert!(artifact_bytes.iter().any(|bytes| bytes == &first_artifact));
    assert_ne!(
        artifact_bytes[0], artifact_bytes[1],
        "workspaces are unique"
    );
    drop(store_guard);

    let cache = manager_root.join("ds4/sources/official.git");
    let worktrees = GitRunner::default()
        .run([
            "--git-dir",
            cache.to_str().unwrap(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .expect("list worktrees after cleanup");
    assert_eq!(worktrees.matches("worktree ").count(), 1);
    let reopened = ManagerReleaseStore::open(ManagerRoot::explicit(manager_root.clone()), "node-a")
        .expect("reopen release store");
    assert_eq!(reopened.snapshot().artifacts.len(), 2);
    assert_eq!(reopened.snapshot().source_receipts.len(), 1);
}

#[tokio::test]
async fn build_failures_never_publish_artifact_records() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();
    assert_eq!(
        run_fetch(backend(&manager_root, &remote, "main", store.clone()))
            .await
            .0,
        JobPhase::Succeeded
    );
    let source_id = format!("source-{}", fixture.main_commit);

    let (unknown_phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&format!("source-{}", "0".repeat(40)), "ds4-server"),
    )
    .await;
    assert_eq!(unknown_phase, JobPhase::Failed);
    let (role_phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&source_id, "unapproved-role"),
    )
    .await;
    assert_eq!(role_phase, JobPhase::Failed);
    let (missing_output_phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&source_id, "ds4-agent"),
    )
    .await;
    assert_eq!(missing_output_phase, JobPhase::Failed);

    let canceled = backend(&manager_root, &remote, "main", store.clone()).execute(
        ManagerExecutionRequest {
            id: "build-canceled".into(),
            kind: JobKind::Build,
            payload_key: build_key(&source_id, "ds4-server"),
            expected_generation: 0,
            runtime_lease: None,
        },
        Arc::new(AtomicBool::new(true)),
    );
    assert!(matches!(
        canceled,
        Err(siderostat::manager::executor::ManagerExecutionError::Canceled)
    ));
    let store_guard = store.lock().expect("store lock");
    assert!(store_guard.snapshot().artifacts.is_empty());
    assert_eq!(store_guard.snapshot().release_pointers.active, None);
}

#[tokio::test]
async fn build_rejects_receipt_without_a_reachable_main_proof() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();
    assert_eq!(
        run_fetch(backend(&manager_root, &remote, "main", store.clone()))
            .await
            .0,
        JobPhase::Succeeded
    );
    let source_receipt_id = format!("source-{}", fixture.main_commit);
    drop(store);

    let index = manager_root.join("ds4/operations/manager-release-store.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index).expect("read release store"))
            .expect("decode release store");
    snapshot["source_receipts"][&source_receipt_id]["main_proof"] =
        serde_json::Value::String("0".repeat(40));
    std::fs::write(
        &index,
        serde_json::to_vec(&snapshot).expect("encode release store"),
    )
    .expect("replace stale proof");
    let store = Arc::new(Mutex::new(
        ManagerReleaseStore::open(ManagerRoot::explicit(manager_root.clone()), "node-a")
            .expect("reopen modified store"),
    ));

    let (phase, _) = run_job(
        backend(&manager_root, &remote, "main", store.clone()),
        JobKind::Build,
        &build_key(&source_receipt_id, "ds4-server"),
    )
    .await;
    assert_eq!(phase, JobPhase::Failed);
    let store_guard = store.lock().expect("store lock");
    assert!(store_guard.snapshot().artifacts.is_empty());
    assert_eq!(store_guard.snapshot().release_pointers.active, None);
}

#[test]
fn bundled_catalog_excludes_unverified_placeholder_sources() {
    let entries = siderostat::manager::catalog::bundled_catalog().expect("bundled catalog");
    assert!(
        entries.is_empty(),
        "placeholder domains and hashes are not downloadable"
    );
}

#[tokio::test]
async fn model_download_and_verify_are_store_backed_and_quarantine_swaps() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let bytes = b"verified fixture model";
    let entry = model_entry(bytes, None, None);
    let transport = Arc::new(ModelHttp::new(vec![Ok(model_response(
        &entry, bytes, None,
    ))]));

    let (phase, _) = run_job(
        model_backend(store.clone(), entry.clone(), transport.clone()),
        JobKind::Download,
        &entry.catalog_id,
    )
    .await;
    assert_eq!(phase, JobPhase::Succeeded);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    let artifact_id = format!("model-{}", siderostat::manager::hex_sha256(bytes));
    {
        let store = store.lock().expect("store lock");
        let artifact = store
            .snapshot()
            .artifacts
            .get(&artifact_id)
            .expect("durable model artifact");
        assert_eq!(artifact.kind, ArtifactKind::Model);
        assert_eq!(artifact.size, bytes.len() as u64);
        assert_eq!(artifact.sha256, siderostat::manager::hex_sha256(bytes));
        assert_eq!(
            artifact.validation_state,
            siderostat::manager::ArtifactState::Verified
        );
        assert_eq!(
            artifact.provenance,
            ArtifactProvenance::Model {
                catalog_id: entry.catalog_id.clone()
            }
        );
        assert_eq!(store.snapshot().release_pointers.active, None);
        assert_eq!(
            std::fs::read(manager_root.join(&artifact.rel_path)).expect("model bytes"),
            bytes
        );
    }

    let verify_transport = Arc::new(ModelHttp::new(vec![]));
    let (verify_phase, _) = run_job(
        model_backend(store.clone(), entry.clone(), verify_transport),
        JobKind::Verify,
        &artifact_id,
    )
    .await;
    assert_eq!(verify_phase, JobPhase::Succeeded);
    let reopened = ManagerReleaseStore::open(ManagerRoot::explicit(manager_root.clone()), "node-a")
        .expect("reopen verified model");
    assert_eq!(reopened.snapshot().artifacts.len(), 1);

    let artifact = reopened
        .snapshot()
        .artifacts
        .get(&artifact_id)
        .expect("artifact");
    std::fs::write(
        manager_root.join(&artifact.rel_path),
        b"swapped model bytes",
    )
    .expect("tamper model");
    let store = Arc::new(Mutex::new(reopened));
    let (tampered_phase, _) = run_job(
        model_backend(store.clone(), entry, Arc::new(ModelHttp::new(vec![]))),
        JobKind::Verify,
        &artifact_id,
    )
    .await;
    assert_eq!(tampered_phase, JobPhase::Failed);
    assert_eq!(
        store.lock().expect("store lock").snapshot().artifacts[&artifact_id].validation_state,
        siderostat::manager::ArtifactState::Quarantined
    );
}

#[tokio::test]
async fn rejected_model_downloads_do_not_publish_records_or_reach_unknown_ids() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let body = b"model bytes";
    let wrong_hash = model_entry(body, None, None);
    let transport = Arc::new(ModelHttp::new(vec![Ok(model_response(
        &wrong_hash,
        body,
        Some("https://untrusted.example.net/model.bin"),
    ))]));
    let (redirect_phase, _) = run_job(
        model_backend(store.clone(), wrong_hash.clone(), transport.clone()),
        JobKind::Download,
        &wrong_hash.catalog_id,
    )
    .await;
    assert_eq!(redirect_phase, JobPhase::Failed);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );
    assert_eq!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .release_pointers
            .active,
        None
    );

    let wrong_digest = model_entry(body, Some(&"f".repeat(64)), None);
    let digest_transport = Arc::new(ModelHttp::new(vec![Ok(model_response(
        &wrong_digest,
        body,
        None,
    ))]));
    let (digest_phase, _) = run_job(
        model_backend(store.clone(), wrong_digest, digest_transport.clone()),
        JobKind::Download,
        "fixture-model-v1",
    )
    .await;
    assert_eq!(digest_phase, JobPhase::Failed);
    assert_eq!(digest_transport.calls.load(Ordering::SeqCst), 1);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );

    let size_entry = model_entry(body, None, None);
    let oversized_body = [body.as_slice(), b"!"].concat();
    let size_transport = Arc::new(ModelHttp::new(vec![Ok(model_response(
        &size_entry,
        &oversized_body,
        None,
    ))]));
    let (size_phase, _) = run_job(
        model_backend(store.clone(), size_entry, size_transport.clone()),
        JobKind::Download,
        "fixture-model-v1",
    )
    .await;
    assert_eq!(size_phase, JobPhase::Failed);
    assert_eq!(size_transport.calls.load(Ordering::SeqCst), 1);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );

    let underrun_entry = model_entry(body, None, Some(body.len() as u64 + 1));
    let underrun_transport = Arc::new(ModelHttp::new(vec![Ok(model_response(
        &underrun_entry,
        body,
        None,
    ))]));
    let (underrun_phase, _) = run_job(
        model_backend(store.clone(), underrun_entry, underrun_transport.clone()),
        JobKind::Download,
        "fixture-model-v1",
    )
    .await;
    assert_eq!(underrun_phase, JobPhase::Failed);
    assert_eq!(underrun_transport.calls.load(Ordering::SeqCst), 1);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );

    let cancel_entry = model_entry(body, None, None);
    let cancel_transport = Arc::new(CancelModelHttp {
        response: model_response(&cancel_entry, body, None),
    });
    let (cancel_phase, _) = run_job(
        model_backend(store.clone(), cancel_entry, cancel_transport),
        JobKind::Download,
        "fixture-model-v1",
    )
    .await;
    assert_eq!(cancel_phase, JobPhase::Failed);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );
    assert!(
        std::fs::read_dir(manager_root.join("ds4/operations"))
            .expect("operations")
            .all(|entry| !entry
                .expect("operations entry")
                .file_name()
                .to_string_lossy()
                .starts_with("model-download-"))
    );

    let no_catalog_transport = Arc::new(ModelHttp::new(vec![]));
    let unknown_backend = RuntimeManagerBackend::for_release_store(store.clone())
        .expect("construct production manager backend")
        .with_transport(no_catalog_transport.clone());
    let (unknown_phase, _) = run_job(unknown_backend, JobKind::Download, "external-model-id").await;
    assert_eq!(unknown_phase, JobPhase::Failed);
    assert_eq!(no_catalog_transport.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .lock()
            .expect("store lock")
            .snapshot()
            .artifacts
            .is_empty()
    );
}

#[tokio::test]
async fn stage_persists_verified_profile_and_inventory_redacts_local_paths() {
    let fixture = fixture();
    let manager_root = fixture.base.join("manager");
    let store = store(&manager_root);
    let model_bytes = b"verified fixture model";
    let entry = model_entry(model_bytes, None, None);
    let (build_id, model_id) =
        publish_stage_artifacts(&manager_root, &store, "ds4-server", model_bytes, &entry);
    let payload_key =
        siderostat::manager::executor::manager_stage_payload_key(&build_id, &model_id)
            .expect("typed artifact IDs");
    let config = StageRuntimeConfig {
        expected_node_role: Some("coordinator".into()),
        expected_family: "ds4".into(),
        context_size: 4096,
        expected_prefix_digest: None,
        config_fingerprint: "c".repeat(64),
        ram_confirmed: false,
    };

    let (phase, _) = run_job(
        stage_backend(store.clone(), entry, config),
        JobKind::Stage,
        &payload_key,
    )
    .await;

    assert_eq!(phase, JobPhase::Succeeded);
    let profile = {
        let guard = store.lock().expect("store lock");
        assert_eq!(guard.snapshot().profiles.len(), 1);
        guard
            .snapshot()
            .profiles
            .values()
            .next()
            .expect("persisted profile")
            .clone()
    };
    assert_eq!(profile.node_role, "coordinator");
    assert_eq!(profile.role_artifact_ids, vec![build_id]);
    assert_eq!(profile.model_artifact_id, model_id);
    assert_eq!(profile.config_fingerprint, "c".repeat(64));
    assert_eq!(profile.compatibility, ProfileCompatibility::Compatible);
    assert_eq!(profile.hardware_readiness, HardwareReadiness::Pending);
    assert!(
        store
            .lock()
            .expect("store lock")
            .set_release_pointers(
                ReleaseIdentity::ManagedProfile(profile.profile_id.clone()),
                None
            )
            .is_err()
    );

    let reopened = ManagerReleaseStore::open(ManagerRoot::explicit(manager_root.clone()), "node-a")
        .expect("reopen staged profile");
    assert_eq!(reopened.snapshot().profiles[&profile.profile_id], profile);
    let staged_inventory = siderostat::manager::api::inventory(reopened.snapshot());
    assert!(!staged_inventory.profiles[0].activation_ready);
    let mut pointer_snapshot = reopened.snapshot().clone();
    pointer_snapshot
        .profiles
        .get_mut(&profile.profile_id)
        .expect("profile")
        .hardware_readiness = HardwareReadiness::Ready;
    pointer_snapshot.release_pointers.active =
        Some(ReleaseIdentity::ManagedProfile(profile.profile_id.clone()));
    pointer_snapshot.release_pointers.previous = Some(ReleaseIdentity::ExternalBaseline {
        config_fingerprint: "d".repeat(64),
        executable_sha256: "e".repeat(64),
        model_sha256: "f".repeat(64),
    });
    let inventory = siderostat::manager::api::inventory(&pointer_snapshot);
    let json = serde_json::to_string(&inventory).expect("serialize inventory");
    assert!(inventory.profiles[0].activation_ready);
    assert_eq!(
        inventory.active_digest, None,
        "stored pointer is not live proof"
    );
    assert_eq!(
        inventory.previous_digest.as_deref(),
        Some("f".repeat(64).as_str())
    );
    let live_digest = &pointer_snapshot.artifacts[&model_id].sha256;
    assert_eq!(
        siderostat::manager::api::inventory_with_live_active_digest(
            &pointer_snapshot,
            Some(live_digest)
        )
        .active_digest
        .as_deref(),
        Some(live_digest.as_str())
    );
    assert!(!json.contains(manager_root.to_str().expect("manager path")));
    assert!(!json.contains("models.example.com"));
    assert!(!json.contains("rel_path"));
}

#[tokio::test]
async fn stage_refuses_role_family_prefix_and_digest_mismatches() {
    for (role, expected_node_role, expected_family, expected_prefix, tamper_model) in [
        ("ds4-server", Some("worker"), "ds4", None, false),
        ("ds4-agent", Some("coordinator"), "ds4", None, false),
        (
            "ds4-server",
            Some("coordinator"),
            "other-family",
            None,
            false,
        ),
        (
            "ds4-server",
            Some("coordinator"),
            "ds4",
            Some("d".repeat(64)),
            false,
        ),
        ("ds4-server", Some("coordinator"), "ds4", None, true),
    ] {
        let fixture = fixture();
        let manager_root = fixture.base.join("manager");
        let store = store(&manager_root);
        let model_bytes = b"verified fixture model";
        let mut entry = model_entry(model_bytes, None, None);
        if expected_prefix.is_some() {
            entry.prefix_file = Some("e".repeat(64));
        }
        let (build_id, model_id) =
            publish_stage_artifacts(&manager_root, &store, role, model_bytes, &entry);
        if tamper_model {
            let path = {
                let guard = store.lock().expect("store lock");
                manager_root.join(&guard.snapshot().artifacts[&model_id].rel_path)
            };
            std::fs::write(path, b"tampered model").expect("tamper model");
        }
        let payload_key =
            siderostat::manager::executor::manager_stage_payload_key(&build_id, &model_id)
                .expect("typed artifact IDs");
        let config = StageRuntimeConfig {
            expected_node_role: expected_node_role.map(str::to_string),
            expected_family: expected_family.into(),
            context_size: 4096,
            expected_prefix_digest: expected_prefix,
            config_fingerprint: "c".repeat(64),
            ram_confirmed: false,
        };
        let (phase, _) = run_job(
            stage_backend(store.clone(), entry, config),
            JobKind::Stage,
            &payload_key,
        )
        .await;
        assert_eq!(phase, JobPhase::Failed);
        assert!(
            store
                .lock()
                .expect("store lock")
                .snapshot()
                .profiles
                .is_empty()
        );
        if tamper_model {
            assert_eq!(
                store.lock().expect("store lock").snapshot().artifacts[&model_id].validation_state,
                siderostat::manager::ArtifactState::Quarantined
            );
        }
    }
}

#[test]
fn build_payload_key_contains_only_receipt_identity_and_allowlisted_role() {
    assert_eq!(
        siderostat::manager::executor::manager_build_payload_key(
            &format!("source-{}", "a".repeat(40)),
            "ds4-server"
        ),
        Some(build_key(
            &format!("source-{}", "a".repeat(40)),
            "ds4-server"
        ))
    );
    assert_eq!(
        siderostat::manager::executor::manager_build_payload_key("source-unknown", "ds4-server"),
        None
    );
    assert_eq!(
        siderostat::manager::executor::manager_build_payload_key(
            &format!("source-{}", "a".repeat(40)),
            "../../bin/sh"
        ),
        None
    );
}

#[test]
fn production_fetch_key_is_fixed_to_official_main() {
    let root = std::env::temp_dir().join("siderostat-fixed-fetch-plan");
    let resolver = ManagerJobInputResolver::official_fetch(root.join("ds4/sources/official.git"));
    let plan = resolver
        .resolve(&fetch_request("job-0"))
        .expect("official fetch resolves");
    let ManagerJobInput::Fetch {
        official,
        remote,
        revision,
        main_ref,
        ..
    } = plan
    else {
        panic!("fixed fetch plan");
    };
    assert_eq!(remote, OFFICIAL_REMOTE);
    assert!(official.matches(OFFICIAL_REMOTE));
    assert_eq!(revision, "main");
    assert_eq!(main_ref, "refs/heads/main");
    assert!(matches!(
        resolver.resolve(&ManagerExecutionRequest {
            payload_key: "arbitrary-url".into(),
            ..fetch_request("job-1")
        }),
        Err(siderostat::manager::executor::ManagerInputError::Rejected)
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn canceled_fetch_does_not_persist_a_receipt_or_change_release_pointers() {
    let fixture = fixture();
    let root = fixture.base.join("manager");
    let store = store(&root);
    let remote = url::Url::from_file_path(&fixture.remote)
        .expect("file URL")
        .to_string();
    let backend = backend(&root, &remote, "main", store.clone());

    let result = backend.execute(fetch_request("job-cancel"), Arc::new(AtomicBool::new(true)));

    assert!(matches!(
        result,
        Err(siderostat::manager::executor::ManagerExecutionError::Canceled)
    ));
    let store = store.lock().expect("store lock");
    assert!(store.snapshot().source_receipts.is_empty());
    assert_eq!(store.snapshot().release_pointers.active, None);
    assert_eq!(store.snapshot().release_pointers.previous, None);
}
