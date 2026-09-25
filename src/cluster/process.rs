use super::{
    ChildLogForwarders, ChildLogRecord, Ds4Command, Ds4LogEvent, spawn_child_log_forwarders,
    spawn_child_log_forwarders_with_events,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    process::{Child, ChildStderr, ChildStdout},
    sync::{RwLock, mpsc},
    time::Instant,
};
use url::Url;

/// The bounded window used to reap a child after SIGKILL.
pub(crate) const SIGKILL_REAP_WINDOW: Duration = Duration::from_secs(5);

pub(crate) fn sigkill_reap_window(stop_timeout: Duration) -> Duration {
    stop_timeout.min(SIGKILL_REAP_WINDOW)
}

mod coordinator;
mod standalone;
mod worker;

pub use coordinator::DistributedCoordinatorSupervisor;
pub use standalone::StandaloneSupervisor;
pub use worker::{DistributedWorkerSupervisor, TpWorkerSupervisor};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProcess {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv: Vec<OsString>,
    pub start_time_micros: u64,
}

/// Process identity used for an authorized startup cleanup.
///
/// This deliberately omits the logical child metadata (profile/generation). A process that was
/// started by an older siderostat, or by another launcher, cannot have that metadata, but it can
/// still be safely targeted when startup cleanup is authorized and the OS-level identity is
/// re-verified immediately before every signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv_sha256: [u8; 32],
    pub process_start_micros: u64,
}

impl ProcessIdentity {
    pub fn from_observed(observed: &ObservedProcess) -> Self {
        Self {
            pid: observed.pid,
            executable: observed.executable.clone(),
            argv_sha256: argv_sha256(observed.executable.as_os_str(), &observed.argv),
            process_start_micros: observed.start_time_micros,
        }
    }

    fn matches(&self, observed: &ObservedProcess) -> bool {
        self.pid == observed.pid
            && self.executable == observed.executable
            && self.argv_sha256 == argv_sha256(observed.executable.as_os_str(), &observed.argv)
            && self.process_start_micros == observed.start_time_micros
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupProcessKind {
    Siderostat,
    Ds4,
}

impl StartupProcessKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Siderostat => "siderostat",
            Self::Ds4 => "ds4-server",
        }
    }
}

/// A locally running process that can conflict with this siderostat instance at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupProcessCandidate {
    pub kind: StartupProcessKind,
    pub observed: ObservedProcess,
}

impl StartupProcessCandidate {
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity::from_observed(&self.observed)
    }

    pub fn command_line(&self) -> String {
        let mut values = Vec::with_capacity(self.observed.argv.len() + 1);
        values.push(self.observed.executable.display().to_string());
        values.extend(
            self.observed
                .argv
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        );
        values.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildIdentity {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv_sha256: [u8; 32],
    pub profile_id: String,
    pub generation: u64,
    pub spawned_at_millis: u64,
    pub process_start_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ds4CommandRole {
    Standalone,
    Coordinator,
    Worker,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum CommandSlotError {
    #[error("manager command does not match the currently validated configuration")]
    ConfigFingerprintMismatch,
    #[error("manager command contains an invalid digest")]
    InvalidDigest,
    #[error("manager command references a path outside managed storage")]
    PathOutsideManagedRoot,
    #[error("manager command artifact is unavailable")]
    ArtifactUnavailable,
    #[error("manager executable is not executable")]
    ArtifactNotExecutable,
    #[error("manager command artifact digest changed")]
    ArtifactDigestMismatch,
    #[error("manager command role is incompatible with this supervisor")]
    RoleMismatch,
    #[error("manager command does not identify exactly one model artifact")]
    ModelArgumentInvalid,
    #[error("a supervised child is running")]
    ChildRunning,
    #[error("no previous supervisor command is available")]
    PreviousCommandUnavailable,
}

/// A command produced from a staged profile and the current validated config. It contains only
/// server-generated argv plus full file digests; the supervisor rechecks the files immediately
/// before every child start.
#[derive(Clone)]
pub struct VerifiedDs4Command {
    command: crate::cluster::Ds4Command,
    managed_root: PathBuf,
    config_fingerprint: String,
    executable_sha256: [u8; 32],
    model_sha256: [u8; 32],
    role: Ds4CommandRole,
    command_sha256: [u8; 32],
}

impl fmt::Debug for VerifiedDs4Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedDs4Command")
            .field("profile_id", &self.command.profile.profile_id)
            .field("command_sha256", &self.digest_hex())
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl VerifiedDs4Command {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn from_staged_profile(
        mut command: crate::cluster::Ds4Command,
        managed_root: &Path,
        staged_config_fingerprint: &str,
        current_config_fingerprint: &str,
        expected_executable_sha256: &str,
        expected_model_sha256: &str,
        role: Ds4CommandRole,
    ) -> Result<Self, CommandSlotError> {
        if staged_config_fingerprint != current_config_fingerprint
            || decode_full_sha256(staged_config_fingerprint).is_none()
        {
            return Err(CommandSlotError::ConfigFingerprintMismatch);
        }
        let executable_sha256 = decode_full_sha256(expected_executable_sha256)
            .ok_or(CommandSlotError::InvalidDigest)?;
        let model_sha256 =
            decode_full_sha256(expected_model_sha256).ok_or(CommandSlotError::InvalidDigest)?;
        let managed_root = tokio::fs::canonicalize(managed_root)
            .await
            .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
        let executable = canonical_managed_executable(&managed_root, &command.executable).await?;
        let working_directory =
            canonical_managed_directory(&managed_root, &command.working_directory).await?;
        let model_index = model_argument_index(&command.argv)?;
        let model_input = resolve_command_path(
            &command.working_directory,
            Path::new(&command.argv[model_index]),
        );
        let model_path = canonical_managed_file(&managed_root, &model_input).await?;
        ensure_command_role(&command, role)?;
        command.executable = executable;
        command.working_directory = working_directory;
        command.argv[model_index] = model_path.into_os_string();
        let verified = Self {
            command,
            managed_root,
            config_fingerprint: staged_config_fingerprint.to_owned(),
            executable_sha256,
            model_sha256,
            role,
            command_sha256: [0; 32],
        };
        verified.verify_files().await?;
        let command_sha256 = command_digest(
            &verified.command,
            &verified.config_fingerprint,
            &verified.executable_sha256,
            &verified.model_sha256,
        );
        Ok(Self {
            command_sha256,
            ..verified
        })
    }

    async fn verify_files(&self) -> Result<(), CommandSlotError> {
        if decode_full_sha256(&self.config_fingerprint).is_none() {
            return Err(CommandSlotError::ConfigFingerprintMismatch);
        }
        ensure_command_role(&self.command, self.role)?;
        let executable =
            canonical_managed_executable(&self.managed_root, &self.command.executable).await?;
        let working_directory =
            canonical_managed_directory(&self.managed_root, &self.command.working_directory)
                .await?;
        let model_index = model_argument_index(&self.command.argv)?;
        let model_path = canonical_managed_file(
            &self.managed_root,
            &resolve_command_path(
                &self.command.working_directory,
                Path::new(&self.command.argv[model_index]),
            ),
        )
        .await?;
        if executable != self.command.executable
            || working_directory != self.command.working_directory
            || model_path != PathBuf::from(&self.command.argv[model_index])
        {
            return Err(CommandSlotError::PathOutsideManagedRoot);
        }
        let executable_digest = sha256_file(&executable).await?;
        let model_digest = sha256_file(&model_path).await?;
        if executable_digest != self.executable_sha256 || model_digest != self.model_sha256 {
            return Err(CommandSlotError::ArtifactDigestMismatch);
        }
        Ok(())
    }

    pub(crate) fn role(&self) -> Ds4CommandRole {
        self.role
    }

    pub(crate) fn digest_hex(&self) -> String {
        hex_digest(&self.command_sha256)
    }

    fn profile_id(&self) -> &str {
        &self.command.profile.profile_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSlotSnapshot {
    pub profile_id: String,
    pub command_sha256: String,
}

#[derive(Debug, Clone)]
enum CommandEntry {
    Startup(crate::cluster::Ds4Command),
    Verified(VerifiedDs4Command),
}

impl CommandEntry {
    async fn command(&self) -> Result<crate::cluster::Ds4Command, CommandSlotError> {
        match self {
            Self::Startup(command) => Ok(command.clone()),
            Self::Verified(command) => {
                command.verify_files().await?;
                Ok(command.command.clone())
            }
        }
    }

    fn snapshot(&self) -> CommandSlotSnapshot {
        match self {
            Self::Startup(command) => CommandSlotSnapshot {
                profile_id: command.profile.profile_id.clone(),
                command_sha256: String::new(),
            },
            Self::Verified(command) => CommandSlotSnapshot {
                profile_id: command.profile_id().to_owned(),
                command_sha256: command.digest_hex(),
            },
        }
    }
}

#[derive(Clone)]
struct CommandSelection {
    current_command: CommandEntry,
    previous_command: Option<CommandEntry>,
}

/// One command owner per supervisor. A stopped supervisor may atomically select a staged
/// candidate while retaining its old command for a later rollback.
struct CommandSlot {
    selection: RwLock<CommandSelection>,
}

impl CommandSlot {
    fn new(initial: crate::cluster::Ds4Command) -> Self {
        Self {
            selection: RwLock::new(CommandSelection {
                current_command: CommandEntry::Startup(initial),
                previous_command: None,
            }),
        }
    }

    async fn current_command(&self) -> Result<crate::cluster::Ds4Command, CommandSlotError> {
        let current = self.selection.read().await.current_command.clone();
        current.command().await
    }

    async fn current_snapshot(&self) -> Result<CommandSlotSnapshot, CommandSlotError> {
        let current = self.selection.read().await.current_command.clone();
        current.command().await?;
        Ok(current.snapshot())
    }

    async fn set_next(&self, next: VerifiedDs4Command) -> Result<(), CommandSlotError> {
        next.verify_files().await?;
        let mut selection = self.selection.write().await;
        selection.previous_command = Some(selection.current_command.clone());
        selection.current_command = CommandEntry::Verified(next);
        Ok(())
    }

    async fn restore_previous(&self) -> Result<CommandSlotSnapshot, CommandSlotError> {
        let mut selection = self.selection.write().await;
        let previous = selection
            .previous_command
            .take()
            .ok_or(CommandSlotError::PreviousCommandUnavailable)?;
        previous.command().await?;
        selection.current_command = previous;
        Ok(selection.current_command.snapshot())
    }
}

fn ensure_command_role(
    command: &crate::cluster::Ds4Command,
    role: Ds4CommandRole,
) -> Result<(), CommandSlotError> {
    let roles = command
        .argv
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            (argument == "--role")
                .then(|| command.argv.get(index + 1))
                .flatten()
        })
        .collect::<Vec<_>>();
    let role_flags = command
        .argv
        .iter()
        .filter(|argument| *argument == "--role")
        .count();
    if role_flags != roles.len() {
        return Err(CommandSlotError::RoleMismatch);
    }
    let tensor_parallel = command
        .argv
        .iter()
        .any(|argument| argument == "--tensor-parallel");
    let compatible = match role {
        Ds4CommandRole::Standalone => roles.is_empty() && !tensor_parallel,
        Ds4CommandRole::Coordinator => roles.len() == 1 && roles[0] == "coordinator",
        Ds4CommandRole::Worker => roles.len() == 1 && roles[0] == "worker",
    };
    if compatible {
        Ok(())
    } else {
        Err(CommandSlotError::RoleMismatch)
    }
}

fn model_argument_index(argv: &[OsString]) -> Result<usize, CommandSlotError> {
    let indexes = argv
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| (argument == "-m").then_some(index + 1))
        .collect::<Vec<_>>();
    if indexes.len() != 1 || indexes[0] >= argv.len() || argv[indexes[0]].is_empty() {
        return Err(CommandSlotError::ModelArgumentInvalid);
    }
    Ok(indexes[0])
}

fn resolve_command_path(working_directory: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory.join(path)
    }
}

async fn canonical_managed_file(root: &Path, path: &Path) -> Result<PathBuf, CommandSlotError> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
    if !canonical.starts_with(root)
        || !tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| CommandSlotError::ArtifactUnavailable)?
            .is_file()
    {
        return Err(CommandSlotError::PathOutsideManagedRoot);
    }
    Ok(canonical)
}

async fn canonical_managed_executable(
    root: &Path,
    path: &Path,
) -> Result<PathBuf, CommandSlotError> {
    let canonical = canonical_managed_file(root, path).await?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(CommandSlotError::ArtifactNotExecutable);
        }
    }
    Ok(canonical)
}

async fn canonical_managed_directory(
    root: &Path,
    path: &Path,
) -> Result<PathBuf, CommandSlotError> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
    if !canonical.starts_with(root)
        || !tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| CommandSlotError::ArtifactUnavailable)?
            .is_dir()
    {
        return Err(CommandSlotError::PathOutsideManagedRoot);
    }
    Ok(canonical)
}

async fn sha256_file(path: &Path) -> Result<[u8; 32], CommandSlotError> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| CommandSlotError::ArtifactUnavailable)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

fn decode_full_sha256(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn command_digest(
    command: &crate::cluster::Ds4Command,
    config_fingerprint: &str,
    executable_sha256: &[u8; 32],
    model_sha256: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"siderostat-manager-command-v1\0");
    hasher.update(config_fingerprint.as_bytes());
    hasher.update(executable_sha256);
    hasher.update(model_sha256);
    hasher.update(command.profile.profile_id.as_bytes());
    hasher.update(command.executable.as_os_str().as_encoded_bytes());
    hasher.update(command.working_directory.as_os_str().as_encoded_bytes());
    for argument in &command.argv {
        hasher.update([0]);
        hasher.update(argument.as_encoded_bytes());
    }
    hasher.finalize().into()
}

impl ChildIdentity {
    fn matches(&self, observed: &ObservedProcess) -> bool {
        ProcessIdentity {
            pid: self.pid,
            executable: self.executable.clone(),
            argv_sha256: self.argv_sha256,
            process_start_micros: self.process_start_micros,
        }
        .matches(observed)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedProcess {
    identity: ChildIdentity,
}

impl fmt::Debug for VerifiedProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedProcess")
            .field("pid", &self.identity.pid)
            .field("profile_id", &self.identity.profile_id)
            .field("generation", &self.identity.generation)
            .finish()
    }
}

pub trait ProcessInspector: Send + Sync + 'static {
    fn observe(&self, pid: u32) -> io::Result<Option<ObservedProcess>>;
}

pub trait ProcessSignaler: Send + Sync + 'static {
    fn signal_process_group(&self, pid: u32, signal: ProcessSignal) -> io::Result<()>;

    /// Signal only the process itself. This is used for authorized adoption/cleanup of processes
    /// that were not spawned into siderostat's process group.
    fn signal_process(&self, pid: u32, signal: ProcessSignal) -> io::Result<()> {
        self.signal_process_group(pid, signal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Terminate,
    Kill,
}

#[derive(Debug, Error)]
pub enum ProcessControlError {
    #[error("owned child is no longer running")]
    NotRunning,
    #[error("process identity no longer matches the owned child")]
    IdentityMismatch,
    #[error("child process has no PID")]
    MissingPid,
    #[error("child stop timed out")]
    StopTimeout,
    #[error("SIGKILL is not allowed for this child")]
    SigkillNotAllowed,
    #[error("child stdout/stderr was already taken")]
    MissingLogPipe,
    #[error("child log channel capacity must be positive")]
    InvalidLogCapacity,
    #[error("child exited before HTTP readiness with status {0}")]
    EarlyExit(std::process::ExitStatus),
    #[error("child HTTP readiness timed out")]
    ReadinessTimeout,
    #[error("DSpark activation was not observed before standalone readiness deadline")]
    DsparkActivationTimeout,
    #[error("readiness timeout and poll interval must be positive")]
    InvalidReadinessTiming,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone)]
pub struct ProcessController {
    inspector: Arc<dyn ProcessInspector>,
    signaler: Arc<dyn ProcessSignaler>,
}

impl ProcessController {
    pub fn new(inspector: Arc<dyn ProcessInspector>, signaler: Arc<dyn ProcessSignaler>) -> Self {
        Self {
            inspector,
            signaler,
        }
    }

    pub fn verify(&self, identity: &ChildIdentity) -> Result<VerifiedProcess, ProcessControlError> {
        let observed = self
            .inspector
            .observe(identity.pid)?
            .ok_or(ProcessControlError::NotRunning)?;
        if !identity.matches(&observed) {
            return Err(ProcessControlError::IdentityMismatch);
        }
        Ok(VerifiedProcess {
            identity: identity.clone(),
        })
    }

    pub fn signal_owned(
        &self,
        identity: &ChildIdentity,
        signal: ProcessSignal,
    ) -> Result<(), ProcessControlError> {
        let verified = self.verify(identity)?;
        self.signaler
            .signal_process_group(verified.identity.pid, signal)?;
        Ok(())
    }

    pub fn signal_approved_process(
        &self,
        identity: &ProcessIdentity,
        signal: ProcessSignal,
    ) -> Result<(), ProcessControlError> {
        let observed = self
            .inspector
            .observe(identity.pid)?
            .ok_or(ProcessControlError::NotRunning)?;
        if !identity.matches(&observed) {
            return Err(ProcessControlError::IdentityMismatch);
        }
        self.signaler
            .signal_process(identity.pid, signal)
            .map_err(ProcessControlError::Io)
    }

    pub async fn stop_recovered_owned(
        &self,
        identity: &ChildIdentity,
        timeout: Duration,
        poll_interval: Duration,
        allow_sigkill: bool,
    ) -> Result<(), ProcessControlError> {
        if timeout.is_zero() || poll_interval.is_zero() {
            return Err(ProcessControlError::InvalidReadinessTiming);
        }
        match self.signal_owned(identity, ProcessSignal::Terminate) {
            Ok(()) => {}
            Err(ProcessControlError::NotRunning) => return Ok(()),
            Err(error) => return Err(error),
        }
        let deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(
                poll_interval.min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
            match self.verify(identity) {
                Err(ProcessControlError::NotRunning) => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) if Instant::now() < deadline => continue,
                Ok(_) if !allow_sigkill => return Err(ProcessControlError::SigkillNotAllowed),
                Ok(_) => break,
            }
        }
        self.signal_owned(identity, ProcessSignal::Kill)?;
        let kill_deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(
                poll_interval.min(kill_deadline.saturating_duration_since(Instant::now())),
            )
            .await;
            match self.verify(identity) {
                Err(ProcessControlError::NotRunning) => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) if Instant::now() < kill_deadline => {}
                Ok(_) => return Err(ProcessControlError::StopTimeout),
            }
        }
    }

    /// Stop a process only after startup cleanup has been authorized.
    ///
    /// Unlike normal supervision this method intentionally permits SIGKILL after the grace
    /// period. The permission comes from the startup cleanup decision, while every signal is still
    /// preceded by a complete identity check.
    pub async fn force_stop_approved(
        &self,
        identity: &ProcessIdentity,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), ProcessControlError> {
        if timeout.is_zero() || poll_interval.is_zero() {
            return Err(ProcessControlError::InvalidReadinessTiming);
        }
        match self.signal_approved_process(identity, ProcessSignal::Terminate) {
            Ok(()) => {}
            Err(ProcessControlError::NotRunning) => return Ok(()),
            Err(error) => return Err(error),
        }
        let deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(
                poll_interval.min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
            match self.verify_process(identity) {
                Err(ProcessControlError::NotRunning) => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) if Instant::now() < deadline => continue,
                Ok(_) => break,
            }
        }
        self.signal_approved_process(identity, ProcessSignal::Kill)?;
        let kill_deadline = Instant::now() + timeout;
        loop {
            tokio::time::sleep(
                poll_interval.min(kill_deadline.saturating_duration_since(Instant::now())),
            )
            .await;
            match self.verify_process(identity) {
                Err(ProcessControlError::NotRunning) => return Ok(()),
                Err(error) => return Err(error),
                Ok(_) if Instant::now() < kill_deadline => {}
                Ok(_) => return Err(ProcessControlError::StopTimeout),
            }
        }
    }

    fn verify_process(
        &self,
        identity: &ProcessIdentity,
    ) -> Result<ProcessIdentity, ProcessControlError> {
        let observed = self
            .inspector
            .observe(identity.pid)?
            .ok_or(ProcessControlError::NotRunning)?;
        if !identity.matches(&observed) {
            return Err(ProcessControlError::IdentityMismatch);
        }
        Ok(identity.clone())
    }
}

#[cfg(target_os = "macos")]
pub fn platform_process_controller() -> ProcessController {
    use crate::cluster::{MacOsProcessInspector, MacOsProcessSignaler};
    ProcessController::new(
        Arc::new(MacOsProcessInspector),
        Arc::new(MacOsProcessSignaler),
    )
}

/// Discover likely stale siderostat/DS4 processes before the new supervisor acquires its state
/// lock or binds its listeners. The non-macOS implementation is intentionally empty because the
/// supported production process-identity API is macOS-specific.
pub fn discover_startup_processes(
    current_pid: u32,
    configured_ds4_binary: &std::path::Path,
) -> io::Result<Vec<StartupProcessCandidate>> {
    #[cfg(target_os = "macos")]
    {
        let configured_name = configured_ds4_binary.file_name();
        let processes = crate::cluster::platform::process::list_processes()?;
        Ok(processes
            .into_iter()
            .filter(|observed| observed.pid != current_pid)
            .filter_map(|observed| {
                let executable_name = observed.executable.file_name();
                let kind = if executable_name.is_some_and(|name| name == "siderostat") {
                    Some(StartupProcessKind::Siderostat)
                } else if executable_name.is_some_and(|name| name == "ds4-server")
                    || configured_name.is_some() && executable_name == configured_name
                {
                    Some(StartupProcessKind::Ds4)
                } else {
                    None
                }?;
                Some(StartupProcessCandidate { kind, observed })
            })
            .collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (current_pid, configured_ds4_binary);
        Ok(Vec::new())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn platform_process_controller() -> ProcessController {
    struct Unsupported;
    impl ProcessInspector for Unsupported {
        fn observe(&self, _pid: u32) -> io::Result<Option<ObservedProcess>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process identity inspection requires macOS",
            ))
        }
    }
    impl ProcessSignaler for Unsupported {
        fn signal_process_group(&self, _pid: u32, _signal: ProcessSignal) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process group signaling requires macOS",
            ))
        }
    }
    ProcessController::new(Arc::new(Unsupported), Arc::new(Unsupported))
}

pub struct ManagedChild {
    child: Child,
    identity: ChildIdentity,
    controller: ProcessController,
}

struct SupervisedChild {
    child: ManagedChild,
    _log_forwarders: ChildLogForwarders,
    log_task: tokio::task::JoinHandle<()>,
}

/// Shared slot management for a single supervised child. All supervisors use the same
/// child_identity / is_running / stop / begin_start lifecycle so that the per-supervisor code
/// only differs where the startup conditions actually differ (readiness waits, dspark activation,
/// route observation).
struct SupervisedSlot {
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
}

impl SupervisedSlot {
    fn new() -> Self {
        Self {
            child: tokio::sync::Mutex::new(None),
        }
    }

    async fn child_identity(&self) -> Option<ChildIdentity> {
        self.child
            .lock()
            .await
            .as_ref()
            .map(|current| current.child.identity().clone())
    }

    async fn is_running(&self) -> anyhow::Result<bool> {
        let mut slot = self.child.lock().await;
        let Some(current) = slot.as_mut() else {
            return Ok(false);
        };
        Ok(current.child.try_wait()?.is_none())
    }

    async fn stop(&self, stop_timeout: Duration, allow_sigkill: bool) -> anyhow::Result<()> {
        let mut slot = self.child.lock().await;
        let Some(mut current) = slot.take() else {
            return Ok(());
        };
        let result = current.child.stop(stop_timeout, allow_sigkill).await;
        current.log_task.abort();
        result?;
        Ok(())
    }

    /// Locks the slot and clears any stale child so a replacement can be placed. Returns `None`
    /// when a child is already running; callers treat that as a no-op start.
    async fn begin_start(
        &self,
    ) -> anyhow::Result<Option<tokio::sync::MutexGuard<'_, Option<SupervisedChild>>>> {
        let mut slot = self.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Ok(None);
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        Ok(Some(slot))
    }

    async fn set_next_command(
        &self,
        commands: &CommandSlot,
        candidate: VerifiedDs4Command,
    ) -> anyhow::Result<()> {
        let mut slot = self.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Err(CommandSlotError::ChildRunning.into());
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        commands.set_next(candidate).await?;
        Ok(())
    }

    async fn restore_previous_command(&self, commands: &CommandSlot) -> anyhow::Result<()> {
        let mut slot = self.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Err(CommandSlotError::ChildRunning.into());
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        commands.restore_previous().await?;
        Ok(())
    }
}

impl ManagedChild {
    #[cfg(target_os = "macos")]
    pub async fn spawn(command: &Ds4Command, generation: u64) -> Result<Self, ProcessControlError> {
        use std::process::Stdio;

        let executable = tokio::fs::canonicalize(&command.executable).await?;
        let mut process = command.tokio_command();
        process.stdout(Stdio::piped());
        process.stderr(Stdio::piped());
        process.kill_on_drop(true);
        // SAFETY: setpgid is async-signal-safe and does not allocate in the child hook.
        unsafe {
            process.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = process.spawn()?;
        let Some(pid) = child.id() else {
            reap_failed_spawn(&mut child).await;
            return Err(ProcessControlError::MissingPid);
        };
        let controller = platform_process_controller();
        let observed = match controller.inspector.observe(pid) {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                reap_failed_spawn(&mut child).await;
                return Err(ProcessControlError::NotRunning);
            }
            Err(error) => {
                reap_failed_spawn(&mut child).await;
                return Err(ProcessControlError::Io(error));
            }
        };
        let expected_argv_sha256 = argv_sha256(executable.as_os_str(), &command.argv);
        let identity = ChildIdentity {
            pid,
            executable,
            argv_sha256: expected_argv_sha256,
            profile_id: command.profile.profile_id.clone(),
            generation,
            spawned_at_millis: system_time_millis(),
            process_start_micros: observed.start_time_micros,
        };
        if let Err(error) = controller.verify(&identity) {
            reap_failed_spawn(&mut child).await;
            return Err(error);
        }
        Ok(Self {
            child,
            identity,
            controller,
        })
    }

    pub fn identity(&self) -> &ChildIdentity {
        &self.identity
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub fn start_log_forwarding(
        &mut self,
        capacity: usize,
    ) -> Result<(mpsc::Receiver<ChildLogRecord>, ChildLogForwarders), ProcessControlError> {
        if capacity == 0 {
            return Err(ProcessControlError::InvalidLogCapacity);
        }
        if self.child.stdout.is_none() || self.child.stderr.is_none() {
            return Err(ProcessControlError::MissingLogPipe);
        }
        let stdout = self.take_stdout().expect("stdout presence checked above");
        let stderr = self.take_stderr().expect("stderr presence checked above");
        Ok(spawn_child_log_forwarders(
            stdout,
            stderr,
            Arc::from(self.identity.profile_id.as_str()),
            self.identity.generation,
            self.identity.pid,
            capacity,
        ))
    }

    pub fn start_log_forwarding_with_events(
        &mut self,
        capacity: usize,
    ) -> Result<
        (
            mpsc::Receiver<ChildLogRecord>,
            mpsc::UnboundedReceiver<Ds4LogEvent>,
            ChildLogForwarders,
        ),
        ProcessControlError,
    > {
        if capacity == 0 {
            return Err(ProcessControlError::InvalidLogCapacity);
        }
        if self.child.stdout.is_none() || self.child.stderr.is_none() {
            return Err(ProcessControlError::MissingLogPipe);
        }
        let stdout = self.take_stdout().expect("stdout presence checked above");
        let stderr = self.take_stderr().expect("stderr presence checked above");
        Ok(spawn_child_log_forwarders_with_events(
            stdout,
            stderr,
            Arc::from(self.identity.profile_id.as_str()),
            self.identity.generation,
            self.identity.pid,
            capacity,
        ))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    pub async fn wait_http_ready(
        &mut self,
        client: &reqwest::Client,
        models_url: &Url,
        startup_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), ProcessControlError> {
        wait_for_http_readiness(client, models_url, startup_timeout, poll_interval, || {
            self.child.try_wait()
        })
        .await
    }

    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub async fn stop(
        &mut self,
        timeout: Duration,
        allow_sigkill: bool,
    ) -> Result<std::process::ExitStatus, ProcessControlError> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(status);
        }
        match self
            .controller
            .signal_owned(&self.identity, ProcessSignal::Terminate)
        {
            Ok(()) => {}
            Err(ProcessControlError::NotRunning) => {}
            Err(error) => return Err(error),
        }
        let graceful_deadline = Instant::now() + timeout;
        if let Some(status) = wait_child_exit_until(&mut self.child, graceful_deadline).await? {
            return Ok(status);
        }
        if !allow_sigkill {
            return Err(ProcessControlError::SigkillNotAllowed);
        }
        match self
            .controller
            .signal_owned(&self.identity, ProcessSignal::Kill)
        {
            Ok(()) | Err(ProcessControlError::NotRunning) => {}
            Err(error) => return Err(error),
        }
        // The configured stop timeout is the complete graceful window. After it expires, keep
        // only a short bounded reaping window for SIGKILL; do not silently add a second full stop
        // timeout to every restart and recovery operation.
        let kill_deadline = Instant::now() + sigkill_reap_window(timeout);
        wait_child_exit_until(&mut self.child, kill_deadline)
            .await?
            .ok_or(ProcessControlError::StopTimeout)
    }
}

async fn wait_child_exit_until(
    child: &mut tokio::process::Child,
    deadline: Instant,
) -> Result<Option<std::process::ExitStatus>, ProcessControlError> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

pub async fn wait_for_http_readiness<F>(
    client: &reqwest::Client,
    models_url: &Url,
    startup_timeout: Duration,
    poll_interval: Duration,
    mut try_wait: F,
) -> Result<(), ProcessControlError>
where
    F: FnMut() -> io::Result<Option<std::process::ExitStatus>>,
{
    if startup_timeout.is_zero() || poll_interval.is_zero() {
        return Err(ProcessControlError::InvalidReadinessTiming);
    }
    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Some(status) = try_wait()? {
            return Err(ProcessControlError::EarlyExit(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessControlError::ReadinessTimeout);
        }
        let attempt_timeout = remaining.min(poll_interval);
        let response =
            tokio::time::timeout(attempt_timeout, client.get(models_url.clone()).send()).await;
        if matches!(response, Ok(Ok(response)) if response.status().is_success()) {
            if let Some(status) = try_wait()? {
                return Err(ProcessControlError::EarlyExit(status));
            }
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessControlError::ReadinessTimeout);
        }
        tokio::time::sleep(remaining.min(poll_interval)).await;
    }
}

async fn reap_failed_spawn(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

pub fn argv_sha256(executable: &OsStr, argv: &[OsString]) -> [u8; 32] {
    let mut digest = Sha256::new();
    update_os_string(&mut digest, executable);
    for argument in argv {
        update_os_string(&mut digest, argument);
    }
    digest.finalize().into()
}

#[cfg(unix)]
fn update_os_string(digest: &mut Sha256, value: &OsStr) {
    let bytes = value.as_bytes();
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn system_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster::{DistributedWorkerLifecycle, Ds4Profile},
        config::{Quantization, Residency, SpeculativeSupport},
    };
    use std::sync::Mutex;

    // The macOS process identity APIs can transiently return EIO while several test children are
    // being exec'd at once. Keep OS-backed child tests isolated while leaving the rest of the
    // Rust test harness parallel.
    static OS_PROCESS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("siderostat-command-slot-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn worker_candidate(
        root: &Path,
        model_path: Option<&Path>,
        profile_id: &str,
        staged_fingerprint: &str,
        runtime_fingerprint: &str,
    ) -> Result<VerifiedDs4Command, CommandSlotError> {
        let managed_root = root.join("managed");
        let bin_dir = managed_root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let executable = bin_dir.join("ds4-test-shell");
        if !executable.exists() {
            std::fs::copy("/bin/sh", &executable).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&executable, permissions).unwrap();
            }
        }
        let model = model_path.map(Path::to_path_buf).unwrap_or_else(|| {
            let model = managed_root.join("model.gguf");
            std::fs::write(&model, b"verified test model").unwrap();
            model
        });
        let model_digest = hex_digest(&sha256_file(&model).await.unwrap());
        let executable_digest = hex_digest(&sha256_file(&executable).await.unwrap());
        let command = crate::cluster::Ds4Command {
            executable,
            working_directory: managed_root.clone(),
            argv: vec![
                OsString::from("-c"),
                OsString::from("trap 'exit 0' TERM; sleep 30"),
                OsString::from("runner"),
                OsString::from("-m"),
                model.into_os_string(),
                OsString::from("--role"),
                OsString::from("worker"),
            ],
            profile: Ds4Profile {
                profile_id: profile_id.to_owned(),
                quantization: Quantization::Mxfp4,
                residency: Residency::Resident,
                speculative_support: SpeculativeSupport::None,
            },
        };
        VerifiedDs4Command::from_staged_profile(
            command,
            &managed_root,
            staged_fingerprint,
            runtime_fingerprint,
            &executable_digest,
            &model_digest,
            Ds4CommandRole::Worker,
        )
        .await
    }

    #[derive(Clone)]
    struct Inspector(Arc<Mutex<Option<ObservedProcess>>>);

    impl ProcessInspector for Inspector {
        fn observe(&self, _pid: u32) -> io::Result<Option<ObservedProcess>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[derive(Default)]
    struct Signaler(Mutex<Vec<(u32, ProcessSignal)>>);

    impl ProcessSignaler for Signaler {
        fn signal_process_group(&self, pid: u32, signal: ProcessSignal) -> io::Result<()> {
            self.0.lock().unwrap().push((pid, signal));
            Ok(())
        }
    }

    fn observed() -> ObservedProcess {
        ObservedProcess {
            pid: 42,
            executable: PathBuf::from("/opt/ds4-server"),
            argv: vec![OsString::from("-m"), OsString::from("/model.gguf")],
            start_time_micros: 123_456,
        }
    }

    #[tokio::test]
    async fn command_slot_revalidates_files_and_restores_the_previous_command() {
        let root = TestRoot::new();
        let initial = crate::cluster::Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "initial-worker".into(),
                quantization: Quantization::Mxfp4,
                residency: Residency::Resident,
                speculative_support: SpeculativeSupport::None,
            },
        };
        let commands = CommandSlot::new(initial);
        let config_fingerprint = "a".repeat(64);
        let candidate = worker_candidate(
            &root.0,
            None,
            "staged-worker",
            &config_fingerprint,
            &config_fingerprint,
        )
        .await
        .unwrap();
        let expected_digest = candidate.digest_hex();
        commands.set_next(candidate).await.unwrap();

        let snapshot = commands.current_snapshot().await.unwrap();
        assert_eq!(snapshot.profile_id, "staged-worker");
        assert_eq!(snapshot.command_sha256, expected_digest);

        let model_path = root.0.join("managed/model.gguf");
        std::fs::write(&model_path, b"replaced after verification").unwrap();
        assert_eq!(
            commands.current_command().await.unwrap_err(),
            CommandSlotError::ArtifactDigestMismatch
        );

        let restored = commands.restore_previous().await.unwrap();
        assert_eq!(restored.profile_id, "initial-worker");
        assert!(restored.command_sha256.is_empty());
    }

    #[tokio::test]
    async fn staged_command_rejects_stale_config_path_escape_and_role_mismatch() {
        let root = TestRoot::new();
        let fingerprint = "b".repeat(64);
        assert_eq!(
            worker_candidate(
                &root.0,
                None,
                "staged-worker",
                &fingerprint,
                &"c".repeat(64),
            )
            .await
            .unwrap_err(),
            CommandSlotError::ConfigFingerprintMismatch
        );

        let outside_model = root.0.join("outside.gguf");
        std::fs::write(&outside_model, b"outside model").unwrap();
        assert_eq!(
            worker_candidate(
                &root.0,
                Some(&outside_model),
                "staged-worker",
                &fingerprint,
                &fingerprint,
            )
            .await
            .unwrap_err(),
            CommandSlotError::PathOutsideManagedRoot
        );

        let managed_root = root.0.join("managed");
        let executable = managed_root.join("bin/ds4-test-shell");
        let model = managed_root.join("model.gguf");
        let executable_digest = hex_digest(&sha256_file(&executable).await.unwrap());
        let model_digest = hex_digest(&sha256_file(&model).await.unwrap());
        let mut command = crate::cluster::Ds4Command {
            executable,
            working_directory: managed_root.clone(),
            argv: vec![
                OsString::from("-m"),
                model.into_os_string(),
                OsString::from("--role"),
                OsString::from("coordinator"),
            ],
            profile: Ds4Profile {
                profile_id: "staged-worker".into(),
                quantization: Quantization::Mxfp4,
                residency: Residency::Resident,
                speculative_support: SpeculativeSupport::None,
            },
        };
        command.argv.shrink_to_fit();
        assert_eq!(
            VerifiedDs4Command::from_staged_profile(
                command,
                &managed_root,
                &fingerprint,
                &fingerprint,
                &executable_digest,
                &model_digest,
                Ds4CommandRole::Worker,
            )
            .await
            .unwrap_err(),
            CommandSlotError::RoleMismatch
        );
    }

    fn identity(observed: &ObservedProcess) -> ChildIdentity {
        ChildIdentity {
            pid: observed.pid,
            executable: observed.executable.clone(),
            argv_sha256: argv_sha256(observed.executable.as_os_str(), &observed.argv),
            profile_id: "standalone".into(),
            generation: 7,
            spawned_at_millis: 100,
            process_start_micros: observed.start_time_micros,
        }
    }

    #[test]
    fn verifies_every_identity_field_before_signaling() {
        let observed = observed();
        let inspector = Arc::new(Inspector(Arc::new(Mutex::new(Some(observed.clone())))));
        let signaler = Arc::new(Signaler::default());
        let controller = ProcessController::new(inspector.clone(), signaler.clone());
        let identity = identity(&observed);
        controller
            .signal_owned(&identity, ProcessSignal::Terminate)
            .unwrap();
        assert_eq!(
            signaler.0.lock().unwrap().as_slice(),
            &[(42, ProcessSignal::Terminate)]
        );

        for mutation in 0..4 {
            let mut changed = observed.clone();
            match mutation {
                0 => changed.pid += 1,
                1 => changed.executable = PathBuf::from("/tmp/other"),
                2 => changed.argv.push(OsString::from("--debug")),
                _ => changed.start_time_micros += 1,
            }
            *inspector.0.lock().unwrap() = Some(changed);
            assert!(matches!(
                controller.signal_owned(&identity, ProcessSignal::Kill),
                Err(ProcessControlError::IdentityMismatch)
            ));
        }
        assert_eq!(signaler.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn pid_reuse_and_unknown_process_never_reach_signaler() {
        let original = observed();
        let slot = Arc::new(Mutex::new(Some(ObservedProcess {
            start_time_micros: original.start_time_micros + 1,
            ..original.clone()
        })));
        let signaler = Arc::new(Signaler::default());
        let controller =
            ProcessController::new(Arc::new(Inspector(slot.clone())), signaler.clone());
        assert!(matches!(
            controller.signal_owned(&identity(&original), ProcessSignal::Terminate),
            Err(ProcessControlError::IdentityMismatch)
        ));
        *slot.lock().unwrap() = None;
        assert!(matches!(
            controller.signal_owned(&identity(&original), ProcessSignal::Terminate),
            Err(ProcessControlError::NotRunning)
        ));
        assert!(signaler.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn approved_external_process_is_terminated_after_identity_recheck() {
        let observed = observed();
        let slot = Arc::new(Mutex::new(Some(observed.clone())));
        let signals = Arc::new(Mutex::new(Vec::new()));
        let signaler = Arc::new(ApprovedSignaler {
            observed: slot.clone(),
            signals: signals.clone(),
        });
        let controller = ProcessController::new(Arc::new(Inspector(slot)), signaler);
        controller
            .force_stop_approved(
                &ProcessIdentity::from_observed(&observed),
                Duration::from_millis(100),
                Duration::from_millis(10),
            )
            .await
            .unwrap();
        assert_eq!(
            signals.lock().unwrap().as_slice(),
            &[(42, ProcessSignal::Terminate)]
        );
    }

    struct ApprovedSignaler {
        observed: Arc<Mutex<Option<ObservedProcess>>>,
        signals: Arc<Mutex<Vec<(u32, ProcessSignal)>>>,
    }

    impl ProcessSignaler for ApprovedSignaler {
        fn signal_process_group(&self, pid: u32, signal: ProcessSignal) -> io::Result<()> {
            self.signals.lock().unwrap().push((pid, signal));
            if signal == ProcessSignal::Terminate {
                *self.observed.lock().unwrap() = None;
            }
            Ok(())
        }
    }

    #[test]
    fn length_framing_prevents_argv_hash_ambiguity() {
        assert_ne!(
            argv_sha256(OsStr::new("/bin/x"), &["ab".into(), "c".into()]),
            argv_sha256(OsStr::new("/bin/x"), &["a".into(), "bc".into()]),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_handles_slow_start_timeout_and_early_exit() {
        use axum::{Router, routing::get};
        use std::os::unix::process::ExitStatusExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            axum::serve(
                listener,
                Router::new().route("/v1/models", get(|| async { "{}" })),
            )
            .await
            .unwrap();
        });
        let url = Url::parse(&format!("http://{address}/v1/models")).unwrap();
        wait_for_http_readiness(
            &reqwest::Client::new(),
            &url,
            Duration::from_millis(500),
            Duration::from_millis(25),
            || Ok(None),
        )
        .await
        .unwrap();
        server.abort();

        let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = unavailable.local_addr().unwrap();
        drop(unavailable);
        let unavailable_url =
            Url::parse(&format!("http://{unavailable_address}/v1/models")).unwrap();
        assert!(matches!(
            wait_for_http_readiness(
                &reqwest::Client::new(),
                &unavailable_url,
                Duration::from_millis(80),
                Duration::from_millis(10),
                || Ok(None),
            )
            .await,
            Err(ProcessControlError::ReadinessTimeout)
        ));

        assert!(matches!(
            wait_for_http_readiness(
                &reqwest::Client::new(),
                &unavailable_url,
                Duration::from_secs(1),
                Duration::from_millis(10),
                || Ok(Some(std::process::ExitStatus::from_raw(7 << 8))),
            )
            .await,
            Err(ProcessControlError::EarlyExit(status)) if status.code() == Some(7)
        ));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_spawn_owns_process_group_and_reaps_verified_child() {
        let _process_test = OS_PROCESS_TEST_LOCK.lock().await;
        use crate::{
            cluster::Ds4Profile,
            config::{Quantization, Residency},
        };

        let command = Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "process-smoke".into(),
                quantization: Quantization::Q2,
                residency: Residency::Resident,
                speculative_support: crate::config::SpeculativeSupport::None,
            },
        };
        let mut child = ManagedChild::spawn(&command, 9).await.unwrap();
        assert_eq!(child.identity().generation, 9);
        let status = child.stop(Duration::from_secs(2), false).await.unwrap();
        assert!(!status.success());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_stop_uses_a_bounded_kill_window_after_term_timeout() {
        let _process_test = OS_PROCESS_TEST_LOCK.lock().await;
        use crate::{
            cluster::Ds4Profile,
            config::{Quantization, Residency},
        };

        let command = Ds4Command {
            executable: PathBuf::from("/bin/sh"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![
                OsString::from("-c"),
                OsString::from("trap '' TERM; sleep 30"),
            ],
            profile: Ds4Profile {
                profile_id: "process-stop-timeout".into(),
                quantization: Quantization::Q2,
                residency: Residency::Resident,
                speculative_support: crate::config::SpeculativeSupport::None,
            },
        };
        let mut child = ManagedChild::spawn(&command, 10).await.unwrap();
        let started = std::time::Instant::now();
        let status = child.stop(Duration::from_millis(100), true).await.unwrap();

        assert!(!status.success());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stop exceeded the bounded kill window: {:?}",
            started.elapsed()
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn distributed_worker_supervisor_starts_and_reaps_one_owned_child() {
        let _process_test = OS_PROCESS_TEST_LOCK.lock().await;
        use crate::{
            cluster::Ds4Profile,
            config::{Quantization, Residency},
        };

        let command = Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "distributed-worker-smoke".into(),
                quantization: Quantization::Mxfp4,
                residency: Residency::Resident,
                speculative_support: crate::config::SpeculativeSupport::None,
            },
        };
        let supervisor = DistributedWorkerSupervisor::new(
            command,
            Duration::from_secs(2),
            false,
            Arc::new(crate::metrics::Metrics::default()),
        );
        supervisor.start(13).await.unwrap();
        assert!(supervisor.is_running().await.unwrap());
        assert_eq!(supervisor.child_identity().await.unwrap().generation, 13);

        supervisor.stop().await.unwrap();
        assert!(!supervisor.is_running().await.unwrap());
        assert!(supervisor.child_identity().await.is_none());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn distributed_worker_command_slot_rejects_live_swap_and_starts_verified_candidate() {
        let _process_test = OS_PROCESS_TEST_LOCK.lock().await;
        let initial = crate::cluster::Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "initial-worker".into(),
                quantization: Quantization::Mxfp4,
                residency: Residency::Resident,
                speculative_support: SpeculativeSupport::None,
            },
        };
        let supervisor = DistributedWorkerSupervisor::new(
            initial,
            Duration::from_secs(1),
            true,
            Arc::new(crate::metrics::Metrics::default()),
        );
        supervisor.start(1).await.unwrap();

        let root = TestRoot::new();
        let fingerprint = "d".repeat(64);
        let candidate = worker_candidate(
            &root.0,
            None,
            "staged-worker-candidate",
            &fingerprint,
            &fingerprint,
        )
        .await
        .unwrap();
        let expected_digest = candidate.digest_hex();
        let expected_argv = argv_sha256(
            candidate.command.executable.as_os_str(),
            &candidate.command.argv,
        );
        assert!(
            supervisor
                .set_next_command(candidate.clone())
                .await
                .is_err()
        );
        assert_eq!(
            supervisor.child_identity().await.unwrap().profile_id,
            "initial-worker"
        );

        supervisor.stop().await.unwrap();
        supervisor.set_next_command(candidate).await.unwrap();
        assert_eq!(
            supervisor.command_snapshot().await.unwrap(),
            CommandSlotSnapshot {
                profile_id: "staged-worker-candidate".into(),
                command_sha256: expected_digest,
            }
        );
        supervisor.start(2).await.unwrap();
        let identity = supervisor.child_identity().await.unwrap();
        assert_eq!(identity.profile_id, "staged-worker-candidate");
        assert_eq!(identity.argv_sha256, expected_argv);
        supervisor.stop().await.unwrap();
    }
}
