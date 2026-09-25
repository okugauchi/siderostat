//! Durable, versioned DS4 Manager release metadata and immutable artifacts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use super::jobs::{JobPersistence, ManagerJob, PersistenceError};
use super::registry::{ArtifactState, BuildRecord, ManagerRoot, SourceRecord};
use std::sync::{Arc, Mutex};

/// Current on-disk release-store schema.
pub const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE_NAME: &str = "manager-release-store.json";
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;

/// Immutable artifact kind controls its managed directory and generated ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Build,
    Model,
}

impl ArtifactKind {
    fn directory(self, root: &ManagerRoot) -> PathBuf {
        match self {
            Self::Build => root.paths().builds,
            Self::Model => root.paths().models,
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Model => "model",
        }
    }
}

/// Immutable provenance attached before an artifact can enter the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactProvenance {
    Build {
        source_receipt_id: String,
        record: BuildRecord,
    },
    Model {
        catalog_id: String,
    },
}

/// Metadata required to publish bytes; the caller cannot choose an ID or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDraft {
    pub kind: ArtifactKind,
    pub expected_sha256: String,
    pub expected_size: u64,
    pub provenance: ArtifactProvenance,
}

/// Persisted immutable artifact record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedArtifactRecord {
    pub id: String,
    pub kind: ArtifactKind,
    pub rel_path: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub validation_state: ArtifactState,
    pub provenance: ArtifactProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCompatibility {
    Compatible,
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareReadiness {
    Pending,
    Ready,
}

/// Persisted profile references only typed IDs and a fingerprint, never raw paths or argv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedProfileRecord {
    pub profile_id: String,
    pub node_role: String,
    pub role_artifact_ids: Vec<String>,
    pub model_artifact_id: String,
    pub model_catalog_id: String,
    pub config_fingerprint: String,
    pub compatibility: ProfileCompatibility,
    pub hardware_readiness: HardwareReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReleaseIdentity {
    ManagedProfile(String),
    ExternalBaseline {
        config_fingerprint: String,
        executable_sha256: String,
        model_sha256: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasePointers {
    pub active: Option<ReleaseIdentity>,
    pub previous: Option<ReleaseIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedActivationPhase {
    Preparing,
    Draining,
    Starting,
    Ready,
    Committing,
    RollingBack,
    Complete,
    RolledBack,
    ManualIntervention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedParticipantRecord {
    pub node_id: String,
    pub candidate_profile_id: String,
    pub candidate_digest: String,
    pub previous_digest: Option<String>,
    pub phase: PersistedActivationPhase,
    pub prepare_ack: Option<String>,
    pub ready_ack: Option<String>,
    pub commit_ack: Option<String>,
}

/// Activation journal schema intentionally has no runtime lease field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedActivationRecord {
    pub operation_id: String,
    pub expected_generation: u64,
    pub policy_epoch: u64,
    pub phase: PersistedActivationPhase,
    pub participants: BTreeMap<String, PersistedParticipantRecord>,
    pub failure_class: Option<String>,
}

/// All durable Manager metadata for one runtime node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerStoreSnapshot {
    pub schema_version: u32,
    pub node_id: String,
    pub source_receipts: BTreeMap<String, SourceRecord>,
    pub artifacts: BTreeMap<String, PersistedArtifactRecord>,
    pub profiles: BTreeMap<String, StagedProfileRecord>,
    pub release_pointers: ReleasePointers,
    pub jobs: BTreeMap<String, ManagerJob>,
    pub next_job_id: u64,
    pub activation_journals: BTreeMap<String, PersistedActivationRecord>,
}

impl ManagerStoreSnapshot {
    fn new(node_id: String) -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            node_id,
            source_receipts: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            profiles: BTreeMap::new(),
            release_pointers: ReleasePointers::default(),
            jobs: BTreeMap::new(),
            next_job_id: 0,
            activation_journals: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Schema(String),
    UnknownSchema(u32),
    NodeMismatch { expected: String, found: String },
    InvalidDigest,
    DigestMismatch,
    SizeMismatch,
    PathOutsideRoot,
    SymlinkEscape,
    InvalidReference(String),
    InvalidRecord(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(_) => f.write_str("manager store I/O failed"),
            Self::Schema(_) => f.write_str("manager store schema is invalid"),
            Self::UnknownSchema(version) => write!(f, "unsupported manager store schema {version}"),
            Self::NodeMismatch { .. } => f.write_str("manager store belongs to another node"),
            Self::InvalidDigest => f.write_str("manager store digest is invalid"),
            Self::DigestMismatch => f.write_str("manager artifact digest mismatch"),
            Self::SizeMismatch => f.write_str("manager artifact size mismatch"),
            Self::PathOutsideRoot => f.write_str("manager store path escapes its root"),
            Self::SymlinkEscape => f.write_str("manager store path uses a symlink"),
            Self::InvalidReference(_) => f.write_str("manager store reference is invalid"),
            Self::InvalidRecord(_) => f.write_str("manager store record is invalid"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Opened durable store. Mutations publish a candidate index before changing memory.
#[derive(Debug, Clone)]
pub struct ManagerReleaseStore {
    root: ManagerRoot,
    canonical_root: PathBuf,
    snapshot: ManagerStoreSnapshot,
}

impl ManagerReleaseStore {
    pub fn open(root: ManagerRoot, node_id: impl Into<String>) -> Result<Self, StoreError> {
        let node_id = node_id.into();
        if node_id.trim().is_empty() {
            return Err(StoreError::InvalidRecord("empty node id".into()));
        }
        fs::create_dir_all(root.root()).map_err(io_error)?;
        set_private_dir(root.root())?;
        let canonical_root = fs::canonicalize(root.root()).map_err(io_error)?;
        for path in [
            root.paths().sources,
            root.paths().builds,
            root.paths().models,
            root.paths().operations,
            root.paths().logs,
        ] {
            fs::create_dir_all(&path).map_err(io_error)?;
            let canonical = fs::canonicalize(&path).map_err(io_error)?;
            if !canonical.starts_with(&canonical_root) {
                return Err(StoreError::SymlinkEscape);
            }
            set_private_dir(&path)?;
        }

        let mut store = Self {
            root,
            canonical_root,
            snapshot: ManagerStoreSnapshot::new(node_id.clone()),
        };
        let index_path = store.index_path();
        match fs::symlink_metadata(&index_path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(StoreError::SymlinkEscape),
            Ok(meta) => {
                if !meta.is_file() || meta.len() > MAX_STORE_BYTES {
                    return Err(StoreError::Schema("invalid store file".into()));
                }
                let bytes = fs::read(&index_path).map_err(io_error)?;
                let snapshot: ManagerStoreSnapshot = serde_json::from_slice(&bytes)
                    .map_err(|error| StoreError::Schema(error.to_string()))?;
                if snapshot.schema_version != STORE_SCHEMA_VERSION {
                    return Err(StoreError::UnknownSchema(snapshot.schema_version));
                }
                if snapshot.node_id != node_id {
                    return Err(StoreError::NodeMismatch {
                        expected: node_id,
                        found: snapshot.node_id,
                    });
                }
                store.validate_snapshot(&snapshot, true)?;
                store.snapshot = snapshot;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                store.persist_candidate(&store.snapshot.clone())?;
            }
            Err(error) => return Err(io_error(error)),
        }
        Ok(store)
    }

    pub fn snapshot(&self) -> &ManagerStoreSnapshot {
        &self.snapshot
    }

    pub fn official_source_cache_path(&self) -> PathBuf {
        self.root.paths().sources.join("official.git")
    }

    pub fn validate_source_cache_path(&self, path: &Path) -> Result<(), StoreError> {
        let expected = self.official_source_cache_path();
        if path != expected {
            return Err(StoreError::PathOutsideRoot);
        }
        let sources = self.root.paths().sources;
        let canonical_sources = fs::canonicalize(&sources).map_err(io_error)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(StoreError::SymlinkEscape);
                }
                let canonical = fs::canonicalize(path).map_err(io_error)?;
                if !canonical.starts_with(canonical_sources) {
                    return Err(StoreError::PathOutsideRoot);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        Ok(())
    }

    /// Create a fresh build workspace under the managed builds directory.
    /// The path is generated here and is never supplied by a job payload.
    pub fn create_build_workspace(&self) -> Result<PathBuf, StoreError> {
        let builds = self.root.paths().builds;
        self.ensure_managed_directory(&builds)?;
        let workspaces = builds.join("workspaces");
        self.ensure_managed_directory(&workspaces)?;
        for _ in 0..4 {
            let path = workspaces.join(format!("build-{}", uuid::Uuid::new_v4()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    set_private_dir(&path)?;
                    self.validate_build_workspace(&path)?;
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(StoreError::InvalidRecord(
            "build workspace allocation".into(),
        ))
    }

    /// Validate an allocated build workspace immediately before passing it to
    /// Git or a build child process.
    pub fn validate_build_workspace(&self, path: &Path) -> Result<(), StoreError> {
        let workspaces = self.root.paths().builds.join("workspaces");
        if path.parent() != Some(workspaces.as_path())
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("build-"))
        {
            return Err(StoreError::PathOutsideRoot);
        }
        self.validate_managed_directory(&self.root.paths().builds)?;
        self.validate_managed_directory(&workspaces)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(StoreError::SymlinkEscape);
            }
            Ok(_) => {
                let canonical = fs::canonicalize(path).map_err(io_error)?;
                if !canonical.starts_with(&self.canonical_root) {
                    return Err(StoreError::PathOutsideRoot);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::InvalidRecord("build workspace missing".into()));
            }
            Err(error) => return Err(io_error(error)),
        }
        Ok(())
    }

    /// Allocate a private temporary file for a catalog-bound model download.
    /// The random path is generated by the store and is never supplied by API input.
    pub fn create_model_download_part_path(&self) -> Result<PathBuf, StoreError> {
        let operations = self.root.paths().operations;
        self.validate_managed_directory(&operations)?;
        for _ in 0..4 {
            let path = operations.join(format!("model-download-{}.part", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    file.sync_all().map_err(io_error)?;
                    self.validate_model_download_part_path(&path)?;
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(StoreError::InvalidRecord(
            "model download allocation".into(),
        ))
    }

    /// Validate a generated download part immediately before write, read, or removal.
    pub fn validate_model_download_part_path(&self, path: &Path) -> Result<(), StoreError> {
        let operations = self.root.paths().operations;
        if path.parent() != Some(operations.as_path())
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("model-download-") && name.ends_with(".part"))
        {
            return Err(StoreError::PathOutsideRoot);
        }
        self.validate_managed_directory(&operations)?;
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StoreError::SymlinkEscape);
        }
        let canonical = fs::canonicalize(path).map_err(io_error)?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(StoreError::PathOutsideRoot);
        }
        Ok(())
    }

    pub fn remove_model_download_part(&self, path: &Path) -> Result<(), StoreError> {
        self.validate_model_download_part_path(path)?;
        fs::remove_file(path).map_err(io_error)?;
        sync_directory(&self.root.paths().operations)
    }

    /// Rehash a catalog-bound model artifact from managed storage. A mismatch is
    /// durably quarantined; a later successful recheck can restore Verified.
    pub fn verify_model_artifact(
        &mut self,
        artifact_id: &str,
        catalog_id: &str,
        expected_sha256: &str,
        expected_size: u64,
    ) -> Result<(), StoreError> {
        if !full_sha256(expected_sha256) || expected_size == 0 {
            return Err(StoreError::InvalidDigest);
        }
        let record = self
            .snapshot
            .artifacts
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| StoreError::InvalidReference("model artifact".into()))?;
        if record.kind != ArtifactKind::Model
            || record.sha256 != expected_sha256
            || record.size != expected_size
            || !matches!(
                &record.provenance,
                ArtifactProvenance::Model { catalog_id: recorded } if recorded == catalog_id
            )
        {
            return Err(StoreError::InvalidReference(
                "model catalog identity".into(),
            ));
        }
        let path = self.resolve_artifact_path(&record.rel_path)?;
        let (actual_size, actual_digest) = digest_file(&path)?;
        let mut candidate = self.snapshot.clone();
        let artifact = candidate
            .artifacts
            .get_mut(artifact_id)
            .ok_or_else(|| StoreError::InvalidReference("model artifact".into()))?;
        if actual_size != expected_size || actual_digest != expected_sha256 {
            artifact.validation_state = ArtifactState::Quarantined;
            self.commit_candidate(candidate)?;
            return if actual_size != expected_size {
                Err(StoreError::SizeMismatch)
            } else {
                Err(StoreError::DigestMismatch)
            };
        }
        artifact.validation_state = ArtifactState::Verified;
        self.commit_candidate(candidate)
    }

    /// Rehash a managed artifact before Stage or activation consumes it. Any
    /// changed file is durably quarantined so no later profile can trust it.
    pub fn verify_managed_artifact(
        &mut self,
        artifact_id: &str,
        expected_kind: ArtifactKind,
    ) -> Result<PersistedArtifactRecord, StoreError> {
        let record = self
            .snapshot
            .artifacts
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| StoreError::InvalidReference("artifact".into()))?;
        if record.kind != expected_kind {
            return Err(StoreError::InvalidReference("artifact kind".into()));
        }
        let path = self.resolve_artifact_path(&record.rel_path)?;
        let (actual_size, actual_digest) = digest_file(&path)?;
        let mut candidate = self.snapshot.clone();
        let artifact = candidate
            .artifacts
            .get_mut(artifact_id)
            .ok_or_else(|| StoreError::InvalidReference("artifact".into()))?;
        if actual_size != record.size || actual_digest != record.sha256 {
            artifact.validation_state = ArtifactState::Quarantined;
            self.commit_candidate(candidate)?;
            return if actual_size != record.size {
                Err(StoreError::SizeMismatch)
            } else {
                Err(StoreError::DigestMismatch)
            };
        }
        if artifact.validation_state != ArtifactState::Verified {
            artifact.validation_state = ArtifactState::Verified;
            self.commit_candidate(candidate)?;
        }
        self.snapshot
            .artifacts
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| StoreError::InvalidReference("artifact".into()))
    }

    /// Revalidate a managed artifact and return its canonical file path for the runtime owner.
    /// This path is an internal process-construction input and is never projected into the API.
    pub(crate) fn verified_artifact_path(
        &mut self,
        artifact_id: &str,
        expected_kind: ArtifactKind,
    ) -> Result<(PersistedArtifactRecord, PathBuf), StoreError> {
        let artifact = self.verify_managed_artifact(artifact_id, expected_kind)?;
        let path = self.resolve_artifact_path(&artifact.rel_path)?;
        Ok((artifact, path))
    }

    /// Persist one immutable activation intent/phase before the runtime performs its next step.
    pub fn record_activation(
        &mut self,
        record: PersistedActivationRecord,
    ) -> Result<(), StoreError> {
        let operation_id = record.operation_id.clone();
        if let Some(existing) = self.snapshot.activation_journals.get(&operation_id) {
            if existing != &record {
                return Err(StoreError::InvalidReference(
                    "activation operation id collision".into(),
                ));
            }
            return Ok(());
        }
        let mut candidate = self.snapshot.clone();
        candidate.activation_journals.insert(operation_id, record);
        self.commit_candidate(candidate)
    }

    /// Advance an activation journal without allowing operation identity or verified artifact
    /// identity to change. Each phase is durable before the lifecycle owner performs its next
    /// external action.
    pub fn advance_activation(
        &mut self,
        record: PersistedActivationRecord,
    ) -> Result<(), StoreError> {
        let existing = self
            .snapshot
            .activation_journals
            .get(&record.operation_id)
            .cloned()
            .ok_or_else(|| StoreError::InvalidReference("activation operation".into()))?;
        if existing == record {
            return Ok(());
        }
        if !activation_identity_matches(&existing, &record)
            || !activation_transition_allowed(existing.phase, record.phase)
            || record.phase == PersistedActivationPhase::Complete
        {
            return Err(StoreError::InvalidReference(
                "activation phase transition".into(),
            ));
        }
        let mut candidate = self.snapshot.clone();
        candidate
            .activation_journals
            .insert(record.operation_id.clone(), record);
        self.commit_candidate(candidate)
    }

    /// Atomically publish a completed activation journal and its active/previous pointers.
    pub fn finish_activation(
        &mut self,
        record: PersistedActivationRecord,
        active: ReleaseIdentity,
        previous: Option<ReleaseIdentity>,
    ) -> Result<(), StoreError> {
        if record.phase != PersistedActivationPhase::Complete
            || record.participants.is_empty()
            || record.participants.values().any(|participant| {
                participant.phase != PersistedActivationPhase::Complete
                    || participant.commit_ack.is_none()
            })
        {
            return Err(StoreError::InvalidRecord(
                "activation completion lacks participant acknowledgements".into(),
            ));
        }
        let existing = self
            .snapshot
            .activation_journals
            .get(&record.operation_id)
            .ok_or_else(|| StoreError::InvalidReference("activation operation".into()))?;
        if !activation_identity_matches(existing, &record)
            || !activation_transition_allowed(existing.phase, PersistedActivationPhase::Complete)
        {
            return Err(StoreError::InvalidReference(
                "activation completion transition".into(),
            ));
        }
        let mut candidate = self.snapshot.clone();
        candidate
            .activation_journals
            .insert(record.operation_id.clone(), record);
        candidate.release_pointers = ReleasePointers {
            active: Some(active),
            previous,
        };
        let active = candidate
            .release_pointers
            .active
            .as_ref()
            .ok_or_else(|| StoreError::InvalidReference("active release".into()))?;
        self.verify_release_artifacts(&candidate, active)?;
        self.commit_candidate(candidate)
    }

    fn ensure_managed_directory(&self, path: &Path) -> Result<(), StoreError> {
        let root = self.root.root();
        if !path.starts_with(root) {
            return Err(StoreError::PathOutsideRoot);
        }
        if fs::canonicalize(root).map_err(io_error)? != self.canonical_root {
            return Err(StoreError::SymlinkEscape);
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| StoreError::PathOutsideRoot)?;
        let mut cursor = root.to_path_buf();
        self.validate_directory_component(&cursor)?;
        for component in relative.components() {
            match component {
                Component::Normal(part) => cursor.push(part),
                _ => return Err(StoreError::PathOutsideRoot),
            }
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(StoreError::SymlinkEscape);
                }
                Ok(_) => self.validate_directory_component(&cursor)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&cursor).map_err(io_error)?;
                    set_private_dir(&cursor)?;
                    self.validate_directory_component(&cursor)?;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    fn validate_managed_directory(&self, path: &Path) -> Result<(), StoreError> {
        let root = self.root.root();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| StoreError::PathOutsideRoot)?;
        let mut cursor = root.to_path_buf();
        self.validate_directory_component(&cursor)?;
        for component in relative.components() {
            match component {
                Component::Normal(part) => cursor.push(part),
                _ => return Err(StoreError::PathOutsideRoot),
            }
            self.validate_directory_component(&cursor)?;
        }
        Ok(())
    }

    fn validate_directory_component(&self, path: &Path) -> Result<(), StoreError> {
        let metadata = fs::symlink_metadata(path).map_err(io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StoreError::SymlinkEscape);
        }
        let canonical = fs::canonicalize(path).map_err(io_error)?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(StoreError::PathOutsideRoot);
        }
        Ok(())
    }

    pub fn root(&self) -> &ManagerRoot {
        &self.root
    }

    /// Load the durable job set and ID sequence from this store.
    pub fn load_jobs(&self) -> (Vec<ManagerJob>, u64) {
        (
            self.snapshot.jobs.values().cloned().collect(),
            self.snapshot.next_job_id,
        )
    }

    /// Atomically replace jobs and next ID while preserving the rest of the snapshot.
    pub fn save_jobs(
        &mut self,
        records: &[ManagerJob],
        next_job_id: u64,
    ) -> Result<(), StoreError> {
        let mut jobs = BTreeMap::new();
        for job in records {
            if job.id.trim().is_empty() || job.progress > 100 || jobs.contains_key(&job.id) {
                return Err(StoreError::InvalidRecord("job journal".into()));
            }
            jobs.insert(job.id.clone(), job.clone());
        }
        let mut candidate = self.snapshot.clone();
        candidate.jobs = jobs;
        candidate.next_job_id = next_job_id;
        self.commit_candidate(candidate)
    }

    pub fn record_source(&mut self, record: SourceRecord) -> Result<String, StoreError> {
        if !full_git_sha(&record.full_commit)
            || !full_git_sha(&record.main_proof)
            || !valid_source_remote(&record.remote)
        {
            return Err(StoreError::InvalidRecord("source receipt".into()));
        }
        let id = format!("source-{}", record.full_commit);
        let mut candidate = self.snapshot.clone();
        if let Some(existing) = candidate.source_receipts.get(&id) {
            // Receipt identity is commit-pinned. A repeated fetch may happen in
            // another second, so fetched_at does not turn the same immutable
            // provenance into a collision.
            if existing.remote != record.remote
                || existing.full_commit != record.full_commit
                || existing.main_proof != record.main_proof
            {
                return Err(StoreError::InvalidReference("source id collision".into()));
            }
            return Ok(id);
        }
        candidate.source_receipts.insert(id.clone(), record);
        self.commit_candidate(candidate)?;
        Ok(id)
    }

    pub fn publish_artifact(
        &mut self,
        source: &Path,
        draft: ArtifactDraft,
    ) -> Result<PersistedArtifactRecord, StoreError> {
        if !full_sha256(&draft.expected_sha256) || draft.expected_size == 0 {
            return Err(StoreError::InvalidDigest);
        }
        if let ArtifactProvenance::Build { record, .. } = &draft.provenance {
            if !super::build::is_approved_role(&record.role)
                || !super::build::is_approved_target(&record.target)
                || record.target != record.role
            {
                return Err(StoreError::InvalidRecord("build target".into()));
            }
        }
        self.validate_provenance(
            &self.snapshot,
            &draft.kind,
            &draft.provenance,
            &draft.expected_sha256,
        )?;
        let source_meta = fs::symlink_metadata(source).map_err(io_error)?;
        if source_meta.file_type().is_symlink() || !source_meta.is_file() {
            return Err(StoreError::SymlinkEscape);
        }
        let source_canonical = fs::canonicalize(source).map_err(io_error)?;
        if !source_canonical.starts_with(&self.canonical_root) {
            return Err(StoreError::PathOutsideRoot);
        }
        let (source_size, source_digest) = digest_file(&source_canonical)?;
        if source_size != draft.expected_size {
            return Err(StoreError::SizeMismatch);
        }
        if source_digest != draft.expected_sha256 {
            return Err(StoreError::DigestMismatch);
        }

        let id = format!("{}-{}", draft.kind.id_prefix(), draft.expected_sha256);
        let filename = format!("{id}.bin");
        let directory = draft.kind.directory(&self.root);
        let canonical_directory = fs::canonicalize(&directory).map_err(io_error)?;
        if !canonical_directory.starts_with(&self.canonical_root) {
            return Err(StoreError::SymlinkEscape);
        }
        let destination = directory.join(filename);
        let rel_path = destination
            .strip_prefix(self.root.root())
            .map_err(|_| StoreError::PathOutsideRoot)?
            .to_path_buf();

        match fs::symlink_metadata(&destination) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(StoreError::SymlinkEscape);
                }
                let (size, digest) = digest_file(&destination)?;
                if size != draft.expected_size {
                    return Err(StoreError::SizeMismatch);
                }
                if digest != draft.expected_sha256 {
                    return Err(StoreError::DigestMismatch);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.publish_blob(&source_canonical, &destination, &draft)?;
            }
            Err(error) => return Err(io_error(error)),
        }

        let record = PersistedArtifactRecord {
            id: id.clone(),
            kind: draft.kind,
            rel_path,
            sha256: draft.expected_sha256,
            size: draft.expected_size,
            validation_state: ArtifactState::Verified,
            provenance: draft.provenance,
        };
        let mut candidate = self.snapshot.clone();
        if let Some(existing) = candidate.artifacts.get(&id) {
            if existing != &record {
                return Err(StoreError::InvalidReference("artifact id collision".into()));
            }
            return Ok(existing.clone());
        }
        candidate.artifacts.insert(id, record.clone());
        self.commit_candidate(candidate)?;
        Ok(record)
    }

    pub fn record_profile(&mut self, record: StagedProfileRecord) -> Result<String, StoreError> {
        let id = record.profile_id.clone();
        let mut candidate = self.snapshot.clone();
        if let Some(existing) = candidate.profiles.get(&id) {
            if existing != &record {
                return Err(StoreError::InvalidReference("profile id collision".into()));
            }
            return Ok(id);
        }
        candidate.profiles.insert(id.clone(), record);
        self.commit_candidate(candidate)?;
        Ok(id)
    }

    pub fn set_release_pointers(
        &mut self,
        active: ReleaseIdentity,
        previous: Option<ReleaseIdentity>,
    ) -> Result<(), StoreError> {
        let mut candidate = self.snapshot.clone();
        candidate.release_pointers = ReleasePointers {
            active: Some(active),
            previous,
        };
        if let Some(active) = candidate.release_pointers.active.as_ref() {
            self.verify_release_artifacts(&candidate, active)?;
        }
        self.commit_candidate(candidate)
    }

    fn index_path(&self) -> PathBuf {
        self.root.paths().operations.join(STORE_FILE_NAME)
    }

    fn commit_candidate(&mut self, candidate: ManagerStoreSnapshot) -> Result<(), StoreError> {
        self.validate_snapshot(&candidate, false)?;
        self.persist_candidate(&candidate)?;
        self.snapshot = candidate;
        Ok(())
    }

    fn persist_candidate(&self, candidate: &ManagerStoreSnapshot) -> Result<(), StoreError> {
        let bytes =
            serde_json::to_vec(candidate).map_err(|error| StoreError::Schema(error.to_string()))?;
        atomic_write(&self.index_path(), &bytes)?;
        Ok(())
    }

    fn publish_blob(
        &self,
        source: &Path,
        destination: &Path,
        draft: &ArtifactDraft,
    ) -> Result<(), StoreError> {
        let directory = destination.parent().ok_or(StoreError::PathOutsideRoot)?;
        let temp = directory.join(format!(
            ".{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("artifact"),
            uuid::Uuid::new_v4()
        ));
        let mut input = File::open(source).map_err(io_error)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io_error)?;
        let copied = std::io::copy(&mut input, &mut output).map_err(io_error)?;
        output.sync_all().map_err(io_error)?;
        if draft.kind == ArtifactKind::Build {
            set_private_executable(&temp)?;
        } else {
            set_private_file(&temp)?;
        }
        drop(output);
        if copied != draft.expected_size {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::SizeMismatch);
        }
        let (size, digest) = digest_file(&temp)?;
        if size != draft.expected_size {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::SizeMismatch);
        }
        if digest != draft.expected_sha256 {
            let _ = fs::remove_file(&temp);
            return Err(StoreError::DigestMismatch);
        }
        fs::rename(&temp, destination).map_err(io_error)?;
        sync_directory(directory)?;
        Ok(())
    }

    fn validate_provenance(
        &self,
        snapshot: &ManagerStoreSnapshot,
        kind: &ArtifactKind,
        provenance: &ArtifactProvenance,
        sha256: &str,
    ) -> Result<(), StoreError> {
        match (kind, provenance) {
            (
                ArtifactKind::Build,
                ArtifactProvenance::Build {
                    source_receipt_id,
                    record,
                },
            ) => {
                let receipt = snapshot
                    .source_receipts
                    .get(source_receipt_id)
                    .ok_or_else(|| StoreError::InvalidReference("source receipt".into()))?;
                if receipt.full_commit != record.source
                    || record.digest != sha256
                    || !full_sha256(&record.help_digest)
                    || record.role.is_empty()
                    || (!record.target.is_empty()
                        && (!super::build::is_approved_role(&record.role)
                            || !super::build::is_approved_target(&record.target)
                            || record.target != record.role))
                {
                    return Err(StoreError::InvalidRecord("build provenance".into()));
                }
            }
            (ArtifactKind::Model, ArtifactProvenance::Model { catalog_id })
                if !catalog_id.trim().is_empty()
                    && catalog_id.len() <= 128
                    && catalog_id.as_bytes()[0].is_ascii_alphanumeric()
                    && catalog_id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    }) => {}
            _ => return Err(StoreError::InvalidRecord("artifact provenance kind".into())),
        }
        Ok(())
    }

    fn validate_snapshot(
        &self,
        snapshot: &ManagerStoreSnapshot,
        verify_artifact_digests: bool,
    ) -> Result<(), StoreError> {
        if snapshot.schema_version != STORE_SCHEMA_VERSION {
            return Err(StoreError::UnknownSchema(snapshot.schema_version));
        }
        if !valid_record_id(&snapshot.node_id) {
            return Err(StoreError::InvalidRecord("node id".into()));
        }
        for (id, source) in &snapshot.source_receipts {
            if id != &format!("source-{}", source.full_commit)
                || !full_git_sha(&source.full_commit)
                || !full_git_sha(&source.main_proof)
                || !valid_source_remote(&source.remote)
            {
                return Err(StoreError::InvalidRecord("source receipt".into()));
            }
        }
        for (id, artifact) in &snapshot.artifacts {
            if id != &artifact.id
                || !full_sha256(&artifact.sha256)
                || !matches!(
                    artifact.validation_state,
                    ArtifactState::Verified | ArtifactState::Quarantined
                )
                || artifact.id != format!("{}-{}", artifact.kind.id_prefix(), artifact.sha256)
            {
                return Err(StoreError::InvalidRecord("artifact metadata".into()));
            }
            let expected = artifact
                .kind
                .directory(&self.root)
                .join(format!("{id}.bin"));
            let expected_rel = expected
                .strip_prefix(self.root.root())
                .map_err(|_| StoreError::PathOutsideRoot)?;
            validate_relative_path(&artifact.rel_path)?;
            if artifact.rel_path != expected_rel {
                return Err(StoreError::PathOutsideRoot);
            }
            let path = self.resolve_artifact_path(&artifact.rel_path)?;
            let metadata = fs::metadata(&path).map_err(io_error)?;
            if !metadata.is_file() {
                return Err(StoreError::InvalidRecord("artifact is not a file".into()));
            }
            if artifact.validation_state == ArtifactState::Verified
                && metadata.len() != artifact.size
            {
                return Err(StoreError::SizeMismatch);
            }
            if verify_artifact_digests && artifact.validation_state == ArtifactState::Verified {
                let (_, digest) = digest_file(&path)?;
                if digest != artifact.sha256 {
                    return Err(StoreError::DigestMismatch);
                }
            }
            self.validate_provenance(
                snapshot,
                &artifact.kind,
                &artifact.provenance,
                &artifact.sha256,
            )?;
        }
        for (id, profile) in &snapshot.profiles {
            if id != &profile.profile_id
                || !valid_record_id(id)
                || !full_sha256(&profile.config_fingerprint)
                || !matches!(
                    profile.node_role.as_str(),
                    "ds4" | "ds4-server" | "coordinator" | "worker"
                )
                || profile.model_catalog_id.trim().is_empty()
                || profile.role_artifact_ids.is_empty()
            {
                return Err(StoreError::InvalidRecord("staged profile".into()));
            }
            let model = snapshot
                .artifacts
                .get(&profile.model_artifact_id)
                .ok_or_else(|| StoreError::InvalidReference("model artifact".into()))?;
            if model.kind != ArtifactKind::Model
                || model.validation_state != ArtifactState::Verified
                || !matches!(
                    &model.provenance,
                    ArtifactProvenance::Model { catalog_id } if catalog_id == &profile.model_catalog_id
                )
            {
                return Err(StoreError::InvalidReference("model provenance".into()));
            }
            for role_id in &profile.role_artifact_ids {
                let role = snapshot
                    .artifacts
                    .get(role_id)
                    .ok_or_else(|| StoreError::InvalidReference("role artifact".into()))?;
                let expected_role = match profile.node_role.as_str() {
                    "coordinator" | "ds4-server" => "ds4-server",
                    "worker" | "ds4" => "ds4",
                    _ => return Err(StoreError::InvalidRecord("staged profile role".into())),
                };
                if role.kind != ArtifactKind::Build
                    || role.validation_state != ArtifactState::Verified
                    || !matches!(
                        &role.provenance,
                        ArtifactProvenance::Build { record, .. }
                            if record.role == expected_role && record.target == expected_role
                    )
                {
                    return Err(StoreError::InvalidReference("role artifact kind".into()));
                }
            }
        }
        for (id, job) in &snapshot.jobs {
            if id != &job.id || job.progress > 100 {
                return Err(StoreError::InvalidRecord("job journal".into()));
            }
        }
        for (id, journal) in &snapshot.activation_journals {
            if id != &journal.operation_id
                || journal.operation_id.trim().is_empty()
                || journal.expected_generation == 0
            {
                return Err(StoreError::InvalidRecord("activation journal".into()));
            }
            for (node_id, participant) in &journal.participants {
                if node_id != &participant.node_id
                    || !full_sha256(&participant.candidate_digest)
                    || participant
                        .previous_digest
                        .as_deref()
                        .is_some_and(|digest| !full_sha256(digest))
                {
                    return Err(StoreError::InvalidRecord("activation participant".into()));
                }
            }
        }
        for pointer in [
            snapshot.release_pointers.active.as_ref(),
            snapshot.release_pointers.previous.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            self.validate_release_identity(snapshot, pointer)?;
        }
        if snapshot.release_pointers.active.is_some()
            && snapshot.release_pointers.active == snapshot.release_pointers.previous
        {
            return Err(StoreError::InvalidReference(
                "active equals previous".into(),
            ));
        }
        Ok(())
    }

    fn validate_release_identity(
        &self,
        snapshot: &ManagerStoreSnapshot,
        identity: &ReleaseIdentity,
    ) -> Result<(), StoreError> {
        match identity {
            ReleaseIdentity::ManagedProfile(profile_id) => {
                let profile = snapshot
                    .profiles
                    .get(profile_id)
                    .ok_or_else(|| StoreError::InvalidReference("release profile".into()))?;
                if profile.compatibility != ProfileCompatibility::Compatible
                    || profile.hardware_readiness != HardwareReadiness::Ready
                {
                    return Err(StoreError::InvalidReference(
                        "release profile is not activation-ready".into(),
                    ));
                }
            }
            ReleaseIdentity::ExternalBaseline {
                config_fingerprint,
                executable_sha256,
                model_sha256,
            } => {
                if !full_sha256(config_fingerprint)
                    || !full_sha256(executable_sha256)
                    || !full_sha256(model_sha256)
                {
                    return Err(StoreError::InvalidDigest);
                }
            }
        }
        Ok(())
    }

    fn verify_release_artifacts(
        &self,
        snapshot: &ManagerStoreSnapshot,
        identity: &ReleaseIdentity,
    ) -> Result<(), StoreError> {
        let ReleaseIdentity::ManagedProfile(profile_id) = identity else {
            return Ok(());
        };
        let profile = snapshot
            .profiles
            .get(profile_id)
            .ok_or_else(|| StoreError::InvalidReference("release profile".into()))?;
        if profile.compatibility != ProfileCompatibility::Compatible
            || profile.hardware_readiness != HardwareReadiness::Ready
        {
            return Err(StoreError::InvalidReference(
                "release profile is not activation-ready".into(),
            ));
        }
        for artifact_id in profile
            .role_artifact_ids
            .iter()
            .chain(std::iter::once(&profile.model_artifact_id))
        {
            let artifact = snapshot
                .artifacts
                .get(artifact_id)
                .ok_or_else(|| StoreError::InvalidReference("release artifact".into()))?;
            if artifact.validation_state != ArtifactState::Verified {
                return Err(StoreError::InvalidReference(
                    "release artifact is not verified".into(),
                ));
            }
            let path = self.resolve_artifact_path(&artifact.rel_path)?;
            let (size, digest) = digest_file(&path)?;
            if size != artifact.size {
                return Err(StoreError::SizeMismatch);
            }
            if digest != artifact.sha256 {
                return Err(StoreError::DigestMismatch);
            }
        }
        Ok(())
    }

    fn resolve_artifact_path(&self, rel_path: &Path) -> Result<PathBuf, StoreError> {
        validate_relative_path(rel_path)?;
        let mut current = self.root.root().to_path_buf();
        for component in rel_path.components() {
            let Component::Normal(part) = component else {
                return Err(StoreError::PathOutsideRoot);
            };
            current.push(part);
            let metadata = fs::symlink_metadata(&current).map_err(io_error)?;
            if metadata.file_type().is_symlink() {
                return Err(StoreError::SymlinkEscape);
            }
        }
        let canonical = fs::canonicalize(&current).map_err(io_error)?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(StoreError::SymlinkEscape);
        }
        Ok(canonical)
    }
}

fn activation_identity_matches(
    existing: &PersistedActivationRecord,
    next: &PersistedActivationRecord,
) -> bool {
    existing.operation_id == next.operation_id
        && existing.expected_generation == next.expected_generation
        && existing.policy_epoch == next.policy_epoch
        && existing.participants.len() == next.participants.len()
        && existing.participants.iter().all(|(node_id, participant)| {
            next.participants
                .get(node_id)
                .is_some_and(|next_participant| {
                    participant.node_id == next_participant.node_id
                        && participant.candidate_profile_id == next_participant.candidate_profile_id
                        && participant.candidate_digest == next_participant.candidate_digest
                        && participant.previous_digest == next_participant.previous_digest
                        && (participant.phase == next_participant.phase
                            || activation_transition_allowed(
                                participant.phase,
                                next_participant.phase,
                            ))
                        && ack_is_monotonic(
                            participant.prepare_ack.as_deref(),
                            next_participant.prepare_ack.as_deref(),
                        )
                        && ack_is_monotonic(
                            participant.ready_ack.as_deref(),
                            next_participant.ready_ack.as_deref(),
                        )
                        && ack_is_monotonic(
                            participant.commit_ack.as_deref(),
                            next_participant.commit_ack.as_deref(),
                        )
                })
        })
}

fn ack_is_monotonic(existing: Option<&str>, next: Option<&str>) -> bool {
    existing.is_none() || existing == next
}

fn activation_transition_allowed(
    from: PersistedActivationPhase,
    to: PersistedActivationPhase,
) -> bool {
    use PersistedActivationPhase as Phase;
    matches!(
        (from, to),
        (
            Phase::Preparing,
            Phase::Draining | Phase::RollingBack | Phase::ManualIntervention
        ) | (
            Phase::Draining,
            Phase::Starting | Phase::RollingBack | Phase::ManualIntervention
        ) | (
            Phase::Starting,
            Phase::Ready | Phase::RollingBack | Phase::ManualIntervention
        ) | (
            Phase::Ready,
            Phase::Committing | Phase::RollingBack | Phase::ManualIntervention
        ) | (
            Phase::Committing,
            Phase::Complete | Phase::RollingBack | Phase::ManualIntervention
        ) | (
            Phase::RollingBack,
            Phase::RolledBack | Phase::ManualIntervention
        )
    )
}

/// Adapter that persists the JobJournal into the shared release-store snapshot.
/// Callers hold the journal lock before this adapter acquires the store lock.
#[derive(Clone)]
pub struct ManagerJobStorePersistence {
    store: Arc<Mutex<ManagerReleaseStore>>,
}

impl ManagerJobStorePersistence {
    pub fn new(store: Arc<Mutex<ManagerReleaseStore>>) -> Self {
        Self { store }
    }
}

impl JobPersistence for ManagerJobStorePersistence {
    fn load(&self) -> Result<(Vec<ManagerJob>, u64), PersistenceError> {
        self.store
            .lock()
            .map_err(|_| PersistenceError::new("manager store lock poisoned"))
            .map(|store| store.load_jobs())
    }

    fn save(&self, records: &[ManagerJob], next_id: u64) -> Result<(), PersistenceError> {
        self.store
            .lock()
            .map_err(|_| PersistenceError::new("manager store lock poisoned"))?
            .save_jobs(records, next_id)
            .map_err(|error| PersistenceError::new(error.to_string()))
    }
}

fn validate_relative_path(path: &Path) -> Result<(), StoreError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(StoreError::PathOutsideRoot);
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(StoreError::PathOutsideRoot);
        }
    }
    Ok(())
}

fn full_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_record_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn full_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_source_remote(remote: &str) -> bool {
    let Ok(url) = url::Url::parse(remote) else {
        return false;
    };
    let clean = url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    if url.scheme() == "https" {
        return clean && url.host_str().is_some();
    }
    #[cfg(feature = "test-support")]
    if url.scheme() == "file" {
        return clean && url.host().is_none();
    }
    false
}

fn digest_file(path: &Path) -> Result<(u64, String), StoreError> {
    use sha2::Digest;
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = sha2::Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .ok_or(StoreError::SizeMismatch)?;
        hasher.update(&buffer[..count]);
    }
    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((size, digest))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let directory = path.parent().ok_or(StoreError::PathOutsideRoot)?;
    fs::create_dir_all(directory).map_err(io_error)?;
    set_private_dir(directory)?;
    let temp = directory.join(format!(".{STORE_FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        set_private_file(&temp)?;
        drop(file);
        fs::rename(&temp, path).map_err(io_error)?;
        sync_directory(directory)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

fn set_private_dir(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    }
    Ok(())
}

fn set_private_executable(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::registry::{BuildRecord, ManagerRoot, SourceRecord, sha256_hex};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("siderostat-h06-store-{tag}-{nanos}"))
    }

    fn source_record() -> SourceRecord {
        SourceRecord {
            remote: "https://github.com/example/ds4-server.git".into(),
            full_commit: "a".repeat(40),
            main_proof: "a".repeat(40),
            fetched_at: 1_758_795_200,
        }
    }

    fn source_file(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join("ds4/operations").join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("create operations");
        fs::write(&path, bytes).expect("write input");
        path
    }

    fn build_record(source: &SourceRecord) -> BuildRecord {
        BuildRecord {
            source: source.full_commit.clone(),
            flags: "release".into(),
            toolchain: "rustc-test".into(),
            arch: "arm64".into(),
            role: "ds4-server".into(),
            target: "ds4-server".into(),
            digest: HELLO_SHA256.into(),
            help_digest: "b".repeat(64),
        }
    }

    fn publish_test_profile(store: &mut ManagerReleaseStore, root: &Path) -> String {
        let source = source_record();
        let source_id = store.record_source(source.clone()).expect("source receipt");
        let build_path = source_file(root, "build-output", b"hello");
        let build = store
            .publish_artifact(
                &build_path,
                ArtifactDraft {
                    kind: ArtifactKind::Build,
                    expected_sha256: HELLO_SHA256.into(),
                    expected_size: 5,
                    provenance: ArtifactProvenance::Build {
                        source_receipt_id: source_id,
                        record: build_record(&source),
                    },
                },
            )
            .expect("build artifact");
        let model_path = source_file(root, "model-output", b"hello");
        let model = store
            .publish_artifact(
                &model_path,
                ArtifactDraft {
                    kind: ArtifactKind::Model,
                    expected_sha256: HELLO_SHA256.into(),
                    expected_size: 5,
                    provenance: ArtifactProvenance::Model {
                        catalog_id: "model-test-1".into(),
                    },
                },
            )
            .expect("model artifact");
        store
            .record_profile(StagedProfileRecord {
                profile_id: "profile-test-1".into(),
                node_role: "coordinator".into(),
                role_artifact_ids: vec![build.id],
                model_artifact_id: model.id,
                model_catalog_id: "model-test-1".into(),
                config_fingerprint: "c".repeat(64),
                compatibility: ProfileCompatibility::Compatible,
                hardware_readiness: HardwareReadiness::Ready,
            })
            .expect("profile")
    }

    #[test]
    fn release_store_reopens_versioned_records_and_artifacts() {
        let root = root("reopen");
        let manager_root = ManagerRoot::explicit(root.clone());
        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "coordinator").expect("open store");
        let profile_id = publish_test_profile(&mut store, &root);
        let source_id = store
            .record_source(source_record())
            .expect("source receipt");
        store
            .set_release_pointers(
                ReleaseIdentity::ManagedProfile(profile_id.clone()),
                Some(ReleaseIdentity::ExternalBaseline {
                    config_fingerprint: "d".repeat(64),
                    executable_sha256: "e".repeat(64),
                    model_sha256: "f".repeat(64),
                }),
            )
            .expect("set release pointers");
        drop(store);

        let reopened =
            ManagerReleaseStore::open(manager_root, "coordinator").expect("reopen store");
        assert_eq!(reopened.snapshot().schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(reopened.snapshot().node_id, "coordinator");
        assert_eq!(reopened.snapshot().source_receipts.len(), 1);
        assert!(reopened.snapshot().source_receipts.contains_key(&source_id));
        assert_eq!(reopened.snapshot().artifacts.len(), 2);
        assert_eq!(
            reopened.snapshot().profiles[&profile_id].profile_id,
            profile_id
        );
        assert_eq!(
            reopened.snapshot().release_pointers.active,
            Some(ReleaseIdentity::ManagedProfile("profile-test-1".into()))
        );
        assert!(matches!(
            reopened.snapshot().release_pointers.previous,
            Some(ReleaseIdentity::ExternalBaseline { .. })
        ));
    }

    #[test]
    fn activation_intent_and_commit_are_durable_store_transitions() {
        let root_path = root("activation");
        let manager_root = ManagerRoot::explicit(root_path.clone());
        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "local-node").expect("open store");
        let profile_id = publish_test_profile(&mut store, &root_path);
        let previous = ReleaseIdentity::ExternalBaseline {
            config_fingerprint: "d".repeat(64),
            executable_sha256: "e".repeat(64),
            model_sha256: "f".repeat(64),
        };
        let participant = PersistedParticipantRecord {
            node_id: "local-node".into(),
            candidate_profile_id: profile_id.clone(),
            candidate_digest: "a".repeat(64),
            previous_digest: Some("f".repeat(64)),
            phase: PersistedActivationPhase::Preparing,
            prepare_ack: Some("prepared".into()),
            ready_ack: None,
            commit_ack: None,
        };
        let preparing = PersistedActivationRecord {
            operation_id: "activate-1".into(),
            expected_generation: 7,
            policy_epoch: 3,
            phase: PersistedActivationPhase::Preparing,
            participants: BTreeMap::from([("local-node".into(), participant.clone())]),
            failure_class: None,
        };
        store
            .record_activation(preparing)
            .expect("persist activation intent");
        drop(store);

        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "local-node").expect("reopen intent");
        assert_eq!(
            store.snapshot().activation_journals["activate-1"].phase,
            PersistedActivationPhase::Preparing
        );
        for phase in [
            PersistedActivationPhase::Draining,
            PersistedActivationPhase::Starting,
            PersistedActivationPhase::Ready,
            PersistedActivationPhase::Committing,
        ] {
            let mut advancing = store.snapshot().activation_journals["activate-1"].clone();
            advancing.phase = phase;
            let participant = advancing
                .participants
                .get_mut("local-node")
                .expect("local participant");
            participant.phase = phase;
            if phase == PersistedActivationPhase::Ready {
                participant.ready_ack = Some("ready".into());
            }
            store
                .advance_activation(advancing)
                .expect("persist next activation phase");
        }
        let committed = PersistedActivationRecord {
            phase: PersistedActivationPhase::Complete,
            participants: BTreeMap::from([(
                "local-node".into(),
                PersistedParticipantRecord {
                    phase: PersistedActivationPhase::Complete,
                    ready_ack: Some("ready".into()),
                    commit_ack: Some("committed".into()),
                    ..participant
                },
            )]),
            ..store.snapshot().activation_journals["activate-1"].clone()
        };
        store
            .finish_activation(
                committed.clone(),
                ReleaseIdentity::ManagedProfile(profile_id.clone()),
                Some(previous),
            )
            .expect("atomically publish active pointer and completion");
        drop(store);

        let reopened =
            ManagerReleaseStore::open(manager_root, "local-node").expect("reopen committed state");
        assert_eq!(
            reopened.snapshot().activation_journals["activate-1"],
            committed
        );
        assert_eq!(
            reopened.snapshot().release_pointers.active,
            Some(ReleaseIdentity::ManagedProfile(profile_id))
        );
        assert!(reopened.snapshot().release_pointers.previous.is_some());
    }

    #[test]
    fn activation_phase_advances_durably_without_changing_immutable_identity() {
        let root_path = root("activation-advance");
        let mut store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root_path.clone()), "local-node")
                .expect("open store");
        let profile_id = publish_test_profile(&mut store, &root_path);
        let participant = PersistedParticipantRecord {
            node_id: "local-node".into(),
            candidate_profile_id: profile_id,
            candidate_digest: "a".repeat(64),
            previous_digest: None,
            phase: PersistedActivationPhase::Preparing,
            prepare_ack: Some("prepared".into()),
            ready_ack: None,
            commit_ack: None,
        };
        let preparing = PersistedActivationRecord {
            operation_id: "activation-phase-1".into(),
            expected_generation: 4,
            policy_epoch: 2,
            phase: PersistedActivationPhase::Preparing,
            participants: BTreeMap::from([("local-node".into(), participant.clone())]),
            failure_class: None,
        };
        store.record_activation(preparing).expect("record intent");
        let draining = PersistedActivationRecord {
            phase: PersistedActivationPhase::Draining,
            participants: BTreeMap::from([(
                "local-node".into(),
                PersistedParticipantRecord {
                    phase: PersistedActivationPhase::Draining,
                    ..participant.clone()
                },
            )]),
            ..store.snapshot().activation_journals["activation-phase-1"].clone()
        };
        store
            .advance_activation(draining.clone())
            .expect("advance to draining");
        assert_eq!(
            store.snapshot().activation_journals["activation-phase-1"],
            draining
        );

        let mut collision = draining;
        collision
            .participants
            .get_mut("local-node")
            .unwrap()
            .candidate_digest = "b".repeat(64);
        assert!(matches!(
            store.advance_activation(collision),
            Err(StoreError::InvalidReference(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn published_build_artifact_is_executable_and_private() {
        use std::os::unix::fs::PermissionsExt;

        let root_path = root("build-mode");
        let mut store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root_path.clone()), "local-node")
                .expect("open store");
        let profile_id = publish_test_profile(&mut store, &root_path);
        let profile = &store.snapshot().profiles[&profile_id];
        let build = &store.snapshot().artifacts[&profile.role_artifact_ids[0]];
        let path = root_path.join(&build.rel_path);
        let mode = fs::metadata(path)
            .expect("published build metadata")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "build artifact must be executable");
        assert_eq!(mode & 0o077, 0, "build artifact must remain private");
    }

    #[test]
    fn build_workspace_is_managed_and_rejects_symlink_replacement() {
        let root_path = root("build-workspace");
        let outside = root("build-workspace-outside");
        fs::create_dir_all(&outside).expect("create outside");
        let store = ManagerReleaseStore::open(ManagerRoot::explicit(root_path.clone()), "node-a")
            .expect("open store");
        let workspace = store.create_build_workspace().expect("allocate workspace");
        store
            .validate_build_workspace(&workspace)
            .expect("validate managed workspace");
        fs::remove_dir(&workspace).expect("remove workspace directory");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &workspace).expect("replace with symlink");
        assert_eq!(
            store.validate_build_workspace(&workspace),
            Err(StoreError::SymlinkEscape)
        );
        let _ = fs::remove_dir_all(&root_path);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn build_publication_rejects_unapproved_persisted_target() {
        let root_path = root("build-target");
        let mut store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root_path.clone()), "node-a")
                .expect("open store");
        let source = source_record();
        let source_id = store.record_source(source.clone()).expect("source receipt");
        let build_path = source_file(&root_path, "unsafe-target", b"hello");
        let mut record = build_record(&source);
        record.target = "../../bin/sh".into();
        let error = store
            .publish_artifact(
                &build_path,
                ArtifactDraft {
                    kind: ArtifactKind::Build,
                    expected_sha256: HELLO_SHA256.into(),
                    expected_size: 5,
                    provenance: ArtifactProvenance::Build {
                        source_receipt_id: source_id,
                        record,
                    },
                },
            )
            .expect_err("reject unapproved target");
        assert_eq!(error, StoreError::InvalidRecord("build target".into()));
        assert!(store.snapshot().artifacts.is_empty());
        let _ = fs::remove_dir_all(&root_path);
    }

    #[test]
    fn legacy_build_record_without_target_remains_readable() {
        let mut value = serde_json::to_value(build_record(&source_record())).expect("serialize");
        value
            .as_object_mut()
            .expect("build record object")
            .remove("target");
        let record: BuildRecord = serde_json::from_value(value).expect("legacy record");
        assert!(record.target.is_empty());
    }

    #[test]
    fn repeated_source_fetch_keeps_commit_pinned_receipt_identity() {
        let root = root("source-idempotent");
        let mut store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root), "node-a").expect("store");
        let first = source_record();
        let id = store.record_source(first.clone()).expect("first receipt");
        let mut later_fetch = first;
        later_fetch.fetched_at += 1;

        assert_eq!(store.record_source(later_fetch), Ok(id.clone()));
        assert_eq!(store.snapshot().source_receipts.len(), 1);
        assert_eq!(
            store.snapshot().source_receipts[&id].fetched_at,
            1_758_795_200,
            "a stable immutable receipt retains the original fetch timestamp"
        );
    }

    #[test]
    fn source_receipt_rejects_remote_credentials_and_query_secrets() {
        let root = root("source-secret");
        let mut store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root), "node-a").expect("store");
        for remote in [
            "https://user:secret@example.com/ds4.git",
            "https://example.com/ds4.git?access_token=secret",
        ] {
            let record = SourceRecord {
                remote: remote.into(),
                ..source_record()
            };
            assert!(matches!(
                store.record_source(record),
                Err(StoreError::InvalidRecord(_))
            ));
        }
        assert!(store.snapshot().source_receipts.is_empty());
    }

    #[test]
    fn artifact_registry_is_a_projection_of_store_release_pointers() {
        let root = root("registry-projection");
        let manager_root = ManagerRoot::explicit(root.clone());
        let mut store = ManagerReleaseStore::open(manager_root, "coordinator").expect("open store");
        let profile_id = publish_test_profile(&mut store, &root);
        let external = ReleaseIdentity::ExternalBaseline {
            config_fingerprint: "d".repeat(64),
            executable_sha256: "e".repeat(64),
            model_sha256: "f".repeat(64),
        };

        store
            .set_release_pointers(
                ReleaseIdentity::ManagedProfile(profile_id.clone()),
                Some(external.clone()),
            )
            .expect("active profile");
        let active = crate::manager::registry::ArtifactRegistry::from_store(&store);
        assert_eq!(active.list_by_state(ArtifactState::Active).len(), 2);
        assert!(active.list_by_state(ArtifactState::Verified).is_empty());

        store
            .set_release_pointers(external, Some(ReleaseIdentity::ManagedProfile(profile_id)))
            .expect("previous profile");
        let previous = crate::manager::registry::ArtifactRegistry::from_store(&store);
        assert_eq!(previous.list_by_state(ArtifactState::Previous).len(), 2);
        assert!(previous.list_by_state(ArtifactState::Active).is_empty());
    }

    #[test]
    fn release_store_rejects_active_pointer_to_missing_profile() {
        let root = root("missing-profile");
        let manager_root = ManagerRoot::explicit(root);
        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "coordinator").expect("open store");
        let error = store
            .set_release_pointers(
                ReleaseIdentity::ManagedProfile("missing-profile".into()),
                None,
            )
            .expect_err("missing profile cannot become active");
        assert!(matches!(error, StoreError::InvalidReference(_)));
        assert!(store.snapshot().release_pointers.active.is_none());
        drop(store);

        let reopened =
            ManagerReleaseStore::open(manager_root, "coordinator").expect("reopen unchanged store");
        assert!(reopened.snapshot().release_pointers.active.is_none());
    }

    #[test]
    fn metadata_updates_do_not_rehash_artifacts_but_reopen_still_checks_digest() {
        let root = root("metadata-update");
        let manager_root = ManagerRoot::explicit(root.clone());
        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "node-a").expect("open store");
        let profile_id = publish_test_profile(&mut store, &root);
        let artifact = store
            .snapshot()
            .artifacts
            .values()
            .next()
            .expect("artifact")
            .clone();
        fs::write(root.join(artifact.rel_path), b"jello").expect("tamper same size");

        assert!(matches!(
            store.set_release_pointers(ReleaseIdentity::ManagedProfile(profile_id), None),
            Err(StoreError::DigestMismatch)
        ));
        assert!(store.snapshot().release_pointers.active.is_none());

        store.save_jobs(&[], 0).expect("metadata-only update");
        drop(store);
        assert!(matches!(
            ManagerReleaseStore::open(manager_root, "node-a"),
            Err(StoreError::DigestMismatch)
        ));
    }

    #[test]
    fn release_store_rejects_unknown_schema_and_wrong_node() {
        let schema_root = root("schema");
        let schema_manager_root = ManagerRoot::explicit(schema_root.clone());
        let store =
            ManagerReleaseStore::open(schema_manager_root.clone(), "node-a").expect("open store");
        let mut snapshot = store.snapshot().clone();
        drop(store);
        snapshot.schema_version = STORE_SCHEMA_VERSION + 1;
        fs::write(
            schema_root.join("ds4/operations").join(STORE_FILE_NAME),
            serde_json::to_vec(&snapshot).expect("serialize"),
        )
        .expect("write future schema");
        assert!(matches!(
            ManagerReleaseStore::open(schema_manager_root, "node-a"),
            Err(StoreError::UnknownSchema(_))
        ));

        let node_root = root("node");
        let node_manager_root = ManagerRoot::explicit(node_root.clone());
        let mut store = ManagerReleaseStore::open(node_manager_root.clone(), "node-a")
            .expect("open node test store");
        publish_test_profile(&mut store, &node_root);
        drop(store);
        assert!(matches!(
            ManagerReleaseStore::open(node_manager_root, "node-b"),
            Err(StoreError::NodeMismatch { .. })
        ));
    }

    #[test]
    fn release_store_rejects_short_digest_and_does_not_publish_wrong_bytes() {
        let root = root("digest");
        let manager_root = ManagerRoot::explicit(root.clone());
        let mut store =
            ManagerReleaseStore::open(manager_root.clone(), "node-a").expect("open store");
        let path = source_file(&root, "wrong-bytes", b"nope!");
        let short = store.publish_artifact(
            &path,
            ArtifactDraft {
                kind: ArtifactKind::Model,
                expected_sha256: "abc".into(),
                expected_size: 5,
                provenance: ArtifactProvenance::Model {
                    catalog_id: "model-test-1".into(),
                },
            },
        );
        assert!(matches!(short, Err(StoreError::InvalidDigest)));

        let mismatch = store.publish_artifact(
            &path,
            ArtifactDraft {
                kind: ArtifactKind::Model,
                expected_sha256: HELLO_SHA256.into(),
                expected_size: 5,
                provenance: ArtifactProvenance::Model {
                    catalog_id: "model-test-1".into(),
                },
            },
        );
        assert!(matches!(mismatch, Err(StoreError::DigestMismatch)));
        assert!(store.snapshot().artifacts.is_empty());
        drop(store);
        let reopened = ManagerReleaseStore::open(manager_root, "node-a").expect("reopen store");
        assert!(reopened.snapshot().artifacts.is_empty());
    }

    #[test]
    fn release_store_rejects_parent_path_and_symlink_escape_on_reopen() {
        let path_root = root("path");
        let path_manager_root = ManagerRoot::explicit(path_root.clone());
        let mut store =
            ManagerReleaseStore::open(path_manager_root.clone(), "node-a").expect("open store");
        publish_test_profile(&mut store, &path_root);
        let mut snapshot = store.snapshot().clone();
        drop(store);
        snapshot
            .artifacts
            .values_mut()
            .next()
            .expect("artifact")
            .rel_path = PathBuf::from("../outside");
        fs::write(
            path_root.join("ds4/operations").join(STORE_FILE_NAME),
            serde_json::to_vec(&snapshot).expect("serialize"),
        )
        .expect("write escaped path");
        assert!(matches!(
            ManagerReleaseStore::open(path_manager_root, "node-a"),
            Err(StoreError::PathOutsideRoot)
        ));

        let symlink_root = root("symlink");
        let symlink_manager_root = ManagerRoot::explicit(symlink_root.clone());
        let mut store = ManagerReleaseStore::open(symlink_manager_root.clone(), "node-a")
            .expect("open symlink test store");
        publish_test_profile(&mut store, &symlink_root);
        let artifact = store
            .snapshot()
            .artifacts
            .values()
            .next()
            .expect("artifact")
            .clone();
        drop(store);
        let target = symlink_root.join(artifact.rel_path);
        fs::remove_file(&target).expect("remove managed artifact");
        let outside = symlink_root.with_extension("outside");
        fs::write(&outside, b"outside").expect("write outside target");
        std::os::unix::fs::symlink(&outside, &target).expect("create symlink");
        assert!(matches!(
            ManagerReleaseStore::open(symlink_manager_root, "node-a"),
            Err(StoreError::SymlinkEscape)
        ));
    }

    #[test]
    fn unreferenced_content_file_is_not_treated_as_a_trusted_artifact() {
        let root = root("orphan");
        let manager_root = ManagerRoot::explicit(root.clone());
        let store = ManagerReleaseStore::open(manager_root.clone(), "node-a").expect("open store");
        drop(store);
        let orphan = root
            .join("ds4/builds")
            .join(format!("build-{HELLO_SHA256}.bin"));
        fs::create_dir_all(orphan.parent().expect("parent")).expect("create builds");
        fs::write(&orphan, b"hello").expect("write orphan blob");

        let reopened =
            ManagerReleaseStore::open(manager_root, "node-a").expect("open store with orphan blob");
        assert!(reopened.snapshot().artifacts.is_empty());
    }

    #[test]
    fn model_verification_quarantine_survives_reopen_and_can_be_reverified() {
        let root = root("model-quarantine");
        let manager_root = ManagerRoot::explicit(root.clone());
        let mut store = ManagerReleaseStore::open(manager_root.clone(), "node-a").expect("open");
        let input = source_file(&root, "model-input", b"model-bytes");
        let model = store
            .publish_artifact(
                &input,
                ArtifactDraft {
                    kind: ArtifactKind::Model,
                    expected_sha256: sha256_hex(b"model-bytes"),
                    expected_size: 11,
                    provenance: ArtifactProvenance::Model {
                        catalog_id: "model-test-1".into(),
                    },
                },
            )
            .expect("publish model");
        store
            .verify_model_artifact(&model.id, "model-test-1", &model.sha256, model.size)
            .expect("verify model");

        fs::write(root.join(&model.rel_path), b"tampered").expect("tamper model");
        assert!(matches!(
            store.verify_model_artifact(&model.id, "model-test-1", &model.sha256, model.size),
            Err(StoreError::SizeMismatch)
        ));
        assert_eq!(
            store.snapshot().artifacts[&model.id].validation_state,
            ArtifactState::Quarantined
        );
        drop(store);

        let mut reopened =
            ManagerReleaseStore::open(manager_root, "node-a").expect("open quarantined store");
        assert_eq!(
            reopened.snapshot().artifacts[&model.id].validation_state,
            ArtifactState::Quarantined
        );
        fs::write(root.join(&model.rel_path), b"model-bytes").expect("restore verified bytes");
        reopened
            .verify_model_artifact(&model.id, "model-test-1", &model.sha256, model.size)
            .expect("reverify model");
        assert_eq!(
            reopened.snapshot().artifacts[&model.id].validation_state,
            ArtifactState::Verified
        );
    }

    #[test]
    fn model_download_parts_are_generated_private_and_contained() {
        let root = root("model-part");
        let store =
            ManagerReleaseStore::open(ManagerRoot::explicit(root.clone()), "node-a").expect("open");
        let part = store
            .create_model_download_part_path()
            .expect("create managed download part");
        assert!(part.starts_with(root.join("ds4/operations")));
        store
            .validate_model_download_part_path(&part)
            .expect("validate generated path");
        assert_eq!(
            store.validate_model_download_part_path(&root.join("outside.part")),
            Err(StoreError::PathOutsideRoot)
        );
        store
            .remove_model_download_part(&part)
            .expect("remove temporary part");
    }
}
