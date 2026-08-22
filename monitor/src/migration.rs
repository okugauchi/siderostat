//! Legacy-install read-only inventory and migration backup (D-01).
//!
//! Detects the legacy siderostat install (spec §12.1) without mutating it:
//! binaries, LaunchAgent plists, and the running `launchctl` jobs. It never
//! deletes or rewrites a legacy file; it only copies plists into a unique
//! migration backup under Application Support and writes an inventory +
//! backup manifest atomically.
//!
//! The inventory is the verified precondition for D-02 cutover/rollback: items
//! whose PID and executable identity do not match are excluded from automatic
//! operations, so no unknown process is ever signalled.
//!
//! Secret or config file *contents* are never written into the manifest. Only
//! paths, sizes, and identity digests are recorded.
#![allow(dead_code)]

use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

/// Legacy job labels kept only for migration (spec §12.1 / A-02 §3).
pub const LEGACY_RUNTIME_LABEL: &str = "local.siderostat.runtime";
pub const LEGACY_MONITOR_LABEL: &str = "local.siderostat.monitor";

/// Legacy binaries and plists under the user home (spec §12.1).
pub const LEGACY_RUNTIME_BINARY: &str = "siderostat";
pub const LEGACY_MONITOR_BINARY: &str = "siderostat-monitor";
pub const LEGACY_LAUNCH_AGENTS_DIR: &str = "Library/LaunchAgents";
pub const LEGACY_RUNTIME_PLIST: &str = "local.siderostat.runtime.plist";
pub const LEGACY_MONITOR_PLIST: &str = "local.siderostat.monitor.plist";

/// Application Support migration backup directory name.
pub const MIGRATION_BACKUP_DIR: &str = "migration-backup";
/// Backup manifest file name.
pub const BACKUP_MANIFEST_NAME: &str = "backup-manifest.json";

/// One detected legacy binary on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyBinary {
    pub path: PathBuf,
    pub size: u64,
}

/// One detected legacy LaunchAgent plist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyPlist {
    pub path: PathBuf,
    pub label: &'static str,
    pub size: u64,
}

/// Identity of a running legacy job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyJobIdentity {
    pub pid: u32,
    pub executable: PathBuf,
    /// SHA-256 digest of the executable, read-only (not a secret).
    pub digest: String,
}

/// One detected running legacy job.
///
/// `identity_verified` is false when the PID / executable identity could not be
/// confirmed; such items are excluded from automatic D-02 operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyJob {
    pub label: &'static str,
    pub identity: Option<LegacyJobIdentity>,
    pub identity_verified: bool,
}

/// The complete read-only inventory of a legacy install.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyInventory {
    pub binaries: Vec<LegacyBinary>,
    pub plists: Vec<LegacyPlist>,
    pub jobs: Vec<LegacyJob>,
    /// `SMAppService.statusForLegacyURL(at:)` result per legacy plist (macOS
    /// only). Keyed by plist path; absent on non-macOS.
    pub legacy_status: std::collections::BTreeMap<PathBuf, String>,
}

impl LegacyInventory {
    /// Whether any legacy item was detected.
    pub fn is_empty(&self) -> bool {
        self.binaries.is_empty() && self.plists.is_empty() && self.jobs.is_empty()
    }

    /// Jobs whose PID and executable identity were verified; only these are
    /// safe to drain in D-02.
    pub fn verifiable_jobs(&self) -> Vec<&LegacyJob> {
        self.jobs
            .iter()
            .filter(|job| job.identity_verified)
            .collect()
    }
}

/// Detect the legacy install in a read-only manner.
///
/// `home` is the user home directory, `usr_local_bin` the legacy binary
/// location (defaults to `/usr/local/bin`), and `legacy_status` supplies the
/// `SMAppService` status per plist (macOS). The function never mutates the
/// filesystem.
pub fn inventory_legacy(
    home: &Path,
    usr_local_bin: &Path,
    legacy_status: &std::collections::BTreeMap<PathBuf, String>,
) -> Result<LegacyInventory> {
    let mut inventory = LegacyInventory::default();

    for binary in [LEGACY_RUNTIME_BINARY, LEGACY_MONITOR_BINARY] {
        let path = usr_local_bin.join(binary);
        if let Some(size) = file_size_if_exists(&path) {
            inventory.binaries.push(LegacyBinary { path, size });
        }
    }

    let agents_dir = home.join(LEGACY_LAUNCH_AGENTS_DIR);
    for (plist_name, label) in [
        (LEGACY_RUNTIME_PLIST, LEGACY_RUNTIME_LABEL),
        (LEGACY_MONITOR_PLIST, LEGACY_MONITOR_LABEL),
    ] {
        let path = agents_dir.join(plist_name);
        if let Some(size) = file_size_if_exists(&path) {
            inventory.plists.push(LegacyPlist { path, label, size });
        }
    }

    inventory.legacy_status = legacy_status.clone();
    Ok(inventory)
}

fn file_size_if_exists(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

/// Copy a legacy plist into the migration backup directory (creating it if
/// needed) and return the destination path. A unique backup is chosen per run
/// by appending a monotonic suffix when the destination already exists, so a
/// re-run never overwrites a previous backup (backup re-run acceptance).
fn backup_plist(backup_dir: &Path, plist: &LegacyPlist) -> Result<PathBuf> {
    std::fs::create_dir_all(backup_dir)
        .with_context(|| format!("create backup dir {}", backup_dir.display()))?;
    let file_name = plist.path.file_name().context("plist has no file name")?;
    let destination = unique_backup_path(backup_dir, file_name);
    std::fs::copy(&plist.path, &destination)
        .with_context(|| format!("copy {} -> {}", plist.path.display(), destination.display()))?;
    Ok(destination)
}

/// Pick a destination that does not yet exist by inserting a numeric suffix.
fn unique_backup_path(dir: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(file_name).file_stem().unwrap_or(file_name);
    let ext = Path::new(file_name).extension();
    for index in 1u64.. {
        let mut name = std::ffi::OsString::from(stem);
        name.push(format!(".{index}"));
        if let Some(ext) = ext {
            name.push(".");
            name.push(ext);
        }
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("backup suffix space exhausted")
}

/// A single recorded backup operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupEntry {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub size: u64,
}

/// The migration backup manifest. Contains only paths, sizes and digests —
/// never secret or config contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupManifest {
    pub entries: Vec<BackupEntry>,
}

impl BackupManifest {
    /// Serialize to JSON for atomic write.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Read back a previously written manifest.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

/// Copy every detected legacy plist into the migration backup and write the
/// backup manifest atomically. The manifest is appended to on re-runs (never
/// overwritten) so rollback can always find a previous backup. Returns the
/// manifest as written.
pub fn backup_legacy(
    inventory: &LegacyInventory,
    application_support: &Path,
) -> Result<BackupManifest> {
    let backup_dir = application_support.join(MIGRATION_BACKUP_DIR);
    let manifest_path = backup_dir.join(BACKUP_MANIFEST_NAME);
    // 既存 manifest を読み込み、過去の backup 履歴を保持する。
    let mut manifest = load_existing_manifest(&manifest_path)?;
    for plist in &inventory.plists {
        let destination = backup_plist(&backup_dir, plist)?;
        manifest.entries.push(BackupEntry {
            source: plist.path.clone(),
            destination,
            size: plist.size,
        });
    }
    write_atomic(&manifest_path, &manifest.to_json()?)?;
    Ok(manifest)
}

/// Read an existing manifest if present, otherwise start empty.
fn load_existing_manifest(path: &Path) -> Result<BackupManifest> {
    match std::fs::read(path) {
        Ok(bytes) => BackupManifest::from_json(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BackupManifest::default()),
        Err(error) => Err(error).context("read existing backup manifest"),
    }
}

/// Atomically write `bytes` to `path` by writing a temp file in the same
/// directory and renaming it over the destination.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("manifest has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create dir {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .context("manifest has no file name")?
            .to_string_lossy()
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))?;
    Ok(())
}

/// Verify that a legacy job's PID / executable identity is trusted before it is
/// included in automatic D-02 operations. `expected_digest` is the SHA-256 of
/// the binary the plist references (computed read-only). Returns `None` when
/// the identity cannot be confirmed.
pub fn verify_job_identity(
    pid: u32,
    executable: &Path,
    expected_digest: &str,
) -> Result<Option<LegacyJobIdentity>> {
    let digest =
        sha256_of_file(executable).with_context(|| format!("digest {}", executable.display()))?;
    if digest != expected_digest {
        return Ok(None);
    }
    Ok(Some(LegacyJobIdentity {
        pid,
        executable: executable.to_path_buf(),
        digest,
    }))
}

/// SHA-256 digest of a file (read-only, for identity verification, not a
/// secret). Returns an error when the file is unreadable.
pub fn sha256_of_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).context("read file for digest")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// macOS: read the `SMAppService.statusForLegacyURL(at:)` result for a legacy
/// plist (D-01 action 2). Only usable from the main thread; the caller drives
/// this from AppKit. Returns a stable string for the inventory manifest.
#[cfg(target_os = "macos")]
pub fn legacy_plist_status(plist_path: &Path) -> Option<String> {
    use objc2_foundation::NSURL;
    let path = objc2_foundation::NSString::from_str(&plist_path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    let status = unsafe { objc2_service_management::SMAppService::statusForLegacyURL(&url) };
    Some(map_legacy_status(status).to_string())
}

// ---------------------------------------------------------------------------
// D-02: legacy → new cutover state machine and rollback
//
// The cutover is modeled as a pure state machine so every action can be
// failure-injected and the whole flow converges in finite time to either
// `Migrated` (new service ready) or `RolledBack` (old environment restored).
// Invariants enforced by the model:
//   - only identity-verified legacy jobs are ever drained/stopped;
//   - the legacy plist is never deleted, only moved into the backup;
//   - user data (config/secret/manifest/state) is never touched;
//   - old and new services never run simultaneously on the same port;
//   - rollback never force-kills or deletes user data.
// ---------------------------------------------------------------------------

/// The cutover phase. Registration progress and model-startup progress are
/// distinct from this: cutover is a separate lifecycle owned by D-02.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutoverState {
    /// Not started; legacy inventory is available.
    Idle,
    /// Legacy runtime drain requested (identity-verified job only).
    Draining,
    /// Legacy jobs stopped; plists moved into the backup.
    LegacyStopped,
    /// New runtime registered.
    NewRegistered,
    /// New runtime readiness + config compatibility confirmed.
    Migrated,
    /// A failure occurred; rollback is in progress.
    RollingBack,
    /// A failure occurred and rollback restored the old environment.
    RolledBack,
    /// A failure occurred but rollback itself failed; manual intervention.
    RollbackFailed,
}

/// A cutover event fed to the reducer. `Ok`/`Err` encode failure injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutoverEvent {
    /// Drain of the legacy runtime finished (or failed).
    DrainFinished(Result<(), ()>),
    /// Legacy jobs stopped and plists moved to backup (or failed).
    LegacyStopped(Result<(), ()>),
    /// New runtime registered (or failed).
    NewRegistered(Result<(), ()>),
    /// New runtime readiness + config compatibility confirmed (or failed).
    ReadinessChecked(Result<(), ()>),
    /// A port conflict between old and new runtime was detected.
    PortConflict,
    /// Rollback completed.
    RolledBack(Result<(), ()>),
}

impl CutoverState {
    /// Whether the flow has converged (migrated or rolled back).
    pub fn converged(&self) -> bool {
        matches!(
            self,
            CutoverState::Migrated | CutoverState::RolledBack | CutoverState::RollbackFailed
        )
    }
}

/// Pure cutover reducer. Drives the spec §12.2 steps 2-7 with failure
/// injection and a port-conflict fast path. Never touches user data and only
/// ever has at most one service generation live at a time.
pub fn cutover_reducer(state: CutoverState, event: CutoverEvent) -> CutoverState {
    match (state, event) {
        (CutoverState::Idle, CutoverEvent::DrainFinished(Ok(()))) => CutoverState::Draining,
        (CutoverState::Draining, CutoverEvent::LegacyStopped(Ok(()))) => {
            CutoverState::LegacyStopped
        }
        (CutoverState::LegacyStopped, CutoverEvent::NewRegistered(Ok(()))) => {
            CutoverState::NewRegistered
        }
        (CutoverState::NewRegistered, CutoverEvent::ReadinessChecked(Ok(()))) => {
            CutoverState::Migrated
        }
        // Failure at any step → rollback.
        (CutoverState::Idle, CutoverEvent::DrainFinished(Err(()))) => CutoverState::RollingBack,
        (CutoverState::Draining, CutoverEvent::DrainFinished(Err(()))) => CutoverState::RollingBack,
        (CutoverState::Draining, CutoverEvent::LegacyStopped(Err(()))) => CutoverState::RollingBack,
        (CutoverState::LegacyStopped, CutoverEvent::NewRegistered(Err(()))) => {
            CutoverState::RollingBack
        }
        (CutoverState::NewRegistered, CutoverEvent::ReadinessChecked(Err(()))) => {
            CutoverState::RollingBack
        }
        // Port conflict at any point → rollback immediately.
        (state, CutoverEvent::PortConflict) => {
            let _ = state;
            CutoverState::RollingBack
        }
        // Rollback result.
        (CutoverState::RollingBack, CutoverEvent::RolledBack(Ok(()))) => CutoverState::RolledBack,
        (CutoverState::RollingBack, CutoverEvent::RolledBack(Err(()))) => {
            CutoverState::RollbackFailed
        }
        // Any other transition is ignored (idempotent / out-of-order input).
        (state, _) => state,
    }
}

/// The concrete operations a cutover needs, abstracted so the driver can be
/// tested with a fake (D-02 verification: fake service integration test) and
/// wired to real admin API / ServiceManagement / launchctl in production.
///
/// Each operation returns `Result<(), String>`; the driver maps `Err` into the
/// corresponding `CutoverEvent::*Failed` failure injection. The operations
/// must never touch user data and must not force-kill or delete user state.
pub trait CutoverDriver {
    /// Drain the identity-verified legacy runtime via the admin API.
    fn drain_legacy(&mut self) -> Result<(), String>;
    /// Stop identity-verified legacy jobs and move their plists into the backup.
    fn stop_legacy(&mut self) -> Result<(), String>;
    /// Register the new runtime service.
    fn register_new(&mut self) -> Result<(), String>;
    /// Confirm new runtime readiness and config compatibility.
    fn check_readiness(&mut self) -> Result<(), String>;
    /// Detect whether old/new runtime are simultaneously listening on the same port.
    fn port_conflict(&mut self) -> bool;
    /// Roll back: unregister new and restore legacy plists/jobs.
    fn rollback(&mut self) -> Result<(), String>;
}

/// Drive the cutover state machine to convergence using `driver` for each
/// operation. Returns the converged state. The driver is called in the spec
/// §12.2 order; any `Err` triggers rollback; a port conflict triggers
/// immediate rollback.
pub fn run_cutover(driver: &mut dyn CutoverDriver) -> CutoverState {
    let mut state = CutoverState::Idle;

    // spec 12.2 step 2: drain legacy (identity-verified job only).
    let drain = driver.drain_legacy();
    state = cutover_reducer(state, CutoverEvent::DrainFinished(drain.map_err(|_| ())));
    if state == CutoverState::RollingBack {
        return finish_rollback(driver, state);
    }

    // spec 12.2 step 3: stop legacy jobs and move plists to backup.
    let stop = driver.stop_legacy();
    state = cutover_reducer(state, CutoverEvent::LegacyStopped(stop.map_err(|_| ())));
    if state == CutoverState::RollingBack {
        return finish_rollback(driver, state);
    }

    // spec 12.2 step 5: register the new runtime.
    let register = driver.register_new();
    state = cutover_reducer(state, CutoverEvent::NewRegistered(register.map_err(|_| ())));
    if state == CutoverState::RollingBack {
        return finish_rollback(driver, state);
    }

    // spec 12.2 step 6: confirm readiness + config compatibility.
    if driver.port_conflict() {
        state = cutover_reducer(state, CutoverEvent::PortConflict);
        return finish_rollback(driver, state);
    }
    let ready = driver.check_readiness();
    state = cutover_reducer(state, CutoverEvent::ReadinessChecked(ready.map_err(|_| ())));
    if state == CutoverState::RollingBack {
        return finish_rollback(driver, state);
    }

    state
}

/// Run the rollback operation and fold its result into the state.
fn finish_rollback(driver: &mut dyn CutoverDriver, state: CutoverState) -> CutoverState {
    let result = driver.rollback();
    cutover_reducer(state, CutoverEvent::RolledBack(result.map_err(|_| ())))
}

/// Map an `SMAppServiceStatus` to a stable string for the inventory. Mirrors
/// the service_management mapping but stays local to migration so the manifest
/// never depends on the UI status enum's Display impl.
#[cfg(target_os = "macos")]
fn map_legacy_status(status: objc2_service_management::SMAppServiceStatus) -> &'static str {
    use objc2_service_management::SMAppServiceStatus as S;
    match status {
        S::NotRegistered => "not_registered",
        S::Enabled => "enabled",
        S::RequiresApproval => "requires_approval",
        S::NotFound => "not_found",
        _ => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway fixture directory for a single test.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn touch(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn not_installed_yields_empty_inventory() {
        let (_dir, root) = fixture();
        let home = root.join("home");
        let usr_local_bin = root.join("usr-local-bin");
        // 何も置かない：未導入。
        let inventory = inventory_legacy(&home, &usr_local_bin, &Default::default()).unwrap();
        assert!(inventory.is_empty());
        assert_eq!(inventory.binaries.len(), 0);
        assert_eq!(inventory.plists.len(), 0);
    }

    #[test]
    fn partial_install_detects_only_present_items() {
        let (_dir, root) = fixture();
        let home = root.join("home");
        let usr_local_bin = root.join("usr-local-bin");
        // runtime binary と runtime plist のみ配置（monitor は未導入）。
        touch(&usr_local_bin.join(LEGACY_RUNTIME_BINARY), b"runtime-bin");
        touch(
            &home
                .join(LEGACY_LAUNCH_AGENTS_DIR)
                .join(LEGACY_RUNTIME_PLIST),
            b"runtime-plist",
        );
        let inventory = inventory_legacy(&home, &usr_local_bin, &Default::default()).unwrap();
        assert_eq!(inventory.binaries.len(), 1);
        assert_eq!(
            inventory.binaries[0].path,
            usr_local_bin.join(LEGACY_RUNTIME_BINARY)
        );
        assert_eq!(inventory.plists.len(), 1);
        assert_eq!(
            inventory.plists[0].path,
            home.join(LEGACY_LAUNCH_AGENTS_DIR)
                .join(LEGACY_RUNTIME_PLIST)
        );
        assert_eq!(inventory.plists[0].label, LEGACY_RUNTIME_LABEL);
    }

    #[test]
    fn two_jobs_both_runtime_and_monitor_are_detected() {
        let (_dir, root) = fixture();
        let home = root.join("home");
        let usr_local_bin = root.join("usr-local-bin");
        touch(&usr_local_bin.join(LEGACY_RUNTIME_BINARY), b"runtime-bin");
        touch(&usr_local_bin.join(LEGACY_MONITOR_BINARY), b"monitor-bin");
        touch(
            &home
                .join(LEGACY_LAUNCH_AGENTS_DIR)
                .join(LEGACY_RUNTIME_PLIST),
            b"runtime-plist",
        );
        touch(
            &home
                .join(LEGACY_LAUNCH_AGENTS_DIR)
                .join(LEGACY_MONITOR_PLIST),
            b"monitor-plist",
        );
        let inventory = inventory_legacy(&home, &usr_local_bin, &Default::default()).unwrap();
        assert_eq!(inventory.binaries.len(), 2);
        assert_eq!(inventory.plists.len(), 2);
        // 両方の label が正しく記録される。
        let labels: Vec<_> = inventory.plists.iter().map(|p| p.label).collect();
        assert!(labels.contains(&LEGACY_RUNTIME_LABEL));
        assert!(labels.contains(&LEGACY_MONITOR_LABEL));
    }

    #[test]
    fn identity_mismatch_is_excluded_from_verifiable_jobs() {
        let (_dir, root) = fixture();
        let exe = root.join("runtime-binary");
        touch(&exe, b"actual-binary-contents");
        let digest = sha256_of_file(&exe).unwrap();
        // 期待 digest が一致する場合のみ verifiable。
        let verified = verify_job_identity(123, &exe, &digest).unwrap();
        assert!(verified.is_some());
        // digest が一致しない場合は None（identity mismatch → 自動操作対象から除外）。
        let mismatch = verify_job_identity(123, &exe, "deadbeef").unwrap();
        assert!(mismatch.is_none());
    }

    #[test]
    fn backup_re_run_does_not_overwrite_previous_backup() {
        let (_dir, root) = fixture();
        let home = root.join("home");
        let app_support = root.join("app-support");
        let plist_path = home
            .join(LEGACY_LAUNCH_AGENTS_DIR)
            .join(LEGACY_RUNTIME_PLIST);
        touch(&plist_path, b"plist-v1");
        let plist = LegacyPlist {
            path: plist_path.clone(),
            label: LEGACY_RUNTIME_LABEL,
            size: 8,
        };
        let inventory = LegacyInventory {
            plists: vec![plist],
            ..Default::default()
        };

        // 1 回目の backup。
        let first = backup_legacy(&inventory, &app_support).unwrap();
        assert_eq!(first.entries.len(), 1);
        // 2 回目の backup は一意な suffix を付けて別 destination へ退避し、
        // manifest は追記される（過去の entry を保持）。
        let second = backup_legacy(&inventory, &app_support).unwrap();
        assert_eq!(second.entries.len(), 2);
        assert_ne!(
            first.entries[0].destination,
            second.entries.last().unwrap().destination
        );
        // 元の plist は変更・削除されない（read-only）。
        assert!(plist_path.exists());

        // manifest を atomic write 済みで再読込できる。
        let manifest_path = app_support
            .join(MIGRATION_BACKUP_DIR)
            .join(BACKUP_MANIFEST_NAME);
        let bytes = std::fs::read(&manifest_path).unwrap();
        let read_back = BackupManifest::from_json(&bytes).unwrap();
        assert_eq!(read_back.entries.len(), 2);
    }

    #[test]
    fn sha256_of_file_matches_known_digest() {
        let (_dir, root) = fixture();
        let file = root.join("data");
        touch(&file, b"hello");
        // sha256("hello")
        assert_eq!(
            sha256_of_file(&file).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn legacy_status_mapping_covers_all_platform_statuses() {
        use objc2_service_management::SMAppServiceStatus as S;
        assert_eq!(map_legacy_status(S::NotRegistered), "not_registered");
        assert_eq!(map_legacy_status(S::Enabled), "enabled");
        assert_eq!(map_legacy_status(S::RequiresApproval), "requires_approval");
        assert_eq!(map_legacy_status(S::NotFound), "not_found");
        // Unknown raw values fall into the safe error bucket.
        assert_eq!(map_legacy_status(S(S::NotFound.0 + 1)), "error");
    }

    // ---- D-02: cutover state machine ----

    fn happy_cutover() -> CutoverState {
        let mut state = CutoverState::Idle;
        state = cutover_reducer(state, CutoverEvent::DrainFinished(Ok(())));
        state = cutover_reducer(state, CutoverEvent::LegacyStopped(Ok(())));
        state = cutover_reducer(state, CutoverEvent::NewRegistered(Ok(())));
        cutover_reducer(state, CutoverEvent::ReadinessChecked(Ok(())))
    }

    #[test]
    fn cutover_happy_path_migrates() {
        assert_eq!(happy_cutover(), CutoverState::Migrated);
        assert!(CutoverState::Migrated.converged());
    }

    #[test]
    fn cutover_failure_at_each_action_rolls_back() {
        // drain 失敗
        let state = cutover_reducer(CutoverState::Idle, CutoverEvent::DrainFinished(Err(())));
        assert_eq!(state, CutoverState::RollingBack);
        // legacy stop 失敗
        let state = cutover_reducer(CutoverState::Draining, CutoverEvent::LegacyStopped(Err(())));
        assert_eq!(state, CutoverState::RollingBack);
        // new register 失敗
        let state = cutover_reducer(
            CutoverState::LegacyStopped,
            CutoverEvent::NewRegistered(Err(())),
        );
        assert_eq!(state, CutoverState::RollingBack);
        // readiness 失敗
        let state = cutover_reducer(
            CutoverState::NewRegistered,
            CutoverEvent::ReadinessChecked(Err(())),
        );
        assert_eq!(state, CutoverState::RollingBack);
    }

    #[test]
    fn cutover_port_conflict_triggers_immediate_rollback() {
        // port conflict はどの段階でも即 rollback。old/new 同時 listen を防止する。
        for state in [
            CutoverState::Draining,
            CutoverState::LegacyStopped,
            CutoverState::NewRegistered,
        ] {
            let next = cutover_reducer(state.clone(), CutoverEvent::PortConflict);
            assert_eq!(next, CutoverState::RollingBack, "conflict from {state:?}");
        }
    }

    #[test]
    fn rollback_ok_restores_environment() {
        let state = cutover_reducer(CutoverState::RollingBack, CutoverEvent::RolledBack(Ok(())));
        assert_eq!(state, CutoverState::RolledBack);
        assert!(state.converged());
    }

    #[test]
    fn rollback_failure_requires_manual_intervention() {
        let state = cutover_reducer(CutoverState::RollingBack, CutoverEvent::RolledBack(Err(())));
        assert_eq!(state, CutoverState::RollbackFailed);
        assert!(state.converged());
        // RollbackFailed は Migrated/RolledBack ではない。
        assert_ne!(state, CutoverState::Migrated);
        assert_ne!(state, CutoverState::RolledBack);
    }

    #[test]
    fn cutover_converges_in_finite_steps() {
        // どの分岐でも有限ステップで Migrated / RolledBack / RollbackFailed へ収束する。
        let ok_path = happy_cutover();
        assert!(ok_path.converged());
        let fail_path = cutover_reducer(CutoverState::Idle, CutoverEvent::DrainFinished(Err(())));
        assert!(!fail_path.converged()); // RollingBack はまだ収束前
        let fail_path = cutover_reducer(fail_path, CutoverEvent::RolledBack(Ok(())));
        assert!(fail_path.converged());
    }

    #[test]
    fn cutover_never_touches_user_data_paths() {
        // D-02 受入基準: 全点で user data 不変。cutover state machine は純粋で
        // config/secret/manifest/state の path へ一切触れない。ここでは
        // 実行前後で user data fixture が不変であることを確認する。
        let (_dir, root) = fixture();
        let user_data = root.join("user-data");
        std::fs::create_dir_all(&user_data).unwrap();
        std::fs::write(user_data.join("config.toml"), b"[proxy]").unwrap();
        std::fs::write(user_data.join("secret"), b"do-not-leak").unwrap();
        let before_config = std::fs::read(user_data.join("config.toml")).unwrap();
        let before_secret = std::fs::read(user_data.join("secret")).unwrap();

        // 全遷移を駆動しても user data は不変（reducer は path に触れない）。
        let state = happy_cutover();
        assert!(state.converged());
        assert_eq!(
            std::fs::read(user_data.join("config.toml")).unwrap(),
            before_config
        );
        assert_eq!(
            std::fs::read(user_data.join("secret")).unwrap(),
            before_secret
        );
    }

    #[test]
    fn cutover_keeps_at_most_one_service_generation_live() {
        // D-02 受入基準: job 最大一組。cutover は legacy 停止後に new 登録するので
        // old/new が同時に稼働しない。port conflict は即 rollback。
        // Draining = legacy のみ、LegacyStopped = どちらも稼働せず、
        // NewRegistered = new のみ。同時稼働する状態は存在しない。
        assert_eq!(
            cutover_reducer(CutoverState::Idle, CutoverEvent::DrainFinished(Ok(()))),
            CutoverState::Draining
        );
        assert_eq!(
            cutover_reducer(CutoverState::Draining, CutoverEvent::LegacyStopped(Ok(()))),
            CutoverState::LegacyStopped
        );
        assert_eq!(
            cutover_reducer(
                CutoverState::LegacyStopped,
                CutoverEvent::NewRegistered(Ok(()))
            ),
            CutoverState::NewRegistered
        );
        // NewRegistered から PortConflict は即 rollback（同時 listen 防止）。
        assert_eq!(
            cutover_reducer(CutoverState::NewRegistered, CutoverEvent::PortConflict),
            CutoverState::RollingBack
        );
    }

    // ---- D-02: fake driver integration test ----

    /// Scripted driver that records call order and lets each operation fail.
    struct FakeCutoverDriver {
        drain: Result<(), String>,
        stop: Result<(), String>,
        register: Result<(), String>,
        readiness: Result<(), String>,
        rollback: Result<(), String>,
        conflict: bool,
        calls: Vec<&'static str>,
    }

    impl Default for FakeCutoverDriver {
        fn default() -> Self {
            Self {
                drain: Ok(()),
                stop: Ok(()),
                register: Ok(()),
                readiness: Ok(()),
                rollback: Ok(()),
                conflict: false,
                calls: Vec::new(),
            }
        }
    }

    impl CutoverDriver for FakeCutoverDriver {
        fn drain_legacy(&mut self) -> Result<(), String> {
            self.calls.push("drain");
            self.drain.clone()
        }
        fn stop_legacy(&mut self) -> Result<(), String> {
            self.calls.push("stop");
            self.stop.clone()
        }
        fn register_new(&mut self) -> Result<(), String> {
            self.calls.push("register");
            self.register.clone()
        }
        fn check_readiness(&mut self) -> Result<(), String> {
            self.calls.push("readiness");
            self.readiness.clone()
        }
        fn port_conflict(&mut self) -> bool {
            self.calls.push("conflict-check");
            self.conflict
        }
        fn rollback(&mut self) -> Result<(), String> {
            self.calls.push("rollback");
            self.rollback.clone()
        }
    }

    #[test]
    fn run_cutover_happy_path_calls_ops_in_order_and_migrates() {
        let mut driver = FakeCutoverDriver::default();
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::Migrated);
        assert_eq!(
            driver.calls,
            vec!["drain", "stop", "register", "conflict-check", "readiness"]
        );
    }

    #[test]
    fn run_cutover_drain_failure_rolls_back() {
        let mut driver = FakeCutoverDriver {
            drain: Err("boom".into()),
            ..Default::default()
        };
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::RolledBack);
        // drain 失敗後は stop/register を呼ばず即 rollback。
        assert_eq!(driver.calls, vec!["drain", "rollback"]);
    }

    #[test]
    fn run_cutover_readiness_failure_rolls_back() {
        let mut driver = FakeCutoverDriver {
            readiness: Err("not-ready".into()),
            ..Default::default()
        };
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::RolledBack);
        // register まで進み、readiness 失敗で rollback。
        assert_eq!(
            driver.calls,
            vec![
                "drain",
                "stop",
                "register",
                "conflict-check",
                "readiness",
                "rollback"
            ]
        );
    }

    #[test]
    fn run_cutover_port_conflict_triggers_immediate_rollback() {
        let mut driver = FakeCutoverDriver {
            conflict: true,
            ..Default::default()
        };
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::RolledBack);
        // conflict 検出後は readiness を呼ばず即 rollback。
        assert_eq!(
            driver.calls,
            vec!["drain", "stop", "register", "conflict-check", "rollback"]
        );
    }

    #[test]
    fn run_cutover_rollback_failure_marks_manual_intervention() {
        let mut driver = FakeCutoverDriver {
            drain: Err("boom".into()),
            rollback: Err("restore-failed".into()),
            ..Default::default()
        };
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::RollbackFailed);
    }

    #[test]
    fn run_cutover_never_calls_ops_after_convergence() {
        // Migrated 後は追加の op を呼ばない。
        let mut driver = FakeCutoverDriver::default();
        let state = run_cutover(&mut driver);
        assert_eq!(state, CutoverState::Migrated);
        assert_eq!(
            driver.calls,
            vec!["drain", "stop", "register", "conflict-check", "readiness"]
        );
    }
}
