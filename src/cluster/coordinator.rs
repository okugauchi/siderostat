use super::{
    AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlResponse, ControlResponseStatus, ControlRole, DistributedControlPhase, NodeDescriptor,
    PeerLease, WorkerEventKind, control::ControlProcessor,
};
use super::{
    ClusterEvent, ClusterEventKind, ClusterFailure, ClusterHandle, ClusterSnapshot, Ds4Hello,
    LocalStandaloneLifecycle, PromotionFailureStatus, PromotionFailureTracker,
    PromotionRetryDecision, PromotionTrackerError, RendezvousControlSnapshot, TransitionError,
};
use crate::{
    admission::DrainError,
    proxy::ModeAwareProxyState,
    target::{ClusterState, ProxyTarget, UnavailableReason},
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use thiserror::Error;

pub trait DistributedCoordinatorLifecycle: Send + Sync + 'static {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>>;
}

pub trait CoordinatorPeerLifecycle: Send + Sync + 'static {
    fn begin_drain(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop_worker(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
}

pub trait CoordinatorLeaseStatus: Send + Sync + 'static {
    fn is_valid(&self) -> bool;
}

impl<F> CoordinatorLeaseStatus for F
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    fn is_valid(&self) -> bool {
        self()
    }
}

#[derive(Debug, Error)]
pub enum CoordinatorLifecycleError {
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error(transparent)]
    Drain(#[from] DrainError),
    #[error("standalone lifecycle failed: {0}")]
    Standalone(#[source] anyhow::Error),
    #[error("distributed coordinator lifecycle failed: {0}")]
    Coordinator(#[source] anyhow::Error),
    #[error("worker control lifecycle failed: {0}")]
    Peer(#[source] anyhow::Error),
    #[error("distributed coordinator startup timed out")]
    StartupTimeout,
    #[error("complete distributed route timed out")]
    CompleteRouteTimeout,
    #[error("promotion failed and Paired Standalone recovery also failed: {0}")]
    Recovery(#[source] anyhow::Error),
    #[error("coordinator lifecycle timeouts must be positive")]
    InvalidTiming,
    #[error(transparent)]
    PromotionTracker(#[from] PromotionTrackerError),
    #[error("worker HELLO promotion requires ready worker control and a valid lease")]
    PrerequisiteMissing,
    #[error("worker lease was lost during distributed promotion")]
    LeaseLost,
}

#[derive(Clone)]
pub struct CoordinatorDistributedRuntime {
    cluster: ClusterHandle,
    proxy: Arc<ModeAwareProxyState>,
    standalone: Arc<dyn LocalStandaloneLifecycle>,
    coordinator: Arc<dyn DistributedCoordinatorLifecycle>,
    peer: Arc<dyn CoordinatorPeerLifecycle>,
    drain_timeout: Duration,
    startup_timeout: Duration,
    complete_route_timeout: Duration,
    route_loss_grace: Duration,
    failures: Arc<tokio::sync::Mutex<PromotionFailureTracker>>,
}

#[derive(Debug, Clone, Copy)]
pub struct CoordinatorRuntimeTimeouts {
    pub drain: Duration,
    pub startup: Duration,
    pub complete_route: Duration,
    pub route_loss_grace: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct PromotionRetryPolicy {
    pub backoff: Duration,
    pub maximum_consecutive_failures: u32,
}

impl CoordinatorDistributedRuntime {
    pub fn new(
        cluster: ClusterHandle,
        proxy: Arc<ModeAwareProxyState>,
        standalone: Arc<dyn LocalStandaloneLifecycle>,
        coordinator: Arc<dyn DistributedCoordinatorLifecycle>,
        peer: Arc<dyn CoordinatorPeerLifecycle>,
        timeouts: CoordinatorRuntimeTimeouts,
        retry: PromotionRetryPolicy,
    ) -> Result<Self, CoordinatorLifecycleError> {
        if timeouts.drain.is_zero()
            || timeouts.startup.is_zero()
            || timeouts.complete_route.is_zero()
            || timeouts.route_loss_grace.is_zero()
        {
            return Err(CoordinatorLifecycleError::InvalidTiming);
        }
        Ok(Self {
            cluster,
            proxy,
            standalone,
            coordinator,
            peer,
            drain_timeout: timeouts.drain,
            startup_timeout: timeouts.startup,
            complete_route_timeout: timeouts.complete_route,
            route_loss_grace: timeouts.route_loss_grace,
            failures: Arc::new(tokio::sync::Mutex::new(PromotionFailureTracker::new(
                retry.backoff,
                retry.maximum_consecutive_failures,
            )?)),
        })
    }

    pub async fn promote_after_hello(
        &self,
        _validated_hello: Ds4Hello,
        control: &CoordinatorControl,
        now_millis: u64,
        lease: Arc<dyn CoordinatorLeaseStatus>,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let prerequisites_ready = control.phase() == DistributedControlPhase::WorkerReady
            && control.peer_present(now_millis);
        self.promote_validated(_validated_hello, prerequisites_ready, lease, now_millis)
            .await
    }

    /// Starts the coordinator after the production control plane has atomically validated its
    /// WorkerReady phase and lease. This avoids retaining a control mutex across drain/startup.
    pub async fn promote_validated(
        &self,
        _validated_hello: Ds4Hello,
        prerequisites_ready: bool,
        lease: Arc<dyn CoordinatorLeaseStatus>,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        if !prerequisites_ready || !lease.is_valid() {
            return Err(CoordinatorLifecycleError::PrerequisiteMissing);
        }
        let current = self.cluster.snapshot();
        let promoting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::WorkerHelloAccepted,
            })
            .await?;
        self.block_transition();

        let result = self.promote_inner(promoting, lease).await;
        match result {
            Ok(snapshot) => {
                self.failures.lock().await.note_success();
                Ok(snapshot)
            }
            Err(error) => {
                if matches!(error, CoordinatorLifecycleError::LeaseLost) {
                    self.recover_solo().await?;
                } else {
                    self.recover_paired().await?;
                    if let Some(failure) = promotion_failure_for_error(&error) {
                        self.enter_promotion_failure(failure, now_millis).await?;
                    }
                }
                Err(error)
            }
        }
    }

    async fn promote_inner(
        &self,
        promoting: ClusterSnapshot,
        lease: Arc<dyn CoordinatorLeaseStatus>,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let local_drain = self
            .proxy
            .admission()
            .drain(promoting.generation, self.drain_timeout);
        let peer_drain = self.peer.begin_drain(promoting.generation);
        let (local_result, peer_result) = tokio::join!(local_drain, peer_drain);
        local_result?;
        peer_result.map_err(CoordinatorLifecycleError::Peer)?;
        if !lease.is_valid() {
            return Err(CoordinatorLifecycleError::LeaseLost);
        }

        self.standalone
            .stop()
            .await
            .map_err(CoordinatorLifecycleError::Standalone)?;
        match tokio::time::timeout(
            self.startup_timeout,
            self.coordinator.start(promoting.generation),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(CoordinatorLifecycleError::Coordinator(error)),
            Err(_) => return Err(CoordinatorLifecycleError::StartupTimeout),
        }
        if !lease.is_valid() {
            return Err(CoordinatorLifecycleError::LeaseLost);
        }
        let starting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: promoting.generation,
                kind: ClusterEventKind::DistributedChildStarted,
            })
            .await?;
        match tokio::time::timeout(self.complete_route_timeout, self.coordinator.wait_ready()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(CoordinatorLifecycleError::Coordinator(error)),
            Err(_) => return Err(CoordinatorLifecycleError::CompleteRouteTimeout),
        }
        if !lease.is_valid() {
            return Err(CoordinatorLifecycleError::LeaseLost);
        }
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::DistributedRouteReady,
            })
            .await?;
        self.proxy.set_target(ready.target, true);
        self.proxy.admission().start_serving();
        Ok(ready)
    }

    pub async fn wait_route_loss_and_demote(
        &self,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        loop {
            if let Err(error) = self.coordinator.wait_route_loss().await {
                let current = self.cluster.snapshot();
                if current.state != ClusterState::DistributedReady {
                    return Ok(current);
                }
                return Err(CoordinatorLifecycleError::Coordinator(error));
            }
            match tokio::time::timeout(self.route_loss_grace, self.coordinator.wait_ready()).await {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => {
                    let current = self.cluster.snapshot();
                    if current.state != ClusterState::DistributedReady {
                        return Ok(current);
                    }
                    return Err(CoordinatorLifecycleError::Coordinator(error));
                }
                Err(_) => {
                    let current = self.cluster.snapshot();
                    if current.state != ClusterState::DistributedReady {
                        return Ok(current);
                    }
                    return self.demote().await;
                }
            }
        }
    }

    pub async fn demote(&self) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let current = self.cluster.snapshot();
        let demoting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::BeginDemotion,
            })
            .await?;
        self.block_transition();
        let local_drain = self
            .proxy
            .admission()
            .drain(demoting.generation, self.drain_timeout);
        let peer_drain = self.peer.begin_drain(demoting.generation);
        let (local_result, peer_result) = tokio::join!(local_drain, peer_drain);
        local_result?;
        peer_result.map_err(CoordinatorLifecycleError::Peer)?;
        self.coordinator
            .stop()
            .await
            .map_err(CoordinatorLifecycleError::Coordinator)?;
        self.peer
            .stop_worker(demoting.generation)
            .await
            .map_err(CoordinatorLifecycleError::Peer)?;
        self.standalone
            .start(demoting.generation)
            .await
            .map_err(CoordinatorLifecycleError::Standalone)?;
        let paired = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: demoting.generation,
                kind: ClusterEventKind::PairingReady,
            })
            .await?;
        self.proxy.set_target(paired.target, true);
        self.proxy.admission().start_serving();
        Ok(paired)
    }

    pub async fn record_promotion_failure(
        &self,
        failure: ClusterFailure,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.enter_promotion_failure(failure, now_millis).await
    }

    pub async fn reconcile_backoff(
        &self,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let current = self.cluster.snapshot();
        if current.state != ClusterState::Backoff
            || !self.failures.lock().await.can_retry(now_millis)
        {
            return Ok(current);
        }
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::BackoffElapsed,
            })
            .await?;
        self.proxy.set_target(ready.target, true);
        self.proxy.admission().start_serving();
        Ok(ready)
    }

    pub async fn operator_reconcile(&self) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.failures.lock().await.operator_reconcile();
        let current = self.cluster.snapshot();
        if current.state != ClusterState::ManualInterventionRequired {
            return Ok(current);
        }
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::OperatorReconcile,
            })
            .await?;
        self.proxy.set_target(ready.target, true);
        self.proxy.admission().start_serving();
        Ok(ready)
    }

    pub async fn promotion_failure_status(&self) -> PromotionFailureStatus {
        self.failures.lock().await.status()
    }

    fn block_transition(&self) {
        self.proxy.admission().block();
        self.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            false,
        );
    }

    async fn recover_paired(&self) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.block_transition();
        let generation = self.cluster.snapshot().generation;
        let coordinator_stop = self.coordinator.stop();
        let worker_stop = self.peer.stop_worker(generation);
        let (coordinator_result, worker_result) = tokio::join!(coordinator_stop, worker_stop);
        coordinator_result.map_err(|error| {
            CoordinatorLifecycleError::Recovery(anyhow::anyhow!(
                "coordinator cleanup failed: {error}"
            ))
        })?;
        worker_result.map_err(|error| {
            CoordinatorLifecycleError::Recovery(anyhow::anyhow!("worker cleanup failed: {error}"))
        })?;
        self.standalone.start(generation).await.map_err(|error| {
            CoordinatorLifecycleError::Recovery(anyhow::anyhow!(
                "standalone recovery failed: {error}"
            ))
        })?;
        let paired = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: generation,
                kind: ClusterEventKind::PromotionFailed,
            })
            .await?;
        self.proxy.set_target(paired.target, true);
        self.proxy.admission().start_serving();
        Ok(paired)
    }

    async fn recover_solo(&self) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.block_transition();
        let generation = self.cluster.snapshot().generation;
        let coordinator_stop = self.coordinator.stop();
        let worker_stop = self.peer.stop_worker(generation);
        let (coordinator_result, worker_result) = tokio::join!(coordinator_stop, worker_stop);
        coordinator_result.map_err(CoordinatorLifecycleError::Coordinator)?;
        worker_result.map_err(CoordinatorLifecycleError::Peer)?;
        let starting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: generation,
                kind: ClusterEventKind::PeerLost,
            })
            .await?;
        self.standalone
            .start(starting.generation)
            .await
            .map_err(CoordinatorLifecycleError::Standalone)?;
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::LocalStandaloneReady,
            })
            .await?;
        self.proxy.set_target(ready.target, true);
        self.proxy.admission().start_serving();
        Ok(ready)
    }

    async fn enter_promotion_failure(
        &self,
        failure: ClusterFailure,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let decision = self.failures.lock().await.record(failure, now_millis)?;
        let current = self.cluster.snapshot();
        let kind = match decision {
            PromotionRetryDecision::Backoff { .. } => ClusterEventKind::EnterBackoff,
            PromotionRetryDecision::ManualIntervention => {
                ClusterEventKind::RequireManualIntervention
            }
        };
        let next = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind,
            })
            .await?;
        let ready = !matches!(next.target, ProxyTarget::Unavailable { .. });
        self.proxy.set_target(next.target, ready);
        if ready {
            self.proxy.admission().start_serving();
        } else {
            self.proxy.admission().block();
        }
        Ok(next)
    }
}

fn promotion_failure_for_error(error: &CoordinatorLifecycleError) -> Option<ClusterFailure> {
    match error {
        CoordinatorLifecycleError::StartupTimeout => {
            Some(ClusterFailure::CoordinatorStartupTimeout)
        }
        _ => None,
    }
}

#[derive(Debug)]
pub struct CoordinatorControl {
    processor: ControlProcessor,
    phase: DistributedControlPhase,
}

impl CoordinatorControl {
    pub fn new(
        descriptor: NodeDescriptor,
        lease: Duration,
        required_stability: Duration,
    ) -> Result<Self, ControlError> {
        if descriptor.role != ControlRole::Coordinator || descriptor.protocol_version != 1 {
            return Err(ControlError::InvalidDescriptor);
        }
        Ok(Self {
            processor: ControlProcessor::new(
                descriptor,
                ControlRole::Worker,
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
                | ControlCommand::Drained
                | ControlCommand::WorkerEvent { .. }
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
            |command| validate_coordinator_command(phase, command),
        )?;
        if response.status == ControlResponseStatus::Applied {
            self.phase = match command {
                ControlCommand::Pair { .. } => DistributedControlPhase::Paired,
                ControlCommand::WorkerEvent {
                    event: WorkerEventKind::Ready,
                } => DistributedControlPhase::WorkerReady,
                ControlCommand::WorkerEvent { .. } => phase,
                ControlCommand::Drained => DistributedControlPhase::Drained,
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

    pub fn note_prepare_sent(&mut self, generation: u64) -> Result<(), ControlError> {
        self.require_generation(generation)?;
        if self.phase != DistributedControlPhase::Paired {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::WorkerPreparing;
        Ok(())
    }

    pub fn prepare_worker_message(
        &self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::Paired {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::PrepareWorker))
    }

    pub fn note_begin_drain_sent(&mut self, generation: u64) -> Result<(), ControlError> {
        self.require_generation(generation)?;
        if self.phase != DistributedControlPhase::WorkerReady {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::Draining;
        Ok(())
    }

    pub fn begin_drain_message(
        &self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::WorkerReady {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::BeginDrain))
    }

    pub fn phase(&self) -> DistributedControlPhase {
        self.phase
    }

    pub fn distributed_ready_message(
        &mut self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::Drained {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::WorkerReady;
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::DistributedReady))
    }

    pub fn cancel_generation_message(
        &mut self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if !matches!(
            self.phase,
            DistributedControlPhase::WorkerPreparing
                | DistributedControlPhase::WorkerReady
                | DistributedControlPhase::Draining
                | DistributedControlPhase::Drained
        ) {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::Paired;
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::CancelGeneration))
    }

    pub fn demote_message(
        &self,
        request_id: impl Into<String>,
    ) -> Result<ControlMessage, ControlError> {
        if self.phase != DistributedControlPhase::Drained {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        Ok(self
            .processor
            .message(request_id.into(), ControlCommand::Demote))
    }

    pub fn note_demote_complete(&mut self, generation: u64) -> Result<(), ControlError> {
        self.require_generation(generation)?;
        if self.phase != DistributedControlPhase::Drained {
            return Err(ControlError::InvalidPhase { phase: self.phase });
        }
        self.phase = DistributedControlPhase::Paired;
        Ok(())
    }

    pub fn rendezvous_snapshot(
        &self,
        state: ClusterState,
        now_millis: u64,
    ) -> RendezvousControlSnapshot {
        let local = self.processor.local_descriptor();
        RendezvousControlSnapshot {
            state,
            generation: local.generation,
            deployment_id: local.deployment_id.clone(),
            lease_valid: self.processor.lease().peer_present(now_millis),
        }
    }

    fn require_generation(&self, generation: u64) -> Result<(), ControlError> {
        let expected = self.processor.generation();
        if generation != expected {
            return Err(ControlError::GenerationMismatch {
                expected,
                received: generation,
            });
        }
        Ok(())
    }
}

fn validate_coordinator_command(
    phase: DistributedControlPhase,
    command: &ControlCommand,
) -> Result<(), ControlError> {
    let valid = match command {
        ControlCommand::Pair { .. } => true,
        ControlCommand::WorkerEvent {
            event: WorkerEventKind::Ready,
        } => phase == DistributedControlPhase::WorkerPreparing,
        ControlCommand::WorkerEvent { .. } => !matches!(phase, DistributedControlPhase::Unpaired),
        ControlCommand::Drained => phase == DistributedControlPhase::Draining,
        ControlCommand::DistributedReady => false,
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
    use crate::{
        cluster::{
            ControlMode, ControlResponseStatus, LocalStandaloneLifecycle, spawn_state_machine,
        },
        proxy::{ModeAwareProxyOptions, ModeAwareTargetSnapshot},
        target::{LocalRole, StableMode},
    };
    use std::{
        net::IpAddr,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct FakeStandalone {
        running: Arc<AtomicBool>,
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    impl LocalStandaloneLifecycle for FakeStandalone {
        fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            let running = self.running.clone();
            let starts = self.starts.clone();
            Box::pin(async move {
                starts.fetch_add(1, Ordering::SeqCst);
                running.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            let running = self.running.clone();
            let stops = self.stops.clone();
            Box::pin(async move {
                stops.fetch_add(1, Ordering::SeqCst);
                running.store(false, Ordering::SeqCst);
                Ok(())
            })
        }

        fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
            let running = self.running.clone();
            Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
        }
    }

    #[derive(Default)]
    struct FakeDistributedCoordinator {
        running: Arc<AtomicBool>,
        start_hangs: Arc<AtomicBool>,
        route_ready: Arc<AtomicBool>,
        route_lost: Arc<AtomicBool>,
        route_changed: Arc<tokio::sync::Notify>,
        stops: Arc<AtomicUsize>,
    }

    impl FakeDistributedCoordinator {
        fn set_route_ready(&self, ready: bool) {
            self.route_ready.store(ready, Ordering::SeqCst);
            if ready {
                self.route_lost.store(false, Ordering::SeqCst);
            }
            self.route_changed.notify_waiters();
        }

        fn lose_route(&self) {
            self.route_ready.store(false, Ordering::SeqCst);
            self.route_lost.store(true, Ordering::SeqCst);
            self.route_changed.notify_waiters();
        }
    }

    impl DistributedCoordinatorLifecycle for FakeDistributedCoordinator {
        fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            let running = self.running.clone();
            let hangs = self.start_hangs.clone();
            Box::pin(async move {
                running.store(true, Ordering::SeqCst);
                if hangs.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }

        fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            let ready = self.route_ready.clone();
            let changed = self.route_changed.clone();
            Box::pin(async move {
                while !ready.load(Ordering::SeqCst) {
                    changed.notified().await;
                }
                Ok(())
            })
        }

        fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            let running = self.running.clone();
            let stops = self.stops.clone();
            Box::pin(async move {
                stops.fetch_add(1, Ordering::SeqCst);
                running.store(false, Ordering::SeqCst);
                Ok(())
            })
        }

        fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            let lost = self.route_lost.clone();
            let changed = self.route_changed.clone();
            Box::pin(async move {
                while !lost.load(Ordering::SeqCst) {
                    changed.notified().await;
                }
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct FakePeer {
        drains: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    impl CoordinatorPeerLifecycle for FakePeer {
        fn begin_drain(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            let drains = self.drains.clone();
            Box::pin(async move {
                drains.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn stop_worker(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            let stops = self.stops.clone();
            Box::pin(async move {
                stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    const NOW: u64 = 10_000;

    fn descriptor(role: ControlRole, node_id: &str) -> NodeDescriptor {
        NodeDescriptor {
            protocol_version: 1,
            node_id: node_id.into(),
            role,
            generation: 7,
            mode: ControlMode::SoloStandalone,
            deployment_id: Some("deployment-a".into()),
        }
    }

    fn authenticated() -> AuthenticatedPeer {
        AuthenticatedPeer::new_for_test("worker-node", IpAddr::from([10, 99, 0, 2]), NOW)
    }

    fn pair(request_id: &str) -> ControlMessage {
        ControlMessage {
            request_id: request_id.into(),
            generation: 7,
            deployment_id: None,
            command: ControlCommand::Pair {
                descriptor: descriptor(ControlRole::Worker, "worker-node"),
            },
        }
    }

    fn coordinator() -> CoordinatorControl {
        CoordinatorControl::new(
            descriptor(ControlRole::Coordinator, "coordinator-node"),
            Duration::from_secs(15),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn ready_worker_control() -> CoordinatorControl {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-promotion"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        control.note_prepare_sent(7).unwrap();
        control
            .handle(
                ControlEndpoint::WorkerEvent,
                ControlMessage {
                    request_id: "ready-promotion".into(),
                    generation: 7,
                    deployment_id: Some("deployment-a".into()),
                    command: ControlCommand::WorkerEvent {
                        event: WorkerEventKind::Ready,
                    },
                },
                &authenticated(),
                true,
                NOW + 5_000,
            )
            .unwrap();
        control
    }

    fn validated_hello() -> Ds4Hello {
        Ds4Hello {
            model_id: 1,
            quant_bits: 2,
            layer_start: 20,
            layer_end: 60,
            has_output: true,
            has_hidden: true,
            context_size: 262_144,
            layer_count: 61,
            listen_port: 9911,
            model_name: "deepseek-v4-flash-mxfp4".into(),
        }
    }

    fn proxy() -> Arc<ModeAwareProxyState> {
        Arc::new(
            ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:8000").unwrap(),
                url::Url::parse("http://10.99.0.1:18082").unwrap(),
                ModeAwareProxyOptions {
                    max_in_flight: 4,
                    request_body_limit_bytes: 4096,
                    response_header_timeout: Duration::from_secs(1),
                    first_body_byte_timeout: Duration::from_secs(1),
                    stream_idle_timeout: Duration::from_secs(1),
                    connect_timeout: Duration::from_secs(1),
                },
            )
            .unwrap(),
        )
    }

    async fn awaiting_hello_cluster() -> (ClusterHandle, tokio::task::JoinHandle<()>) {
        let (cluster, task) =
            spawn_state_machine(ClusterSnapshot::booting(LocalRole::Coordinator), 16);
        let mut generation = 0;
        for kind in [
            ClusterEventKind::BeginSoloStandalone,
            ClusterEventKind::LocalStandaloneReady,
            ClusterEventKind::BeginPairing,
            ClusterEventKind::PairingReady,
            ClusterEventKind::BeginPromotion,
        ] {
            generation = cluster
                .apply(ClusterEvent {
                    expected_generation: generation,
                    kind,
                })
                .await
                .unwrap()
                .generation;
        }
        assert_eq!(cluster.snapshot().state, ClusterState::AwaitingWorkerHello);
        (cluster, task)
    }

    async fn promotion_runtime(
        coordinator: Arc<FakeDistributedCoordinator>,
    ) -> (
        CoordinatorDistributedRuntime,
        Arc<ModeAwareProxyState>,
        Arc<FakeStandalone>,
        Arc<FakePeer>,
        tokio::task::JoinHandle<()>,
    ) {
        let (cluster, task) = awaiting_hello_cluster().await;
        let proxy = proxy();
        proxy.set_target(ProxyTarget::LocalStandalone, true);
        proxy.admission().start_serving();
        let standalone = Arc::new(FakeStandalone::default());
        standalone.running.store(true, Ordering::SeqCst);
        let peer = Arc::new(FakePeer::default());
        let runtime = CoordinatorDistributedRuntime::new(
            cluster,
            proxy.clone(),
            standalone.clone(),
            coordinator,
            peer.clone(),
            CoordinatorRuntimeTimeouts {
                drain: Duration::from_millis(200),
                startup: Duration::from_millis(30),
                complete_route: Duration::from_millis(200),
                route_loss_grace: Duration::from_millis(10),
            },
            PromotionRetryPolicy {
                backoff: Duration::from_millis(50),
                maximum_consecutive_failures: 3,
            },
        )
        .unwrap();
        (runtime, proxy, standalone, peer, task)
    }

    #[test]
    fn authenticated_scoped_pair_becomes_present_only_after_stability() {
        let mut control = coordinator();
        let response = control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        assert_eq!(response.status, ControlResponseStatus::Applied);
        assert!(!control.peer_present(NOW + 4_999));
        assert!(control.peer_present(NOW + 5_000));
        assert!(!control.peer_present(NOW + 15_000));
    }

    #[test]
    fn duplicate_is_idempotent_and_renews_lease() {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        let duplicate = control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW + 4_000,
            )
            .unwrap();
        assert_eq!(duplicate.status, ControlResponseStatus::Duplicate);
        assert_eq!(duplicate.lease_expires_at_millis, Some(NOW + 19_000));
        assert!(control.peer_present(NOW + 18_999));
    }

    #[test]
    fn authenticated_node_descriptor_poll_renews_an_active_lease() {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        let response = control
            .node_descriptor(&authenticated(), true, NOW + 5_000)
            .unwrap();
        assert_eq!(response.lease_expires_at_millis, Some(NOW + 20_000));
        assert!(control.peer_present(NOW + 19_999));
    }

    #[test]
    fn stale_generation_and_changed_duplicate_are_conflicts() {
        let mut control = coordinator();
        let mut stale = pair("pair-old");
        stale.generation = 6;
        assert_eq!(
            control.handle(ControlEndpoint::Pair, stale, &authenticated(), true, NOW,),
            Err(ControlError::GenerationMismatch {
                expected: 7,
                received: 6,
            })
        );
        assert_eq!(
            ControlError::GenerationMismatch {
                expected: 7,
                received: 6,
            }
            .http_status(),
            409
        );

        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        let mut changed = pair("pair-1");
        if let ControlCommand::Pair { descriptor } = &mut changed.command {
            descriptor.mode = ControlMode::Transitioning;
        }
        assert_eq!(
            control.handle(
                ControlEndpoint::Pair,
                changed,
                &authenticated(),
                true,
                NOW + 1,
            ),
            Err(ControlError::IdempotencyConflict)
        );
    }

    #[test]
    fn route_loss_or_lease_interruption_removes_peer_presence() {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        assert!(control.peer_present(NOW + 5_000));
        control.invalidate_route();
        assert!(!control.peer_present(NOW + 5_001));

        let mut unscoped = coordinator();
        assert_eq!(
            unscoped.handle(
                ControlEndpoint::Pair,
                pair("pair-2"),
                &authenticated(),
                false,
                NOW,
            ),
            Err(ControlError::RouteNotScoped)
        );
        assert!(!unscoped.peer_present(NOW + 5_000));
    }

    #[test]
    fn deployment_mismatch_returns_precondition_failed() {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        let message = ControlMessage {
            request_id: "event-1".into(),
            generation: 7,
            deployment_id: Some("deployment-b".into()),
            command: ControlCommand::WorkerEvent {
                event: WorkerEventKind::Ready,
            },
        };
        let error = control
            .handle(
                ControlEndpoint::WorkerEvent,
                message,
                &authenticated(),
                true,
                NOW + 1,
            )
            .unwrap_err();
        assert_eq!(error, ControlError::DeploymentMismatch);
        assert_eq!(error.http_status(), 412);
    }

    #[test]
    fn distributed_ack_sequence_rejects_reorder_duplicate_change_and_old_generation() {
        let mut control = coordinator();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-distributed"),
                &authenticated(),
                true,
                NOW,
            )
            .unwrap();
        let prepare = control.prepare_worker_message("prepare-1").unwrap();
        assert_eq!(prepare.generation, 7);
        assert_eq!(prepare.deployment_id.as_deref(), Some("deployment-a"));
        control.note_prepare_sent(7).unwrap();
        assert_eq!(
            control.rendezvous_snapshot(ClusterState::AwaitingWorkerHello, NOW + 5_000),
            RendezvousControlSnapshot {
                state: ClusterState::AwaitingWorkerHello,
                generation: 7,
                deployment_id: Some("deployment-a".into()),
                lease_valid: true,
            }
        );

        let ready = ControlMessage {
            request_id: "ready-1".into(),
            generation: 7,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::WorkerEvent {
                event: WorkerEventKind::Ready,
            },
        };
        control
            .handle(
                ControlEndpoint::WorkerEvent,
                ready.clone(),
                &authenticated(),
                true,
                NOW + 1,
            )
            .unwrap();
        assert_eq!(control.phase(), DistributedControlPhase::WorkerReady);
        assert_eq!(
            control
                .handle(
                    ControlEndpoint::WorkerEvent,
                    ready,
                    &authenticated(),
                    true,
                    NOW + 2,
                )
                .unwrap()
                .status,
            ControlResponseStatus::Duplicate
        );

        let drained = ControlMessage {
            request_id: "drained-1".into(),
            generation: 7,
            deployment_id: Some("deployment-a".into()),
            command: ControlCommand::Drained,
        };
        assert_eq!(
            control.handle(
                ControlEndpoint::Drained,
                drained.clone(),
                &authenticated(),
                true,
                NOW + 3,
            ),
            Err(ControlError::InvalidPhase {
                phase: DistributedControlPhase::WorkerReady
            })
        );
        assert_eq!(
            control.begin_drain_message("drain-1").unwrap().generation,
            7
        );
        control.note_begin_drain_sent(7).unwrap();
        control
            .handle(
                ControlEndpoint::Drained,
                drained.clone(),
                &authenticated(),
                true,
                NOW + 4,
            )
            .unwrap();
        assert_eq!(control.phase(), DistributedControlPhase::Drained);
        let demote = control.demote_message("demote-1").unwrap();
        assert_eq!(demote.generation, 7);
        control.note_demote_complete(demote.generation).unwrap();
        assert_eq!(control.phase(), DistributedControlPhase::Paired);

        control.advance_generation(8);
        assert_eq!(
            control.handle(
                ControlEndpoint::Drained,
                drained,
                &authenticated(),
                true,
                NOW + 5,
            ),
            Err(ControlError::GenerationMismatch {
                expected: 8,
                received: 7
            })
        );
    }

    #[tokio::test]
    async fn promotion_waits_for_in_flight_stream_and_complete_route_before_serving() {
        let coordinator = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
        let permit = proxy.admission().try_acquire(true).unwrap();
        let control = ready_worker_control();
        let promotion = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runtime
                    .promote_after_hello(
                        validated_hello(),
                        &control,
                        NOW + 5_000,
                        Arc::new(|| true),
                    )
                    .await
            }
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(standalone.stops.load(Ordering::SeqCst), 0);
        assert!(!coordinator.running.load(Ordering::SeqCst));
        drop(permit);
        tokio::time::timeout(Duration::from_millis(100), async {
            while !coordinator.running.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            runtime.cluster.snapshot().state,
            ClusterState::DistributedStarting
        );
        assert_eq!(
            proxy.target_snapshot(),
            ModeAwareTargetSnapshot {
                target: ProxyTarget::Unavailable {
                    reason: UnavailableReason::Transition,
                },
                ready: false,
            }
        );
        assert!(!promotion.is_finished());

        coordinator.set_route_ready(true);
        let ready = promotion.await.unwrap().unwrap();
        assert_eq!(ready.stable_mode, StableMode::DistributedMxfp4);
        assert_eq!(ready.state, ClusterState::DistributedReady);
        assert_eq!(peer.drains.load(Ordering::SeqCst), 1);
        assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
        assert!(proxy.target_snapshot().ready);
        task.abort();
    }

    #[tokio::test]
    async fn hello_without_ready_worker_control_never_starts_promotion() {
        let child = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, _, _, task) = promotion_runtime(child.clone()).await;
        let control = coordinator();

        assert!(matches!(
            runtime
                .promote_after_hello(validated_hello(), &control, NOW, Arc::new(|| true))
                .await,
            Err(CoordinatorLifecycleError::PrerequisiteMissing)
        ));
        assert_eq!(
            runtime.cluster.snapshot().state,
            ClusterState::AwaitingWorkerHello
        );
        assert!(!child.running.load(Ordering::SeqCst));
        assert!(proxy.target_snapshot().ready);
        task.abort();
    }

    #[tokio::test]
    async fn coordinator_startup_timeout_reaps_children_and_enters_serving_backoff() {
        let coordinator = Arc::new(FakeDistributedCoordinator::default());
        coordinator.start_hangs.store(true, Ordering::SeqCst);
        let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
        let control = ready_worker_control();

        assert!(matches!(
            runtime
                .promote_after_hello(validated_hello(), &control, NOW + 5_000, Arc::new(|| true),)
                .await,
            Err(CoordinatorLifecycleError::StartupTimeout)
        ));
        assert!(!coordinator.running.load(Ordering::SeqCst));
        assert!(standalone.running.load(Ordering::SeqCst));
        assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.cluster.snapshot().state, ClusterState::Backoff);
        assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
        assert!(proxy.target_snapshot().ready);
        task.abort();
    }

    #[tokio::test]
    async fn lease_loss_after_child_start_rejects_route_and_recovers_solo() {
        let coordinator = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
        let control = ready_worker_control();
        let valid = Arc::new(AtomicBool::new(true));
        let status = valid.clone();
        let promotion = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runtime
                    .promote_after_hello(
                        validated_hello(),
                        &control,
                        NOW + 5_000,
                        Arc::new(move || status.load(Ordering::SeqCst)),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_millis(100), async {
            while !coordinator.running.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        valid.store(false, Ordering::SeqCst);
        coordinator.set_route_ready(true);

        assert!(matches!(
            promotion.await.unwrap(),
            Err(CoordinatorLifecycleError::LeaseLost)
        ));
        assert_eq!(
            runtime.cluster.snapshot().state,
            ClusterState::SoloStandaloneReady
        );
        assert!(!coordinator.running.load(Ordering::SeqCst));
        assert!(standalone.running.load(Ordering::SeqCst));
        assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
        assert!(proxy.target_snapshot().ready);
        task.abort();
    }

    #[tokio::test]
    async fn incomplete_route_never_serves_and_route_loss_demotes_after_drain() {
        let coordinator = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
        let control = ready_worker_control();
        let promotion = tokio::spawn({
            let runtime = runtime.clone();
            async move {
                runtime
                    .promote_after_hello(
                        validated_hello(),
                        &control,
                        NOW + 5_000,
                        Arc::new(|| true),
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(!promotion.is_finished());
        assert!(!proxy.target_snapshot().ready);
        coordinator.set_route_ready(true);
        promotion.await.unwrap().unwrap();

        let permit = proxy.admission().try_acquire(true).unwrap();
        let demotion = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait_route_loss_and_demote().await }
        });
        coordinator.lose_route();
        tokio::time::sleep(Duration::from_millis(2)).await;
        coordinator.set_route_ready(true);
        tokio::time::sleep(Duration::from_millis(12)).await;
        assert!(!demotion.is_finished());

        coordinator.lose_route();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!demotion.is_finished());
        assert_eq!(standalone.starts.load(Ordering::SeqCst), 0);
        drop(permit);

        let paired = demotion.await.unwrap().unwrap();
        assert_eq!(paired.stable_mode, StableMode::PairedStandalone);
        assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
        assert_eq!(peer.drains.load(Ordering::SeqCst), 2);
        assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
        assert!(!coordinator.running.load(Ordering::SeqCst));
        assert!(standalone.running.load(Ordering::SeqCst));
        assert!(proxy.target_snapshot().ready);
        task.abort();
    }

    #[tokio::test]
    async fn manual_demotion_wins_route_loss_monitor_race() {
        let coordinator = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, _, peer, task) = promotion_runtime(coordinator.clone()).await;
        coordinator.set_route_ready(true);
        runtime
            .promote_after_hello(
                validated_hello(),
                &ready_worker_control(),
                NOW + 5_000,
                Arc::new(|| true),
            )
            .await
            .unwrap();

        let permit = proxy.admission().try_acquire(true).unwrap();
        coordinator.lose_route();
        let route_monitor = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait_route_loss_and_demote().await }
        });
        tokio::time::sleep(Duration::from_millis(2)).await;
        let manual = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.demote().await }
        });
        tokio::time::sleep(Duration::from_millis(12)).await;

        let monitored = route_monitor.await.unwrap().unwrap();
        assert_eq!(monitored.state, ClusterState::Demoting);
        assert_eq!(peer.drains.load(Ordering::SeqCst), 2);

        drop(permit);
        let paired = manual.await.unwrap().unwrap();
        assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
        assert_eq!(peer.drains.load(Ordering::SeqCst), 2);
        task.abort();
    }

    #[tokio::test]
    async fn third_same_promotion_failure_stops_auto_retry_but_keeps_serving() {
        let child = Arc::new(FakeDistributedCoordinator::default());
        let (runtime, proxy, _, _, task) = promotion_runtime(child).await;

        for attempt in 1_u64..=3 {
            let now = (attempt - 1) * 50;
            let failed = runtime
                .record_promotion_failure(ClusterFailure::CoordinatorStartupTimeout, now)
                .await
                .unwrap();
            if attempt < 3 {
                assert_eq!(failed.state, ClusterState::Backoff);
                assert_eq!(runtime.reconcile_backoff(now + 49).await.unwrap(), failed);
                let paired = runtime.reconcile_backoff(now + 50).await.unwrap();
                assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
                runtime
                    .cluster
                    .apply(ClusterEvent {
                        expected_generation: paired.generation,
                        kind: ClusterEventKind::BeginPromotion,
                    })
                    .await
                    .unwrap();
            } else {
                assert_eq!(failed.state, ClusterState::ManualInterventionRequired);
            }
        }

        let status = runtime.promotion_failure_status().await;
        assert_eq!(status.consecutive, 3);
        assert!(status.manual);
        assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
        assert!(proxy.target_snapshot().ready);
        assert_eq!(
            runtime.reconcile_backoff(u64::MAX).await.unwrap().state,
            ClusterState::ManualInterventionRequired
        );
        assert_eq!(
            runtime.operator_reconcile().await.unwrap().state,
            ClusterState::PairedStandaloneReady
        );
        assert_eq!(runtime.promotion_failure_status().await.consecutive, 0);
        task.abort();
    }
}
