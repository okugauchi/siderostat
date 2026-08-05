mod bonjour;
mod discovery;
mod network_events;
mod network_snapshot;
mod platform;
mod role;
mod state;

pub use bonjour::{BonjourFailure, BonjourLifecycle, BonjourRegistration};
pub use discovery::{
    CandidateError, CandidateSource, DiscoveryCandidate, DiscoveryInput, DiscoveryTracker,
    ResolvedBonjourService,
};
pub use network_events::{
    NetworkEvent, NetworkEventHandle, NetworkEventKind, RescanReason, RescanRequest,
    SpawnNetworkMonitorError, spawn_network_event_monitor,
};
pub use network_snapshot::{
    InterfaceObservation, Ipv4Assignment, NetworkObservation, NetworkServiceObservation,
    NetworkSnapshot, PeerObservation, ThunderboltIpState,
};
#[cfg(target_os = "macos")]
pub use platform::{
    bonjour::{BonjourPlatformEvent, MacOsBonjourOperation, bridge0_interface_index},
    macos::MacOsDynamicStoreWatcher,
};
pub use role::{RoleAssessment, assess_role};
pub use state::{
    ClusterEvent, ClusterEventKind, ClusterHandle, ClusterSnapshot, TransitionError,
    spawn_state_machine,
};
