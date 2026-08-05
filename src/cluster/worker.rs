use super::{
    AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlResponse, ControlRole, NodeDescriptor, PeerLease, control::ControlProcessor,
};
use std::time::Duration;

#[derive(Debug)]
pub struct WorkerControl {
    processor: ControlProcessor,
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
        self.processor
            .handle(endpoint, message, authenticated, route_scoped, now_millis)
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
                event: "ready".into(),
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
}
