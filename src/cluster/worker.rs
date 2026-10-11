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

/// TP worker のライフサイクル。Prepared（child 開始と生存のみ）と Connected（DS4
/// の session 完了観測）を分離して公開する（C02）。coordinator 起動を待たない。
pub trait TpWorkerLifecycle: Send + Sync + 'static {
    /// worker child を起動し、生成と生存だけの Prepared を返す。coordinator は不要。
    /// `worker_connected=true` 固定値は返さない。
    fn prepare(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<TpWorkerPrepared>>;
    /// DS4 の session 完了観測（例: Ds4LogEvent::WorkerRegistered / CompleteRouteReady）を
    /// Connected として記録する。実観測がなければ Connected にならない。
    fn observe_connected(
        &self,
        observation: TpConnectedObservation,
    ) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>>;
    /// Optional child identity for diagnostics. Defaults to `None`。
    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

/// TP worker の Prepared 結果。child 生成・生存のみを表す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TpWorkerPrepared {
    /// 起動した child の generation。
    pub generation: u64,
    /// child identity（あれば）。PID 再利用・identity 不一致の検出に用いる。
    pub identity: Option<ChildIdentity>,
}

/// Connected 観測の種類。DS4 の実ログ観測に由来する。固定値禁止（C02）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpConnectedObservation {
    /// Ds4LogEvent::WorkerRegistered 等の worker session 完了観測。
    WorkerSessionReady,
    /// Ds4LogEvent::CompleteRouteReady 等の route 完了観測。
    CompleteRouteReady,
}

/// TP worker の純粋状態機械。Prepared / Connected / Failed / Cancelled を追跡する。
/// 実プロセス操作は行わず、状態遷移の判定のみを提供する（C02）。reducer と同じく
/// 第二の実装経路を作らず、fake でも本番でも同じ判定を使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpWorkerPhase {
    /// まだ起動していない。
    Idle,
    /// child 生成・生存のみ確認済み。coordinator 起動は未完了でもよい。
    Prepared,
    /// DS4 の session 完了観測あり。route 公開可能。
    Connected,
    /// early exit / identity 不明などの失敗。
    Failed,
    /// cancel 済み。後続の late ready は無視。
    Cancelled,
}

impl TpWorkerPhase {
    pub fn name(self) -> &'static str {
        match self {
            TpWorkerPhase::Idle => "idle",
            TpWorkerPhase::Prepared => "prepared",
            TpWorkerPhase::Connected => "connected",
            TpWorkerPhase::Failed => "failed",
            TpWorkerPhase::Cancelled => "cancelled",
        }
    }
}

/// TP worker の状態遷移を表す。不変値ベースで、`transition` が新状態を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpWorkerTracker {
    phase: TpWorkerPhase,
    /// 現在の session 世代。旧 session の late ready を無視するために用いる。
    generation: u64,
    cancelled: bool,
}

impl TpWorkerTracker {
    pub fn new() -> Self {
        Self {
            phase: TpWorkerPhase::Idle,
            generation: 0,
            cancelled: false,
        }
    }

    pub fn phase(&self) -> TpWorkerPhase {
        self.phase
    }

    /// worker の Prepared を記録する。coordinator 起動を待たずに Prepared になる。
    pub fn note_prepared(&mut self, generation: u64) -> TpWorkerPhase {
        if self.cancelled {
            self.phase = TpWorkerPhase::Cancelled;
            return self.phase;
        }
        self.generation = generation;
        self.phase = TpWorkerPhase::Prepared;
        self.phase
    }

    /// DS4 の session 完了観測を Connected として記録する。
    /// Prepared 前・cancel 後・世代不一致の観測は無視して現在状態を返す。
    pub fn note_connected(&mut self, generation: u64) -> TpWorkerPhase {
        if self.cancelled || self.phase != TpWorkerPhase::Prepared {
            return self.phase;
        }
        if generation != self.generation {
            // 旧世代の late ready は無視。
            return self.phase;
        }
        self.phase = TpWorkerPhase::Connected;
        self.phase
    }

    /// early exit / identity 不明などの失敗。cancel 済みなら失敗にしない。
    pub fn note_failed(&mut self) -> TpWorkerPhase {
        if self.cancelled {
            self.phase = TpWorkerPhase::Cancelled;
            return self.phase;
        }
        self.phase = TpWorkerPhase::Failed;
        self.phase
    }

    /// cancel。後続の late ready は無視される。既に Connected なら維持する。
    pub fn cancel(&mut self) -> TpWorkerPhase {
        self.cancelled = true;
        if self.phase == TpWorkerPhase::Connected {
            self.phase
        } else {
            self.phase = TpWorkerPhase::Cancelled;
            self.phase
        }
    }
}

impl Default for TpWorkerTracker {
    fn default() -> Self {
        Self::new()
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
                | ControlCommand::PrepareRestart
                | ControlCommand::CancelRestart
        ) {
            return Err(ControlError::CommandNotAllowed);
        }
        let phase = self.phase;
        let command = message.command.clone();
        let peer_present = self.processor.lease().peer_present(now_millis);
        let response = self.processor.handle_validated(
            endpoint,
            message,
            authenticated,
            route_scoped,
            now_millis,
            |command| validate_worker_command(phase, peer_present, command),
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
                ControlCommand::PrepareRestart | ControlCommand::CancelRestart => phase,
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

    pub fn reset_for_repair(&mut self, now_millis: u64) {
        self.phase = if self.processor.lease().peer_present(now_millis) {
            DistributedControlPhase::Paired
        } else {
            DistributedControlPhase::Unpaired
        };
    }

    pub fn worker_ready_message(
        &mut self,
        request_id: impl Into<String>,
        child_generation: u64,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::WorkerPreparing {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::WorkerReady;
        Ok(self.processor.message(
            request_id.into(),
            ControlCommand::WorkerEvent {
                event: WorkerEventKind::ReadyWithChildGeneration { child_generation },
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

    /// 分散worker childの異常終了をcoordinatorへ通知する。authenticatedな故障通知として
    /// coordinator側のrecovery ownerへ渡し、route monitorより早く安全遷移を開始できる。
    pub fn child_exited_message(
        &mut self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if !matches!(
            self.phase,
            DistributedControlPhase::WorkerReady
                | DistributedControlPhase::Draining
                | DistributedControlPhase::Drained
        ) {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::Paired;
        Ok(self.processor.message(
            request_id.into(),
            ControlCommand::WorkerEvent {
                event: WorkerEventKind::Exited,
            },
        ))
    }

    pub fn phase(&self) -> DistributedControlPhase {
        self.phase
    }
}

fn validate_worker_command(
    phase: DistributedControlPhase,
    peer_present: bool,
    command: &ControlCommand,
) -> Result<(), ControlError> {
    let valid = match command {
        // A new Pair is valid while unpaired or paired, and after the current peer lease has
        // actually been lost. Do not let a delayed Pair reset WorkerPreparing/WorkerReady/
        // Draining/Drained during promotion; that would leave the mode state at
        // DistributedReady while DistributedReady control is rejected as out of order.
        ControlCommand::Pair { .. } => {
            matches!(
                phase,
                DistributedControlPhase::Unpaired | DistributedControlPhase::Paired
            ) || !peer_present
        }
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
        ControlCommand::PrepareRestart | ControlCommand::CancelRestart => {
            !matches!(phase, DistributedControlPhase::Unpaired)
        }
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

    #[test]
    fn tp_worker_prepared_does_not_require_coordinator() {
        let mut tracker = TpWorkerTracker::new();
        assert_eq!(tracker.phase(), TpWorkerPhase::Idle);
        // coordinator 未起動でも Prepared になる（C02: worker spawn + 生存のみ）。
        assert_eq!(tracker.note_prepared(7), TpWorkerPhase::Prepared);
        assert_eq!(tracker.phase(), TpWorkerPhase::Prepared);
    }

    #[test]
    fn tp_worker_connected_only_after_observation() {
        let mut tracker = TpWorkerTracker::new();
        // Prepared 前に Connected 観測は無視。
        assert_eq!(tracker.note_connected(1), TpWorkerPhase::Idle);
        tracker.note_prepared(1);
        // 世代不一致の観測は無視。
        assert_eq!(tracker.note_connected(2), TpWorkerPhase::Prepared);
        // 同世代の実観測で Connected。
        assert_eq!(tracker.note_connected(1), TpWorkerPhase::Connected);
        assert_eq!(tracker.phase(), TpWorkerPhase::Connected);
    }

    #[test]
    fn tp_worker_early_exit_is_failed() {
        let mut tracker = TpWorkerTracker::new();
        tracker.note_prepared(3);
        assert_eq!(tracker.note_failed(), TpWorkerPhase::Failed);
        assert_eq!(tracker.phase(), TpWorkerPhase::Failed);
    }

    #[test]
    fn tp_worker_cancel_ignores_late_ready() {
        let mut tracker = TpWorkerTracker::new();
        tracker.note_prepared(5);
        assert_eq!(tracker.cancel(), TpWorkerPhase::Cancelled);
        // cancel 後の late ready は無視（Connected に進まない）。
        assert_eq!(tracker.note_connected(5), TpWorkerPhase::Cancelled);
        // cancel 後の note_prepared も Cancelled のまま。
        assert_eq!(tracker.note_prepared(6), TpWorkerPhase::Cancelled);
    }

    #[test]
    fn tp_worker_phase_names_are_stable() {
        assert_eq!(TpWorkerPhase::Idle.name(), "idle");
        assert_eq!(TpWorkerPhase::Prepared.name(), "prepared");
        assert_eq!(TpWorkerPhase::Connected.name(), "connected");
        assert_eq!(TpWorkerPhase::Failed.name(), "failed");
        assert_eq!(TpWorkerPhase::Cancelled.name(), "cancelled");
    }

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
            worker.handle(
                ControlEndpoint::Pair,
                ControlMessage {
                    request_id: "delayed-pair".into(),
                    generation: 3,
                    deployment_id: None,
                    command: ControlCommand::Pair {
                        descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
                    },
                },
                &authenticated,
                true,
                1_003,
            ),
            Err(ControlError::InvalidPhase {
                phase: DistributedControlPhase::WorkerPreparing,
            })
        );
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
        let ready = worker.worker_ready_message("ready-flow", 42).unwrap();
        assert_eq!(ready.generation, 3);
        assert_eq!(ready.deployment_id.as_deref(), Some("deployment-a"));
        assert_eq!(
            ready.command,
            ControlCommand::WorkerEvent {
                event: WorkerEventKind::ReadyWithChildGeneration {
                    child_generation: 42,
                },
            }
        );
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

    #[test]
    fn worker_accepts_planned_restart_commands_only_after_pairing() {
        let mut worker = WorkerControl::new(
            descriptor(ControlRole::Worker, "worker"),
            Duration::from_secs(15),
            Duration::ZERO,
        )
        .unwrap();
        let authenticated =
            AuthenticatedPeer::new_for_test("coordinator", IpAddr::from([10, 99, 0, 1]), 1_000);
        let prepare = ControlMessage {
            request_id: "prepare-restart-unpaired".into(),
            generation: 3,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::PrepareRestart,
        };
        assert!(matches!(
            worker.handle(
                ControlEndpoint::PrepareRestart,
                prepare,
                &authenticated,
                true,
                1_000,
            ),
            Err(ControlError::InvalidPhase {
                phase: DistributedControlPhase::Unpaired
            })
        ));

        worker
            .handle(
                ControlEndpoint::Pair,
                ControlMessage {
                    request_id: "planned-restart-pair".into(),
                    generation: 3,
                    deployment_id: None,
                    command: ControlCommand::Pair {
                        descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
                    },
                },
                &authenticated,
                true,
                1_001,
            )
            .unwrap();
        worker
            .handle(
                ControlEndpoint::PrepareWorker,
                ControlMessage {
                    request_id: "planned-restart-prepare-worker".into(),
                    generation: 3,
                    deployment_id: Some("deployment-a".into()),
                    command: ControlCommand::PrepareWorker,
                },
                &authenticated,
                true,
                1_002,
            )
            .unwrap();
        worker
            .worker_ready_message("planned-restart-ready", 42)
            .unwrap();

        assert!(
            worker
                .handle(
                    ControlEndpoint::PrepareRestart,
                    ControlMessage {
                        request_id: "planned-restart-prepare".into(),
                        generation: 3,
                        deployment_id: Some("deployment-a".into()),
                        command: ControlCommand::PrepareRestart,
                    },
                    &authenticated,
                    true,
                    1_003,
                )
                .is_ok()
        );
        assert!(
            worker
                .handle(
                    ControlEndpoint::CancelRestart,
                    ControlMessage {
                        request_id: "planned-restart-cancel".into(),
                        generation: 3,
                        deployment_id: Some("deployment-a".into()),
                        command: ControlCommand::CancelRestart,
                    },
                    &authenticated,
                    true,
                    1_004,
                )
                .is_ok()
        );

        assert!(
            worker
                .handle(
                    ControlEndpoint::Demote,
                    ControlMessage {
                        request_id: "planned-restart-demote-retry".into(),
                        generation: 3,
                        deployment_id: Some("deployment-a".into()),
                        command: ControlCommand::Demote,
                    },
                    &authenticated,
                    true,
                    1_005,
                )
                .is_ok(),
            "a planned demote retry must be accepted after the worker already returned to Paired"
        );
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
