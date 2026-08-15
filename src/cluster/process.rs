use super::{
    ChildLogForwarders, ChildLogRecord, Ds4Command, Ds4LogEvent, spawn_child_log_forwarders,
    spawn_child_log_forwarders_with_events,
};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    process::{Child, ChildStderr, ChildStdout},
    sync::mpsc,
    time::Instant,
};
use url::Url;

mod coordinator;
mod standalone;
mod worker;

pub use coordinator::DistributedCoordinatorSupervisor;
pub use standalone::StandaloneSupervisor;
pub use worker::DistributedWorkerSupervisor;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProcess {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv: Vec<OsString>,
    pub start_time_micros: u64,
}

/// Process identity used for an operator-approved startup cleanup.
///
/// This deliberately omits the logical child metadata (profile/generation). A process that was
/// started by an older siderostat, or by another launcher, cannot have that metadata, but it can
/// still be safely targeted when the operator has explicitly approved the cleanup and the OS-level
/// identity is re-verified immediately before every signal.
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

    /// Signal only the process itself. This is used for operator-approved adoption/cleanup of
    /// processes that were not spawned into siderostat's process group.
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

    /// Stop a process only after an operator has approved it for cleanup.
    ///
    /// Unlike normal supervision this method intentionally permits SIGKILL after the grace
    /// period. The permission comes from the explicit startup confirmation, while every signal is
    /// still preceded by a complete identity check.
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
        if let Some(status) = wait_child_exit(&mut self.child, timeout).await? {
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
        wait_child_exit(&mut self.child, timeout)
            .await?
            .ok_or(ProcessControlError::StopTimeout)
    }
}

async fn wait_child_exit(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<Option<std::process::ExitStatus>, ProcessControlError> {
    let deadline = Instant::now() + timeout;
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
    use crate::cluster::DistributedWorkerLifecycle;
    use std::sync::Mutex;

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
        use crate::{
            cluster::Ds4Profile,
            config::{ModelVariant, Residency},
        };

        let command = Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "process-smoke".into(),
                model_variant: ModelVariant::Q2,
                residency: Residency::Resident,
                dspark_required: false,
            },
        };
        let mut child = ManagedChild::spawn(&command, 9).await.unwrap();
        assert_eq!(child.identity().generation, 9);
        let status = child.stop(Duration::from_secs(2), false).await.unwrap();
        assert!(!status.success());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn distributed_worker_supervisor_starts_and_reaps_one_owned_child() {
        use crate::{
            cluster::Ds4Profile,
            config::{ModelVariant, Residency},
        };

        let command = Ds4Command {
            executable: PathBuf::from("/bin/sleep"),
            working_directory: PathBuf::from("/tmp"),
            argv: vec![OsString::from("30")],
            profile: Ds4Profile {
                profile_id: "distributed-worker-smoke".into(),
                model_variant: ModelVariant::Mxfp4,
                residency: Residency::Resident,
                dspark_required: false,
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
}
