use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::io::AsyncReadExt;

pub const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributedManifest {
    pub schema_version: u32,
    pub profile: String,
    pub ds4_binary_sha256: String,
    pub compatible_ds4_binary_sha256: Vec<String>,
    pub ds4_source_commit: String,
    pub model_sha256: String,
    pub model_size: u64,
    pub checkpoint: String,
    pub model_family: String,
    pub quantization: String,
    pub context_size: u64,
    pub coordinator_layers: String,
    pub worker_layers: String,
    pub ds4_wire_schema: String,
    pub argv_profile_sha256: String,
}

impl DistributedManifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != DEPLOYMENT_MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema_version));
        }
        for digest in [
            &self.ds4_binary_sha256,
            &self.model_sha256,
            &self.argv_profile_sha256,
        ] {
            validate_sha256(digest)?;
        }
        if self.compatible_ds4_binary_sha256.is_empty()
            || self.compatible_ds4_binary_sha256.len() > 8
            || !self
                .compatible_ds4_binary_sha256
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        {
            return Err(ManifestError::InvalidBinaryCompatibilitySet);
        }
        for digest in &self.compatible_ds4_binary_sha256 {
            validate_sha256(digest)?;
        }
        if self
            .compatible_ds4_binary_sha256
            .binary_search(&self.ds4_binary_sha256)
            .is_err()
        {
            return Err(ManifestError::BinaryNotApproved);
        }
        validate_source_commit(&self.ds4_source_commit)?;
        for value in [
            &self.profile,
            &self.ds4_source_commit,
            &self.checkpoint,
            &self.model_family,
            &self.quantization,
            &self.coordinator_layers,
            &self.worker_layers,
            &self.ds4_wire_schema,
        ] {
            if value.trim().is_empty() {
                return Err(ManifestError::EmptyField);
            }
        }
        if self.model_size == 0 || self.context_size == 0 {
            return Err(ManifestError::ZeroSize);
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        Ok(canonical_json(self)?)
    }

    pub fn deployment_id(&self) -> Result<String, ManifestError> {
        self.validate()?;
        let compatibility = DistributedCompatibilityManifest {
            schema_version: self.schema_version,
            profile: &self.profile,
            compatible_ds4_binary_sha256: &self.compatible_ds4_binary_sha256,
            ds4_source_commit: &self.ds4_source_commit,
            model_sha256: &self.model_sha256,
            model_size: self.model_size,
            checkpoint: &self.checkpoint,
            model_family: &self.model_family,
            quantization: &self.quantization,
            context_size: self.context_size,
            coordinator_layers: &self.coordinator_layers,
            worker_layers: &self.worker_layers,
            ds4_wire_schema: &self.ds4_wire_schema,
            argv_profile_sha256: &self.argv_profile_sha256,
        };
        Ok(lower_hex(&Sha256::digest(canonical_json(&compatibility)?)))
    }

    pub fn compatible_with(&self, other: &Self) -> Result<bool, ManifestError> {
        self.validate()?;
        other.validate()?;
        Ok(self.profile == other.profile
            && self.compatible_ds4_binary_sha256 == other.compatible_ds4_binary_sha256
            && self.ds4_source_commit == other.ds4_source_commit
            && self.model_sha256 == other.model_sha256
            && self.model_size == other.model_size
            && self.checkpoint == other.checkpoint
            && self.model_family == other.model_family
            && self.quantization == other.quantization
            && self.context_size == other.context_size
            && self.coordinator_layers == other.coordinator_layers
            && self.worker_layers == other.worker_layers
            && self.ds4_wire_schema == other.ds4_wire_schema
            && self.argv_profile_sha256 == other.argv_profile_sha256)
    }
}

#[derive(Serialize)]
struct DistributedCompatibilityManifest<'a> {
    schema_version: u32,
    profile: &'a str,
    compatible_ds4_binary_sha256: &'a [String],
    ds4_source_commit: &'a str,
    model_sha256: &'a str,
    model_size: u64,
    checkpoint: &'a str,
    model_family: &'a str,
    quantization: &'a str,
    context_size: u64,
    coordinator_layers: &'a str,
    worker_layers: &'a str,
    ds4_wire_schema: &'a str,
    argv_profile_sha256: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandaloneManifest {
    pub schema_version: u32,
    pub profile: String,
    pub profile_id: String,
    pub ds4_binary_sha256: String,
    pub model_sha256: String,
    pub checkpoint: String,
    pub model_variant: String,
    pub residency: String,
    pub context_size: u64,
    pub argv_profile_sha256: String,
}

impl StandaloneManifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != DEPLOYMENT_MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema_version));
        }
        for digest in [
            &self.ds4_binary_sha256,
            &self.model_sha256,
            &self.argv_profile_sha256,
        ] {
            validate_sha256(digest)?;
        }
        for value in [
            &self.profile,
            &self.profile_id,
            &self.checkpoint,
            &self.model_variant,
            &self.residency,
        ] {
            if value.trim().is_empty() {
                return Err(ManifestError::EmptyField);
            }
        }
        if self.context_size == 0 {
            return Err(ManifestError::ZeroSize);
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        self.validate()?;
        Ok(canonical_json(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileFingerprint {
    pub device_id: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_nanos: u128,
    pub sha256: String,
    pub computed_at_millis: u64,
}

impl FileFingerprint {
    pub async fn is_stale(&self, path: &Path) -> io::Result<bool> {
        let current = fingerprint_metadata(path).await?;
        Ok(self.device_id != current.device_id
            || self.inode != current.inode
            || self.size != current.size
            || self.modified_nanos != current.modified_nanos)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintCacheState {
    Missing,
    Stale(FileFingerprint),
    Fresh(FileFingerprint),
}

#[derive(Debug, Default)]
pub struct FingerprintCache {
    entries: Mutex<HashMap<String, FileFingerprint>>,
}

impl FingerprintCache {
    pub async fn state(&self, profile: &str, path: &Path) -> io::Result<FingerprintCacheState> {
        let cached = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(profile)
            .cloned();
        let Some(cached) = cached else {
            return Ok(FingerprintCacheState::Missing);
        };
        if cached.is_stale(path).await? {
            Ok(FingerprintCacheState::Stale(cached))
        } else {
            Ok(FingerprintCacheState::Fresh(cached))
        }
    }

    fn insert(&self, profile: String, fingerprint: FileFingerprint) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(profile, fingerprint);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintJobStatus {
    Running,
    Complete(FileFingerprint),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintJob {
    pub job_id: String,
    pub profile: String,
    pub status: FingerprintJobStatus,
}

#[derive(Debug, Error)]
pub enum FingerprintJobError {
    #[error("a fingerprint job is already running for profile {0}")]
    AlreadyRunning(String),
}

#[derive(Debug, Default, Clone)]
pub struct FingerprintJobs {
    inner: Arc<FingerprintJobsInner>,
}

#[derive(Debug, Default)]
struct FingerprintJobsInner {
    cache: Arc<FingerprintCache>,
    jobs: Mutex<HashMap<String, FingerprintJob>>,
}

impl FingerprintJobs {
    pub fn cache(&self) -> Arc<FingerprintCache> {
        self.inner.cache.clone()
    }

    pub fn start(
        &self,
        profile: impl Into<String>,
        path: PathBuf,
    ) -> Result<FingerprintJob, FingerprintJobError> {
        let profile = profile.into();
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if jobs
            .get(&profile)
            .is_some_and(|job| job.status == FingerprintJobStatus::Running)
        {
            return Err(FingerprintJobError::AlreadyRunning(profile));
        }
        let job = FingerprintJob {
            job_id: uuid::Uuid::new_v4().to_string(),
            profile: profile.clone(),
            status: FingerprintJobStatus::Running,
        };
        jobs.insert(profile.clone(), job.clone());
        drop(jobs);

        let inner = self.inner.clone();
        let job_id = job.job_id.clone();
        tokio::spawn(async move {
            let result = fingerprint_file(&path).await;
            if let Ok(fingerprint) = &result {
                inner.cache.insert(profile.clone(), fingerprint.clone());
            }
            let status = match result {
                Ok(fingerprint) => FingerprintJobStatus::Complete(fingerprint),
                Err(error) => FingerprintJobStatus::Failed(error.to_string()),
            };
            let mut jobs = inner
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if jobs
                .get(&profile)
                .is_some_and(|current| current.job_id == job_id)
            {
                jobs.insert(
                    profile.clone(),
                    FingerprintJob {
                        job_id,
                        profile,
                        status,
                    },
                );
            }
        });
        Ok(job)
    }

    pub fn get(&self, profile: &str) -> Option<FingerprintJob> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(profile)
            .cloned()
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("unsupported manifest schema version {0}")]
    UnsupportedSchema(u32),
    #[error("manifest SHA-256 fields must contain 64 lowercase hexadecimal characters")]
    InvalidSha256,
    #[error("compatible DS4 binary SHA-256 values must be a sorted, unique list of 1 to 8 digests")]
    InvalidBinaryCompatibilitySet,
    #[error("local DS4 binary SHA-256 is not in the approved compatibility set")]
    BinaryNotApproved,
    #[error("DS4 source commit must be a full lowercase hexadecimal Git object ID")]
    InvalidSourceCommit,
    #[error("manifest string fields must be non-empty")]
    EmptyField,
    #[error("manifest size fields must be positive")]
    ZeroSize,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub async fn fingerprint_file(path: &Path) -> io::Result<FileFingerprint> {
    let initial = fingerprint_metadata(path).await?;
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        tokio::task::yield_now().await;
    }
    let final_metadata = fingerprint_metadata(path).await?;
    if initial != final_metadata {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file metadata changed while fingerprinting",
        ));
    }
    Ok(FileFingerprint {
        device_id: initial.device_id,
        inode: initial.inode,
        size: initial.size,
        modified_nanos: initial.modified_nanos,
        sha256: lower_hex(&digest.finalize()),
        computed_at_millis: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileMetadata {
    device_id: u64,
    inode: u64,
    size: u64,
    modified_nanos: u128,
}

async fn fingerprint_metadata(path: &Path) -> io::Result<FileMetadata> {
    let metadata = tokio::fs::metadata(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let modified_nanos = (metadata.mtime() as i128)
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec() as i128)
            .try_into()
            .unwrap_or_default();
        Ok(FileMetadata {
            device_id: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_nanos,
        })
    }
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    // serde_json's default map is a BTreeMap, so recursively serialized object keys are sorted.
    serde_json::to_vec(&serde_json::to_value(value)?)
}

fn validate_sha256(value: &str) -> Result<(), ManifestError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ManifestError::InvalidSha256)
    }
}

fn validate_source_commit(value: &str) -> Result<(), ManifestError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ManifestError::InvalidSourceCommit)
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn manifest() -> DistributedManifest {
        DistributedManifest {
            schema_version: 2,
            profile: "distributed-mxfp4".into(),
            ds4_binary_sha256: "11".repeat(32),
            compatible_ds4_binary_sha256: vec!["11".repeat(32), "44".repeat(32)],
            ds4_source_commit: "b0309611041655f4e45671cfd9c9886aff161406".into(),
            model_sha256: "22".repeat(32),
            model_size: 100,
            checkpoint: "flash-0731".into(),
            model_family: "deepseek-v4-flash".into(),
            quantization: "mxfp4-experts".into(),
            context_size: 262_144,
            coordinator_layers: "0:19".into(),
            worker_layers: "20:output".into(),
            ds4_wire_schema: "ds4d-v1-hello40".into(),
            argv_profile_sha256: "33".repeat(32),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_and_deployment_changes_only_with_content() {
        let manifest = manifest();
        let json = String::from_utf8(manifest.canonical_json().unwrap()).unwrap();
        let keys = [
            "argv_profile_sha256",
            "checkpoint",
            "compatible_ds4_binary_sha256",
            "context_size",
            "coordinator_layers",
            "ds4_binary_sha256",
            "ds4_source_commit",
            "ds4_wire_schema",
            "model_family",
            "model_sha256",
            "model_size",
            "profile",
            "quantization",
            "schema_version",
            "worker_layers",
        ];
        let positions = keys
            .iter()
            .map(|key| json.find(&format!("\"{key}\"")).unwrap())
            .collect::<Vec<_>>();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            manifest.deployment_id().unwrap(),
            manifest.clone().deployment_id().unwrap()
        );
        let mut different = manifest.clone();
        different.context_size += 1;
        assert_ne!(
            manifest.deployment_id().unwrap(),
            different.deployment_id().unwrap()
        );
        different.context_size -= 1;
        different.ds4_source_commit = "55".repeat(20);
        assert_ne!(
            manifest.deployment_id().unwrap(),
            different.deployment_id().unwrap()
        );
        assert!(!manifest.compatible_with(&different).unwrap());
    }

    #[test]
    fn approved_native_builds_share_one_deployment_but_unknown_builds_fail_closed() {
        let coordinator = manifest();
        let mut worker = coordinator.clone();
        worker.ds4_binary_sha256 = "44".repeat(32);
        assert!(coordinator.compatible_with(&worker).unwrap());
        assert_eq!(
            coordinator.deployment_id().unwrap(),
            worker.deployment_id().unwrap()
        );

        worker.ds4_binary_sha256 = "55".repeat(32);
        assert!(matches!(
            worker.validate(),
            Err(ManifestError::BinaryNotApproved)
        ));

        let mut changed_approval = coordinator.clone();
        changed_approval.compatible_ds4_binary_sha256 = vec!["11".repeat(32), "55".repeat(32)];
        assert_ne!(
            coordinator.deployment_id().unwrap(),
            changed_approval.deployment_id().unwrap()
        );
    }

    #[tokio::test]
    async fn async_fingerprint_cache_detects_file_change() {
        let path = std::env::temp_dir().join(format!("fingerprint-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"first").await.unwrap();
        let fingerprint = fingerprint_file(&path).await.unwrap();
        let cache = FingerprintCache::default();
        cache.insert("distributed".into(), fingerprint.clone());
        assert!(matches!(
            cache.state("distributed", &path).await.unwrap(),
            FingerprintCacheState::Fresh(_)
        ));
        tokio::fs::write(&path, b"second-content").await.unwrap();
        assert!(matches!(
            cache.state("distributed", &path).await.unwrap(),
            FingerprintCacheState::Stale(_)
        ));
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn one_async_job_per_profile_completes_with_digest() {
        let path = std::env::temp_dir().join(format!("fingerprint-job-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, vec![0x5a; 2 * 1024 * 1024])
            .await
            .unwrap();
        let jobs = FingerprintJobs::default();
        let first = jobs.start("distributed", path.clone()).unwrap();
        assert!(matches!(
            jobs.start("distributed", path.clone()),
            Err(FingerprintJobError::AlreadyRunning(_))
        ));
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let job = jobs.get("distributed").unwrap();
                if !matches!(job.status, FingerprintJobStatus::Running) {
                    break job;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed.job_id, first.job_id);
        assert!(matches!(
            completed.status,
            FingerprintJobStatus::Complete(_)
        ));
        tokio::fs::remove_file(path).await.unwrap();
    }
}
