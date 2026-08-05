use crate::target::{ClusterState, LocalRole, ProxyTarget, StableMode, resolve_target};
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
    BeginDemotion,
    PeerLost,
    EnterBackoff,
    RequireManualIntervention,
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
                current = next;
                publisher.send_replace(next);
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
}
