use super::bonjour::BonjourFailure;
use std::{collections::BTreeSet, net::Ipv4Addr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSource {
    Bonjour,
    StaticFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBonjourService {
    pub generation: u64,
    pub interface_index: u32,
    pub node_id: String,
    pub protocol_version: u16,
    pub address: Ipv4Addr,
    pub port: u16,
    pub route_scoped_to_interface: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryInput {
    pub generation: u64,
    pub expected_interface_index: u32,
    pub local_node_id: String,
    pub local_address: Ipv4Addr,
    pub expected_peer_address: Ipv4Addr,
    pub expected_prefix: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiscoveryCandidate {
    pub source: CandidateSource,
    pub address: Ipv4Addr,
    pub port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateError {
    OldGeneration,
    SelfResult,
    WrongInterface,
    WrongProtocol,
    WrongSubnet,
    UnexpectedAddress,
    RouteNotScoped,
    InvalidPort,
    StaticFallbackNotAllowed,
}

#[derive(Debug)]
pub struct DiscoveryTracker {
    input: DiscoveryInput,
    candidates: BTreeSet<DiscoveryCandidate>,
}

impl DiscoveryTracker {
    pub fn new(input: DiscoveryInput) -> Self {
        Self {
            input,
            candidates: BTreeSet::new(),
        }
    }

    pub fn accept_bonjour(
        &mut self,
        service: &ResolvedBonjourService,
    ) -> Result<bool, CandidateError> {
        if service.generation != self.input.generation {
            return Err(CandidateError::OldGeneration);
        }
        if service.node_id == self.input.local_node_id
            || service.address == self.input.local_address
        {
            return Err(CandidateError::SelfResult);
        }
        if service.interface_index != self.input.expected_interface_index {
            return Err(CandidateError::WrongInterface);
        }
        if service.protocol_version != 1 {
            return Err(CandidateError::WrongProtocol);
        }
        if service.port == 0 {
            return Err(CandidateError::InvalidPort);
        }
        if !same_subnet(
            self.input.local_address,
            service.address,
            self.input.expected_prefix,
        ) {
            return Err(CandidateError::WrongSubnet);
        }
        if service.address != self.input.expected_peer_address {
            return Err(CandidateError::UnexpectedAddress);
        }
        if !service.route_scoped_to_interface {
            return Err(CandidateError::RouteNotScoped);
        }
        Ok(self.candidates.insert(DiscoveryCandidate {
            source: CandidateSource::Bonjour,
            address: service.address,
            port: service.port,
        }))
    }

    pub fn static_fallback(
        &mut self,
        failure: BonjourFailure,
        port: u16,
        route_scoped_to_interface: bool,
    ) -> Result<bool, CandidateError> {
        if !failure.allows_static_fallback() {
            return Err(CandidateError::StaticFallbackNotAllowed);
        }
        if port == 0 {
            return Err(CandidateError::InvalidPort);
        }
        if !route_scoped_to_interface {
            return Err(CandidateError::RouteNotScoped);
        }
        Ok(self.candidates.insert(DiscoveryCandidate {
            source: CandidateSource::StaticFallback,
            address: self.input.expected_peer_address,
            port,
        }))
    }

    pub fn candidates(&self) -> Vec<DiscoveryCandidate> {
        self.candidates.iter().copied().collect()
    }
}

fn same_subnet(left: Ipv4Addr, right: Ipv4Addr, prefix: u8) -> bool {
    if prefix > 32 {
        return false;
    }
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(left) & mask == u32::from(right) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> DiscoveryInput {
        DiscoveryInput {
            generation: 5,
            expected_interface_index: 13,
            local_node_id: "local".into(),
            local_address: Ipv4Addr::new(10, 99, 0, 1),
            expected_peer_address: Ipv4Addr::new(10, 99, 0, 2),
            expected_prefix: 30,
        }
    }

    fn service() -> ResolvedBonjourService {
        ResolvedBonjourService {
            generation: 5,
            interface_index: 13,
            node_id: "peer".into(),
            protocol_version: 1,
            address: Ipv4Addr::new(10, 99, 0, 2),
            port: 9920,
            route_scoped_to_interface: true,
        }
    }

    #[test]
    fn accepts_one_valid_candidate_and_deduplicates_it() {
        let mut tracker = DiscoveryTracker::new(input());
        assert_eq!(tracker.accept_bonjour(&service()), Ok(true));
        assert_eq!(tracker.accept_bonjour(&service()), Ok(false));
        assert_eq!(tracker.candidates().len(), 1);
    }

    #[test]
    fn rejects_self_interface_subnet_address_and_route_failures() {
        type MutationCase = (CandidateError, fn(&mut ResolvedBonjourService));
        let cases: [MutationCase; 5] = [
            (
                CandidateError::SelfResult,
                |value: &mut ResolvedBonjourService| value.node_id = "local".into(),
            ),
            (
                CandidateError::WrongInterface,
                |value: &mut ResolvedBonjourService| value.interface_index = 4,
            ),
            (
                CandidateError::WrongSubnet,
                |value: &mut ResolvedBonjourService| value.address = Ipv4Addr::new(10, 99, 1, 2),
            ),
            (
                CandidateError::UnexpectedAddress,
                |value: &mut ResolvedBonjourService| value.address = Ipv4Addr::new(10, 99, 0, 3),
            ),
            (
                CandidateError::RouteNotScoped,
                |value: &mut ResolvedBonjourService| value.route_scoped_to_interface = false,
            ),
        ];
        for (expected, mutate) in cases {
            let mut value = service();
            mutate(&mut value);
            assert_eq!(
                DiscoveryTracker::new(input()).accept_bonjour(&value),
                Err(expected)
            );
        }
    }

    #[test]
    fn rejects_stale_generation_wrong_protocol_and_invalid_port() {
        // N-03 truth table: a stale candidate (old generation), a wrong protocol version, and a
        // zero port are all rejected, so none of them can make the peer present.
        let mut stale = service();
        stale.generation = 4; // input generation is 5
        assert_eq!(
            DiscoveryTracker::new(input()).accept_bonjour(&stale),
            Err(CandidateError::OldGeneration)
        );

        let mut wrong_protocol = service();
        wrong_protocol.protocol_version = 2;
        assert_eq!(
            DiscoveryTracker::new(input()).accept_bonjour(&wrong_protocol),
            Err(CandidateError::WrongProtocol)
        );

        let mut invalid_port = service();
        invalid_port.port = 0;
        assert_eq!(
            DiscoveryTracker::new(input()).accept_bonjour(&invalid_port),
            Err(CandidateError::InvalidPort)
        );
    }

    #[test]
    fn permission_failure_uses_scoped_static_fallback_only() {
        let mut tracker = DiscoveryTracker::new(input());
        assert_eq!(
            tracker.static_fallback(BonjourFailure::NotPermitted, 9920, true),
            Ok(true)
        );
        assert_eq!(
            tracker.candidates()[0].source,
            CandidateSource::StaticFallback
        );
        assert_eq!(
            DiscoveryTracker::new(input()).static_fallback(BonjourFailure::Other(-1), 9920, true,),
            Err(CandidateError::StaticFallbackNotAllowed)
        );
    }
}
