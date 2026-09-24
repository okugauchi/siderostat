//! Bounded manager job executor and cancellation bridge.

use std::cell::Cell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::manager::catalog;
use crate::manager::jobs::{JobJournal, JobKind, JobPhase};
use crate::manager::{activation, build, download, registry, rollback, source, stage, verify};

/// The immutable context passed from a submitted job to its backend.
#[derive(Debug, Clone)]
pub struct ManagerExecutionRequest {
    pub id: String,
    pub kind: JobKind,
    pub payload_key: String,
    pub expected_generation: u64,
    pub runtime_lease: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerExecutionOutcome {
    pub progress: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerExecutionError {
    Canceled,
    InputRejected(String),
    Domain(String),
    Unavailable,
}

pub trait ManagerExecutionBackend: Send + Sync + 'static {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError>;
}

impl<T: ManagerExecutionBackend + ?Sized> ManagerExecutionBackend for Arc<T> {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
        (**self).execute(request, cancel)
    }
}

/// A payload key resolves to one concrete, typed domain operation. Keys never
/// become paths, URLs, commands, or model names by themselves.
#[derive(Clone)]
pub enum ManagerJobInput {
    Fetch {
        cache: PathBuf,
        official: source::OfficialRemote,
        remote: String,
        revision: String,
        main_ref: String,
    },
    Build {
        request: Box<build::BuildRequest>,
        source: registry::SourceRecord,
    },
    Download {
        spec: download::DownloadSpec,
        catalog_id: String,
        part_path: PathBuf,
    },
    Verify {
        registry: Arc<Mutex<registry::ArtifactRegistry>>,
        artifact_id: String,
        expected_sha256: String,
    },
    Stage {
        request: Box<stage::StageRequest>,
        registry: Arc<Mutex<registry::ArtifactRegistry>>,
        artifact_ids: Vec<String>,
        model_artifact_id: String,
        model_artifact_path: PathBuf,
    },
    Activate(activation::ActivationRequest),
    Rollback(rollback::RollbackRequest),
}

impl ManagerJobInput {
    fn kind(&self) -> JobKind {
        match self {
            Self::Fetch { .. } => JobKind::Fetch,
            Self::Build { .. } => JobKind::Build,
            Self::Download { .. } => JobKind::Download,
            Self::Verify { .. } => JobKind::Verify,
            Self::Stage { .. } => JobKind::Stage,
            Self::Activate(_) => JobKind::Activate,
            Self::Rollback(_) => JobKind::Rollback,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerInputError {
    Rejected,
    Unavailable,
}

/// Explicitly configured payload keys. An empty resolver is safe for a runtime
/// that has not yet connected its source, catalog, registry, and cluster state.
#[derive(Default)]
pub struct ManagerJobInputResolver {
    plans: HashMap<(JobKind, String), ManagerJobInput>,
    model_catalog: HashMap<String, catalog::ModelCatalogEntry>,
}

impl ManagerJobInputResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        kind: JobKind,
        key: impl Into<String>,
        plan: ManagerJobInput,
    ) -> Result<(), ManagerInputError> {
        let key = key.into();
        if key.is_empty() || plan.kind() != kind {
            return Err(ManagerInputError::Rejected);
        }
        self.plans.insert((kind, key), plan);
        Ok(())
    }

    /// Register an entry only after its catalog rules, full checksum, URL and
    /// size have been checked. Plans can reference this catalog by ID only.
    pub fn register_catalog_entry(
        &mut self,
        entry: catalog::ModelCatalogEntry,
    ) -> Result<(), ManagerInputError> {
        if entry.catalog_id.is_empty()
            || entry.url.is_empty()
            || !full_sha256(&entry.sha256)
            || entry.size == 0
        {
            return Err(ManagerInputError::Unavailable);
        }
        let entry = catalog::validate_entry(entry).map_err(|_| ManagerInputError::Rejected)?;
        self.model_catalog.insert(entry.catalog_id.clone(), entry);
        Ok(())
    }

    pub fn resolve(
        &self,
        request: &ManagerExecutionRequest,
    ) -> Result<ManagerJobInput, ManagerInputError> {
        self.resolve_inner(request, true)
    }

    fn resolve_inner(
        &self,
        request: &ManagerExecutionRequest,
        check_real_inputs: bool,
    ) -> Result<ManagerJobInput, ManagerInputError> {
        if request.id.is_empty() || request.payload_key.is_empty() {
            return Err(ManagerInputError::Rejected);
        }
        if matches!(request.kind, JobKind::Activate | JobKind::Rollback)
            && (request.expected_generation == 0
                || request
                    .runtime_lease
                    .as_deref()
                    .is_none_or(|lease| lease.trim().is_empty()))
        {
            return Err(ManagerInputError::Rejected);
        }
        let Some(plan) = self.plans.get(&(request.kind, request.payload_key.clone())) else {
            return if matches!(request.kind, JobKind::Download | JobKind::Stage)
                && self.model_catalog.is_empty()
            {
                Err(ManagerInputError::Unavailable)
            } else {
                Err(ManagerInputError::Rejected)
            };
        };
        match plan {
            ManagerJobInput::Fetch {
                cache,
                remote,
                revision,
                main_ref,
                ..
            } => {
                if cache.as_os_str().is_empty()
                    || remote.is_empty()
                    || revision.is_empty()
                    || main_ref.is_empty()
                {
                    return Err(ManagerInputError::Unavailable);
                }
            }
            ManagerJobInput::Build {
                request: req,
                source,
            } => {
                if req.source != source.full_commit
                    || source.remote.is_empty()
                    || source.main_proof.is_empty()
                    || !full_git_commit(&source.full_commit)
                {
                    return Err(ManagerInputError::Unavailable);
                }
                if check_real_inputs && !pinned_checkout_matches(req) {
                    return Err(ManagerInputError::Unavailable);
                }
            }
            ManagerJobInput::Download {
                spec,
                catalog_id,
                part_path,
            } => {
                let entry = self
                    .model_catalog
                    .get(catalog_id)
                    .ok_or(ManagerInputError::Unavailable)?;
                if spec.url != entry.url
                    || spec.expected_size != entry.size
                    || spec.sha256 != entry.sha256
                    || spec.redirect_allowlist != entry.redirect_allowlist
                {
                    return Err(ManagerInputError::Rejected);
                }
                if check_real_inputs && part_path.parent().is_none_or(|parent| !parent.is_dir()) {
                    return Err(ManagerInputError::Unavailable);
                }
            }
            ManagerJobInput::Verify {
                registry,
                artifact_id,
                expected_sha256,
            } => {
                if !full_sha256(expected_sha256) {
                    return Err(ManagerInputError::Unavailable);
                }
                if check_real_inputs {
                    let registry = registry
                        .lock()
                        .map_err(|_| ManagerInputError::Unavailable)?;
                    let record = registry
                        .get(artifact_id)
                        .ok_or(ManagerInputError::Unavailable)?;
                    let path = registry.root().root().join(&record.rel_path);
                    if !path.is_file() || registry.resolve_within_root(&record.rel_path).is_err() {
                        return Err(ManagerInputError::Unavailable);
                    }
                }
            }
            ManagerJobInput::Stage {
                request: req,
                registry,
                artifact_ids,
                model_artifact_id,
                model_artifact_path,
            } => {
                if req.role_artifacts.is_empty()
                    || model_artifact_id.is_empty()
                    || model_artifact_path.as_os_str().is_empty()
                {
                    return Err(ManagerInputError::Unavailable);
                }
                let entry = self
                    .model_catalog
                    .get(&req.model.catalog_id)
                    .ok_or(ManagerInputError::Unavailable)?;
                let planned_model = catalog::validate_entry(req.model.clone())
                    .map_err(|_| ManagerInputError::Rejected)?;
                if &planned_model != entry {
                    return Err(ManagerInputError::Rejected);
                }
                if check_real_inputs {
                    let registry = registry
                        .lock()
                        .map_err(|_| ManagerInputError::Unavailable)?;
                    if artifact_ids.len() != req.role_artifacts.len() {
                        return Err(ManagerInputError::Unavailable);
                    }
                    for (id, path) in artifact_ids.iter().zip(&req.role_artifacts) {
                        verified_record_matches(&registry, id, path, None)?;
                    }
                    verified_record_matches(
                        &registry,
                        model_artifact_id,
                        model_artifact_path,
                        Some(&entry.sha256),
                    )?;
                    if registry
                        .get(model_artifact_id)
                        .is_none_or(|record| record.kind != "model")
                    {
                        return Err(ManagerInputError::Rejected);
                    }
                }
            }
            ManagerJobInput::Activate(_) | ManagerJobInput::Rollback(_) => {}
        }
        let mut plan = plan.clone();
        match &mut plan {
            ManagerJobInput::Download {
                spec, catalog_id, ..
            } => {
                let entry = self
                    .model_catalog
                    .get(catalog_id)
                    .expect("resolved catalog");
                // The catalog is authoritative; only credentials come from the
                // configured transport plan after identity checks above.
                let credentials = spec.credentials.take();
                *spec = download::DownloadSpec {
                    url: entry.url.clone(),
                    expected_size: entry.size,
                    sha256: entry.sha256.clone(),
                    redirect_allowlist: entry.redirect_allowlist.clone(),
                    credentials,
                };
            }
            ManagerJobInput::Activate(input) => {
                input.expected_generation = request.expected_generation;
                input.runtime_lease = request.runtime_lease.clone().expect("checked lease");
            }
            ManagerJobInput::Rollback(input) => {
                if input.previous_digest.is_empty() {
                    return Err(ManagerInputError::Unavailable);
                }
                input.expected_generation = request.expected_generation;
                input.runtime_lease = request.runtime_lease.clone().expect("checked lease");
            }
            _ => {}
        }
        Ok(plan)
    }
}

fn full_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn full_git_commit(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn pinned_checkout_matches(req: &build::BuildRequest) -> bool {
    if !req.workspace.is_dir() || !req.workspace.join(".git").exists() {
        return false;
    }
    let Some(path) = req.workspace.to_str() else {
        return false;
    };
    let git = source::GitRunner::default();
    let Ok(head) = git.run(["-C", path, "rev-parse", "--verify", "HEAD^{commit}"]) else {
        return false;
    };
    if head != req.source {
        return false;
    }
    matches!(git.run(["-C", path, "status", "--porcelain", "--untracked-files=all"]), Ok(status) if status.is_empty())
}

fn verified_record_matches(
    registry: &registry::ArtifactRegistry,
    id: &str,
    path: &std::path::Path,
    expected_sha256: Option<&str>,
) -> Result<(), ManagerInputError> {
    let record = registry.get(id).ok_or(ManagerInputError::Unavailable)?;
    if record.state != registry::ArtifactState::Verified || !full_sha256(&record.sha256) {
        return Err(ManagerInputError::Unavailable);
    }
    if registry.root().root().join(&record.rel_path) != path {
        return Err(ManagerInputError::Rejected);
    }
    if !path.is_file() {
        return Err(ManagerInputError::Unavailable);
    }
    let resolved = registry
        .resolve_within_root(&record.rel_path)
        .map_err(|_| ManagerInputError::Unavailable)?;
    let actual = file_sha256_streaming(&resolved)?;
    if actual != record.sha256 || expected_sha256.is_some_and(|expected| actual != expected) {
        return Err(ManagerInputError::Unavailable);
    }
    Ok(())
}

fn file_sha256_streaming(path: &std::path::Path) -> Result<String, ManagerInputError> {
    use sha2::Digest;

    let mut file = std::fs::File::open(path).map_err(|_| ManagerInputError::Unavailable)?;
    let mut hasher = sha2::Sha256::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let count = std::io::Read::read(&mut file, &mut chunk)
            .map_err(|_| ManagerInputError::Unavailable)?;
        if count == 0 {
            break;
        }
        hasher.update(&chunk[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Production adapter. Activation and rollback require a connected cluster
/// runtime; until that bridge is supplied, they fail before changing state.
pub struct RuntimeManagerBackend {
    resolver: ManagerJobInputResolver,
    transport: Option<Arc<dyn download::HttpTransport + Send + Sync>>,
}

impl RuntimeManagerBackend {
    pub fn new(resolver: ManagerJobInputResolver) -> Self {
        Self {
            resolver,
            transport: None,
        }
    }

    pub fn without_model_catalog() -> Self {
        Self::new(ManagerJobInputResolver::new())
    }

    pub fn with_transport(
        mut self,
        transport: Arc<dyn download::HttpTransport + Send + Sync>,
    ) -> Self {
        self.transport = Some(transport);
        self
    }
}

impl ManagerExecutionBackend for RuntimeManagerBackend {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        let plan = self
            .resolver
            .resolve(&request)
            .map_err(|error| match error {
                ManagerInputError::Rejected => {
                    ManagerExecutionError::InputRejected("invalid manager job input".into())
                }
                ManagerInputError::Unavailable => ManagerExecutionError::Unavailable,
            })?;
        match plan {
            ManagerJobInput::Fetch {
                cache,
                official,
                remote,
                revision,
                main_ref,
            } => {
                source::stage_source_cancellable(
                    &cache, &official, &remote, &revision, &main_ref, &cancel,
                )
                .map_err(|error| match error {
                    source::SourceError::Canceled => ManagerExecutionError::Canceled,
                    other => ManagerExecutionError::Domain(other.to_string()),
                })?;
            }
            ManagerJobInput::Build { request, .. } => {
                build::build_artifacts(&request, &cancel)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Download {
                spec, part_path, ..
            } => {
                let transport = self
                    .transport
                    .as_ref()
                    .ok_or(ManagerExecutionError::Unavailable)?;
                download::download_bounded(&spec, transport.as_ref(), &part_path, None, &cancel)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Verify {
                registry,
                artifact_id,
                expected_sha256,
            } => {
                let mut registry = registry
                    .lock()
                    .map_err(|_| ManagerExecutionError::Unavailable)?;
                verify::verify_artifact(&mut registry, &artifact_id, &expected_sha256)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Stage { mut request, .. } => {
                request.model = catalog::validate_entry(request.model.clone())
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
                let result = stage::stage_profile(*request)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
                if result.status != stage::StagedProfileStatus::Validated {
                    return Err(ManagerExecutionError::Unavailable);
                }
            }
            ManagerJobInput::Activate(_) | ManagerJobInput::Rollback(_) => {
                return Err(ManagerExecutionError::Unavailable);
            }
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        Ok(ManagerExecutionOutcome { progress: 100 })
    }
}

/// Deterministic adapter for integration tests. Its seven explicit plans use
/// the same request/result boundary while avoiding network and child processes.
#[cfg(feature = "test-support")]
pub struct FixtureManagerBackend {
    resolver: ManagerJobInputResolver,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "test-support")]
impl Default for FixtureManagerBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "test-support")]
impl FixtureManagerBackend {
    pub fn new() -> Self {
        let mut resolver = ManagerJobInputResolver::new();
        let digest = verify::hex_sha256(b"fixture");
        let model = catalog::ModelCatalogEntry {
            catalog_id: "fixture-model".into(),
            url: "fixture://model".into(),
            redirect_allowlist: vec![],
            size: 7,
            sha256: digest.clone(),
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
            status: catalog::CapabilityStatus::Candidate,
        };
        resolver
            .register_catalog_entry(model.clone())
            .expect("valid fixture catalog");
        let inputs = [
            (
                JobKind::Fetch,
                ManagerJobInput::Fetch {
                    cache: PathBuf::from("fixture-cache"),
                    official: source::OfficialRemote::new("fixture://source"),
                    remote: "fixture://source".into(),
                    revision: "main".into(),
                    main_ref: "refs/heads/main".into(),
                },
            ),
            (
                JobKind::Build,
                ManagerJobInput::Build {
                    request: Box::new(build::BuildRequest::new(
                        "ds4",
                        "build",
                        "a".repeat(40),
                        "fixture-workspace",
                        "out.bin",
                    )),
                    source: registry::SourceRecord {
                        remote: "fixture://source".into(),
                        full_commit: "a".repeat(40),
                        main_proof: "refs/heads/main".into(),
                        fetched_at: 1,
                    },
                },
            ),
            (
                JobKind::Download,
                ManagerJobInput::Download {
                    spec: download::DownloadSpec::new("fixture://model", 7, digest.clone()),
                    catalog_id: "fixture-model".into(),
                    part_path: PathBuf::from("fixture-model.part"),
                },
            ),
            (
                JobKind::Verify,
                ManagerJobInput::Verify {
                    registry: Arc::new(Mutex::new(registry::ArtifactRegistry::new(
                        registry::ManagerRoot::explicit(PathBuf::from("fixture-root")),
                    ))),
                    artifact_id: "fixture-artifact".into(),
                    expected_sha256: digest.clone(),
                },
            ),
            (
                JobKind::Stage,
                ManagerJobInput::Stage {
                    request: Box::new(stage::StageRequest {
                        profile_id: "fixture-stage".into(),
                        role_artifacts: vec![PathBuf::from("fixture-artifact")],
                        model,
                        expected_family: "ds4".into(),
                        context_size: 4096,
                        expected_prefix_digest: None,
                        ram_confirmed: true,
                    }),
                    registry: Arc::new(Mutex::new(registry::ArtifactRegistry::new(
                        registry::ManagerRoot::explicit(PathBuf::from("fixture-root")),
                    ))),
                    artifact_ids: vec!["fixture-artifact".into()],
                    model_artifact_id: "fixture-model".into(),
                    model_artifact_path: PathBuf::from("fixture-root/model.bin"),
                },
            ),
            (
                JobKind::Activate,
                ManagerJobInput::Activate(activation::ActivationRequest {
                    operation_id: "fixture-activate".into(),
                    expected_generation: 1,
                    runtime_lease: "fixture-lease".into(),
                    policy_epoch: 1,
                    nodes: vec!["local".into(), "peer".into()],
                }),
            ),
            (
                JobKind::Rollback,
                ManagerJobInput::Rollback(rollback::RollbackRequest {
                    operation_id: "fixture-rollback".into(),
                    expected_generation: 1,
                    runtime_lease: "fixture-lease".into(),
                    policy_epoch: 1,
                    previous_digest: digest,
                    nodes: vec!["local".into(), "peer".into()],
                }),
            ),
        ];
        for (kind, input) in inputs {
            resolver
                .register(kind, format!("fixture-{kind}"), input)
                .expect("valid fixture plan");
        }
        Self {
            resolver,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    pub fn domain_call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[cfg(feature = "test-support")]
impl ManagerExecutionBackend for FixtureManagerBackend {
    fn execute(
        &self,
        request: ManagerExecutionRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        let input = self
            .resolver
            .resolve_inner(&request, false)
            .map_err(|error| match error {
                ManagerInputError::Rejected => {
                    ManagerExecutionError::InputRejected("invalid fixture input".into())
                }
                ManagerInputError::Unavailable => ManagerExecutionError::Unavailable,
            })?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Use each domain's pure validation or state transition. Fetch/build/
        // download/verify avoid their external IO boundaries in this fixture.
        match input {
            ManagerJobInput::Fetch {
                official, remote, ..
            } if official.matches(&remote) => {}
            ManagerJobInput::Build { request, .. }
                if build::is_approved_role(&request.role)
                    && build::is_approved_target(&request.target) => {}
            ManagerJobInput::Download { spec, .. } => {
                download::check_capacity(u64::MAX, spec.expected_size, 1, 0)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Verify {
                expected_sha256, ..
            } if expected_sha256 == verify::hex_sha256(b"fixture") => {}
            ManagerJobInput::Stage { mut request, .. } => {
                request.model = catalog::validate_entry(request.model.clone())
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
                stage::stage_profile(*request)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Activate(input) => {
                if !matches!(
                    activation::prepare_activation(input, &FixtureNodeProvider),
                    Ok(activation::PrepareOutcome::Prepared(_))
                ) {
                    return Err(ManagerExecutionError::Unavailable);
                }
            }
            ManagerJobInput::Rollback(input) => {
                rollback::rollback_to_previous(
                    input,
                    &FixtureNodeProvider,
                    &mut FixtureRecovery,
                    false,
                )
                .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            _ => {
                return Err(ManagerExecutionError::InputRejected(
                    "invalid fixture plan".into(),
                ));
            }
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        Ok(ManagerExecutionOutcome { progress: 100 })
    }
}

#[cfg(feature = "test-support")]
struct FixtureNodeProvider;

#[cfg(feature = "test-support")]
impl activation::NodeArtifactProvider for FixtureNodeProvider {
    fn verified_artifact(&self, _node: &activation::NodeId) -> Option<String> {
        Some(verify::hex_sha256(b"fixture"))
    }
}

#[cfg(feature = "test-support")]
struct FixtureRecovery;

#[cfg(feature = "test-support")]
impl rollback::PreviousRecovery for FixtureRecovery {
    fn start_previous_and_wait_ready(&mut self) -> Result<(), activation::ActivationError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerExecutorError {
    QueueFull,
    QueueClosed,
    JobNotFound,
    RequestMismatch,
}

impl std::fmt::Display for ManagerExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::QueueFull => "manager executor queue is full",
            Self::QueueClosed => "manager executor queue is closed",
            Self::JobNotFound => "manager job not found",
            Self::RequestMismatch => "manager job request does not match journal",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ManagerExecutorError {}

struct QueuedExecution {
    request: ManagerExecutionRequest,
    cancel: Arc<AtomicBool>,
}

struct ExecutorState {
    sender: Mutex<Option<mpsc::Sender<QueuedExecution>>>,
    cancellation: Mutex<HashMap<String, Arc<AtomicBool>>>,
    journal: Arc<Mutex<JobJournal>>,
}

#[derive(Clone)]
pub struct ManagerExecutorHandle {
    state: Arc<ExecutorState>,
}

impl ManagerExecutorHandle {
    /// Enqueue a journaled job once. A second submission for the same ID is ignored.
    pub fn submit(&self, request: ManagerExecutionRequest) -> Result<(), ManagerExecutorError> {
        let mut journal = self.state.journal.lock().expect("manager journal poisoned");
        let phase = journal
            .get(&request.id)
            .map(|job| job.phase)
            .ok_or(ManagerExecutorError::JobNotFound)?;
        if !matches!(phase, JobPhase::Running | JobPhase::Cancelling) {
            return Ok(());
        }
        if !journal.matches_request(&request.id, request.kind, &request.payload_key) {
            return Err(ManagerExecutorError::RequestMismatch);
        }

        let mut cancellation = self
            .state
            .cancellation
            .lock()
            .expect("manager cancellation registry poisoned");
        if cancellation.contains_key(&request.id) {
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(phase == JobPhase::Cancelling));
        cancellation.insert(request.id.clone(), cancel.clone());

        let result = self
            .state
            .sender
            .lock()
            .expect("manager executor sender poisoned")
            .as_ref()
            .map_or(Err(ManagerExecutorError::QueueClosed), |sender| {
                sender
                    .try_send(QueuedExecution {
                        request: request.clone(),
                        cancel,
                    })
                    .map_err(|error| match error {
                        mpsc::error::TrySendError::Full(_) => ManagerExecutorError::QueueFull,
                        mpsc::error::TrySendError::Closed(_) => ManagerExecutorError::QueueClosed,
                    })
            });

        if let Err(error) = result {
            cancellation.remove(&request.id);
            let _ = journal.fail_if_running(&request.id, error.to_string());
        }
        result
    }

    /// Mark a running job as cancelling and notify its queued or active backend.
    pub fn cancel(&self, id: &str) -> Result<(), ManagerExecutorError> {
        // Journal first, then cancellation registry: the worker uses the same order.
        let mut journal = self.state.journal.lock().expect("manager journal poisoned");
        let phase = journal
            .get(id)
            .map(|job| job.phase)
            .ok_or(ManagerExecutorError::JobNotFound)?;
        if matches!(phase, JobPhase::Running | JobPhase::Cancelling) {
            journal
                .request_cancel(id)
                .map_err(|_| ManagerExecutorError::JobNotFound)?;
            if let Some(cancel) = self
                .state
                .cancellation
                .lock()
                .expect("manager cancellation registry poisoned")
                .get(id)
            {
                cancel.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Close submissions and let the worker drain jobs already in the queue.
    pub fn shutdown_for_test(&self) {
        self.state
            .sender
            .lock()
            .expect("manager executor sender poisoned")
            .take();
    }
}

pub struct ManagerExecutor;

const MANAGER_PANIC_MESSAGE: &str = "manager process panic (details redacted)";

thread_local! {
    static BACKEND_PANIC_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

struct BackendPanicMarker(bool);

impl BackendPanicMarker {
    fn enter() -> Self {
        Self(BACKEND_PANIC_ACTIVE.with(|active| active.replace(true)))
    }
}

impl Drop for BackendPanicMarker {
    fn drop(&mut self) {
        BACKEND_PANIC_ACTIVE.with(|active| active.set(self.0));
    }
}

fn install_backend_panic_dispatcher() {
    std::panic::set_hook(Box::new(|_| {
        if !BACKEND_PANIC_ACTIVE.with(Cell::get) {
            eprintln!("{MANAGER_PANIC_MESSAGE}");
        }
    }));
}

fn execute_backend<B: ManagerExecutionBackend>(
    backend: Arc<B>,
    request: ManagerExecutionRequest,
    cancel: Arc<AtomicBool>,
) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
    // Reinstall just before execution in case another component replaced it.
    // No previous hook or panic payload is copied into this dispatcher.
    install_backend_panic_dispatcher();
    let marker = BackendPanicMarker::enter();
    let result = catch_unwind(AssertUnwindSafe(|| backend.execute(request, cancel)))
        .unwrap_or(Err(ManagerExecutionError::Unavailable));
    drop(marker);
    result
}

impl ManagerExecutor {
    pub fn start<B: ManagerExecutionBackend>(
        journal: Arc<Mutex<JobJournal>>,
        backend: B,
    ) -> (ManagerExecutorHandle, tokio::task::JoinHandle<()>) {
        install_backend_panic_dispatcher();
        let (sender, mut receiver) = mpsc::channel(16);
        let state = Arc::new(ExecutorState {
            sender: Mutex::new(Some(sender)),
            cancellation: Mutex::new(HashMap::new()),
            journal,
        });
        let handle = ManagerExecutorHandle {
            state: state.clone(),
        };
        let backend = Arc::new(backend);
        let guard = WorkerExitGuard(state.clone());
        let worker = tokio::spawn(async move {
            let _guard = guard;
            while let Some(queued) = receiver.recv().await {
                let id = queued.request.id.clone();
                let result = if queued.cancel.load(Ordering::SeqCst) {
                    Err(ManagerExecutionError::Canceled)
                } else {
                    let backend = backend.clone();
                    let cancel = queued.cancel.clone();
                    tokio::task::spawn_blocking(move || {
                        execute_backend(backend, queued.request, cancel)
                    })
                    .await
                    .unwrap_or(Err(ManagerExecutionError::Unavailable))
                };
                let mut journal = state.journal.lock().expect("manager journal poisoned");
                if queued.cancel.load(Ordering::SeqCst)
                    || journal.is_cancelling(&id).unwrap_or(false)
                {
                    let _ = journal.fail_if_running(&id, "manager job canceled");
                } else {
                    match result {
                        Ok(outcome) => {
                            let _ = outcome.progress;
                            let _ = journal.succeed_if_running(&id);
                        }
                        Err(error) => {
                            let safe_message = public_error(&error);
                            tracing::warn!(job_id = %id, error = safe_message, "manager job failed");
                            let _ = journal.fail_if_running(&id, safe_message);
                        }
                    }
                }
                state
                    .cancellation
                    .lock()
                    .expect("manager cancellation registry poisoned")
                    .remove(&id);
            }
        });
        (handle, worker)
    }
}

/// An aborted worker must close every accepted job; no journal entry stays running.
struct WorkerExitGuard(Arc<ExecutorState>);

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        let mut journal = self.0.journal.lock().expect("manager journal poisoned");
        let mut cancellation = self
            .0
            .cancellation
            .lock()
            .expect("manager cancellation registry poisoned");
        for (id, cancel) in cancellation.drain() {
            cancel.store(true, Ordering::SeqCst);
            let _ = journal.fail_if_running(&id, "manager executor unavailable");
        }
    }
}

/// Backend text is never copied into a public job or a trace event. It may
/// contain URL userinfo, bearer tokens, query values, or raw build logs.
fn public_error(error: &ManagerExecutionError) -> &'static str {
    match error {
        ManagerExecutionError::Canceled => "manager job canceled",
        ManagerExecutionError::InputRejected(_) => "manager job input rejected",
        ManagerExecutionError::Domain(_) => "manager backend failed",
        ManagerExecutionError::Unavailable => "manager backend unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::jobs::{JobKind, JobPhase};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone)]
    enum FixtureOutcome {
        Success {
            progress: u8,
        },
        SuccessAfterCancel {
            started: Arc<AtomicBool>,
        },
        Error(String),
        InputRejected(String),
        Canceled,
        Panic,
        Block {
            started: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
        },
    }

    struct FixtureBackend(FixtureOutcome);

    struct CountingBackend(Arc<AtomicUsize>);

    struct BlockingCountingBackend {
        calls: Arc<AtomicUsize>,
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }

    struct GitCancelBackend(PathBuf);

    impl ManagerExecutionBackend for GitCancelBackend {
        fn execute(
            &self,
            request: ManagerExecutionRequest,
            cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            if request.kind == JobKind::Fetch {
                source::GitRunner::new(&self.0)
                    .run_cancellable(["ignored"], &cancel)
                    .map_err(|error| match error {
                        source::SourceError::Canceled => ManagerExecutionError::Canceled,
                        other => ManagerExecutionError::Domain(other.to_string()),
                    })?;
            }
            Ok(ManagerExecutionOutcome { progress: 100 })
        }
    }

    impl ManagerExecutionBackend for CountingBackend {
        fn execute(
            &self,
            _request: ManagerExecutionRequest,
            _cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ManagerExecutionOutcome { progress: 100 })
        }
    }

    impl ManagerExecutionBackend for BlockingCountingBackend {
        fn execute(
            &self,
            _request: ManagerExecutionRequest,
            _cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            Ok(ManagerExecutionOutcome { progress: 100 })
        }
    }

    impl ManagerExecutionBackend for FixtureBackend {
        fn execute(
            &self,
            _request: ManagerExecutionRequest,
            cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            match &self.0 {
                FixtureOutcome::Success { progress } => Ok(ManagerExecutionOutcome {
                    progress: *progress,
                }),
                FixtureOutcome::SuccessAfterCancel { started } => {
                    started.store(true, Ordering::SeqCst);
                    while !cancel.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    Ok(ManagerExecutionOutcome { progress: 100 })
                }
                FixtureOutcome::Error(message) => {
                    Err(ManagerExecutionError::Domain(message.clone()))
                }
                FixtureOutcome::InputRejected(message) => {
                    Err(ManagerExecutionError::InputRejected(message.clone()))
                }
                FixtureOutcome::Canceled => Err(ManagerExecutionError::Canceled),
                FixtureOutcome::Panic => panic!("backend panic secret"),
                FixtureOutcome::Block { started, release } => {
                    started.store(true, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                    Ok(ManagerExecutionOutcome { progress: 100 })
                }
            }
        }
    }

    fn test_executor(
        outcome: FixtureOutcome,
    ) -> (
        ManagerExecutorHandle,
        tokio::task::JoinHandle<()>,
        Arc<Mutex<JobJournal>>,
    ) {
        let journal = Arc::new(Mutex::new(JobJournal::new()));
        let (handle, worker) = ManagerExecutor::start(journal.clone(), FixtureBackend(outcome));
        (handle, worker, journal)
    }

    fn fixture_request(
        journal: &Arc<Mutex<JobJournal>>,
        kind: JobKind,
        key: &str,
    ) -> ManagerExecutionRequest {
        let id = journal.lock().unwrap().enqueue(kind, key).expect("enqueue");
        ManagerExecutionRequest {
            id,
            kind,
            payload_key: key.to_string(),
            expected_generation: 0,
            runtime_lease: None,
        }
    }

    #[tokio::test]
    async fn backend_success_reaches_succeeded() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        let request = fixture_request(&journal, JobKind::Build, "fixture-build");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Succeeded
        );
    }

    #[tokio::test]
    async fn backend_error_reaches_failed_without_secret() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Error(
            "https://user:secret@example.invalid/?token=secret raw build log: secret".into(),
        ));
        let request = fixture_request(&journal, JobKind::Verify, "fixture-verify");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
        assert!(!job.error.contains("raw build log"));
    }

    #[tokio::test]
    async fn cancel_wins_over_late_backend_success() {
        let started = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::SuccessAfterCancel {
            started: started.clone(),
        });
        let request = fixture_request(&journal, JobKind::Download, "fixture-download");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        handle.cancel(&id).expect("cancel");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(job.cancel);
    }

    #[tokio::test]
    async fn canceled_git_fetch_releases_executor_for_next_job() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!(
            "siderostat-executor-fetch-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let marker = base.join("started");
        let script = base.join("slow-git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 5 &\nprintf '%s' \"$$\" > '{}'\nwait\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = Arc::new(Mutex::new(JobJournal::new()));
        let (handle, worker) = ManagerExecutor::start(journal.clone(), GitCancelBackend(script));
        let first = fixture_request(&journal, JobKind::Fetch, "source");
        let first_id = first.id.clone();
        handle.submit(first).unwrap();
        let second = fixture_request(&journal, JobKind::Build, "next");
        let second_id = second.id.clone();
        handle.submit(second).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("git started");
        handle.cancel(&first_id).unwrap();
        handle.shutdown_for_test();
        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .expect("cancel releases worker")
            .expect("worker");
        let journal = journal.lock().unwrap();
        let canceled = journal.get(&first_id).unwrap();
        assert_eq!(canceled.phase, JobPhase::Failed);
        assert!(canceled.cancel);
        assert_eq!(journal.get(&second_id).unwrap().phase, JobPhase::Succeeded);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn closed_queue_marks_existing_job_failed() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        handle.shutdown_for_test();
        let request = fixture_request(&journal, JobKind::Stage, "fixture-stage");
        let id = request.id.clone();
        let result = handle.submit(request);
        assert!(matches!(result, Err(ManagerExecutorError::QueueClosed)));
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn backend_input_rejection_is_redacted() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::InputRejected(
            "Bearer secret token=secret".into(),
        ));
        let request = fixture_request(&journal, JobKind::Fetch, "fixture-fetch");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
        assert!(!job.error.contains("Bearer"));
    }

    #[tokio::test]
    async fn backend_canceled_reaches_failed() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Canceled);
        let request = fixture_request(&journal, JobKind::Rollback, "fixture-rollback");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn backend_panic_reaches_failed_without_panic_text() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Panic);
        let request = fixture_request(&journal, JobKind::Activate, "fixture-activate");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        handle.shutdown_for_test();
        worker.await.expect("worker");
        let job = journal.lock().unwrap().get(&id).unwrap().clone();
        assert_eq!(job.phase, JobPhase::Failed);
        assert!(!job.error.contains("secret"));
    }

    #[tokio::test]
    async fn backend_panic_hook_hides_payload() {
        if std::env::var_os("SIDEROSTAT_EXECUTOR_PANIC_CHILD").is_some() {
            let (handle, worker, journal) = test_executor(FixtureOutcome::Panic);
            let request = fixture_request(&journal, JobKind::Build, "panic-hook");
            let id = request.id.clone();
            handle.submit(request).expect("queue");
            handle.shutdown_for_test();
            worker.await.expect("worker");
            assert_eq!(
                journal.lock().unwrap().get(&id).unwrap().phase,
                JobPhase::Failed
            );
            let _ = std::panic::catch_unwind(|| panic!("hook-delegated-marker"));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "manager::executor::tests::backend_panic_hook_hides_payload",
                "--nocapture",
            ])
            .env("SIDEROSTAT_EXECUTOR_PANIC_CHILD", "1")
            .output()
            .expect("child test");
        assert!(output.status.success());
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !visible.contains("backend panic secret"),
            "panic payload leaked: {visible}"
        );
        assert!(
            !visible.contains("hook-delegated-marker"),
            "unrelated panic payload leaked"
        );
        assert!(visible.contains("manager process panic (details redacted)"));
    }

    #[tokio::test]
    async fn external_hook_replacement_cannot_leak_later_backend_panic() {
        if std::env::var_os("SIDEROSTAT_EXECUTOR_REPLACED_HOOK_CHILD").is_some() {
            let (first, first_worker, first_journal) =
                test_executor(FixtureOutcome::Success { progress: 100 });
            first
                .submit(fixture_request(
                    &first_journal,
                    JobKind::Build,
                    "first-backend",
                ))
                .expect("first queue");
            first.shutdown_for_test();
            first_worker.await.expect("first worker");

            let (second, second_worker, second_journal) = test_executor(FixtureOutcome::Panic);
            std::panic::set_hook(Box::new(|info| eprintln!("LEAKING HOOK: {info}")));
            let request = fixture_request(&second_journal, JobKind::Build, "second-backend");
            let id = request.id.clone();
            second.submit(request).expect("second queue");
            second.shutdown_for_test();
            second_worker.await.expect("second worker");
            assert_eq!(
                second_journal.lock().unwrap().get(&id).unwrap().phase,
                JobPhase::Failed
            );
            let _ = std::panic::catch_unwind(|| panic!("unrelated-after-marker"));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "manager::executor::tests::external_hook_replacement_cannot_leak_later_backend_panic",
                "--nocapture",
            ])
            .env("SIDEROSTAT_EXECUTOR_REPLACED_HOOK_CHILD", "1")
            .output()
            .expect("child test");
        assert!(output.status.success());
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !visible.contains("LEAKING HOOK"),
            "external hook was not replaced"
        );
        assert!(
            !visible.contains("backend panic secret"),
            "backend panic payload leaked"
        );
        assert!(
            !visible.contains("unrelated-after-marker"),
            "unrelated panic payload leaked"
        );
        assert!(visible.contains("manager process panic (details redacted)"));
    }

    #[tokio::test]
    async fn manager_hook_replaces_leaking_hook_before_worker_execution() {
        if std::env::var_os("SIDEROSTAT_EXECUTOR_CUSTOM_HOOK_CHILD").is_some() {
            std::panic::set_hook(Box::new(|info| eprintln!("LEAKING PRIOR HOOK: {info}")));
            let (handle, worker, _journal) =
                test_executor(FixtureOutcome::Success { progress: 100 });
            let _ = std::panic::catch_unwind(|| panic!("before-worker-marker"));
            handle.shutdown_for_test();
            worker.await.expect("worker");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "manager::executor::tests::manager_hook_replaces_leaking_hook_before_worker_execution",
                "--nocapture",
            ])
            .env("SIDEROSTAT_EXECUTOR_CUSTOM_HOOK_CHILD", "1")
            .output()
            .expect("child test");
        assert!(
            output.status.success(),
            "child test failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!visible.contains("LEAKING PRIOR HOOK"));
        assert!(!visible.contains("before-worker-marker"));
        assert!(visible.contains("manager process panic (details redacted)"));
    }

    #[tokio::test]
    async fn unrelated_thread_panic_is_redacted_during_backend_call() {
        if std::env::var_os("SIDEROSTAT_EXECUTOR_UNRELATED_PANIC_CHILD").is_some() {
            let started = Arc::new(AtomicBool::new(false));
            let release = Arc::new(AtomicBool::new(false));
            let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
                started: started.clone(),
                release: release.clone(),
            });
            handle
                .submit(fixture_request(
                    &journal,
                    JobKind::Build,
                    "hook-concurrency",
                ))
                .expect("queue");
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !started.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("backend started");
            std::thread::spawn(|| {
                let _ = std::panic::catch_unwind(|| panic!("unrelated-thread-marker"));
            })
            .join()
            .expect("unrelated thread");
            release.store(true, Ordering::SeqCst);
            handle.shutdown_for_test();
            worker.await.expect("worker");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "manager::executor::tests::unrelated_thread_panic_is_redacted_during_backend_call",
                "--nocapture",
            ])
            .env("SIDEROSTAT_EXECUTOR_UNRELATED_PANIC_CHILD", "1")
            .output()
            .expect("child test");
        assert!(output.status.success());
        let visible = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !visible.contains("unrelated-thread-marker"),
            "unrelated panic payload leaked"
        );
        assert!(visible.contains("manager process panic (details redacted)"));
    }

    #[tokio::test]
    async fn mismatched_kind_or_payload_never_reaches_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let journal = Arc::new(Mutex::new(JobJournal::new()));
        let (handle, worker) =
            ManagerExecutor::start(journal.clone(), CountingBackend(calls.clone()));
        for (kind, key) in [(JobKind::Verify, "real-key"), (JobKind::Build, "wrong-key")] {
            let mut request = fixture_request(&journal, JobKind::Build, "real-key");
            request.kind = kind;
            request.payload_key = key.to_string();
            assert_eq!(
                handle.submit(request.clone()),
                Err(ManagerExecutorError::RequestMismatch)
            );
            assert_eq!(
                journal.lock().unwrap().get(&request.id).unwrap().phase,
                JobPhase::Running
            );
            journal
                .lock()
                .unwrap()
                .fail_if_running(&request.id, "request rejected by caller")
                .expect("caller closes rejected job");
        }
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn malformed_duplicate_cannot_fail_accepted_job() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
            started: started.clone(),
            release: release.clone(),
        });
        let request = fixture_request(&journal, JobKind::Build, "accepted-key");
        let id = request.id.clone();
        handle.submit(request.clone()).expect("correct submission");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        let mut malformed = request;
        malformed.payload_key = "wrong-key".to_string();
        let result = handle.submit(malformed);
        let phase_after_mismatch = journal.lock().unwrap().get(&id).unwrap().phase;
        release.store(true, Ordering::SeqCst);
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(result, Err(ManagerExecutorError::RequestMismatch));
        assert_eq!(phase_after_mismatch, JobPhase::Running);
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Succeeded
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_duplicate_submits_execute_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let journal = Arc::new(Mutex::new(JobJournal::new()));
        let (handle, worker) = ManagerExecutor::start(
            journal.clone(),
            BlockingCountingBackend {
                calls: calls.clone(),
                started: started.clone(),
                release: release.clone(),
            },
        );
        let request = fixture_request(&journal, JobKind::Fetch, "same-key");
        handle.submit(request.clone()).expect("first submit");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        let mut submits = Vec::new();
        for _ in 0..16 {
            let handle = handle.clone();
            let request = request.clone();
            submits.push(tokio::spawn(async move { handle.submit(request) }));
        }
        for submit in submits {
            assert_eq!(submit.await.expect("submit task"), Ok(()));
        }
        release.store(true, Ordering::SeqCst);
        handle.shutdown_for_test();
        worker.await.expect("worker");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn full_queue_marks_rejected_job_failed() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
            started: started.clone(),
            release: release.clone(),
        });
        handle
            .submit(fixture_request(&journal, JobKind::Fetch, "active"))
            .expect("active queued");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        for index in 0..16 {
            let request = fixture_request(&journal, JobKind::Build, &format!("waiting-{index}"));
            handle.submit(request).expect("queue slot");
        }
        let rejected = fixture_request(&journal, JobKind::Verify, "rejected");
        let id = rejected.id.clone();
        assert_eq!(
            handle.submit(rejected),
            Err(ManagerExecutorError::QueueFull)
        );
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
        release.store(true, Ordering::SeqCst);
        handle.shutdown_for_test();
        worker.await.expect("worker");
    }

    #[tokio::test]
    async fn aborted_worker_fails_accepted_job() {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (handle, worker, journal) = test_executor(FixtureOutcome::Block {
            started: started.clone(),
            release: release.clone(),
        });
        let request = fixture_request(&journal, JobKind::Build, "active");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        worker.abort();
        assert!(worker.await.is_err());
        release.store(true, Ordering::SeqCst);
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }

    #[tokio::test]
    async fn worker_aborted_before_first_poll_fails_accepted_job() {
        let (handle, worker, journal) = test_executor(FixtureOutcome::Success { progress: 100 });
        let request = fixture_request(&journal, JobKind::Stage, "never-started");
        let id = request.id.clone();
        handle.submit(request).expect("queue");
        worker.abort();
        assert!(worker.await.is_err());
        assert_eq!(
            journal.lock().unwrap().get(&id).unwrap().phase,
            JobPhase::Failed
        );
    }
}
