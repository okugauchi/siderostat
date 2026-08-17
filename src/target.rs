#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRole {
    Coordinator,
    Worker,
    Unknown,
}

impl LocalRole {
    pub fn name(self) -> &'static str {
        match self {
            LocalRole::Coordinator => "coordinator",
            LocalRole::Worker => "worker",
            LocalRole::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableMode {
    SoloStandalone,
    PairedStandalone,
    DistributedMxfp4,
}

impl StableMode {
    pub fn name(self) -> &'static str {
        match self {
            StableMode::SoloStandalone => "solo-standalone",
            StableMode::PairedStandalone => "paired-standalone",
            StableMode::DistributedMxfp4 => "distributed-mxfp4",
        }
    }
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

impl ClusterState {
    pub fn name(self) -> &'static str {
        match self {
            ClusterState::Booting => "booting",
            ClusterState::SoloStandaloneStarting => "solo-standalone-starting",
            ClusterState::SoloStandaloneReady => "solo-standalone-ready",
            ClusterState::Pairing => "pairing",
            ClusterState::PairedStandaloneReady => "paired-standalone-ready",
            ClusterState::AwaitingWorkerHello => "awaiting-worker-hello",
            ClusterState::Promoting => "promoting",
            ClusterState::DistributedStarting => "distributed-starting",
            ClusterState::DistributedReady => "distributed-ready",
            ClusterState::Demoting => "demoting",
            ClusterState::Backoff => "backoff",
            ClusterState::ManualInterventionRequired => "manual-intervention-required",
        }
    }
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
    #[test]
    fn enum_names_are_stable_metric_labels() {
        assert_eq!(LocalRole::Coordinator.name(), "coordinator");
        assert_eq!(LocalRole::Worker.name(), "worker");
        assert_eq!(LocalRole::Unknown.name(), "unknown");

        assert_eq!(StableMode::SoloStandalone.name(), "solo-standalone");
        assert_eq!(StableMode::PairedStandalone.name(), "paired-standalone");
        assert_eq!(StableMode::DistributedMxfp4.name(), "distributed-mxfp4");

        assert_eq!(ClusterState::Booting.name(), "booting");
        assert_eq!(
            ClusterState::SoloStandaloneStarting.name(),
            "solo-standalone-starting"
        );
        assert_eq!(
            ClusterState::SoloStandaloneReady.name(),
            "solo-standalone-ready"
        );
        assert_eq!(ClusterState::Pairing.name(), "pairing");
        assert_eq!(
            ClusterState::PairedStandaloneReady.name(),
            "paired-standalone-ready"
        );
        assert_eq!(
            ClusterState::AwaitingWorkerHello.name(),
            "awaiting-worker-hello"
        );
        assert_eq!(ClusterState::Promoting.name(), "promoting");
        assert_eq!(
            ClusterState::DistributedStarting.name(),
            "distributed-starting"
        );
        assert_eq!(ClusterState::DistributedReady.name(), "distributed-ready");
        assert_eq!(ClusterState::Demoting.name(), "demoting");
        assert_eq!(ClusterState::Backoff.name(), "backoff");
        assert_eq!(
            ClusterState::ManualInterventionRequired.name(),
            "manual-intervention-required"
        );
    }
}
