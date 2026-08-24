use crate::{
    cluster::{
        AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
        ControlResponse, ControlResponseStatus, ControlRole, DistributedControlPhase,
        NodeDescriptor, PeerLease, RendezvousControlSnapshot, WorkerEventKind,
        control::ControlProcessor,
    },
    target::ClusterState,
};
use std::time::Duration;

#[derive(Debug)]
pub struct CoordinatorControl {
    processor: ControlProcessor,
    phase: DistributedControlPhase,
    peer_distributed_child_generation: Option<u64>,
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
            peer_distributed_child_generation: None,
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
        let peer_present = self.processor.lease().peer_present(now_millis);
        let response = self.processor.handle_validated(
            endpoint,
            message,
            authenticated,
            route_scoped,
            now_millis,
            |command| validate_coordinator_command(phase, peer_present, command),
        )?;
        if response.status == ControlResponseStatus::Applied {
            self.phase = match command {
                ControlCommand::Pair { .. } => {
                    self.peer_distributed_child_generation = None;
                    DistributedControlPhase::Paired
                }
                ControlCommand::WorkerEvent {
                    event: WorkerEventKind::Ready,
                } => {
                    self.peer_distributed_child_generation = None;
                    DistributedControlPhase::WorkerReady
                }
                ControlCommand::WorkerEvent {
                    event: WorkerEventKind::ReadyWithChildGeneration { child_generation },
                } => {
                    self.peer_distributed_child_generation = Some(child_generation);
                    DistributedControlPhase::WorkerReady
                }
                ControlCommand::WorkerEvent { .. } => {
                    self.peer_distributed_child_generation = None;
                    phase
                }
                ControlCommand::Drained => DistributedControlPhase::Drained,
                ControlCommand::CancelGeneration | ControlCommand::Demote => {
                    self.peer_distributed_child_generation = None;
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
        self.peer_distributed_child_generation = None;
        self.phase = if self.processor.lease().descriptor().is_some() {
            DistributedControlPhase::Paired
        } else {
            DistributedControlPhase::Unpaired
        };
    }

    pub fn reset_for_repair(&mut self, now_millis: u64) {
        self.peer_distributed_child_generation = None;
        self.phase = if self.processor.lease().peer_present(now_millis) {
            DistributedControlPhase::Paired
        } else {
            DistributedControlPhase::Unpaired
        };
    }

    /// Session-authority hook: compute the candidate generation from the peer's reported
    /// generation and, if it is higher than the local control session generation, advance the
    /// local generation before sending the Pair offer (design §4). This makes the negotiated
    /// session generation direction-independent. Returns the candidate actually used.
    pub fn propose_candidate(&mut self, peer_generation: u64) -> Result<u64, ControlError> {
        let candidate = self.processor.candidate_generation(peer_generation)?;
        if candidate > self.processor.generation() {
            self.processor.advance_generation(candidate);
            self.phase = if self.processor.lease().descriptor().is_some() {
                DistributedControlPhase::Paired
            } else {
                DistributedControlPhase::Unpaired
            };
        }
        Ok(candidate)
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

    pub fn peer_distributed_child_generation(&self) -> Option<u64> {
        self.peer_distributed_child_generation
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
        self.peer_distributed_child_generation = None;
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
    peer_present: bool,
    command: &ControlCommand,
) -> Result<(), ControlError> {
    let valid = match command {
        // See the worker-side guard: an active peer session must not be reset by a delayed Pair
        // while the coordinator is draining or promoting. Re-pair remains valid after lease loss.
        ControlCommand::Pair { .. } => {
            matches!(
                phase,
                DistributedControlPhase::Unpaired | DistributedControlPhase::Paired
            ) || !peer_present
        }
        ControlCommand::WorkerEvent {
            event: WorkerEventKind::Ready | WorkerEventKind::ReadyWithChildGeneration { .. },
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
