use super::{
    AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
    ControlResponse, ControlRole, NodeDescriptor, PeerLease, control::ControlProcessor,
};
use std::time::Duration;

#[derive(Debug)]
pub struct CoordinatorControl {
    processor: ControlProcessor,
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

    pub fn advance_generation(&mut self, generation: u64) {
        self.processor.advance_generation(generation);
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
                event: "ready".into(),
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
}
