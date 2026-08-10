use super::role::{RoleAssessment, assess_role};
use crate::target::LocalRole;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThunderboltIpState {
    ServiceMissing,
    ServiceDisabled,
    InterfaceUnavailable,
    AddressMissing,
    AddressConflict,
    ReadyNoPeer,
    PeerCandidateFound,
    AuthenticatedPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Assignment {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkServiceObservation {
    pub enabled: bool,
    pub ipv4_enabled: bool,
    pub configured_addresses: Vec<Ipv4Assignment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceObservation {
    pub name: String,
    pub up: bool,
    pub ipv4_addresses: Vec<Ipv4Assignment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerObservation {
    pub candidate_address: Option<Ipv4Addr>,
    pub route_scoped_to_interface: bool,
    pub authenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkObservation {
    pub expected_interface: String,
    pub coordinator_address: Ipv4Addr,
    pub worker_address: Ipv4Addr,
    pub expected_prefix: u8,
    pub service: Option<NetworkServiceObservation>,
    pub interface: Option<InterfaceObservation>,
    pub peer: PeerObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkSnapshot {
    pub state: ThunderboltIpState,
    pub role: LocalRole,
    pub local_address: Option<Ipv4Addr>,
    pub expected_peer_address: Option<Ipv4Addr>,
    pub peer_present: bool,
}

impl NetworkSnapshot {
    pub fn from_observation(observation: &NetworkObservation) -> Self {
        let Some(service) = &observation.service else {
            return unavailable(ThunderboltIpState::ServiceMissing);
        };
        if !service.enabled || !service.ipv4_enabled {
            return unavailable(ThunderboltIpState::ServiceDisabled);
        }
        let Some(interface) = &observation.interface else {
            return unavailable(ThunderboltIpState::InterfaceUnavailable);
        };
        if interface.name != observation.expected_interface || !interface.up {
            return unavailable(ThunderboltIpState::InterfaceUnavailable);
        }

        let role = assess_role(
            &interface.ipv4_addresses,
            observation.coordinator_address,
            observation.worker_address,
            observation.expected_prefix,
        );
        let RoleAssessment::Known {
            role,
            address: local_address,
        } = role
        else {
            return unavailable(match role {
                RoleAssessment::Missing => ThunderboltIpState::AddressMissing,
                RoleAssessment::Conflict => ThunderboltIpState::AddressConflict,
                RoleAssessment::Known { .. } => unreachable!(),
            });
        };
        if assess_role(
            &service.configured_addresses,
            observation.coordinator_address,
            observation.worker_address,
            observation.expected_prefix,
        ) != (RoleAssessment::Known {
            role,
            address: local_address,
        }) {
            return unavailable(ThunderboltIpState::AddressConflict);
        }

        let expected_peer_address = match role {
            LocalRole::Coordinator => observation.worker_address,
            LocalRole::Worker => observation.coordinator_address,
            LocalRole::Unknown => unreachable!(),
        };
        let candidate_valid = observation.peer.candidate_address == Some(expected_peer_address)
            && observation.peer.route_scoped_to_interface;
        let state = if candidate_valid && observation.peer.authenticated {
            ThunderboltIpState::AuthenticatedPeer
        } else if candidate_valid {
            ThunderboltIpState::PeerCandidateFound
        } else {
            ThunderboltIpState::ReadyNoPeer
        };
        Self {
            state,
            role,
            local_address: Some(local_address),
            expected_peer_address: Some(expected_peer_address),
            peer_present: state == ThunderboltIpState::AuthenticatedPeer,
        }
    }
}

fn unavailable(state: ThunderboltIpState) -> NetworkSnapshot {
    NetworkSnapshot {
        state,
        role: LocalRole::Unknown,
        local_address: None,
        expected_peer_address: None,
        peer_present: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COORDINATOR: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
    const WORKER: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 2);

    fn address(value: Ipv4Addr) -> Ipv4Assignment {
        Ipv4Assignment {
            address: value,
            prefix: 30,
        }
    }

    fn ready_observation() -> NetworkObservation {
        NetworkObservation {
            expected_interface: "bridge0".into(),
            coordinator_address: COORDINATOR,
            worker_address: WORKER,
            expected_prefix: 30,
            service: Some(NetworkServiceObservation {
                enabled: true,
                ipv4_enabled: true,
                configured_addresses: vec![address(COORDINATOR)],
            }),
            interface: Some(InterfaceObservation {
                name: "bridge0".into(),
                up: true,
                ipv4_addresses: vec![address(COORDINATOR)],
            }),
            peer: PeerObservation::default(),
        }
    }

    #[test]
    fn covers_every_thunderbolt_ip_state() {
        let mut observation = ready_observation();
        observation.service = None;
        assert_state(&observation, ThunderboltIpState::ServiceMissing);

        observation = ready_observation();
        observation.service.as_mut().unwrap().enabled = false;
        assert_state(&observation, ThunderboltIpState::ServiceDisabled);

        observation = ready_observation();
        observation.interface.as_mut().unwrap().up = false;
        assert_state(&observation, ThunderboltIpState::InterfaceUnavailable);

        observation = ready_observation();
        observation
            .interface
            .as_mut()
            .unwrap()
            .ipv4_addresses
            .clear();
        assert_state(&observation, ThunderboltIpState::AddressMissing);

        observation = ready_observation();
        observation.interface.as_mut().unwrap().ipv4_addresses = vec![address(WORKER)];
        assert_state(&observation, ThunderboltIpState::AddressConflict);

        observation = ready_observation();
        assert_state(&observation, ThunderboltIpState::ReadyNoPeer);

        observation.peer.candidate_address = Some(WORKER);
        observation.peer.route_scoped_to_interface = true;
        assert_state(&observation, ThunderboltIpState::PeerCandidateFound);

        observation.peer.authenticated = true;
        let snapshot = NetworkSnapshot::from_observation(&observation);
        assert_eq!(snapshot.state, ThunderboltIpState::AuthenticatedPeer);
        assert!(snapshot.peer_present);
    }

    #[test]
    fn wrong_route_or_candidate_never_becomes_peer_present() {
        let mut observation = ready_observation();
        observation.peer = PeerObservation {
            candidate_address: Some(WORKER),
            route_scoped_to_interface: false,
            authenticated: true,
        };
        let snapshot = NetworkSnapshot::from_observation(&observation);
        assert_eq!(snapshot.state, ThunderboltIpState::ReadyNoPeer);
        assert!(!snapshot.peer_present);
    }

    fn assert_state(observation: &NetworkObservation, expected: ThunderboltIpState) {
        let snapshot = NetworkSnapshot::from_observation(observation);
        assert_eq!(snapshot.state, expected);
        assert!(!snapshot.peer_present);
    }
}
