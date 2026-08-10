use super::network_snapshot::Ipv4Assignment;
use crate::target::LocalRole;
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleAssessment {
    Missing,
    Conflict,
    Known { role: LocalRole, address: Ipv4Addr },
}

pub fn assess_role(
    addresses: &[Ipv4Assignment],
    coordinator_address: Ipv4Addr,
    worker_address: Ipv4Addr,
    expected_prefix: u8,
) -> RoleAssessment {
    let relevant = addresses
        .iter()
        .filter(|assignment| !assignment.address.is_link_local())
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        return RoleAssessment::Missing;
    }
    if relevant.len() != 1 {
        return RoleAssessment::Conflict;
    }
    let assignment = relevant[0];
    if assignment.prefix != expected_prefix {
        return RoleAssessment::Conflict;
    }
    let role = if assignment.address == coordinator_address {
        LocalRole::Coordinator
    } else if assignment.address == worker_address {
        LocalRole::Worker
    } else {
        return RoleAssessment::Conflict;
    };
    RoleAssessment::Known {
        role,
        address: assignment.address,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COORDINATOR: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
    const WORKER: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 2);

    fn address(address: Ipv4Addr, prefix: u8) -> Ipv4Assignment {
        Ipv4Assignment { address, prefix }
    }

    #[test]
    fn maps_fixed_addresses_and_rejects_unknown_role() {
        assert_eq!(
            assess_role(&[address(COORDINATOR, 30)], COORDINATOR, WORKER, 30),
            RoleAssessment::Known {
                role: LocalRole::Coordinator,
                address: COORDINATOR,
            }
        );
        assert_eq!(
            assess_role(&[address(WORKER, 30)], COORDINATOR, WORKER, 30),
            RoleAssessment::Known {
                role: LocalRole::Worker,
                address: WORKER,
            }
        );
        assert_eq!(
            assess_role(
                &[address(Ipv4Addr::new(10, 99, 0, 9), 30)],
                COORDINATOR,
                WORKER,
                30,
            ),
            RoleAssessment::Conflict
        );
        assert_eq!(
            assess_role(&[], COORDINATOR, WORKER, 30),
            RoleAssessment::Missing
        );
    }

    #[test]
    fn rejects_prefix_and_multiple_address_conflicts() {
        assert_eq!(
            assess_role(&[address(COORDINATOR, 24)], COORDINATOR, WORKER, 30),
            RoleAssessment::Conflict
        );
        assert_eq!(
            assess_role(
                &[address(COORDINATOR, 30), address(WORKER, 30)],
                COORDINATOR,
                WORKER,
                30,
            ),
            RoleAssessment::Conflict
        );
    }
}
