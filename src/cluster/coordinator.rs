use super::{
    ChildIdentity, ClusterEvent, ClusterEventKind, ClusterFailure, ClusterHandle, ClusterSnapshot,
    DistributedControlPhase, Ds4Hello, LocalStandaloneLifecycle, PromotionFailureStatus,
    PromotionFailureTracker, PromotionRetryDecision, PromotionTrackerError, TransitionError,
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
    /// Whether the child process is currently running. Defaults to `false`.
    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Optional child identity for diagnostics. Defaults to `None`.
    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

pub trait CoordinatorPeerLifecycle: Send + Sync + 'static {
    fn begin_drain(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
    fn begin_drain_with_timeout(
        &self,
        generation: u64,
        _timeout: Duration,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        self.begin_drain(generation)
    }
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
        self.promote_validated_with_admission(
            _validated_hello,
            prerequisites_ready,
            lease,
            now_millis,
            true,
        )
        .await
    }

    pub async fn promote_validated_for_recovery(
        &self,
        validated_hello: Ds4Hello,
        prerequisites_ready: bool,
        lease: Arc<dyn CoordinatorLeaseStatus>,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.promote_validated_with_admission(
            validated_hello,
            prerequisites_ready,
            lease,
            now_millis,
            false,
        )
        .await
    }

    async fn promote_validated_with_admission(
        &self,
        _validated_hello: Ds4Hello,
        prerequisites_ready: bool,
        lease: Arc<dyn CoordinatorLeaseStatus>,
        now_millis: u64,
        resume_admission: bool,
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

        let result = self.promote_inner(promoting, lease, resume_admission).await;
        match result {
            Ok(snapshot) => {
                self.failures.lock().await.note_success();
                Ok(snapshot)
            }
            Err(error) => {
                if matches!(error, CoordinatorLifecycleError::LeaseLost) {
                    self.recover_solo(resume_admission).await?;
                } else {
                    self.recover_paired(resume_admission).await?;
                    if let Some(failure) = promotion_failure_for_error(&error) {
                        self.enter_promotion_failure(failure, now_millis, resume_admission)
                            .await?;
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
        resume_admission: bool,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        let local_drain = self
            .proxy
            .admission()
            .drain(promoting.generation, self.drain_timeout);
        let peer_drain = self.peer.begin_drain(promoting.generation);
        let (local_result, peer_result) = tokio::join!(local_drain, peer_drain);
        local_result?;
        peer_result.map_err(CoordinatorLifecycleError::Peer)?;
        if !resume_admission {
            self.proxy.admission().reset_blocked_generation();
        }
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
        if resume_admission {
            self.proxy.admission().start_serving();
        }
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
        self.demote_with_drain_timeout_and_admission(self.drain_timeout, true)
            .await
    }

    pub async fn demote_with_drain_timeout(
        &self,
        drain_timeout: Duration,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.demote_with_drain_timeout_and_admission(drain_timeout, true)
            .await
    }

    pub async fn demote_for_recovery(
        &self,
        drain_timeout: Duration,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.demote_with_drain_timeout_and_admission(drain_timeout, false)
            .await
    }

    async fn demote_with_drain_timeout_and_admission(
        &self,
        drain_timeout: Duration,
        resume_admission: bool,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
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
            .drain(demoting.generation, drain_timeout);
        let peer_drain = self
            .peer
            .begin_drain_with_timeout(demoting.generation, drain_timeout);
        let (local_result, peer_result) = tokio::join!(local_drain, peer_drain);
        local_result?;
        peer_result.map_err(CoordinatorLifecycleError::Peer)?;
        if !resume_admission {
            self.proxy.admission().reset_blocked_generation();
        }
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
        if resume_admission {
            self.proxy.admission().start_serving();
        }
        Ok(paired)
    }

    pub async fn record_promotion_failure(
        &self,
        failure: ClusterFailure,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
        self.enter_promotion_failure(failure, now_millis, true)
            .await
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

    async fn recover_paired(
        &self,
        resume_admission: bool,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
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
        if resume_admission {
            self.proxy.admission().start_serving();
        }
        Ok(paired)
    }

    async fn recover_solo(
        &self,
        resume_admission: bool,
    ) -> Result<ClusterSnapshot, CoordinatorLifecycleError> {
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
        if resume_admission {
            self.proxy.admission().start_serving();
        }
        Ok(ready)
    }

    async fn enter_promotion_failure(
        &self,
        failure: ClusterFailure,
        now_millis: u64,
        resume_admission: bool,
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
        if ready && resume_admission {
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

mod control;
pub use control::CoordinatorControl;

#[cfg(test)]
#[cfg(test)]
mod tests;
