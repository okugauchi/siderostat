use crate::target::{ClusterState, LocalRole, ProxyTarget, StableMode, resolve_target};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterSnapshot {
    pub generation: u64,
    pub role: LocalRole,
    pub stable_mode: StableMode,
    pub state: ClusterState,
    pub target: ProxyTarget,
    pub local_standalone_ready: bool,
}

impl ClusterSnapshot {
    pub fn booting(role: LocalRole) -> Self {
        Self::booting_at(role, 0)
    }

    pub fn booting_at(role: LocalRole, generation: u64) -> Self {
        Self::new(
            generation,
            role,
            StableMode::SoloStandalone,
            ClusterState::Booting,
            false,
        )
    }

    fn new(
        generation: u64,
        role: LocalRole,
        stable_mode: StableMode,
        state: ClusterState,
        local_standalone_ready: bool,
    ) -> Self {
        Self {
            generation,
            role,
            stable_mode,
            state,
            target: resolve_target(role, stable_mode, state, local_standalone_ready),
            local_standalone_ready,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterEventKind {
    BeginSoloStandalone,
    LocalStandaloneReady,
    LocalStandaloneLost,
    BeginPairing,
    PairingReady,
    WorkerHelloAccepted,
    BeginPromotion,
    DistributedChildStarted,
    DistributedRouteReady,
    PromotionFailed,
    BeginDemotion,
    PeerLost,
    EnterBackoff,
    BackoffElapsed,
    RequireManualIntervention,
    OperatorReconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterFailure {
    PeerAbsent,
    BridgeUnavailable,
    BridgeAddressInvalid,
    BonjourUnavailable { static_fallback: bool },
    UnauthenticatedDiscovery,
    InvalidControlHmac,
    InvalidPeerProxyToken,
    DeploymentMismatch,
    ManifestStale,
    HelloTimeout,
    UnknownDs4Schema,
    CoordinatorStartupTimeout,
    RouteIncomplete,
    PeerLeaseLost,
    ChildIdentityUnknown,
    StandaloneStartFailed,
    DrainTimeout,
    StateCorrupt { standalone_safe: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOwner {
    PeriodicReconcile,
    RouteLossMonitor,
    Admin,
    Control,
    Promotion,
    Recovery,
}

impl EventOwner {
    pub fn name(self) -> &'static str {
        match self {
            EventOwner::PeriodicReconcile => "periodic-reconcile",
            EventOwner::RouteLossMonitor => "route-loss-monitor",
            EventOwner::Admin => "admin",
            EventOwner::Control => "control",
            EventOwner::Promotion => "promotion",
            EventOwner::Recovery => "recovery",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    MaintainCurrent,
    RejectRequest,
    RetryStaticDiscovery,
    SoloStandalone,
    PairedStandalone,
    PromotionBackoff,
    ManualIntervention,
    Unavailable,
}

pub fn failure_action(failure: ClusterFailure) -> FailureAction {
    match failure {
        ClusterFailure::PeerAbsent
        | ClusterFailure::BridgeUnavailable
        | ClusterFailure::BridgeAddressInvalid
        | ClusterFailure::PeerLeaseLost => FailureAction::SoloStandalone,
        ClusterFailure::BonjourUnavailable {
            static_fallback: true,
        } => FailureAction::RetryStaticDiscovery,
        ClusterFailure::BonjourUnavailable {
            static_fallback: false,
        } => FailureAction::SoloStandalone,
        ClusterFailure::UnauthenticatedDiscovery => FailureAction::MaintainCurrent,
        ClusterFailure::InvalidControlHmac | ClusterFailure::InvalidPeerProxyToken => {
            FailureAction::RejectRequest
        }
        ClusterFailure::DeploymentMismatch | ClusterFailure::ManifestStale => {
            FailureAction::PairedStandalone
        }
        // spec.md §31: HELLO timeout and coordinator startup timeout back off and retry
        // promotion; an unknown HELLO/log schema is refused promotion but stays Paired
        // Standalone (no backoff) so an operator can correct the peer.
        ClusterFailure::HelloTimeout | ClusterFailure::CoordinatorStartupTimeout => {
            FailureAction::PromotionBackoff
        }
        ClusterFailure::UnknownDs4Schema => FailureAction::PairedStandalone,
        ClusterFailure::RouteIncomplete => FailureAction::PairedStandalone,
        ClusterFailure::ChildIdentityUnknown => FailureAction::ManualIntervention,
        ClusterFailure::StandaloneStartFailed => FailureAction::Unavailable,
        ClusterFailure::DrainTimeout => FailureAction::ManualIntervention,
        ClusterFailure::StateCorrupt {
            standalone_safe: true,
        } => FailureAction::SoloStandalone,
        ClusterFailure::StateCorrupt {
            standalone_safe: false,
        } => FailureAction::ManualIntervention,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromotionRetryDecision {
    Backoff { retry_at_millis: u64 },
    ManualIntervention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromotionFailureStatus {
    pub failure: Option<ClusterFailure>,
    pub consecutive: u32,
    pub retry_at_millis: Option<u64>,
    pub manual: bool,
}

#[derive(Debug, Clone)]
pub struct PromotionFailureTracker {
    backoff: Duration,
    maximum: u32,
    status: PromotionFailureStatus,
}

impl PromotionFailureTracker {
    pub fn new(backoff: Duration, maximum: u32) -> Result<Self, PromotionTrackerError> {
        if backoff.is_zero() || maximum == 0 {
            return Err(PromotionTrackerError::InvalidPolicy);
        }
        Ok(Self {
            backoff,
            maximum,
            status: PromotionFailureStatus {
                failure: None,
                consecutive: 0,
                retry_at_millis: None,
                manual: false,
            },
        })
    }

    pub fn record(
        &mut self,
        failure: ClusterFailure,
        now_millis: u64,
    ) -> Result<PromotionRetryDecision, PromotionTrackerError> {
        if failure_action(failure) != FailureAction::PromotionBackoff {
            return Err(PromotionTrackerError::NotPromotionFailure);
        }
        self.status.consecutive = if self.status.failure == Some(failure) {
            self.status.consecutive.saturating_add(1)
        } else {
            1
        };
        self.status.failure = Some(failure);
        if self.status.consecutive >= self.maximum {
            self.status.retry_at_millis = None;
            self.status.manual = true;
            return Ok(PromotionRetryDecision::ManualIntervention);
        }
        let backoff_millis = self.backoff.as_millis().try_into().unwrap_or(u64::MAX);
        let retry_at_millis = now_millis.saturating_add(backoff_millis);
        self.status.retry_at_millis = Some(retry_at_millis);
        self.status.manual = false;
        Ok(PromotionRetryDecision::Backoff { retry_at_millis })
    }

    pub fn can_retry(&self, now_millis: u64) -> bool {
        !self.status.manual
            && self
                .status
                .retry_at_millis
                .is_none_or(|retry_at| now_millis >= retry_at)
    }

    pub fn note_success(&mut self) {
        self.reset();
    }

    pub fn operator_reconcile(&mut self) {
        self.reset();
    }

    pub fn status(&self) -> PromotionFailureStatus {
        self.status
    }

    fn reset(&mut self) {
        self.status = PromotionFailureStatus {
            failure: None,
            consecutive: 0,
            retry_at_millis: None,
            manual: false,
        };
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PromotionTrackerError {
    #[error("promotion backoff and maximum failures must be positive")]
    InvalidPolicy,
    #[error("failure is not eligible for promotion retry")]
    NotPromotionFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterEvent {
    pub expected_generation: u64,
    pub kind: ClusterEventKind,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    #[error("stale cluster generation: expected {expected}, current {current}")]
    StaleGeneration { expected: u64, current: u64 },
    #[error("invalid cluster transition from {from:?} via {event:?}")]
    InvalidTransition {
        from: ClusterState,
        event: ClusterEventKind,
    },
    #[error("cluster state machine stopped")]
    Stopped,
}

struct Command {
    event: ClusterEvent,
    reply: oneshot::Sender<Result<ClusterSnapshot, TransitionError>>,
}

#[derive(Clone)]
pub struct ClusterHandle {
    commands: mpsc::Sender<Command>,
    snapshots: watch::Receiver<ClusterSnapshot>,
}

impl ClusterHandle {
    pub fn snapshot(&self) -> ClusterSnapshot {
        *self.snapshots.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<ClusterSnapshot> {
        self.snapshots.clone()
    }

    pub async fn apply(&self, event: ClusterEvent) -> Result<ClusterSnapshot, TransitionError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command { event, reply })
            .await
            .map_err(|_| TransitionError::Stopped)?;
        response.await.map_err(|_| TransitionError::Stopped)?
    }
}

pub fn spawn_state_machine(
    initial: ClusterSnapshot,
    channel_capacity: usize,
) -> (ClusterHandle, tokio::task::JoinHandle<()>) {
    assert!(
        channel_capacity > 0,
        "channel capacity must be greater than zero"
    );
    let (commands, mut receiver) = mpsc::channel::<Command>(channel_capacity);
    let (publisher, snapshots) = watch::channel(initial);
    let task = tokio::spawn(async move {
        let mut current = initial;
        while let Some(command) = receiver.recv().await {
            let result = transition(current, command.event);
            if let Ok(next) = result {
                tracing::info!(
                    event = cluster_event_name(command.event.kind, next.state),
                    from = ?current.state,
                    to = ?next.state,
                    reason = ?command.event.kind,
                    result = "success",
                    generation = next.generation,
                    "cluster state transition"
                );
                current = next;
                publisher.send_replace(next);
            } else if let Err(error) = &result {
                tracing::warn!(
                    event = "cluster-transition-rejected",
                    from = ?current.state,
                    to = ?current.state,
                    reason = ?command.event.kind,
                    result = "rejected",
                    generation = current.generation,
                    error = %error,
                    "cluster state transition rejected"
                );
            }
            let _ = command.reply.send(result);
        }
    });
    (
        ClusterHandle {
            commands,
            snapshots,
        },
        task,
    )
}

pub(crate) fn transition_name(from: ClusterState, to: ClusterState) -> &'static str {
    match (from, to) {
        (ClusterState::SoloStandaloneReady, ClusterState::Pairing) => "pair",
        (ClusterState::PairedStandaloneReady, ClusterState::AwaitingWorkerHello)
        | (ClusterState::AwaitingWorkerHello, ClusterState::Promoting)
        | (ClusterState::Promoting, ClusterState::DistributedStarting)
        | (ClusterState::DistributedStarting, ClusterState::DistributedReady) => "promote",
        (ClusterState::DistributedReady, ClusterState::Demoting)
        | (ClusterState::Demoting, ClusterState::PairedStandaloneReady) => "demote",
        _ => "reconcile",
    }
}

fn cluster_event_name(event: ClusterEventKind, state: ClusterState) -> &'static str {
    match (event, state) {
        (_, ClusterState::SoloStandaloneReady) => "solo_standalone_ready",
        (ClusterEventKind::BeginPairing, _) => "pairing_started",
        (_, ClusterState::PairedStandaloneReady) => "paired_standalone_ready",
        (ClusterEventKind::BeginPromotion, _) => "promotion_started",
        (ClusterEventKind::WorkerHelloAccepted, _) => "ds4_hello_received",
        (_, ClusterState::DistributedReady) => "distributed_route_ready",
        (ClusterEventKind::BeginDemotion, _) => "demotion_started",
        (ClusterEventKind::PeerLost, _) => "fallback_ready",
        (ClusterEventKind::RequireManualIntervention, _) => "manual_intervention_required",
        _ => "cluster_transition",
    }
}

fn transition(
    current: ClusterSnapshot,
    event: ClusterEvent,
) -> Result<ClusterSnapshot, TransitionError> {
    if event.expected_generation != current.generation {
        return Err(TransitionError::StaleGeneration {
            expected: event.expected_generation,
            current: current.generation,
        });
    }
    let (mode, state, local_ready) = match (current.state, event.kind) {
        (ClusterState::Booting, ClusterEventKind::BeginSoloStandalone)
        | (ClusterState::Backoff, ClusterEventKind::BeginSoloStandalone) => (
            StableMode::SoloStandalone,
            ClusterState::SoloStandaloneStarting,
            false,
        ),
        (ClusterState::SoloStandaloneStarting, ClusterEventKind::LocalStandaloneReady) => (
            StableMode::SoloStandalone,
            ClusterState::SoloStandaloneReady,
            true,
        ),
        (ClusterState::SoloStandaloneReady, ClusterEventKind::LocalStandaloneLost) => (
            StableMode::SoloStandalone,
            ClusterState::SoloStandaloneStarting,
            false,
        ),
        (ClusterState::SoloStandaloneReady, ClusterEventKind::BeginPairing) => (
            StableMode::SoloStandalone,
            ClusterState::Pairing,
            current.local_standalone_ready,
        ),
        (ClusterState::Pairing, ClusterEventKind::PairingReady) => (
            StableMode::PairedStandalone,
            ClusterState::PairedStandaloneReady,
            current.role != LocalRole::Worker,
        ),
        (ClusterState::PairedStandaloneReady, ClusterEventKind::BeginPromotion) => (
            StableMode::PairedStandalone,
            ClusterState::AwaitingWorkerHello,
            current.local_standalone_ready,
        ),
        (ClusterState::AwaitingWorkerHello, ClusterEventKind::WorkerHelloAccepted) => (
            StableMode::PairedStandalone,
            ClusterState::Promoting,
            current.local_standalone_ready,
        ),
        (ClusterState::Promoting, ClusterEventKind::DistributedChildStarted) => (
            StableMode::PairedStandalone,
            ClusterState::DistributedStarting,
            false,
        ),
        (ClusterState::DistributedStarting, ClusterEventKind::DistributedRouteReady) => (
            StableMode::DistributedMxfp4,
            ClusterState::DistributedReady,
            current.role != LocalRole::Worker,
        ),
        (
            ClusterState::AwaitingWorkerHello
            | ClusterState::Promoting
            | ClusterState::DistributedStarting,
            ClusterEventKind::PromotionFailed,
        ) => (
            StableMode::PairedStandalone,
            ClusterState::PairedStandaloneReady,
            current.role != LocalRole::Worker,
        ),
        (ClusterState::DistributedReady, ClusterEventKind::BeginDemotion) => {
            (StableMode::DistributedMxfp4, ClusterState::Demoting, false)
        }
        (ClusterState::Demoting, ClusterEventKind::PairingReady) => (
            StableMode::PairedStandalone,
            ClusterState::PairedStandaloneReady,
            current.role != LocalRole::Worker,
        ),
        (
            ClusterState::Pairing
            | ClusterState::PairedStandaloneReady
            | ClusterState::AwaitingWorkerHello
            | ClusterState::Promoting
            | ClusterState::DistributedStarting
            | ClusterState::DistributedReady
            | ClusterState::Demoting,
            ClusterEventKind::PeerLost,
        ) => (
            StableMode::SoloStandalone,
            ClusterState::SoloStandaloneStarting,
            false,
        ),
        (_, ClusterEventKind::EnterBackoff) => (
            current.stable_mode,
            ClusterState::Backoff,
            current.local_standalone_ready,
        ),
        (ClusterState::Backoff, ClusterEventKind::BackoffElapsed)
        | (ClusterState::ManualInterventionRequired, ClusterEventKind::OperatorReconcile) => (
            current.stable_mode,
            stable_ready_state(current.stable_mode),
            current.local_standalone_ready,
        ),
        (_, ClusterEventKind::RequireManualIntervention) => (
            current.stable_mode,
            ClusterState::ManualInterventionRequired,
            current.local_standalone_ready,
        ),
        _ => {
            return Err(TransitionError::InvalidTransition {
                from: current.state,
                event: event.kind,
            });
        }
    };
    Ok(ClusterSnapshot::new(
        current.generation + 1,
        current.role,
        mode,
        state,
        local_ready,
    ))
}

fn stable_ready_state(mode: StableMode) -> ClusterState {
    match mode {
        StableMode::SoloStandalone => ClusterState::SoloStandaloneReady,
        StableMode::PairedStandalone => ClusterState::PairedStandaloneReady,
        StableMode::DistributedMxfp4 => ClusterState::DistributedReady,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(generation: u64, kind: ClusterEventKind) -> ClusterEvent {
        ClusterEvent {
            expected_generation: generation,
            kind,
        }
    }

    #[tokio::test]
    async fn rejects_old_generation_without_changing_snapshot() {
        let (handle, task) =
            spawn_state_machine(ClusterSnapshot::booting(LocalRole::Coordinator), 4);
        let next = handle
            .apply(event(0, ClusterEventKind::BeginSoloStandalone))
            .await
            .unwrap();
        let error = handle
            .apply(event(0, ClusterEventKind::LocalStandaloneReady))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            TransitionError::StaleGeneration {
                expected: 0,
                current: 1
            }
        );
        assert_eq!(handle.snapshot(), next);
        task.abort();
    }

    #[tokio::test]
    async fn rejects_invalid_transition_without_incrementing_generation() {
        let initial = ClusterSnapshot::booting(LocalRole::Coordinator);
        let (handle, task) = spawn_state_machine(initial, 4);
        let error = handle
            .apply(event(0, ClusterEventKind::DistributedRouteReady))
            .await
            .unwrap_err();
        assert!(matches!(error, TransitionError::InvalidTransition { .. }));
        assert_eq!(handle.snapshot(), initial);
        task.abort();
    }

    #[tokio::test]
    async fn concurrent_events_are_serialized_by_single_writer() {
        let (handle, task) =
            spawn_state_machine(ClusterSnapshot::booting(LocalRole::Coordinator), 16);
        let requests = (0..16).map(|_| {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .apply(event(0, ClusterEventKind::BeginSoloStandalone))
                    .await
            })
        });
        let results = futures::future::join_all(requests).await;
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(Ok(_))))
                .count(),
            1
        );
        assert_eq!(handle.snapshot().generation, 1);
        assert_eq!(
            handle.snapshot().state,
            ClusterState::SoloStandaloneStarting
        );
        task.abort();
    }

    #[test]
    fn transition_name_classifies_promotion_and_demotion_paths() {
        assert_eq!(
            transition_name(ClusterState::SoloStandaloneReady, ClusterState::Pairing),
            "pair"
        );
        assert_eq!(
            transition_name(
                ClusterState::PairedStandaloneReady,
                ClusterState::AwaitingWorkerHello
            ),
            "promote"
        );
        assert_eq!(
            transition_name(ClusterState::AwaitingWorkerHello, ClusterState::Promoting),
            "promote"
        );
        assert_eq!(
            transition_name(ClusterState::Promoting, ClusterState::DistributedStarting),
            "promote"
        );
        assert_eq!(
            transition_name(
                ClusterState::DistributedStarting,
                ClusterState::DistributedReady
            ),
            "promote"
        );
        assert_eq!(
            transition_name(ClusterState::DistributedReady, ClusterState::Demoting),
            "demote"
        );
        assert_eq!(
            transition_name(ClusterState::Demoting, ClusterState::PairedStandaloneReady),
            "demote"
        );
        assert_eq!(
            transition_name(ClusterState::Booting, ClusterState::SoloStandaloneStarting),
            "reconcile"
        );
    }
}
