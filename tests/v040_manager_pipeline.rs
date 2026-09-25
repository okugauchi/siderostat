#![cfg(feature = "test-support")]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use siderostat::manager::executor::{
    ManagerExecutionBackend, ManagerExecutionRequest, ManagerJobInput, ManagerJobInputResolver,
    RuntimeManagerBackend,
};
use siderostat::manager::jobs::{JobJournal, JobKind, JobPhase};
use siderostat::manager::registry::ManagerRoot;
use siderostat::manager::store::ManagerReleaseStore;
use siderostat::manager::{GitRunner, ManagerExecutor, OfficialRemote};

const OFFICIAL_REMOTE: &str = "https://github.com/antirez/ds4.git";

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

async fn run_fetch(backend: RuntimeManagerBackend) -> (JobPhase, Arc<Mutex<JobJournal>>) {
    let journal = Arc::new(Mutex::new(JobJournal::new()));
    let id = journal
        .lock()
        .expect("journal lock")
        .enqueue(JobKind::Fetch, "official")
        .expect("enqueue fetch");
    let (executor, worker) = ManagerExecutor::start(journal.clone(), backend);
    executor.submit(fetch_request(&id)).expect("submit fetch");
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
