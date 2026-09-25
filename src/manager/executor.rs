//! Bounded manager job executor and cancellation bridge.

use std::cell::Cell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::manager::catalog;
use crate::manager::jobs::{JobJournal, JobKind, JobPhase, ManagerJobError};
use crate::manager::process::{CommandSpec, GroupRunner};
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
    BuildFromReceipt {
        source_receipt_id: String,
        role: String,
    },
    Download {
        spec: download::DownloadSpec,
        catalog_id: String,
        part_path: PathBuf,
    },
    DownloadFromCatalog {
        catalog_id: String,
    },
    Verify {
        registry: Arc<Mutex<registry::ArtifactRegistry>>,
        artifact_id: String,
        expected_sha256: String,
    },
    VerifyManagedArtifact {
        artifact_id: String,
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
            Self::BuildFromReceipt { .. } => JobKind::Build,
            Self::Download { .. } => JobKind::Download,
            Self::DownloadFromCatalog { .. } => JobKind::Download,
            Self::Verify { .. } => JobKind::Verify,
            Self::VerifyManagedArtifact { .. } => JobKind::Verify,
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

pub const OFFICIAL_DS4_REMOTE: &str = "https://github.com/antirez/ds4.git";
pub const OFFICIAL_FETCH_KEY: &str = "official";
pub const OFFICIAL_DS4_MAIN_REF: &str = "refs/heads/main";

/// Format the only data accepted by a receipt-backed Build request.
pub fn manager_build_payload_key(source_receipt_id: &str, role: &str) -> Option<String> {
    (valid_source_receipt_id(source_receipt_id) && build::is_approved_role(role))
        .then(|| format!("{source_receipt_id}:{role}"))
}

fn parse_manager_build_payload_key(value: &str) -> Option<(String, String)> {
    let (source_receipt_id, role) = value.split_once(':')?;
    if value.matches(':').count() != 1
        || manager_build_payload_key(source_receipt_id, role).as_deref() != Some(value)
    {
        return None;
    }
    Some((source_receipt_id.to_owned(), role.to_owned()))
}

fn valid_source_receipt_id(value: &str) -> bool {
    value.strip_prefix("source-").is_some_and(full_git_commit)
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_catalog_entry(
        &mut self,
        entry: catalog::ModelCatalogEntry,
    ) -> Result<(), ManagerInputError> {
        self.insert_catalog_entry(entry)
    }

    fn insert_catalog_entry(
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

    /// Production Fetch accepts one UI key and always resolves it to the
    /// canonical upstream remote and pinned main branch.
    pub fn official_fetch(cache: PathBuf) -> Self {
        let mut resolver = Self::new();
        resolver
            .register(
                JobKind::Fetch,
                OFFICIAL_FETCH_KEY,
                ManagerJobInput::Fetch {
                    cache,
                    official: source::OfficialRemote::new(OFFICIAL_DS4_REMOTE),
                    remote: OFFICIAL_DS4_REMOTE.into(),
                    revision: "main".into(),
                    main_ref: OFFICIAL_DS4_MAIN_REF.into(),
                },
            )
            .expect("fixed official fetch plan is valid");
        resolver
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
            if request.kind == JobKind::Build {
                if let Some((source_receipt_id, role)) =
                    parse_manager_build_payload_key(&request.payload_key)
                {
                    return Ok(ManagerJobInput::BuildFromReceipt {
                        source_receipt_id,
                        role,
                    });
                }
            }
            if request.kind == JobKind::Download
                && self.model_catalog.contains_key(&request.payload_key)
            {
                return Ok(ManagerJobInput::DownloadFromCatalog {
                    catalog_id: request.payload_key.clone(),
                });
            }
            if request.kind == JobKind::Verify && valid_model_artifact_id(&request.payload_key) {
                return Ok(ManagerJobInput::VerifyManagedArtifact {
                    artifact_id: request.payload_key.clone(),
                });
            }
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
            ManagerJobInput::BuildFromReceipt {
                source_receipt_id,
                role,
            } => {
                if !valid_source_receipt_id(source_receipt_id) || !build::is_approved_role(role) {
                    return Err(ManagerInputError::Rejected);
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
            ManagerJobInput::DownloadFromCatalog { catalog_id } => {
                if !self.model_catalog.contains_key(catalog_id) {
                    return Err(ManagerInputError::Unavailable);
                }
            }
            ManagerJobInput::VerifyManagedArtifact { artifact_id } => {
                if !valid_model_artifact_id(artifact_id) {
                    return Err(ManagerInputError::Rejected);
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

fn valid_model_artifact_id(value: &str) -> bool {
    value.strip_prefix("model-").is_some_and(full_sha256)
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
    manager_store: Option<Arc<Mutex<crate::manager::store::ManagerReleaseStore>>>,
}

impl RuntimeManagerBackend {
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(resolver: ManagerJobInputResolver) -> Self {
        Self::new_runtime(resolver)
    }

    fn new_runtime(resolver: ManagerJobInputResolver) -> Self {
        Self {
            resolver,
            transport: None,
            manager_store: None,
        }
    }

    /// Construct the production source path. The UI key cannot supply a remote,
    /// revision, local path, or command argument.
    pub fn for_release_store(
        store: Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
    ) -> anyhow::Result<Self> {
        let cache = store
            .lock()
            .map_err(|_| anyhow::anyhow!("manager store lock poisoned"))?
            .official_source_cache_path();
        let mut resolver = ManagerJobInputResolver::official_fetch(cache);
        for entry in catalog::bundled_catalog()? {
            resolver
                .insert_catalog_entry(entry)
                .map_err(|_| anyhow::anyhow!("bundled manager catalog is invalid"))?;
        }
        let transport = Arc::new(download::ReqwestHttpTransport::new()?);
        Ok(Self::new_runtime(resolver)
            .with_manager_store_internal(store)
            .with_transport_internal(transport))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_manager_store(
        self,
        store: Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
    ) -> Self {
        self.with_manager_store_internal(store)
    }

    fn with_manager_store_internal(
        mut self,
        store: Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
    ) -> Self {
        self.manager_store = Some(store);
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn without_model_catalog() -> Self {
        Self::new_runtime(ManagerJobInputResolver::new())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_transport(self, transport: Arc<dyn download::HttpTransport + Send + Sync>) -> Self {
        self.with_transport_internal(transport)
    }

    fn with_transport_internal(
        mut self,
        transport: Arc<dyn download::HttpTransport + Send + Sync>,
    ) -> Self {
        self.transport = Some(transport);
        self
    }
}

impl RuntimeManagerBackend {
    fn build_from_receipt(
        &self,
        source_receipt_id: &str,
        role: &str,
        cancel: &AtomicBool,
    ) -> Result<(), ManagerExecutionError> {
        if !valid_source_receipt_id(source_receipt_id) || !build::is_approved_role(role) {
            return Err(ManagerExecutionError::InputRejected(
                "invalid build input".into(),
            ));
        }
        let store = self
            .manager_store
            .as_ref()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let (receipt, cache, workspace) = {
            let store_guard = store
                .lock()
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            let receipt = store_guard
                .snapshot()
                .source_receipts
                .get(source_receipt_id)
                .cloned()
                .ok_or(ManagerExecutionError::Unavailable)?;
            if format!("source-{}", receipt.full_commit) != source_receipt_id
                || !full_git_commit(&receipt.full_commit)
                || !full_git_commit(&receipt.main_proof)
                || !approved_source_remote(&receipt.remote)
            {
                return Err(ManagerExecutionError::Unavailable);
            }
            let cache = store_guard.official_source_cache_path();
            store_guard
                .validate_source_cache_path(&cache)
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            let workspace = store_guard
                .create_build_workspace()
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            (receipt, cache, workspace)
        };

        let result =
            self.build_from_receipt_in_workspace(store, &cache, &workspace, &receipt, role, cancel);
        let cleanup = cleanup_build_workspace(store, &cache, &workspace);
        match cleanup {
            Err(error) => Err(error),
            Ok(()) => result,
        }
    }

    fn build_from_receipt_in_workspace(
        &self,
        store: &Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
        cache: &std::path::Path,
        workspace: &std::path::Path,
        receipt: &registry::SourceRecord,
        role: &str,
        cancel: &AtomicBool,
    ) -> Result<(), ManagerExecutionError> {
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        validate_build_paths(store, cache, workspace)?;
        let cache_text = cache.to_str().ok_or(ManagerExecutionError::Unavailable)?;
        let workspace_text = workspace
            .to_str()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let git = source::GitRunner::default();
        let commit_expr = format!("{}^{{commit}}", receipt.full_commit);
        validate_build_paths(store, cache, workspace)?;
        let commit = git
            .run_cancellable(
                [
                    "--git-dir",
                    cache_text,
                    "rev-parse",
                    "--verify",
                    commit_expr.as_str(),
                ],
                cancel,
            )
            .map_err(map_source_error)?;
        if commit != receipt.full_commit {
            return Err(ManagerExecutionError::Unavailable);
        }
        let main_expr = format!("{}^{{commit}}", receipt.main_proof);
        validate_build_paths(store, cache, workspace)?;
        let main_commit = git
            .run_cancellable(
                [
                    "--git-dir",
                    cache_text,
                    "rev-parse",
                    "--verify",
                    main_expr.as_str(),
                ],
                cancel,
            )
            .map_err(map_source_error)?;
        if main_commit != receipt.main_proof {
            return Err(ManagerExecutionError::Unavailable);
        }
        validate_build_paths(store, cache, workspace)?;
        git.run_cancellable(
            [
                "--git-dir",
                cache_text,
                "merge-base",
                "--is-ancestor",
                receipt.full_commit.as_str(),
                receipt.main_proof.as_str(),
            ],
            cancel,
        )
        .map_err(map_source_error)?;
        validate_build_paths(store, cache, workspace)?;
        git.run_cancellable(
            [
                "--git-dir",
                cache_text,
                "worktree",
                "add",
                "--detach",
                workspace_text,
                receipt.full_commit.as_str(),
            ],
            cancel,
        )
        .map_err(map_source_error)?;
        validate_build_paths(store, cache, workspace)?;
        let head = git
            .run_cancellable(
                [
                    "-C",
                    workspace_text,
                    "rev-parse",
                    "--verify",
                    "HEAD^{commit}",
                ],
                cancel,
            )
            .map_err(map_source_error)?;
        validate_build_paths(store, cache, workspace)?;
        let status = git
            .run_cancellable(
                [
                    "-C",
                    workspace_text,
                    "status",
                    "--porcelain",
                    "--untracked-files=all",
                ],
                cancel,
            )
            .map_err(map_source_error)?;
        if head != receipt.full_commit || !status.is_empty() {
            return Err(ManagerExecutionError::Unavailable);
        }

        validate_build_paths(store, cache, workspace)?;
        let make_version = GroupRunner::new()
            .run_group(
                &CommandSpec::minimal("make", vec!["--version".into()], workspace),
                cancel,
            )
            .map_err(|error| {
                if cancel.load(Ordering::SeqCst) {
                    ManagerExecutionError::Canceled
                } else {
                    ManagerExecutionError::Domain(error.to_string())
                }
            })?;
        let toolchain = make_version
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim();
        if toolchain.is_empty() || toolchain.len() > 256 {
            return Err(ManagerExecutionError::Unavailable);
        }
        let request = build::BuildRequest {
            role: role.into(),
            target: role.into(),
            flags: "default".into(),
            toolchain: toolchain.into(),
            arch: std::env::consts::ARCH.into(),
            source: receipt.full_commit.clone(),
            make_program: "make".into(),
            workspace: workspace.to_path_buf(),
            output_rel: PathBuf::from(role),
            help_rel: PathBuf::from("help.txt"),
            disk_needed: 2 << 30,
        };
        validate_build_paths(store, cache, workspace)?;
        let outcome = build::build_artifacts(&request, cancel).map_err(map_build_error)?;
        if cancel.load(Ordering::SeqCst) {
            return Err(ManagerExecutionError::Canceled);
        }
        let output = workspace.join(&request.output_rel);
        let metadata = std::fs::symlink_metadata(&output)
            .map_err(|_| ManagerExecutionError::Domain("build output unavailable".into()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
            return Err(ManagerExecutionError::Domain(
                "build output unavailable".into(),
            ));
        }
        let draft = crate::manager::store::ArtifactDraft {
            kind: crate::manager::store::ArtifactKind::Build,
            expected_sha256: outcome.record.digest.clone(),
            expected_size: metadata.len(),
            provenance: crate::manager::store::ArtifactProvenance::Build {
                source_receipt_id: format!("source-{}", receipt.full_commit),
                record: outcome.record,
            },
        };
        store
            .lock()
            .map_err(|_| ManagerExecutionError::Unavailable)?
            .publish_artifact(&output, draft)
            .map_err(|_| {
                ManagerExecutionError::Domain("build artifact publication failed".into())
            })?;
        Ok(())
    }
}

fn approved_source_remote(remote: &str) -> bool {
    if remote == OFFICIAL_DS4_REMOTE {
        return true;
    }
    #[cfg(feature = "test-support")]
    {
        url::Url::parse(remote).is_ok_and(|url| url.scheme() == "file" && url.host_str().is_none())
    }
    #[cfg(not(feature = "test-support"))]
    {
        false
    }
}

fn map_build_error(error: build::BuildError) -> ManagerExecutionError {
    match error {
        build::BuildError::Canceled => ManagerExecutionError::Canceled,
        other => ManagerExecutionError::Domain(other.to_string()),
    }
}

fn map_source_error(error: source::SourceError) -> ManagerExecutionError {
    match error {
        source::SourceError::Canceled => ManagerExecutionError::Canceled,
        _ => ManagerExecutionError::Domain("pinned source validation failed".into()),
    }
}

fn validate_build_paths(
    store: &Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
    cache: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<(), ManagerExecutionError> {
    let store_guard = store
        .lock()
        .map_err(|_| ManagerExecutionError::Unavailable)?;
    store_guard
        .validate_source_cache_path(cache)
        .and_then(|_| store_guard.validate_build_workspace(workspace))
        .map_err(|_| ManagerExecutionError::Unavailable)
}

fn cleanup_build_workspace(
    store: &Arc<Mutex<crate::manager::store::ManagerReleaseStore>>,
    cache: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<(), ManagerExecutionError> {
    let store_guard = store
        .lock()
        .map_err(|_| ManagerExecutionError::Unavailable)?;
    store_guard
        .validate_source_cache_path(cache)
        .and_then(|_| store_guard.validate_build_workspace(workspace))
        .map_err(|_| ManagerExecutionError::Unavailable)?;
    drop(store_guard);
    let cache_text = cache.to_str().ok_or(ManagerExecutionError::Unavailable)?;
    let workspace_text = workspace
        .to_str()
        .ok_or(ManagerExecutionError::Unavailable)?;
    let git = source::GitRunner::default();
    // Cleanup is local metadata work and must still run after cancellation.
    let _ = git.run([
        "--git-dir",
        cache_text,
        "worktree",
        "remove",
        "--force",
        workspace_text,
    ]);
    {
        let store_guard = store
            .lock()
            .map_err(|_| ManagerExecutionError::Unavailable)?;
        if std::fs::symlink_metadata(workspace).is_ok() {
            store_guard
                .validate_build_workspace(workspace)
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            std::fs::remove_dir_all(workspace).map_err(|_| {
                ManagerExecutionError::Domain("build workspace cleanup failed".into())
            })?;
        }
    }
    git.run(["--git-dir", cache_text, "worktree", "prune"])
        .map_err(map_source_error)?;
    Ok(())
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
                let store = self
                    .manager_store
                    .as_ref()
                    .ok_or(ManagerExecutionError::Unavailable)?;
                store
                    .lock()
                    .map_err(|_| ManagerExecutionError::Unavailable)?
                    .validate_source_cache_path(&cache)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
                let record = source::stage_source_cancellable(
                    &cache, &official, &remote, &revision, &main_ref, &cancel,
                )
                .map_err(|error| match error {
                    source::SourceError::Canceled => ManagerExecutionError::Canceled,
                    other => ManagerExecutionError::Domain(other.to_string()),
                })?;
                if cancel.load(Ordering::SeqCst) {
                    return Err(ManagerExecutionError::Canceled);
                }
                store
                    .lock()
                    .map_err(|_| ManagerExecutionError::Unavailable)?
                    .record_source(record)
                    .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            }
            ManagerJobInput::Build { request, .. } => {
                build::build_artifacts(&request, &cancel).map_err(map_build_error)?;
            }
            ManagerJobInput::BuildFromReceipt {
                source_receipt_id,
                role,
            } => {
                self.build_from_receipt(&source_receipt_id, &role, &cancel)?;
            }
            ManagerJobInput::Download {
                spec, part_path, ..
            } => {
                let transport = self
                    .transport
                    .as_ref()
                    .ok_or(ManagerExecutionError::Unavailable)?;
                download::download_bounded(&spec, transport.as_ref(), &part_path, None, &cancel)
                    .map_err(map_download_error)?;
            }
            ManagerJobInput::DownloadFromCatalog { catalog_id } => {
                self.download_catalog_model(&catalog_id, &cancel)?;
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
            ManagerJobInput::VerifyManagedArtifact { artifact_id } => {
                self.verify_catalog_model(&artifact_id)?;
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

impl RuntimeManagerBackend {
    fn download_catalog_model(
        &self,
        catalog_id: &str,
        cancel: &AtomicBool,
    ) -> Result<(), ManagerExecutionError> {
        let entry = self
            .resolver
            .model_catalog
            .get(catalog_id)
            .cloned()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let transport = self
            .transport
            .as_ref()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let store = self
            .manager_store
            .as_ref()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let part_path = store
            .lock()
            .map_err(|_| ManagerExecutionError::Unavailable)?
            .create_model_download_part_path()
            .map_err(|_| ManagerExecutionError::Unavailable)?;
        let spec = download::DownloadSpec {
            url: entry.url.clone(),
            expected_size: entry.size,
            sha256: entry.sha256.clone(),
            redirect_allowlist: entry.redirect_allowlist.clone(),
            credentials: None,
        };
        let result = (|| {
            if cancel.load(Ordering::SeqCst) {
                return Err(ManagerExecutionError::Canceled);
            }
            store
                .lock()
                .map_err(|_| ManagerExecutionError::Unavailable)?
                .validate_model_download_part_path(&part_path)
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            download::download_bounded(&spec, transport.as_ref(), &part_path, None, cancel)
                .map_err(map_download_error)?;
            if cancel.load(Ordering::SeqCst) {
                return Err(ManagerExecutionError::Canceled);
            }
            let mut store = store
                .lock()
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            store
                .validate_model_download_part_path(&part_path)
                .map_err(|_| ManagerExecutionError::Unavailable)?;
            store
                .publish_artifact(
                    &part_path,
                    crate::manager::store::ArtifactDraft {
                        kind: crate::manager::store::ArtifactKind::Model,
                        expected_sha256: entry.sha256.clone(),
                        expected_size: entry.size,
                        provenance: crate::manager::store::ArtifactProvenance::Model {
                            catalog_id: entry.catalog_id.clone(),
                        },
                    },
                )
                .map_err(|error| ManagerExecutionError::Domain(error.to_string()))?;
            Ok(())
        })();
        let cleanup = store
            .lock()
            .map_err(|_| ManagerExecutionError::Unavailable)?
            .remove_model_download_part(&part_path)
            .map_err(|_| ManagerExecutionError::Domain("model download cleanup failed".into()));
        cleanup?;
        result
    }

    fn verify_catalog_model(&self, artifact_id: &str) -> Result<(), ManagerExecutionError> {
        if !valid_model_artifact_id(artifact_id) {
            return Err(ManagerExecutionError::InputRejected(
                "invalid model artifact identity".into(),
            ));
        }
        let store = self
            .manager_store
            .as_ref()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let mut store = store
            .lock()
            .map_err(|_| ManagerExecutionError::Unavailable)?;
        let record = store
            .snapshot()
            .artifacts
            .get(artifact_id)
            .cloned()
            .ok_or(ManagerExecutionError::Unavailable)?;
        let crate::manager::store::ArtifactProvenance::Model { catalog_id } = &record.provenance
        else {
            return Err(ManagerExecutionError::InputRejected(
                "artifact is not a catalog model".into(),
            ));
        };
        let entry = self
            .resolver
            .model_catalog
            .get(catalog_id)
            .ok_or(ManagerExecutionError::Unavailable)?;
        verify::verify_stored_model(
            &mut store,
            artifact_id,
            catalog_id,
            &entry.sha256,
            entry.size,
        )
        .map_err(|error| ManagerExecutionError::Domain(error.to_string()))
    }
}

fn map_download_error(error: download::DownloadError) -> ManagerExecutionError {
    match error {
        download::DownloadError::Canceled => ManagerExecutionError::Canceled,
        other => ManagerExecutionError::Domain(other.to_string()),
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
            ManagerJobInput::DownloadFromCatalog { .. }
            | ManagerJobInput::VerifyManagedArtifact { .. } => {
                return Err(ManagerExecutionError::InputRejected(
                    "managed fixture operation is not registered".into(),
                ));
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
    Persistence,
}

impl std::fmt::Display for ManagerExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::QueueFull => "manager executor queue is full",
            Self::QueueClosed => "manager executor queue is closed",
            Self::JobNotFound => "manager job not found",
            Self::RequestMismatch => "manager job request does not match journal",
            Self::Persistence => "manager job storage unavailable",
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
            if phase == JobPhase::Running {
                journal.request_cancel(id).map_err(|error| match error {
                    ManagerJobError::NotFound => ManagerExecutorError::JobNotFound,
                    ManagerJobError::Persistence => ManagerExecutorError::Persistence,
                    ManagerJobError::InvalidTransition => ManagerExecutorError::RequestMismatch,
                    ManagerJobError::UnknownKind | ManagerJobError::IdExhausted => {
                        ManagerExecutorError::RequestMismatch
                    }
                })?;
            }
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
