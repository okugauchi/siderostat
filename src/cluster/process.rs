use super::{
    ChildLogForwarders, ChildLogRecord, Ds4Command, Ds4LogEvent, spawn_child_log_forwarders,
    spawn_child_log_forwarders_with_events,
};
use crate::cluster::{
    DistributedCoordinatorLifecycle, DistributedWorkerLifecycle, LocalStandaloneLifecycle,
};
use futures::future::BoxFuture;
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

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProcess {
    pub pid: u32,
    pub executable: PathBuf,
    pub argv: Vec<OsString>,
    pub start_time_micros: u64,
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
        self.pid == observed.pid
            && self.executable == observed.executable
            && self.argv_sha256 == argv_sha256(observed.executable.as_os_str(), &observed.argv)
            && self.process_start_micros == observed.start_time_micros
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
}

#[cfg(target_os = "macos")]
pub fn platform_process_controller() -> ProcessController {
    use crate::cluster::{MacOsProcessInspector, MacOsProcessSignaler};
    ProcessController::new(
        Arc::new(MacOsProcessInspector),
        Arc::new(MacOsProcessSignaler),
    )
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

#[derive(Clone)]
pub struct StandaloneSupervisor {
    inner: Arc<StandaloneSupervisorInner>,
}

#[derive(Clone)]
pub struct DistributedWorkerSupervisor {
    inner: Arc<DistributedWorkerSupervisorInner>,
}

struct DistributedWorkerSupervisorInner {
    command: Ds4Command,
    stop_timeout: Duration,
    allow_sigkill: bool,
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
}

#[derive(Clone)]
pub struct DistributedCoordinatorSupervisor {
    inner: Arc<DistributedCoordinatorSupervisorInner>,
}

struct DistributedCoordinatorSupervisorInner {
    command: Ds4Command,
    models_url: Url,
    client: reqwest::Client,
    http_startup_timeout: Duration,
    poll_interval: Duration,
    stop_timeout: Duration,
    allow_sigkill: bool,
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
    route: tokio::sync::watch::Sender<CoordinatorRouteState>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CoordinatorRouteState {
    http_ready: bool,
    worker_registered: bool,
    complete_route: bool,
}

struct StandaloneSupervisorInner {
    command: Ds4Command,
    models_url: Url,
    client: reqwest::Client,
    startup_timeout: Duration,
    poll_interval: Duration,
    stop_timeout: Duration,
    allow_sigkill: bool,
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
}

struct SupervisedChild {
    child: ManagedChild,
    _log_forwarders: ChildLogForwarders,
    log_task: tokio::task::JoinHandle<()>,
}

impl StandaloneSupervisor {
    pub fn new(
        command: Ds4Command,
        models_url: Url,
        startup_timeout: Duration,
        poll_interval: Duration,
        stop_timeout: Duration,
        allow_sigkill: bool,
    ) -> Self {
        Self {
            inner: Arc::new(StandaloneSupervisorInner {
                command,
                models_url,
                client: reqwest::Client::new(),
                startup_timeout,
                poll_interval,
                stop_timeout,
                allow_sigkill,
                child: tokio::sync::Mutex::new(None),
            }),
        }
    }

    pub async fn child_identity(&self) -> Option<ChildIdentity> {
        self.inner
            .child
            .lock()
            .await
            .as_ref()
            .map(|current| current.child.identity().clone())
    }

    #[cfg(target_os = "macos")]
    async fn start_inner(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Ok(());
        }
        *slot = None;
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
                tracing::info!(
                    profile = %record.profile_id,
                    generation = record.generation,
                    pid = record.pid,
                    stream = ?record.stream,
                    truncated = record.truncated,
                    event = ?record.event,
                    line_bytes = record.line.len(),
                    "DS4 child log"
                );
            }
        });
        if let Err(error) = child
            .wait_http_ready(
                &self.inner.client,
                &self.inner.models_url,
                self.inner.startup_timeout,
                self.inner.poll_interval,
            )
            .await
        {
            let _ = child
                .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
                .await;
            log_task.abort();
            return Err(error.into());
        }
        *slot = Some(SupervisedChild {
            child,
            _log_forwarders: forwarders,
            log_task,
        });
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    async fn start_inner(&self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("managed DS4 supervision requires macOS")
    }

    async fn stop_inner(&self) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        let Some(mut current) = slot.take() else {
            return Ok(());
        };
        let result = current
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await;
        current.log_task.abort();
        result?;
        Ok(())
    }

    async fn is_running_inner(&self) -> anyhow::Result<bool> {
        let mut slot = self.inner.child.lock().await;
        let Some(current) = slot.as_mut() else {
            return Ok(false);
        };
        Ok(current.child.try_wait()?.is_none())
    }
}

impl DistributedWorkerSupervisor {
    pub fn new(command: Ds4Command, stop_timeout: Duration, allow_sigkill: bool) -> Self {
        Self {
            inner: Arc::new(DistributedWorkerSupervisorInner {
                command,
                stop_timeout,
                allow_sigkill,
                child: tokio::sync::Mutex::new(None),
            }),
        }
    }

    pub async fn child_identity(&self) -> Option<ChildIdentity> {
        self.inner
            .child
            .lock()
            .await
            .as_ref()
            .map(|current| current.child.identity().clone())
    }

    #[cfg(target_os = "macos")]
    async fn start_inner(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Ok(());
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
                tracing::info!(
                    profile = %record.profile_id,
                    generation = record.generation,
                    pid = record.pid,
                    stream = ?record.stream,
                    truncated = record.truncated,
                    event = ?record.event,
                    line_bytes = record.line.len(),
                    "DS4 distributed worker log"
                );
            }
        });
        *slot = Some(SupervisedChild {
            child,
            _log_forwarders: forwarders,
            log_task,
        });
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    async fn start_inner(&self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("managed DS4 supervision requires macOS")
    }

    async fn stop_inner(&self) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        let Some(mut current) = slot.take() else {
            return Ok(());
        };
        let result = current
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await;
        current.log_task.abort();
        result?;
        Ok(())
    }

    async fn is_running_inner(&self) -> anyhow::Result<bool> {
        let mut slot = self.inner.child.lock().await;
        let Some(current) = slot.as_mut() else {
            return Ok(false);
        };
        Ok(current.child.try_wait()?.is_none())
    }
}

impl DistributedCoordinatorSupervisor {
    pub fn new(
        command: Ds4Command,
        models_url: Url,
        http_startup_timeout: Duration,
        poll_interval: Duration,
        stop_timeout: Duration,
        allow_sigkill: bool,
    ) -> Self {
        let (route, _) = tokio::sync::watch::channel(CoordinatorRouteState::default());
        Self {
            inner: Arc::new(DistributedCoordinatorSupervisorInner {
                command,
                models_url,
                client: reqwest::Client::new(),
                http_startup_timeout,
                poll_interval,
                stop_timeout,
                allow_sigkill,
                child: tokio::sync::Mutex::new(None),
                route,
            }),
        }
    }

    #[cfg(target_os = "macos")]
    async fn start_inner(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Ok(());
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        self.inner
            .route
            .send_replace(CoordinatorRouteState::default());
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let stdout = child
            .take_stdout()
            .ok_or(ProcessControlError::MissingLogPipe)?;
        let stderr = child
            .take_stderr()
            .ok_or(ProcessControlError::MissingLogPipe)?;
        let (mut logs, mut events, forwarders) = spawn_child_log_forwarders_with_events(
            stdout,
            stderr,
            Arc::from(child.identity().profile_id.as_str()),
            child.identity().generation,
            child.identity().pid,
            256,
        );
        let route = self.inner.route.clone();
        let log_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = events.recv() => {
                        let mut state = *route.borrow();
                        match event {
                            Ds4LogEvent::WorkerRegistered { .. } => state.worker_registered = true,
                            Ds4LogEvent::CompleteRouteReady { .. } => state.complete_route = true,
                            Ds4LogEvent::WorkerRemoved { .. } => {
                                state.worker_registered = false;
                                state.complete_route = false;
                            }
                            Ds4LogEvent::RouteIncomplete { .. } => state.complete_route = false,
                            Ds4LogEvent::HttpListening { .. } => {}
                        }
                        route.send_replace(state);
                    }
                    Some(record) = logs.recv() => {
                        tracing::info!(
                            profile = %record.profile_id,
                            generation = record.generation,
                            pid = record.pid,
                            stream = ?record.stream,
                            truncated = record.truncated,
                            event = ?record.event,
                            line_bytes = record.line.len(),
                            "DS4 distributed coordinator log"
                        );
                    }
                    else => break,
                }
            }
        });
        if let Err(error) = child
            .wait_http_ready(
                &self.inner.client,
                &self.inner.models_url,
                self.inner.http_startup_timeout,
                self.inner.poll_interval,
            )
            .await
        {
            let _ = child
                .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
                .await;
            log_task.abort();
            return Err(error.into());
        }
        let mut state = *self.inner.route.borrow();
        state.http_ready = true;
        self.inner.route.send_replace(state);
        *slot = Some(SupervisedChild {
            child,
            _log_forwarders: forwarders,
            log_task,
        });
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    async fn start_inner(&self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("managed DS4 supervision requires macOS")
    }

    async fn stop_inner(&self) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        let Some(mut current) = slot.take() else {
            self.inner
                .route
                .send_replace(CoordinatorRouteState::default());
            return Ok(());
        };
        let result = current
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await;
        current.log_task.abort();
        self.inner
            .route
            .send_replace(CoordinatorRouteState::default());
        result?;
        Ok(())
    }

    async fn is_running_inner(&self) -> anyhow::Result<bool> {
        let mut slot = self.inner.child.lock().await;
        let Some(current) = slot.as_mut() else {
            return Ok(false);
        };
        Ok(current.child.try_wait()?.is_none())
    }

    async fn wait_ready_inner(&self) -> anyhow::Result<()> {
        let mut route = self.inner.route.subscribe();
        loop {
            let state = *route.borrow_and_update();
            if state.http_ready && state.worker_registered && state.complete_route {
                return Ok(());
            }
            if !self.is_running_inner().await? {
                anyhow::bail!("distributed coordinator exited before complete route");
            }
            tokio::select! {
                result = route.changed() => result.map_err(|_| anyhow::anyhow!("route monitor stopped"))?,
                () = tokio::time::sleep(self.inner.poll_interval) => {}
            }
        }
    }

    async fn wait_route_loss_inner(&self) -> anyhow::Result<()> {
        let mut route = self.inner.route.subscribe();
        loop {
            let state = *route.borrow_and_update();
            if !state.http_ready || !state.worker_registered || !state.complete_route {
                return Ok(());
            }
            if !self.is_running_inner().await? {
                return Ok(());
            }
            tokio::select! {
                result = route.changed() => result.map_err(|_| anyhow::anyhow!("route monitor stopped"))?,
                () = tokio::time::sleep(self.inner.poll_interval) => {}
            }
        }
    }
}

impl LocalStandaloneLifecycle for StandaloneSupervisor {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.start_inner(generation).await })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.stop_inner().await })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.is_running_inner().await })
    }
}

impl DistributedWorkerLifecycle for DistributedWorkerSupervisor {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.start_inner(generation).await })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.stop_inner().await })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.is_running_inner().await })
    }
}

impl DistributedCoordinatorLifecycle for DistributedCoordinatorSupervisor {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.start_inner(generation).await })
    }

    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.wait_ready_inner().await })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.stop_inner().await })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.wait_route_loss_inner().await })
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
            Err(ProcessControlError::NotRunning) => return Ok(self.child.wait().await?),
            Err(error) => return Err(error),
        }
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(status) => Ok(status?),
            Err(_) if !allow_sigkill => Err(ProcessControlError::SigkillNotAllowed),
            Err(_) => {
                match self
                    .controller
                    .signal_owned(&self.identity, ProcessSignal::Kill)
                {
                    Ok(()) => {}
                    Err(ProcessControlError::NotRunning) => return Ok(self.child.wait().await?),
                    Err(error) => return Err(error),
                }
                tokio::time::timeout(timeout, self.child.wait())
                    .await
                    .map_err(|_| ProcessControlError::StopTimeout)?
                    .map_err(ProcessControlError::Io)
            }
        }
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
            },
        };
        let supervisor = DistributedWorkerSupervisor::new(command, Duration::from_secs(2), false);
        supervisor.start(13).await.unwrap();
        assert!(supervisor.is_running().await.unwrap());
        assert_eq!(supervisor.child_identity().await.unwrap().generation, 13);

        supervisor.stop().await.unwrap();
        assert!(!supervisor.is_running().await.unwrap());
        assert!(supervisor.child_identity().await.is_none());
    }
}
