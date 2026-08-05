mod auth;
mod bonjour;
mod control;
mod coordinator;
mod discovery;
mod ds4_command;
mod ds4_log;
mod manifest;
mod network_events;
mod network_snapshot;
mod platform;
mod process;
mod restart;
mod role;
mod runtime;
mod state;
mod state_store;
mod worker;

pub use auth::{
    AuthError, AuthenticatedPeer, ControlAuthenticator, ControlSecret, SignedControlHeaders,
};
pub use bonjour::{BonjourFailure, BonjourLifecycle, BonjourRegistration};
pub use control::{
    BoundedControlBody, ControlCommand, ControlEndpoint, ControlError, ControlMessage, ControlMode,
    ControlRequest, ControlResponse, ControlResponseStatus, ControlRole, HEADER_NODE, HEADER_NONCE,
    HEADER_SIGNATURE, HEADER_TIMESTAMP, NodeDescriptor, PeerLease,
};
pub use coordinator::CoordinatorControl;
pub use discovery::{
    CandidateError, CandidateSource, DiscoveryCandidate, DiscoveryInput, DiscoveryTracker,
    ResolvedBonjourService,
};
pub use ds4_command::{Ds4Command, Ds4CommandError, Ds4Profile, build_standalone_command};
pub use ds4_log::{
    ChildLogForwarders, ChildLogRecord, ChildLogStream, Ds4LogEvent, MAX_CHILD_LOG_LINE_BYTES,
    parse_ds4_log_event, spawn_child_log_forwarders,
};
pub use manifest::{
    DEPLOYMENT_MANIFEST_SCHEMA_VERSION, DistributedManifest, FileFingerprint, FingerprintCache,
    FingerprintCacheState, FingerprintJob, FingerprintJobError, FingerprintJobStatus,
    FingerprintJobs, ManifestError, StandaloneManifest, fingerprint_file,
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
    process::{MacOsProcessInspector, MacOsProcessSignaler},
};
pub use process::platform_process_controller;
pub use process::{
    ChildIdentity, ManagedChild, ObservedProcess, ProcessControlError, ProcessController,
    ProcessInspector, ProcessSignal, ProcessSignaler, StandaloneSupervisor, VerifiedProcess,
    argv_sha256, wait_for_http_readiness,
};
pub use restart::{
    RestartDecision, RestartManualReason, RestartReconcileError, reconcile_restart,
    required_port_available,
};
pub use role::{RoleAssessment, assess_role};
pub use runtime::{LocalStandaloneLifecycle, ModeRuntime, RuntimeError, RuntimePeerControl};
pub use state::{
    ClusterEvent, ClusterEventKind, ClusterHandle, ClusterSnapshot, TransitionError,
    spawn_state_machine,
};
pub use state_store::{
    PERSISTENT_STATE_SCHEMA_VERSION, PersistentChild, PersistentClusterState,
    PersistentFailureCode, PersistentMode, PersistentProxyTarget, StateStore, StateStoreError,
};
pub use worker::WorkerControl;
