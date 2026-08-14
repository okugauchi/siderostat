use super::{
    AuthenticatedPeer, ChildIdentity, ClusterFailure, ControlCommand, ControlEndpoint,
    ControlError, ControlMessage, ControlResponse, ControlResponseStatus, ControlRole,
    DistributedControlPhase, NodeDescriptor, PeerLease, WorkerEventKind, control::ControlProcessor,
    runtime::LocalStandaloneLifecycle,
};
use crate::admission::{AdmissionGate, DrainError};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use thiserror::Error;

pub trait DistributedWorkerLifecycle: Send + Sync + 'static {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>>;
    /// Optional child identity for diagnostics. Defaults to `None`.
    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

pub trait WorkerLeaseStatus: Send + Sync + 'static {
    fn is_valid(&self) -> bool;
}

impl<F> WorkerLeaseStatus for F
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    fn is_valid(&self) -> bool {
        self()
    }
}

#[derive(Debug, Error)]
pub enum WorkerLifecycleError {
    #[error("worker lifecycle timeouts must be positive")]
    InvalidTiming,
    #[error(transparent)]
    Drain(#[from] DrainError),
    #[error("standalone lifecycle failed: {0}")]
    Standalone(#[source] anyhow::Error),
    #[error("distributed worker lifecycle failed: {0}")]
    Worker(#[source] anyhow::Error),
    #[error("distributed worker startup timed out")]
    StartupTimeout,
    #[error("distributed worker exited during startup")]
    EarlyExit,
    #[error("coordinator lease was lost")]
    LeaseLost,
    #[error("distributed worker cleanup failed after {cause}: {cleanup}")]
    Cleanup {
        cause: Box<WorkerLifecycleError>,
        #[source]
        cleanup: anyhow::Error,
    },
}

impl WorkerLifecycleError {
    pub fn cluster_failure(&self) -> ClusterFailure {
        match self {
            Self::StartupTimeout | Self::EarlyExit => ClusterFailure::HelloTimeout,
            Self::LeaseLost => ClusterFailure::PeerLeaseLost,
            Self::Drain(_) => ClusterFailure::DrainTimeout,
            Self::Standalone(_) | Self::Worker(_) | Self::Cleanup { .. } => {
                ClusterFailure::ChildIdentityUnknown
            }
            Self::InvalidTiming => ClusterFailure::StateCorrupt {
                standalone_safe: false,
            },
        }
    }
}

#[derive(Clone)]
pub struct WorkerDistributedRuntime {
    admission: AdmissionGate,
    standalone: Arc<dyn LocalStandaloneLifecycle>,
    worker: Arc<dyn DistributedWorkerLifecycle>,
    drain_timeout: Duration,
    startup_timeout: Duration,
    poll_interval: Duration,
}

impl WorkerDistributedRuntime {
    pub fn new(
        admission: AdmissionGate,
        standalone: Arc<dyn LocalStandaloneLifecycle>,
        worker: Arc<dyn DistributedWorkerLifecycle>,
        drain_timeout: Duration,
        startup_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, WorkerLifecycleError> {
        if drain_timeout.is_zero() || startup_timeout.is_zero() || poll_interval.is_zero() {
            return Err(WorkerLifecycleError::InvalidTiming);
        }
        Ok(Self {
            admission,
            standalone,
            worker,
            drain_timeout,
            startup_timeout,
            poll_interval,
        })
    }

    pub async fn prepare(
        &self,
        generation: u64,
        lease: Arc<dyn WorkerLeaseStatus>,
    ) -> Result<(), WorkerLifecycleError> {
        self.admission.drain(generation, self.drain_timeout).await?;
        if !lease.is_valid() {
            return self.fail_and_cleanup(WorkerLifecycleError::LeaseLost).await;
        }
        if let Err(error) = self.standalone.stop().await {
            return self
                .fail_and_cleanup(WorkerLifecycleError::Standalone(error))
                .await;
        }
        if !lease.is_valid() {
            return self.fail_and_cleanup(WorkerLifecycleError::LeaseLost).await;
        }

        match tokio::time::timeout(self.startup_timeout, self.worker.start(generation)).await {
            Err(_) => {
                return self
                    .fail_and_cleanup(WorkerLifecycleError::StartupTimeout)
                    .await;
            }
            Ok(Err(error)) => {
                return self
                    .fail_and_cleanup(WorkerLifecycleError::Worker(error))
                    .await;
            }
            Ok(Ok(())) => {}
        }
        if !lease.is_valid() {
            return self.fail_and_cleanup(WorkerLifecycleError::LeaseLost).await;
        }
        match self.worker.is_running().await {
            Ok(true) => Ok(()),
            Ok(false) => self.fail_and_cleanup(WorkerLifecycleError::EarlyExit).await,
            Err(error) => {
                self.fail_and_cleanup(WorkerLifecycleError::Worker(error))
                    .await
            }
        }
    }

    pub async fn wait_for_failure(
        &self,
        lease: Arc<dyn WorkerLeaseStatus>,
    ) -> WorkerLifecycleError {
        loop {
            if !lease.is_valid() {
                return self.cleanup_failure(WorkerLifecycleError::LeaseLost).await;
            }
            match self.worker.is_running().await {
                Ok(true) => tokio::time::sleep(self.poll_interval).await,
                Ok(false) => {
                    return self.cleanup_failure(WorkerLifecycleError::EarlyExit).await;
                }
                Err(error) => {
                    return self
                        .cleanup_failure(WorkerLifecycleError::Worker(error))
                        .await;
                }
            }
        }
    }

    pub async fn cancel(&self) -> Result<(), WorkerLifecycleError> {
        self.admission.block();
        self.worker
            .stop()
            .await
            .map_err(WorkerLifecycleError::Worker)
    }

    async fn fail_and_cleanup<T>(
        &self,
        cause: WorkerLifecycleError,
    ) -> Result<T, WorkerLifecycleError> {
        Err(self.cleanup_failure(cause).await)
    }

    async fn cleanup_failure(&self, cause: WorkerLifecycleError) -> WorkerLifecycleError {
        self.admission.block();
        match self.worker.stop().await {
            Ok(()) => cause,
            Err(cleanup) => WorkerLifecycleError::Cleanup {
                cause: Box::new(cause),
                cleanup,
            },
        }
    }
}

#[derive(Debug)]
pub struct WorkerControl {
    processor: ControlProcessor,
    phase: DistributedControlPhase,
}

impl WorkerControl {
    pub fn new(
        descriptor: NodeDescriptor,
        lease: Duration,
        required_stability: Duration,
    ) -> Result<Self, ControlError> {
        if descriptor.role != ControlRole::Worker || descriptor.protocol_version != 1 {
            return Err(ControlError::InvalidDescriptor);
        }
        Ok(Self {
            processor: ControlProcessor::new(
                descriptor,
                ControlRole::Coordinator,
                lease,
                required_stability,
            ),
            phase: DistributedControlPhase::Unpaired,
        })
    }

    pub fn node_descriptor(
        &mut self,
        authenticated: &AuthenticatedPeer,
        route_scoped: bool,
        now_millis: u64,
    ) -> Result<ControlResponse, ControlError> {
        self.processor
            .descriptor_response(authenticated, route_scoped, now_millis)
    }

    pub fn handle(
        &mut self,
        endpoint: ControlEndpoint,
        message: ControlMessage,
        authenticated: &AuthenticatedPeer,
        route_scoped: bool,
        now_millis: u64,
    ) -> Result<ControlResponse, ControlError> {
        if !matches!(
            message.command,
            ControlCommand::Pair { .. }
                | ControlCommand::PrepareWorker
                | ControlCommand::BeginDrain
                | ControlCommand::DistributedReady
                | ControlCommand::CancelGeneration
                | ControlCommand::Demote
        ) {
            return Err(ControlError::CommandNotAllowed);
        }
        let phase = self.phase;
        let command = message.command.clone();
        let response = self.processor.handle_validated(
            endpoint,
            message,
            authenticated,
            route_scoped,
            now_millis,
            |command| validate_worker_command(phase, command),
        )?;
        if response.status == ControlResponseStatus::Applied {
            self.phase = match command {
                ControlCommand::Pair { .. } => DistributedControlPhase::Paired,
                ControlCommand::PrepareWorker => DistributedControlPhase::WorkerPreparing,
                ControlCommand::BeginDrain => DistributedControlPhase::Draining,
                ControlCommand::DistributedReady => DistributedControlPhase::WorkerReady,
                ControlCommand::CancelGeneration | ControlCommand::Demote => {
                    DistributedControlPhase::Paired
                }
                _ => phase,
            };
        }
        Ok(response)
    }

    pub fn peer_present(&self, now_millis: u64) -> bool {
        self.processor.lease().peer_present(now_millis)
    }

    pub fn peer_lease(&self) -> &PeerLease {
        self.processor.lease()
    }

    pub fn generation(&self) -> u64 {
        self.processor.generation()
    }

    pub fn invalidate_route(&mut self) {
        self.processor.lease_mut().invalidate_route();
    }

    pub fn advance_generation(&mut self, generation: u64) {
        self.processor.advance_generation(generation);
        self.phase = if self.processor.lease().descriptor().is_some() {
            DistributedControlPhase::Paired
        } else {
            DistributedControlPhase::Unpaired
        };
    }

    pub fn worker_ready_message(
        &mut self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::WorkerPreparing {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::WorkerReady;
        Ok(self.processor.message(
            request_id.into(),
            ControlCommand::WorkerEvent {
                event: WorkerEventKind::Ready,
            },
        ))
    }

    pub fn drained_message(
        &mut self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::Draining {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::Drained;
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::Drained))
    }

    pub fn phase(&self) -> DistributedControlPhase {
        self.phase
    }
}

fn validate_worker_command(
    phase: DistributedControlPhase,
    command: &ControlCommand,
) -> Result<(), ControlError> {
    let valid = match command {
        ControlCommand::Pair { .. } => true,
        ControlCommand::PrepareWorker => phase == DistributedControlPhase::Paired,
        ControlCommand::BeginDrain => phase == DistributedControlPhase::WorkerReady,
        ControlCommand::DistributedReady => phase == DistributedControlPhase::Drained,
        ControlCommand::CancelGeneration => matches!(
            phase,
            DistributedControlPhase::WorkerPreparing
                | DistributedControlPhase::WorkerReady
                | DistributedControlPhase::Draining
                | DistributedControlPhase::Drained
        ),
        ControlCommand::Demote => !matches!(phase, DistributedControlPhase::Unpaired),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ControlError::InvalidPhase { phase })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ControlMode;
    use std::{
        collections::VecDeque,
        net::IpAddr,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    #[derive(Clone, Copy)]
    enum StartBehavior {
        Running,
        Exit,
        Hang,
    }

    struct FakeWorker {
        behaviors: Mutex<VecDeque<StartBehavior>>,
        running: AtomicBool,
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl FakeWorker {
        fn new(behaviors: impl IntoIterator<Item = StartBehavior>) -> Self {
            Self {
                behaviors: Mutex::new(behaviors.into_iter().collect()),
                running: AtomicBool::new(false),
                starts: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
            }
        }
    }

    impl DistributedWorkerLifecycle for FakeWorker {
        fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let behavior = self
                .behaviors
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(StartBehavior::Running);
            match behavior {
                StartBehavior::Running => {
                    self.running.store(true, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                }
                StartBehavior::Exit => {
                    self.running.store(false, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                }
                StartBehavior::Hang => {
                    self.running.store(true, Ordering::SeqCst);
                    Box::pin(std::future::pending())
                }
            }
        }

        fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.running.store(false, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
            let running = self.running.load(Ordering::SeqCst);
            Box::pin(async move { Ok(running) })
        }
    }

    #[derive(Default)]
    struct FakeStandalone {
        stops: AtomicUsize,
    }

    impl LocalStandaloneLifecycle for FakeStandalone {
        fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
            Box::pin(async { Ok(false) })
        }
    }

    fn distributed_runtime(
        worker: Arc<FakeWorker>,
        standalone: Arc<FakeStandalone>,
    ) -> (AdmissionGate, WorkerDistributedRuntime) {
        let admission = AdmissionGate::new(4);
        admission.start_serving();
        let runtime = WorkerDistributedRuntime::new(
            admission.clone(),
            standalone,
            worker,
            Duration::from_millis(50),
            Duration::from_millis(20),
            Duration::from_millis(2),
        )
        .unwrap();
        (admission, runtime)
    }

    fn descriptor(role: ControlRole, node_id: &str) -> NodeDescriptor {
        NodeDescriptor {
            protocol_version: 1,
            node_id: node_id.into(),
            role,
            generation: 3,
            mode: ControlMode::SoloStandalone,
            deployment_id: Some("deployment-a".into()),
        }
    }

    #[test]
    fn worker_accepts_only_an_authenticated_coordinator_descriptor() {
        let mut worker = WorkerControl::new(
            descriptor(ControlRole::Worker, "worker"),
            Duration::from_secs(15),
            Duration::from_secs(5),
        )
        .unwrap();
        let authenticated =
            AuthenticatedPeer::new_for_test("coordinator", IpAddr::from([10, 99, 0, 1]), 1_000);
        let pair = ControlMessage {
            request_id: "pair-1".into(),
            generation: 3,
            deployment_id: None,
            command: ControlCommand::Pair {
                descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
            },
        };
        assert!(
            worker
                .handle(ControlEndpoint::Pair, pair, &authenticated, true, 1_000,)
                .is_ok()
        );

        let forbidden = ControlMessage {
            request_id: "event-1".into(),
            generation: 3,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::WorkerEvent {
                event: WorkerEventKind::Ready,
            },
        };
        assert_eq!(
            worker.handle(
                ControlEndpoint::WorkerEvent,
                forbidden,
                &authenticated,
                true,
                2_000,
            ),
            Err(ControlError::CommandNotAllowed)
        );
    }

    #[test]
    fn worker_prepare_ready_drain_is_idempotent_and_cancel_handles_drop() {
        let mut worker = WorkerControl::new(
            descriptor(ControlRole::Worker, "worker"),
            Duration::from_secs(15),
            Duration::ZERO,
        )
        .unwrap();
        let authenticated =
            AuthenticatedPeer::new_for_test("coordinator", IpAddr::from([10, 99, 0, 1]), 1_000);
        let pair = ControlMessage {
            request_id: "pair-flow".into(),
            generation: 3,
            deployment_id: None,
            command: ControlCommand::Pair {
                descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
            },
        };
        worker
            .handle(ControlEndpoint::Pair, pair, &authenticated, true, 1_000)
            .unwrap();
        let prepare = ControlMessage {
            request_id: "prepare-flow".into(),
            generation: 3,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::PrepareWorker,
        };
        worker
            .handle(
                ControlEndpoint::PrepareWorker,
                prepare.clone(),
                &authenticated,
                true,
                1_001,
            )
            .unwrap();
        assert_eq!(worker.phase(), DistributedControlPhase::WorkerPreparing);
        assert_eq!(
            worker
                .handle(
                    ControlEndpoint::PrepareWorker,
                    prepare,
                    &authenticated,
                    true,
                    1_002,
                )
                .unwrap()
                .status,
            ControlResponseStatus::Duplicate
        );

        let drain = ControlMessage {
            request_id: "drain-flow".into(),
            generation: 3,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::BeginDrain,
        };
        assert!(matches!(
            worker.handle(
                ControlEndpoint::BeginDrain,
                drain.clone(),
                &authenticated,
                true,
                1_003,
            ),
            Err(ControlError::InvalidPhase { .. })
        ));
        let ready = worker.worker_ready_message("ready-flow").unwrap();
        assert_eq!(ready.generation, 3);
        assert_eq!(ready.deployment_id.as_deref(), Some("deployment-a"));
        worker
            .handle(
                ControlEndpoint::BeginDrain,
                drain,
                &authenticated,
                true,
                1_004,
            )
            .unwrap();
        let drained = worker.drained_message("drained-flow").unwrap();
        assert_eq!(drained.command, ControlCommand::Drained);
        assert_eq!(worker.phase(), DistributedControlPhase::Drained);

        let cancel = ControlMessage {
            request_id: "cancel-flow".into(),
            generation: 3,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::CancelGeneration,
        };
        worker
            .handle(
                ControlEndpoint::CancelGeneration,
                cancel,
                &authenticated,
                true,
                1_005,
            )
            .unwrap();
        assert_eq!(worker.phase(), DistributedControlPhase::Paired);
    }

    #[tokio::test]
    async fn worker_prepare_drains_standalone_and_starts_one_child() {
        let worker = Arc::new(FakeWorker::new([StartBehavior::Running]));
        let standalone = Arc::new(FakeStandalone::default());
        let (admission, runtime) = distributed_runtime(worker.clone(), standalone.clone());

        runtime.prepare(9, Arc::new(|| true)).await.unwrap();

        assert_eq!(
            admission.snapshot().state,
            crate::admission::AdmissionState::Blocked
        );
        assert_eq!(standalone.stops.load(Ordering::SeqCst), 1);
        assert_eq!(worker.starts.load(Ordering::SeqCst), 1);
        assert!(worker.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn worker_start_timeout_cleans_up_without_an_orphan() {
        let worker = Arc::new(FakeWorker::new([StartBehavior::Hang]));
        let standalone = Arc::new(FakeStandalone::default());
        let (_, runtime) = distributed_runtime(worker.clone(), standalone);

        assert!(matches!(
            runtime.prepare(10, Arc::new(|| true)).await,
            Err(WorkerLifecycleError::StartupTimeout)
        ));
        assert!(!worker.running.load(Ordering::SeqCst));
        assert_eq!(worker.stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn early_exit_is_cleaned_and_same_generation_can_retry() {
        let worker = Arc::new(FakeWorker::new([
            StartBehavior::Exit,
            StartBehavior::Running,
        ]));
        let standalone = Arc::new(FakeStandalone::default());
        let (_, runtime) = distributed_runtime(worker.clone(), standalone);

        assert!(matches!(
            runtime.prepare(11, Arc::new(|| true)).await,
            Err(WorkerLifecycleError::EarlyExit)
        ));
        assert!(!worker.running.load(Ordering::SeqCst));
        runtime.prepare(11, Arc::new(|| true)).await.unwrap();
        assert_eq!(worker.starts.load(Ordering::SeqCst), 2);
        assert!(worker.running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn valid_lease_allows_ds4_reconnect_then_loss_stops_child() {
        let worker = Arc::new(FakeWorker::new([StartBehavior::Running]));
        let standalone = Arc::new(FakeStandalone::default());
        let (_, runtime) = distributed_runtime(worker.clone(), standalone);
        let valid = Arc::new(AtomicBool::new(true));
        let lease_flag = valid.clone();
        let lease: Arc<dyn WorkerLeaseStatus> = Arc::new(move || lease_flag.load(Ordering::SeqCst));
        runtime.prepare(12, lease.clone()).await.unwrap();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(8),
                runtime.wait_for_failure(lease.clone())
            )
            .await
            .is_err()
        );
        assert_eq!(worker.starts.load(Ordering::SeqCst), 1);
        assert!(worker.running.load(Ordering::SeqCst));

        valid.store(false, Ordering::SeqCst);
        assert!(matches!(
            runtime.wait_for_failure(lease).await,
            WorkerLifecycleError::LeaseLost
        ));
        assert!(!worker.running.load(Ordering::SeqCst));
        assert_eq!(worker.stops.load(Ordering::SeqCst), 1);
    }
}
