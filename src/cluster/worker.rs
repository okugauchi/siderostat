use super::{
    AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlResponse, ControlResponseStatus, ControlRole, DistributedControlPhase, NodeDescriptor,
    PeerLease, WorkerEventKind, control::ControlProcessor,
};
use std::time::Duration;

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
    use std::net::IpAddr;

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
}
