mod admin;
mod artifacts;
mod auth;
mod bonjour;
mod capability;
mod control;
mod coordinator;
mod discovery;
mod dry_run;
mod ds4_command;
mod ds4_hello;
mod ds4_log;
mod manifest;
mod network_events;
mod network_evidence;
mod network_snapshot;
mod operation;
mod platform;
mod policy;
mod policy_journal;
mod process;
mod production;
mod recovery_tp;
mod restart;
mod role;
mod runtime;
mod state;
mod state_store;
mod tp;
mod worker;

pub use admin::{
    AdminAction, AdminController, AdminExecutor, AdminFuture, AdminJob, AdminJobState,
    AdminStartError, FingerprintProfile, encode_token,
};
pub use artifacts::{ResolveError, ResolvedDs4Profile, convert_layer_parallel};
pub use auth::{
    AuthError, AuthenticatedPeer, ControlAuthenticator, ControlSecret, SignedControlHeaders,
};
pub use bonjour::{BonjourFailure, BonjourLifecycle, BonjourRegistration};
pub use capability::{
    CAPABILITY_MANIFEST_SCHEMA_VERSION, CapabilityAssessment, CapabilityError, CapabilityStatus,
    Ds4CapabilityManifest, ExecutableKind, MainAncestryProof, RoleArtifact, RoleKind, Verification,
};
pub use control::{
    BoundedControlBody, ControlCommand, ControlEndpoint, ControlError, ControlMessage, ControlMode,
    ControlRequest, ControlResponse, ControlResponseStatus, ControlRole, DistributedControlPhase,
    HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, NodeDescriptor, PeerLease,
    WorkerEventKind,
};
pub use coordinator::{
    CoordinatorControl, CoordinatorDistributedRuntime, CoordinatorLeaseStatus,
    CoordinatorLifecycleError, CoordinatorPeerLifecycle, CoordinatorRuntimeTimeouts,
    DistributedCoordinatorLifecycle, PromotionRetryPolicy,
};
pub use discovery::{
    CandidateError, CandidateSource, DiscoveryCandidate, DiscoveryInput, DiscoveryTracker,
    ResolvedBonjourService,
};
pub(crate) use dry_run::{
    DryRunCoordinatorLifecycle, DryRunHello, DryRunRouteProbe, DryRunWorkerLifecycle,
};
pub use ds4_command::{
    Ds4Command, Ds4CommandError, Ds4Profile, build_distributed_coordinator_command,
    build_distributed_worker_command, build_standalone_command, build_tp_coordinator_command,
    build_tp_worker_command,
};
pub use ds4_hello::{
    DS4D_HELLO_KIND, DS4D_MAGIC, Ds4Hello, Ds4HelloError, HELLO_FIXED_BYTES,
    HELLO_MAX_MODEL_NAME_BYTES, RendezvousControlSnapshot, RendezvousListener,
    WorkerHelloExpectation, build_hello_frame, parse_hello_frame, read_hello_frame,
    validate_worker_hello,
};
pub use ds4_log::{
    ChildLogForwarders, ChildLogRecord, ChildLogStream, Ds4LogEvent, MAX_CHILD_LOG_LINE_BYTES,
    parse_ds4_log_event, spawn_child_log_forwarders, spawn_child_log_forwarders_with_events,
};
pub use manifest::{
    DEPLOYMENT_MANIFEST_SCHEMA_VERSION, DistributedManifest, FileFingerprint, FingerprintCache,
    FingerprintCacheState, FingerprintJob, FingerprintJobError, FingerprintJobStatus,
    FingerprintJobs, ManifestError, ModelIdentity, StandaloneManifest,
    TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION, TpDeploymentManifest, fingerprint_file,
};
pub use network_events::{
    NetworkEvent, NetworkEventHandle, NetworkEventKind, RescanReason, RescanRequest,
    SpawnNetworkMonitorError, spawn_network_event_monitor,
};
pub use network_evidence::NetworkEvidence;
pub use network_snapshot::{
    InterfaceObservation, Ipv4Assignment, NetworkObservation, NetworkServiceObservation,
    NetworkSnapshot, PeerObservation, ThunderboltIpState,
};
pub use operation::{
    IdempotencyOutcome, OperationEnvelope, OperationId, OperationKind, OperationLease,
    OperationLeaseError, PolicyEpoch, TpSessionId, canonical_body_hash,
};
#[cfg(target_os = "macos")]
pub use platform::{
    bonjour::{BonjourPlatformEvent, MacOsBonjourOperation, bridge0_interface_index},
    macos::MacOsDynamicStoreWatcher,
    process::{MacOsProcessInspector, MacOsProcessSignaler},
    rdma::{
        RdmaCommandRunner, RdmaDeviceInfo, RdmaObservation, RdmaProbe, RdmaProbeError,
        RdmaProbeRequest, is_stale_epoch,
    },
};
pub use policy::OperationPolicy;
pub use policy_journal::{PolicyJournal, PolicyJournalView};
pub use process::platform_process_controller;
pub use process::{
    ChildIdentity, DistributedCoordinatorSupervisor, DistributedWorkerSupervisor, ManagedChild,
    ObservedProcess, ProcessControlError, ProcessController, ProcessIdentity, ProcessInspector,
    ProcessSignal, ProcessSignaler, StandaloneSupervisor, StartupProcessCandidate,
    StartupProcessKind, TpWorkerSupervisor, VerifiedProcess, argv_sha256,
    discover_startup_processes, wait_for_http_readiness,
};
#[cfg(feature = "test-support")]
pub use production::PairTiming;
pub use production::policy::{
    ForceApplyCoordinator, ForceNodePhase, POLICY_CONTROL_PROTOCOL_VERSION, PolicyControlError,
    PolicyControlPhase, PolicyControlRequest, PolicyControlResponse, PolicyControlState,
    PolicyControlStatus, PolicyControlVerdict, canonical_request_hash,
};
pub use production::tp::{TP_PRODUCTION_PROTOCOL_VERSION, TpStartVerdict, check_tp_start};
pub use production::{
    ChildDiagnostics, ChildrenDiagnostics, ControlSessionDiagnostics, LeaseDiagnostics,
    OperatorReconcileOutcome, PeerDiagnostics, ProductionClusterRuntime, ProductionControlClient,
    ProductionDiagnostics, detect_cluster_role,
};
pub use recovery_tp::{TpFailureKind, TpRecoveryDecision, TpRecoveryOwner, TpRecoveryTracker};
pub use restart::{
    RestartDecision, RestartManualReason, RestartReconcileError, reconcile_restart,
    required_port_available,
};
pub use role::{RoleAssessment, assess_role};
pub use runtime::{LocalStandaloneLifecycle, ModeRuntime, RuntimeError, RuntimePeerControl};
pub(crate) use state::transition_name;
pub use state::{
    ClusterEvent, ClusterEventKind, ClusterFailure, ClusterHandle, ClusterSnapshot, EventOwner,
    FailureAction, PromotionFailureStatus, PromotionFailureTracker, PromotionRetryDecision,
    PromotionTrackerError, TransitionError, failure_action, spawn_state_machine,
};
pub use state_store::{
    PERSISTENT_STATE_SCHEMA_VERSION, PERSISTENT_STATE_SCHEMA_VERSION_V1, PersistentChild,
    PersistentClusterState, PersistentFailureCode, PersistentMode, PersistentOperationPhase,
    PersistentPendingOperation, PersistentProxyTarget, StateStore, StateStoreError,
};
pub use tp::{TpReadiness, TpReadinessEvent, TpSessionState};
pub use worker::{
    DistributedWorkerLifecycle, TpConnectedObservation, TpWorkerLifecycle, TpWorkerPhase,
    TpWorkerPrepared, TpWorkerTracker, WorkerControl, WorkerDistributedRuntime, WorkerLeaseStatus,
    WorkerLifecycleError,
};
