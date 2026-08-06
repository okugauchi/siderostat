#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRole {
    Coordinator,
    Worker,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableMode {
    SoloStandalone,
    PairedStandalone,
    DistributedMxfp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterState {
    Booting,
    SoloStandaloneStarting,
    SoloStandaloneReady,
    Pairing,
    PairedStandaloneReady,
    AwaitingWorkerHello,
    Promoting,
    DistributedStarting,
    DistributedReady,
    Demoting,
    Backoff,
    ManualInterventionRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyTarget {
    LocalStandalone,
    Coordinator,
    Unavailable { reason: UnavailableReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    Transition,
    InconsistentStableState,
    UnknownRoleWithoutLocalStandalone,
}

pub fn resolve_target(
    role: LocalRole,
    stable_mode: StableMode,
    state: ClusterState,
    local_standalone_ready: bool,
) -> ProxyTarget {
    if state == ClusterState::ManualInterventionRequired && local_standalone_ready {
        return ProxyTarget::LocalStandalone;
    }
    if state == ClusterState::Backoff {
        return match (stable_mode, role, local_standalone_ready) {
            (StableMode::SoloStandalone, _, true)
            | (StableMode::PairedStandalone, LocalRole::Coordinator, true) => {
                ProxyTarget::LocalStandalone
            }
            (StableMode::PairedStandalone, LocalRole::Worker, _) => ProxyTarget::Coordinator,
            _ => ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
        };
    }
    if !matches!(
        state,
        ClusterState::SoloStandaloneReady
            | ClusterState::PairedStandaloneReady
            | ClusterState::DistributedReady
    ) {
        return ProxyTarget::Unavailable {
            reason: UnavailableReason::Transition,
        };
    }
    if role == LocalRole::Unknown {
        return if local_standalone_ready {
            ProxyTarget::LocalStandalone
        } else {
            ProxyTarget::Unavailable {
                reason: UnavailableReason::UnknownRoleWithoutLocalStandalone,
            }
        };
    }

    match (stable_mode, state, role) {
        (
            StableMode::SoloStandalone,
            ClusterState::SoloStandaloneReady,
            LocalRole::Coordinator | LocalRole::Worker,
        ) => ProxyTarget::LocalStandalone,
        (
            StableMode::PairedStandalone,
            ClusterState::PairedStandaloneReady,
            LocalRole::Coordinator,
        ) => ProxyTarget::LocalStandalone,
        (StableMode::PairedStandalone, ClusterState::PairedStandaloneReady, LocalRole::Worker) => {
            ProxyTarget::Coordinator
        }
        (StableMode::DistributedMxfp4, ClusterState::DistributedReady, LocalRole::Coordinator) => {
            ProxyTarget::LocalStandalone
        }
        (StableMode::DistributedMxfp4, ClusterState::DistributedReady, LocalRole::Worker) => {
            ProxyTarget::Coordinator
        }
        (_, ClusterState::SoloStandaloneReady, _)
        | (_, ClusterState::PairedStandaloneReady, _)
        | (_, ClusterState::DistributedReady, _) => ProxyTarget::Unavailable {
            reason: UnavailableReason::InconsistentStableState,
        },
        _ => ProxyTarget::Unavailable {
            reason: UnavailableReason::Transition,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_every_stable_mode_role_row() {
        let rows = [
            (
                LocalRole::Coordinator,
                StableMode::SoloStandalone,
                ClusterState::SoloStandaloneReady,
                ProxyTarget::LocalStandalone,
            ),
            (
                LocalRole::Worker,
                StableMode::SoloStandalone,
                ClusterState::SoloStandaloneReady,
                ProxyTarget::LocalStandalone,
            ),
            (
                LocalRole::Coordinator,
                StableMode::PairedStandalone,
                ClusterState::PairedStandaloneReady,
                ProxyTarget::LocalStandalone,
            ),
            (
                LocalRole::Worker,
                StableMode::PairedStandalone,
                ClusterState::PairedStandaloneReady,
                ProxyTarget::Coordinator,
            ),
            (
                LocalRole::Coordinator,
                StableMode::DistributedMxfp4,
                ClusterState::DistributedReady,
                ProxyTarget::LocalStandalone,
            ),
            (
                LocalRole::Worker,
                StableMode::DistributedMxfp4,
                ClusterState::DistributedReady,
                ProxyTarget::Coordinator,
            ),
        ];
        for (role, mode, state, expected) in rows {
            assert_eq!(resolve_target(role, mode, state, false), expected);
        }
    }

    #[test]
    fn transition_states_are_unavailable() {
        let transitions = [
            ClusterState::Booting,
            ClusterState::SoloStandaloneStarting,
            ClusterState::Pairing,
            ClusterState::AwaitingWorkerHello,
            ClusterState::Promoting,
            ClusterState::DistributedStarting,
            ClusterState::Demoting,
        ];
        for state in transitions {
            assert_eq!(
                resolve_target(
                    LocalRole::Coordinator,
                    StableMode::SoloStandalone,
                    state,
                    true,
                ),
                ProxyTarget::Unavailable {
                    reason: UnavailableReason::Transition,
                }
            );
        }
    }

    #[test]
    fn backoff_and_manual_keep_a_safe_standalone_target_serving() {
        assert_eq!(
            resolve_target(
                LocalRole::Coordinator,
                StableMode::PairedStandalone,
                ClusterState::Backoff,
                true,
            ),
            ProxyTarget::LocalStandalone
        );
        assert_eq!(
            resolve_target(
                LocalRole::Worker,
                StableMode::SoloStandalone,
                ClusterState::ManualInterventionRequired,
                true,
            ),
            ProxyTarget::LocalStandalone
        );
        assert!(matches!(
            resolve_target(
                LocalRole::Worker,
                StableMode::PairedStandalone,
                ClusterState::ManualInterventionRequired,
                false,
            ),
            ProxyTarget::Unavailable { .. }
        ));
        assert!(matches!(
            resolve_target(
                LocalRole::Coordinator,
                StableMode::DistributedMxfp4,
                ClusterState::Backoff,
                false,
            ),
            ProxyTarget::Unavailable { .. }
        ));
    }

    #[test]
    fn inconsistent_mode_and_ready_state_are_unavailable() {
        assert_eq!(
            resolve_target(
                LocalRole::Worker,
                StableMode::SoloStandalone,
                ClusterState::DistributedReady,
                true,
            ),
            ProxyTarget::Unavailable {
                reason: UnavailableReason::InconsistentStableState,
            }
        );
    }

    #[test]
    fn unknown_role_uses_only_local_readiness() {
        assert_eq!(
            resolve_target(
                LocalRole::Unknown,
                StableMode::DistributedMxfp4,
                ClusterState::SoloStandaloneReady,
                true,
            ),
            ProxyTarget::LocalStandalone
        );
        assert_eq!(
            resolve_target(
                LocalRole::Unknown,
                StableMode::SoloStandalone,
                ClusterState::SoloStandaloneReady,
                false,
            ),
            ProxyTarget::Unavailable {
                reason: UnavailableReason::UnknownRoleWithoutLocalStandalone,
            }
        );
    }
}
