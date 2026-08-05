use super::RendezvousControlSnapshot;
use super::{
    AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlResponse, ControlResponseStatus, ControlRole, DistributedControlPhase, NodeDescriptor,
    PeerLease, WorkerEventKind, control::ControlProcessor,
};
use crate::target::ClusterState;
use std::time::Duration;

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
    use crate::cluster::{ControlMode, ControlResponseStatus};
    use std::net::IpAddr;

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
}
