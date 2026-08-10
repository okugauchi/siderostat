use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use thiserror::Error;

pub const PERSISTENT_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistentMode {
    SoloStandalone,
    PairedStandalone,
    DistributedMxfp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistentProxyTarget {
    LocalStandalone,
    Coordinator,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistentFailureCode {
    ChildStart,
    ChildExit,
    ChildIdentity,
    DrainTimeout,
    PeerLost,
    RouteLost,
    StateCorrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentChild {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv_sha256: String,
    pub spawned_at_millis: u64,
    pub process_start_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentClusterState {
    pub schema_version: u32,
    pub generation: u64,
    pub desired_mode: PersistentMode,
    pub last_stable_mode: PersistentMode,
    pub cluster_state: String,
    pub proxy_target: PersistentProxyTarget,
    pub active_profile: Option<String>,
    pub child: Option<PersistentChild>,
    pub last_failure: Option<PersistentFailureCode>,
}

impl PersistentClusterState {
    pub fn validate(&self) -> Result<(), StateStoreError> {
        if self.schema_version != PERSISTENT_STATE_SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchema(self.schema_version));
        }
        if self.cluster_state.trim().is_empty()
            || self
                .active_profile
                .as_ref()
                .is_some_and(|profile| profile.trim().is_empty())
        {
            return Err(StateStoreError::InvalidState(
                "cluster_state and present active_profile must be non-empty",
            ));
        }
        if let Some(child) = &self.child {
            if !child.executable.is_absolute() {
                return Err(StateStoreError::InvalidState(
                    "child executable must be absolute",
                ));
            }
            if child.argv_sha256.len() != 64
                || !child
                    .argv_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(StateStoreError::InvalidState(
                    "child argv_sha256 must contain 64 hexadecimal characters",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum StateStoreError {
    #[error("another siderostat instance holds {0}")]
    Locked(PathBuf),
    #[error("unsupported persistent state schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid persistent state: {0}")]
    InvalidState(&'static str),
    #[error("refusing stale generation {attempted}; current generation is {current}")]
    StaleGeneration { attempted: u64, current: u64 },
    #[error("corrupt persistent state was preserved at {path}: {reason}")]
    CorruptPreserved { path: PathBuf, reason: String },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct StateStore {
    path: PathBuf,
    _lock: File,
    last_generation: Mutex<Option<u64>>,
}

impl StateStore {
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, StateStoreError> {
        let path = path.into();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let lock_path = sibling_path(&path, "lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        set_private_permissions(&lock)?;
        if !try_lock_exclusive(&lock)? {
            return Err(StateStoreError::Locked(lock_path));
        }
        Ok(Self {
            path,
            _lock: lock,
            last_generation: Mutex::new(None),
        })
    }

    pub fn load(&self) -> Result<Option<PersistentClusterState>, StateStoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let state: PersistentClusterState = match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(error) => return Err(self.preserve_corrupt(error.to_string())?),
        };
        if let Err(error) = state.validate() {
            return Err(self.preserve_corrupt(error.to_string())?);
        }
        *self
            .last_generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(state.generation);
        Ok(Some(state))
    }

    pub fn save(&self, state: &PersistentClusterState) -> Result<(), StateStoreError> {
        state.validate()?;
        let mut last_generation = self
            .last_generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = *last_generation
            && state.generation < current
        {
            return Err(StateStoreError::StaleGeneration {
                attempted: state.generation,
                current,
            });
        }
        let parent = self.path.parent().expect("validated state parent");
        let temporary = sibling_path(&self.path, "tmp");
        let bytes = serde_json::to_vec_pretty(state)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        set_private_permissions(&file)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        File::open(parent)?.sync_all()?;
        *last_generation = Some(state.generation);
        Ok(())
    }

    fn preserve_corrupt(&self, reason: String) -> Result<StateStoreError, io::Error> {
        let preserved = sibling_path(&self.path, &format!("corrupt-{}", uuid::Uuid::new_v4()));
        fs::rename(&self.path, &preserved)?;
        if let Some(parent) = self.path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(StateStoreError::CorruptPreserved {
            path: preserved,
            reason,
        })
    }
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cluster-state.json");
    path.with_file_name(format!(".{name}.{suffix}"))
}

#[cfg(unix)]
fn set_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    // SAFETY: flock only reads the valid owned file descriptor and has no pointer arguments.
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_state_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("ds4-state-test-{}", uuid::Uuid::new_v4()))
            .join("cluster-state.json")
    }

    fn state(generation: u64) -> PersistentClusterState {
        PersistentClusterState {
            schema_version: PERSISTENT_STATE_SCHEMA_VERSION,
            generation,
            desired_mode: PersistentMode::SoloStandalone,
            last_stable_mode: PersistentMode::SoloStandalone,
            cluster_state: "solo-standalone-ready".into(),
            proxy_target: PersistentProxyTarget::LocalStandalone,
            active_profile: Some("q2-resident".into()),
            child: Some(PersistentChild {
                pid: 42,
                executable: PathBuf::from("/opt/ds4/ds4-server"),
                argv_sha256: "ab".repeat(32),
                spawned_at_millis: 100,
                process_start_micros: 200,
            }),
            last_failure: None,
        }
    }

    #[test]
    fn atomic_save_ignores_partial_temp_and_rejects_old_generation() {
        let path = temporary_state_path();
        let store = StateStore::acquire(&path).unwrap();
        store.save(&state(8)).unwrap();
        fs::write(sibling_path(&path, "tmp"), b"{partial").unwrap();
        assert_eq!(store.load().unwrap().unwrap(), state(8));
        assert!(matches!(
            store.save(&state(7)),
            Err(StateStoreError::StaleGeneration {
                attempted: 7,
                current: 8
            })
        ));
        assert_eq!(store.load().unwrap().unwrap(), state(8));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_json_is_preserved_and_not_treated_as_state() {
        let path = temporary_state_path();
        let store = StateStore::acquire(&path).unwrap();
        fs::write(&path, b"{not-json").unwrap();
        let preserved = match store.load().unwrap_err() {
            StateStoreError::CorruptPreserved { path, .. } => path,
            error => panic!("unexpected error: {error}"),
        };
        assert!(!path.exists());
        assert_eq!(fs::read(preserved).unwrap(), b"{not-json");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn lock_is_exclusive_and_json_has_no_secret_or_token_field() {
        let path = temporary_state_path();
        let store = StateStore::acquire(&path).unwrap();
        assert!(matches!(
            StateStore::acquire(&path),
            Err(StateStoreError::Locked(_))
        ));
        store.save(&state(1)).unwrap();
        let json = fs::read_to_string(&path).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
        drop(store);
        assert!(StateStore::acquire(&path).is_ok());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
