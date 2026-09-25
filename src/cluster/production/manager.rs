//! Runtime-owned command bridge for Manager activation and rollback.

#[cfg(feature = "test-support")]
use std::sync::atomic::AtomicU8;
use std::{
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

/// Wire protocol version carried by the authenticated Manager control routes.
pub const MANAGER_PEER_PROTOCOL_VERSION: u16 = 2;

#[cfg(feature = "test-support")]
static LOST_MANAGER_PEER_ACK_PHASE: AtomicU8 = AtomicU8::new(0);

#[cfg(feature = "test-support")]
pub(crate) fn lose_next_manager_peer_ack_for_test(phase: ManagerPeerPhase) {
    LOST_MANAGER_PEER_ACK_PHASE.store(manager_peer_phase_code(phase), Ordering::Release);
}

#[cfg(feature = "test-support")]
fn consume_lost_manager_peer_ack_for_test(phase: ManagerPeerPhase) -> bool {
    LOST_MANAGER_PEER_ACK_PHASE
        .compare_exchange(
            manager_peer_phase_code(phase),
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[cfg(feature = "test-support")]
const fn manager_peer_phase_code(phase: ManagerPeerPhase) -> u8 {
    match phase {
        ManagerPeerPhase::Status => 1,
        ManagerPeerPhase::ForwardActivate => 2,
        ManagerPeerPhase::ForwardRollback => 3,
        ManagerPeerPhase::Prepare => 4,
        ManagerPeerPhase::Drain => 5,
        ManagerPeerPhase::Start => 6,
        ManagerPeerPhase::Commit => 7,
        ManagerPeerPhase::Rollback => 8,
    }
}

/// Operation accepted by one authenticated Manager participant route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerPeerPhase {
    Status,
    ForwardActivate,
    ForwardRollback,
    Prepare,
    Drain,
    Start,
    Commit,
    Rollback,
}

impl ManagerPeerPhase {
    pub const fn path(self) -> &'static str {
        match self {
            Self::Status => "/v2/manager/status",
            Self::ForwardActivate => "/v2/manager/forward-activate",
            Self::ForwardRollback => "/v2/manager/forward-rollback",
            Self::Prepare => "/v2/manager/prepare",
            Self::Drain => "/v2/manager/drain",
            Self::Start => "/v2/manager/start",
            Self::Commit => "/v2/manager/commit",
            Self::Rollback => "/v2/manager/rollback",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::ForwardActivate => "forward_activate",
            Self::ForwardRollback => "forward_rollback",
            Self::Prepare => "prepare",
            Self::Drain => "drain",
            Self::Start => "start",
            Self::Commit => "commit",
            Self::Rollback => "rollback",
        }
    }
}

/// Minimal request envelope. All profile IDs and digests are node-local references; the wire
/// protocol intentionally has no artifact bytes, paths, URLs, argv, lease, or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPeerRequest {
    pub operation_id: String,
    pub profile_id: String,
    pub candidate_digest: String,
    pub source_commit: String,
    pub model_digest: String,
    pub previous_digest: Option<String>,
    pub expected_generation: u64,
    pub policy_epoch: u64,
    pub phase: ManagerPeerPhase,
    pub ack_id: Option<String>,
}

impl ManagerPeerRequest {
    pub fn validate(&self) -> Result<(), ManagerPeerProtocolError> {
        if !valid_peer_token(&self.operation_id)
            || !valid_peer_token(&self.profile_id)
            || !full_digest(&self.candidate_digest)
            || !full_git_sha(&self.source_commit)
            || !full_digest(&self.model_digest)
            || self
                .previous_digest
                .as_deref()
                .is_some_and(|digest| !full_digest(digest))
            || self.expected_generation == 0
        {
            return Err(ManagerPeerProtocolError::InvalidRequest);
        }
        match self.phase {
            ManagerPeerPhase::Status
            | ManagerPeerPhase::ForwardActivate
            | ManagerPeerPhase::ForwardRollback
            | ManagerPeerPhase::Prepare
                if self.ack_id.is_none() => {}
            ManagerPeerPhase::Status
            | ManagerPeerPhase::ForwardActivate
            | ManagerPeerPhase::ForwardRollback
            | ManagerPeerPhase::Prepare => {
                return Err(ManagerPeerProtocolError::InvalidRequest);
            }
            ManagerPeerPhase::Drain
            | ManagerPeerPhase::Start
            | ManagerPeerPhase::Commit
            | ManagerPeerPhase::Rollback
                if self.ack_id.as_deref().is_some_and(valid_peer_token) => {}
            _ => return Err(ManagerPeerProtocolError::InvalidRequest),
        }
        Ok(())
    }
}

/// Durable acknowledgement returned by a participant after its phase has completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPeerResponse {
    pub protocol_version: u16,
    pub node_id: String,
    pub operation_id: String,
    pub profile_id: String,
    pub candidate_digest: String,
    pub source_commit: String,
    pub model_digest: String,
    pub previous_digest: Option<String>,
    pub expected_generation: u64,
    pub policy_epoch: u64,
    pub phase: ManagerPeerPhase,
    pub ack_id: String,
    /// Sanitized local staged candidates matching the requested source and model identity.
    #[serde(default)]
    pub profiles: Vec<ManagerPeerProfileSummary>,
    /// Digest of this node's currently active release, if one is durably recorded.
    #[serde(default)]
    pub active_release_digest: Option<String>,
    /// Live-observed model digest, only when the child matches the active release.
    #[serde(default)]
    pub active_model_digest: Option<String>,
    /// Node-local profile ID referenced by the previous release pointer; external baselines use
    /// the reserved `external-baseline` identity.
    #[serde(default)]
    pub previous_profile_id: Option<String>,
    /// Model digest referenced by the previous release pointer.
    #[serde(default)]
    pub previous_model_digest: Option<String>,
    /// Whether the previous release bytes/config can be revalidated on this node.
    #[serde(default)]
    pub previous_release_ready: bool,
    /// Local durable phase, if one transaction is unresolved or one journal exists.
    #[serde(default)]
    pub activation_phase: Option<crate::manager::store::PersistedActivationPhase>,
    /// Fixed allowlisted peer transaction failure class.
    #[serde(default)]
    pub activation_failure_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerPeerProfileSummary {
    pub profile_id: String,
    pub node_role: String,
    pub candidate_digest: String,
    pub source_commit: String,
    pub model_digest: String,
    pub model_catalog_id: String,
    pub config_fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerPeerProtocolError {
    InvalidRequest,
    WrongRole,
    WrongPeer,
    Unavailable,
    Conflict,
    StaleGeneration,
    StalePolicyEpoch,
    NotReady,
    LifecycleFailure,
}

impl std::fmt::Display for ManagerPeerProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "manager peer request is invalid",
            Self::WrongRole => "manager peer route is not available for this role",
            Self::WrongPeer => "manager peer identity does not match the paired node",
            Self::Unavailable => "manager peer participant is unavailable",
            Self::Conflict => "manager operation ID was reused with different content",
            Self::StaleGeneration => "manager peer generation is stale",
            Self::StalePolicyEpoch => "manager peer policy epoch is stale",
            Self::NotReady => "manager peer participant is not ready",
            Self::LifecycleFailure => "manager peer lifecycle operation failed",
        })
    }
}

impl std::error::Error for ManagerPeerProtocolError {}

/// In-memory half of the participant gate. The activation journal remains durable; this guard
/// prevents policy/recovery/pairing from taking ownership between the authenticated phases.
pub(super) struct ManagerPeerActive {
    operation_id: String,
    profile_id: String,
    candidate_digest: String,
    previous_digest: Option<String>,
    expected_generation: u64,
    policy_epoch: u64,
    source_commit: String,
    model_digest: String,
    _lease: crate::cluster::OperationLeaseGuard,
}

impl ManagerPeerActive {
    fn matches(&self, request: &ManagerPeerRequest) -> bool {
        self.operation_id == request.operation_id
            && self.profile_id == request.profile_id
            && self.candidate_digest == request.candidate_digest
            && self.previous_digest == request.previous_digest
            && self.expected_generation == request.expected_generation
            && self.policy_epoch == request.policy_epoch
            && self.source_commit == request.source_commit
            && self.model_digest == request.model_digest
    }
}

fn valid_peer_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn full_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn full_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl super::ProductionClusterRuntime {
    /// Authenticated, strict Manager participant endpoint. The peer identity and live control
    /// lease are checked before any durable state or child lifecycle can change.
    pub(super) async fn handle_manager_peer(
        &self,
        route_phase: ManagerPeerPhase,
        body: axum::body::Bytes,
        source: std::net::SocketAddr,
        headers: axum::http::HeaderMap,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::cluster::{
            ControlRequest, HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP,
            SignedControlHeaders,
        };

        let path = route_phase.path();
        let signed = SignedControlHeaders::from_header_values(
            super::header(&headers, HEADER_NODE)?,
            super::header(&headers, HEADER_TIMESTAMP)?,
            super::header(&headers, HEADER_NONCE)?,
            super::header(&headers, HEADER_SIGNATURE)?,
        )?;
        let authenticated = ControlRequest {
            method: "POST",
            path_and_query: path,
            body: &body,
            source_ip: source.ip(),
            headers: &signed,
        }
        .authenticate(&self.inner.authenticator, super::now_millis())?;
        let request: ManagerPeerRequest =
            serde_json::from_slice(&body).map_err(|_| ManagerPeerProtocolError::InvalidRequest)?;
        request.validate()?;
        if request.phase != route_phase {
            return Err(ManagerPeerProtocolError::InvalidRequest.into());
        }
        let (expected_peer, expected_peer_role) = self.peer_identity().await;
        if expected_peer.as_deref() != Some(authenticated.node_id()) {
            return Err(ManagerPeerProtocolError::WrongPeer.into());
        }
        validate_manager_peer_route_role(self.inner.role, expected_peer_role, request.phase)?;
        let active_rollback = if request.phase == ManagerPeerPhase::Rollback {
            self.inner
                .manager_peer_guard
                .lock()
                .await
                .as_ref()
                .is_some_and(|active| active.matches(&request))
                || manager_peer_rollback_ack_exists(self, &request)
        } else {
            false
        };
        if !self.inner.network.route_scoped()
            || !self.peer_present().await
            || (!self.inner.lease.valid() && !active_rollback)
        {
            return Err(ManagerPeerProtocolError::Unavailable.into());
        }
        let current = self.inner.mode.snapshot();
        if active_rollback {
            // The authenticated coordinator may unwind its exact durable transaction after a
            // policy change or generation advance. The active operation lease binds this
            // exception to the original request; it cannot be used for another operation.
        } else {
            validate_manager_peer_epoch(
                &request,
                current.generation,
                self.policy_epoch(),
                self.inner.policy_pending.load(Ordering::Acquire),
            )?;
        }
        if !active_rollback
            && (current.state != crate::target::ClusterState::PairedStandaloneReady
                || self.operator_policy() == crate::cluster::OperationPolicy::ForcedStandalone)
        {
            return Err(ManagerPeerProtocolError::NotReady.into());
        }
        let store = self
            .inner
            .manager_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or(ManagerPeerProtocolError::Unavailable)?;
        let result = match request.phase {
            ManagerPeerPhase::Status => {
                let (
                    node_id,
                    active_release_digest,
                    previous,
                    profiles,
                    activation_phase,
                    activation_failure_class,
                ) = {
                    let mut store = store
                        .lock()
                        .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
                    let snapshot = store.snapshot();
                    let node_id = snapshot.node_id.clone();
                    let active_release_digest = snapshot
                        .release_pointers
                        .active
                        .as_ref()
                        .map(release_identity_digest);
                    let previous = snapshot.release_pointers.previous.clone();
                    let profiles = manager_peer_profiles(&mut store, &request, self.inner.role)?;
                    let activation_phase =
                        crate::manager::api::inventory_activation_phase(store.snapshot());
                    let activation_failure_class =
                        crate::manager::api::inventory_activation_failure_class(store.snapshot());
                    (
                        node_id,
                        active_release_digest,
                        previous,
                        profiles,
                        activation_phase,
                        activation_failure_class,
                    )
                };
                let active_model_digest = if manager_child_matches_current_release(self, &store)
                    .await
                    .unwrap_or(false)
                {
                    manager_release_model_digest(&store, true)
                } else {
                    None
                };
                let previous_model_digest = manager_release_model_digest(&store, false);
                let previous_release_ready = match previous.as_ref() {
                    Some(crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id)) => {
                        verified_profile_command(&store, &self.inner.config, profile_id)
                            .await
                            .is_ok()
                    }
                    Some(
                        identity @ crate::manager::store::ReleaseIdentity::ExternalBaseline {
                            ..
                        },
                    ) => verify_external_baseline(&self.inner.config, identity)
                        .await
                        .is_ok(),
                    None => false,
                };
                let mut response = manager_peer_response(
                    &node_id,
                    &request,
                    manager_peer_ack_id(&node_id, &request, ManagerPeerPhase::Status),
                );
                response.profiles = profiles;
                response.active_release_digest = active_release_digest;
                response.active_model_digest = active_model_digest;
                response.previous_profile_id = previous
                    .as_ref()
                    .map(crate::manager::api::release_identity_profile_id);
                response.previous_model_digest = previous_model_digest;
                response.previous_release_ready = previous_release_ready;
                response.activation_phase = activation_phase;
                response.activation_failure_class = activation_failure_class;
                Ok(response)
            }
            ManagerPeerPhase::ForwardActivate => {
                forward_peer_manager_operation(self, &store, &request, false).await
            }
            ManagerPeerPhase::ForwardRollback => {
                forward_peer_manager_operation(self, &store, &request, true).await
            }
            ManagerPeerPhase::Prepare => {
                let node_id = self.prepare_peer_participant(&store, &request).await?;
                let ack_id = {
                    let store = store
                        .lock()
                        .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
                    let participant = store
                        .snapshot()
                        .activation_journals
                        .get(&request.operation_id)
                        .and_then(|record| record.participants.get(&node_id))
                        .ok_or(ManagerPeerProtocolError::Unavailable)?;
                    participant
                        .prepare_ack
                        .clone()
                        .ok_or(ManagerPeerProtocolError::Unavailable)?
                };
                Ok(manager_peer_response(&node_id, &request, ack_id))
            }
            ManagerPeerPhase::Drain => self.drain_peer_participant(&store, &request).await,
            ManagerPeerPhase::Start => self.start_peer_participant(&store, &request).await,
            ManagerPeerPhase::Commit => self.commit_peer_participant(&store, &request).await,
            ManagerPeerPhase::Rollback => self.rollback_peer_participant(&store, &request).await,
        };
        #[cfg(feature = "test-support")]
        if result.is_ok() && consume_lost_manager_peer_ack_for_test(request.phase) {
            return Err(ManagerPeerProtocolError::Unavailable.into());
        }
        result
    }

    async fn peer_identity(&self) -> (Option<String>, Option<crate::cluster::ControlRole>) {
        match &self.inner.control {
            super::RoleControl::Coordinator(control) => {
                let control = control.lock().await;
                let descriptor = control.peer_lease().descriptor();
                (
                    descriptor.map(|value| value.node_id.clone()),
                    descriptor.map(|value| value.role),
                )
            }
            super::RoleControl::Worker(control) => {
                let control = control.lock().await;
                let descriptor = control.peer_lease().descriptor();
                (
                    descriptor.map(|value| value.node_id.clone()),
                    descriptor.map(|value| value.role),
                )
            }
        }
    }

    async fn mark_peer_manual_intervention(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        active: &mut Option<ManagerPeerActive>,
        record: &mut crate::manager::store::PersistedActivationRecord,
        failure_class: &'static str,
    ) {
        set_peer_phase(
            record,
            crate::manager::store::PersistedActivationPhase::ManualIntervention,
        );
        record.failure_class = Some(failure_class.into());
        if let Ok(mut store) = store.lock() {
            let _ = store.advance_activation(record.clone());
        }
        let _ = self.inner.mode.require_manager_manual_intervention().await;
        active.take();
    }

    async fn prepare_peer_participant(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        request: &ManagerPeerRequest,
    ) -> Result<String, ManagerPeerProtocolError> {
        use crate::manager::store::{
            HardwareReadiness, PersistedActivationPhase as Phase, PersistedActivationRecord,
            PersistedParticipantRecord, ProfileCompatibility,
        };

        let (node_id, candidate_digest, previous_digest, baseline) = {
            let mut store = store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
            let node_id = store.snapshot().node_id.clone();
            if request.profile_id == "external-baseline" {
                let Some(baseline) = store.snapshot().release_pointers.previous.clone() else {
                    return Err(ManagerPeerProtocolError::NotReady);
                };
                let crate::manager::store::ReleaseIdentity::ExternalBaseline {
                    model_sha256, ..
                } = &baseline
                else {
                    return Err(ManagerPeerProtocolError::NotReady);
                };
                let previous = store
                    .snapshot()
                    .release_pointers
                    .active
                    .clone()
                    .ok_or(ManagerPeerProtocolError::NotReady)?;
                let candidate_digest = release_identity_digest(&baseline);
                let previous_digest = release_identity_digest(&previous);
                let source_commit = "0".repeat(40);
                validate_manager_peer_release_digests(
                    request,
                    &candidate_digest,
                    &source_commit,
                    model_sha256,
                    &previous_digest,
                )?;
                (node_id, candidate_digest, previous_digest, Some(baseline))
            } else {
                let profile = store
                    .snapshot()
                    .profiles
                    .get(&request.profile_id)
                    .cloned()
                    .ok_or(ManagerPeerProtocolError::NotReady)?;
                if profile.compatibility != ProfileCompatibility::Compatible
                    || profile.hardware_readiness != HardwareReadiness::Ready
                    || profile.role_artifact_ids.len() != 1
                {
                    return Err(ManagerPeerProtocolError::NotReady);
                }
                let build = store
                    .verify_managed_artifact(
                        &profile.role_artifact_ids[0],
                        crate::manager::store::ArtifactKind::Build,
                    )
                    .map_err(|_| ManagerPeerProtocolError::NotReady)?;
                let model = store
                    .verify_managed_artifact(
                        &profile.model_artifact_id,
                        crate::manager::store::ArtifactKind::Model,
                    )
                    .map_err(|_| ManagerPeerProtocolError::NotReady)?;
                let crate::manager::store::ArtifactProvenance::Build { record, .. } =
                    &build.provenance
                else {
                    return Err(ManagerPeerProtocolError::NotReady);
                };
                let source_commit = record.source.clone();
                let model_digest = model.sha256.clone();
                let candidate_digest = digest_text(&format!(
                    "manager-release-v1\n{}\n{}",
                    build.sha256, model.sha256
                ));
                let previous = store
                    .snapshot()
                    .release_pointers
                    .active
                    .clone()
                    .ok_or(ManagerPeerProtocolError::NotReady)?;
                let previous_digest = release_identity_digest(&previous);
                validate_manager_peer_release_digests(
                    request,
                    &candidate_digest,
                    &source_commit,
                    &model_digest,
                    &previous_digest,
                )?;
                (node_id, candidate_digest, previous_digest, None)
            }
        };
        if let Some(baseline) = baseline.as_ref() {
            verify_external_baseline(&self.inner.config, baseline)
                .await
                .map_err(|_| ManagerPeerProtocolError::NotReady)?;
        }

        let ack_id = manager_peer_ack_id(&node_id, request, ManagerPeerPhase::Prepare);
        let record = PersistedActivationRecord {
            operation_id: request.operation_id.clone(),
            expected_generation: request.expected_generation,
            policy_epoch: request.policy_epoch,
            phase: Phase::Preparing,
            participants: std::collections::BTreeMap::from([(
                node_id.clone(),
                PersistedParticipantRecord {
                    node_id: node_id.clone(),
                    candidate_profile_id: request.profile_id.clone(),
                    candidate_digest,
                    previous_digest: Some(previous_digest),
                    phase: Phase::Preparing,
                    prepare_ack: Some(ack_id),
                    drain_ack: None,
                    ready_ack: None,
                    commit_ack: None,
                    rollback_ack: None,
                },
            )]),
            failure_class: None,
        };
        let mut active = self.inner.manager_peer_guard.lock().await;
        if active
            .as_ref()
            .is_some_and(|current| !current.matches(request))
        {
            return Err(ManagerPeerProtocolError::Conflict);
        }
        let mut durable = store
            .lock()
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
        if let Some(existing) = durable
            .snapshot()
            .activation_journals
            .get(&request.operation_id)
        {
            let existing_participant = existing
                .participants
                .get(&node_id)
                .ok_or(ManagerPeerProtocolError::Conflict)?;
            if existing.expected_generation != request.expected_generation
                || existing.policy_epoch != request.policy_epoch
                || existing_participant.candidate_profile_id != request.profile_id
                || existing_participant.candidate_digest != request.candidate_digest
                || existing_participant.previous_digest != request.previous_digest
            {
                return Err(ManagerPeerProtocolError::Conflict);
            }
            if matches!(
                existing.phase,
                Phase::ManualIntervention | Phase::RollingBack | Phase::RolledBack
            ) {
                return Err(ManagerPeerProtocolError::NotReady);
            }
        } else {
            durable
                .record_activation(record)
                .map_err(|error| match error {
                    crate::manager::store::StoreError::InvalidReference(_) => {
                        ManagerPeerProtocolError::Conflict
                    }
                    _ => ManagerPeerProtocolError::Unavailable,
                })?;
        }
        let phase = durable
            .snapshot()
            .activation_journals
            .get(&request.operation_id)
            .map(|value| value.phase)
            .ok_or(ManagerPeerProtocolError::Unavailable)?;
        drop(durable);
        if active.is_none()
            && !matches!(
                phase,
                Phase::Complete | Phase::RolledBack | Phase::ManualIntervention
            )
        {
            let lease = self
                .claim_manager_activation(manager_operation_uuid(&request.operation_id))
                .map_err(|_| ManagerPeerProtocolError::Conflict)?;
            *active = Some(ManagerPeerActive {
                operation_id: request.operation_id.clone(),
                profile_id: request.profile_id.clone(),
                candidate_digest: request.candidate_digest.clone(),
                previous_digest: request.previous_digest.clone(),
                expected_generation: request.expected_generation,
                policy_epoch: request.policy_epoch,
                source_commit: request.source_commit.clone(),
                model_digest: request.model_digest.clone(),
                _lease: lease,
            });
        }
        Ok(node_id)
    }

    async fn drain_peer_participant(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        request: &ManagerPeerRequest,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::PersistedActivationPhase as Phase;
        let mut active = self.inner.manager_peer_guard.lock().await;
        let record = load_peer_record(store, request)?;
        let node_id = store_node_id(store)?;
        let participant = peer_participant(&record, &node_id)?.clone();
        let expected_ack = participant
            .prepare_ack
            .as_deref()
            .ok_or(ManagerPeerProtocolError::NotReady)?;
        if request.ack_id.as_deref() != Some(expected_ack) {
            return Err(ManagerPeerProtocolError::Conflict.into());
        }
        if let Some(ack_id) = participant.drain_ack.clone() {
            return Ok(manager_peer_response(&node_id, request, ack_id));
        }
        ensure_active_peer(&active, request)?;
        if record.phase != Phase::Preparing {
            return Err(ManagerPeerProtocolError::NotReady.into());
        }
        let mut intent = record;
        set_peer_phase(&mut intent, Phase::Draining);
        store
            .lock()
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?
            .advance_activation(intent.clone())
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?;

        let generation = request.expected_generation;
        if self
            .inner
            .proxy
            .admission()
            .drain(generation, self.inner.config.cluster.timeouts.drain)
            .await
            .is_err()
        {
            return self
                .abort_peer_drain(store, &mut active, intent, request, &node_id)
                .await;
        }
        self.inner.proxy.set_target(
            crate::target::ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            false,
        );
        if self.inner.standalone.stop().await.is_err()
            || self.inner.standalone.is_running().await.unwrap_or(true)
        {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut intent,
                "peer-drain-unconfirmed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        let ack_id = manager_peer_ack_id(&node_id, request, ManagerPeerPhase::Drain);
        if let Some(participant) = intent.participants.get_mut(&node_id) {
            participant.drain_ack = Some(ack_id.clone());
        }
        let persisted = store
            .lock()
            .ok()
            .is_some_and(|mut store| store.advance_activation(intent.clone()).is_ok());
        if !persisted {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut intent,
                "peer-drain-ack-persist-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        Ok(manager_peer_response(&node_id, request, ack_id))
    }

    async fn abort_peer_drain(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        active: &mut Option<ManagerPeerActive>,
        mut record: crate::manager::store::PersistedActivationRecord,
        request: &ManagerPeerRequest,
        node_id: &str,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::PersistedActivationPhase as Phase;
        if manager_child_matches_current_release(self, store)
            .await
            .unwrap_or(false)
        {
            self.inner.proxy.admission().start_serving();
            self.inner
                .proxy
                .set_target(self.inner.mode.snapshot().target, true);
            set_peer_phase(&mut record, Phase::RollingBack);
            record.failure_class = Some("peer-drain-failed".into());
            let intent_persisted = store
                .lock()
                .ok()
                .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
            if !intent_persisted {
                self.mark_peer_manual_intervention(
                    store,
                    active,
                    &mut record,
                    "peer-drain-rollback-intent-persist-failed",
                )
                .await;
                return Err(ManagerPeerProtocolError::LifecycleFailure.into());
            }
            set_peer_phase(&mut record, Phase::RolledBack);
            let ack_id = manager_peer_ack_id(node_id, request, ManagerPeerPhase::Rollback);
            set_peer_ack(&mut record, node_id, ManagerPeerPhase::Rollback, &ack_id);
            let outcome_persisted = store
                .lock()
                .ok()
                .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
            if !outcome_persisted {
                self.mark_peer_manual_intervention(
                    store,
                    active,
                    &mut record,
                    "peer-drain-rollback-ack-persist-failed",
                )
                .await;
                return Err(ManagerPeerProtocolError::LifecycleFailure.into());
            }
            active.take();
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        self.mark_peer_manual_intervention(
            store,
            active,
            &mut record,
            "peer-drain-failed-child-not-running",
        )
        .await;
        Err(ManagerPeerProtocolError::LifecycleFailure.into())
    }

    async fn start_peer_participant(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        request: &ManagerPeerRequest,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::PersistedActivationPhase as Phase;
        let mut active = self.inner.manager_peer_guard.lock().await;
        let mut record = load_peer_record(store, request)?;
        let node_id = store_node_id(store)?;
        let participant = peer_participant(&record, &node_id)?.clone();
        let expected_ack = participant
            .drain_ack
            .as_deref()
            .ok_or(ManagerPeerProtocolError::NotReady)?;
        if request.ack_id.as_deref() != Some(expected_ack) {
            return Err(ManagerPeerProtocolError::Conflict.into());
        }
        if let Some(ack_id) = participant.ready_ack.clone() {
            return Ok(manager_peer_response(&node_id, request, ack_id));
        }
        ensure_active_peer(&active, request)?;
        if record.phase != Phase::Draining {
            return Err(ManagerPeerProtocolError::NotReady.into());
        }
        let command = if request.profile_id == "external-baseline" {
            let baseline = activation_target_identity(store, &request.profile_id)
                .map_err(|_| ManagerPeerProtocolError::NotReady)?;
            verify_external_baseline(&self.inner.config, &baseline)
                .await
                .map_err(|_| ManagerPeerProtocolError::NotReady)?;
            None
        } else {
            Some(
                verified_profile_command(store, &self.inner.config, &request.profile_id)
                    .await
                    .map_err(|_| ManagerPeerProtocolError::NotReady)?,
            )
        };
        set_peer_phase(&mut record, Phase::Starting);
        store
            .lock()
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?
            .advance_activation(record.clone())
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
        let command_selected = match command {
            Some(command) => self.set_manager_command(command).await,
            None => {
                self.restore_manager_command(crate::cluster::process::Ds4CommandRole::Standalone)
                    .await
            }
        };
        if command_selected.is_err()
            || self
                .inner
                .standalone
                .start(request.expected_generation)
                .await
                .is_err()
            || !manager_child_matches_profile(self, store, &request.profile_id).await
        {
            return self
                .rollback_failed_peer_start(store, &mut active, record, request, &node_id)
                .await;
        }
        set_peer_phase(&mut record, Phase::Ready);
        let ack_id = manager_peer_ack_id(&node_id, request, ManagerPeerPhase::Start);
        set_peer_ack(&mut record, &node_id, ManagerPeerPhase::Start, &ack_id);
        let persisted = store
            .lock()
            .ok()
            .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
        if !persisted {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut record,
                "peer-ready-ack-persist-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        Ok(manager_peer_response(&node_id, request, ack_id))
    }

    async fn rollback_failed_peer_start(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        active: &mut Option<ManagerPeerActive>,
        mut record: crate::manager::store::PersistedActivationRecord,
        request: &ManagerPeerRequest,
        node_id: &str,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::PersistedActivationPhase as Phase;
        set_peer_phase(&mut record, Phase::RollingBack);
        record.failure_class = Some("peer-candidate-start-failed".into());
        let persisted = store
            .lock()
            .ok()
            .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
        if !persisted {
            self.mark_peer_manual_intervention(
                store,
                active,
                &mut record,
                "peer-rollback-intent-persist-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        let stopped = self.inner.standalone.stop().await.is_ok()
            && !self.inner.standalone.is_running().await.unwrap_or(true);
        let restored = stopped
            && self
                .restore_manager_command(crate::cluster::process::Ds4CommandRole::Standalone)
                .await
                .is_ok()
            && self
                .inner
                .standalone
                .start(request.expected_generation)
                .await
                .is_ok()
            && manager_child_matches_current_release(self, store)
                .await
                .unwrap_or(false);
        if restored {
            set_peer_phase(&mut record, Phase::RolledBack);
            let ack_id = manager_peer_ack_id(node_id, request, ManagerPeerPhase::Rollback);
            set_peer_ack(&mut record, node_id, ManagerPeerPhase::Rollback, &ack_id);
            let persisted = store
                .lock()
                .ok()
                .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
            if !persisted {
                self.mark_peer_manual_intervention(
                    store,
                    active,
                    &mut record,
                    "peer-rollback-ack-persist-failed",
                )
                .await;
                return Err(ManagerPeerProtocolError::LifecycleFailure.into());
            }
            self.inner
                .proxy
                .set_target(self.inner.mode.snapshot().target, true);
            self.inner.proxy.admission().start_serving();
            active.take();
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        set_peer_phase(&mut record, Phase::ManualIntervention);
        record.failure_class = Some("peer-previous-restore-failed".into());
        self.mark_peer_manual_intervention(
            store,
            active,
            &mut record,
            "peer-previous-restore-failed",
        )
        .await;
        Err(ManagerPeerProtocolError::LifecycleFailure.into())
    }

    async fn commit_peer_participant(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        request: &ManagerPeerRequest,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::PersistedActivationPhase as Phase;
        let mut active = self.inner.manager_peer_guard.lock().await;
        let mut record = load_peer_record(store, request)?;
        let node_id = store_node_id(store)?;
        let participant = peer_participant(&record, &node_id)?.clone();
        let ready_ack = participant
            .ready_ack
            .as_deref()
            .ok_or(ManagerPeerProtocolError::NotReady)?;
        let commit_ack = manager_peer_ack_id(node_id.as_str(), request, ManagerPeerPhase::Commit);
        if request.ack_id.as_deref() == Some(ready_ack) {
            if participant.commit_ack.is_some() {
                let ack_id = participant.commit_ack.clone().unwrap_or(commit_ack);
                return Ok(manager_peer_response(&node_id, request, ack_id));
            }
            ensure_active_peer(&active, request)?;
            if record.phase != Phase::Ready {
                return Err(ManagerPeerProtocolError::NotReady.into());
            }
            set_peer_phase(&mut record, Phase::Committing);
            set_peer_ack(&mut record, &node_id, ManagerPeerPhase::Commit, &commit_ack);
            store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .advance_activation(record)
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
            return Ok(manager_peer_response(&node_id, request, commit_ack));
        }
        if request.ack_id.as_deref() != Some(commit_ack.as_str())
            || participant.commit_ack.as_deref() != Some(commit_ack.as_str())
        {
            return Err(ManagerPeerProtocolError::Conflict.into());
        }
        if record.phase != Phase::Complete {
            ensure_active_peer(&active, request)?;
            if record.phase != Phase::Committing
                || !manager_child_matches_profile(self, store, &request.profile_id).await
            {
                return Err(ManagerPeerProtocolError::NotReady.into());
            }
            let previous = {
                let store = store
                    .lock()
                    .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
                let snapshot = store.snapshot();
                snapshot
                    .release_pointers
                    .active
                    .clone()
                    .ok_or(ManagerPeerProtocolError::NotReady)?
            };
            let candidate = activation_target_identity(store, &request.profile_id)
                .map_err(|_| ManagerPeerProtocolError::NotReady)?;
            set_peer_phase(&mut record, Phase::Complete);
            store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .finish_activation(record, candidate.clone(), Some(previous))
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
        } else {
            let candidate = activation_target_identity(store, &request.profile_id)
                .map_err(|_| ManagerPeerProtocolError::NotReady)?;
            let active_identity = store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .snapshot()
                .release_pointers
                .active
                .clone();
            if active_identity.as_ref() != Some(&candidate)
                || !manager_child_matches_profile(self, store, &request.profile_id).await
            {
                return Err(ManagerPeerProtocolError::NotReady.into());
            }
        }
        self.inner
            .proxy
            .set_target(self.inner.mode.snapshot().target, true);
        self.inner.proxy.admission().start_serving();
        active.take();
        Ok(manager_peer_response(&node_id, request, commit_ack))
    }

    async fn rollback_peer_participant(
        &self,
        store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        request: &ManagerPeerRequest,
    ) -> Result<ManagerPeerResponse, super::ControlHttpError> {
        use crate::manager::store::{PersistedActivationPhase as Phase, ReleaseIdentity};
        let mut active = self.inner.manager_peer_guard.lock().await;
        let mut record = load_peer_record(store, request)?;
        let node_id = store_node_id(store)?;
        let participant = peer_participant(&record, &node_id)?.clone();
        if let Some(ack_id) = participant.rollback_ack.clone() {
            return Ok(manager_peer_response(&node_id, request, ack_id));
        }
        ensure_active_peer(&active, request)?;
        let recognized_ack = [
            participant.prepare_ack.as_deref(),
            participant.drain_ack.as_deref(),
            participant.ready_ack.as_deref(),
            participant.commit_ack.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|ack_id| request.ack_id.as_deref() == Some(ack_id));
        if !recognized_ack {
            return Err(ManagerPeerProtocolError::Conflict.into());
        }
        let phase_before_rollback = record.phase;
        if phase_before_rollback == Phase::Preparing {
            set_peer_phase(&mut record, Phase::RollingBack);
            record.failure_class = Some("peer-rollback-before-drain".into());
            store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .advance_activation(record.clone())
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
            set_peer_phase(&mut record, Phase::RolledBack);
            let ack_id = manager_peer_ack_id(&node_id, request, ManagerPeerPhase::Rollback);
            set_peer_ack(&mut record, &node_id, ManagerPeerPhase::Rollback, &ack_id);
            store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .advance_activation(record)
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
            active.take();
            return Ok(manager_peer_response(&node_id, request, ack_id));
        }
        if phase_before_rollback == Phase::Draining && participant.drain_ack.is_none() {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut record,
                "peer-drain-effect-ambiguous",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        set_peer_phase(&mut record, Phase::RollingBack);
        record.failure_class = Some("peer-rollback-requested".into());
        store
            .lock()
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?
            .advance_activation(record.clone())
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?;

        let child_running = self.inner.standalone.is_running().await.unwrap_or(false);
        let runtime_generation = self.inner.mode.snapshot().generation;
        let candidate_command_selected = participant.ready_ack.is_some()
            || matches!(
                phase_before_rollback,
                Phase::Starting | Phase::Ready | Phase::Committing | Phase::Complete
            );
        self.inner.proxy.set_target(
            crate::target::ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            false,
        );
        if self
            .inner
            .proxy
            .admission()
            .drain(runtime_generation, self.inner.config.cluster.timeouts.drain)
            .await
            .is_err()
        {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut record,
                "peer-rollback-drain-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        let stopped = !child_running
            || (self.inner.standalone.stop().await.is_ok()
                && !self.inner.standalone.is_running().await.unwrap_or(true));
        let command_restored = stopped
            && (!candidate_command_selected
                || self
                    .restore_manager_command(crate::cluster::process::Ds4CommandRole::Standalone)
                    .await
                    .is_ok());
        let restarted = command_restored
            && self
                .inner
                .standalone
                .start(runtime_generation)
                .await
                .is_ok()
            && manager_child_matches_current_release(self, store)
                .await
                .unwrap_or(false);
        if !restarted {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut record,
                "peer-rollback-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        let (active_identity, previous_identity) = {
            let store = store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
            let snapshot = store.snapshot();
            let candidate = ReleaseIdentity::ManagedProfile(request.profile_id.clone());
            if snapshot.release_pointers.active.as_ref() == Some(&candidate) {
                let previous = snapshot
                    .release_pointers
                    .previous
                    .clone()
                    .ok_or(ManagerPeerProtocolError::NotReady)?;
                (Some(previous), Some(candidate))
            } else {
                (
                    snapshot.release_pointers.active.clone(),
                    snapshot.release_pointers.previous.clone(),
                )
            }
        };
        if let Some(active_identity) = active_identity {
            let pointers_persisted = store.lock().ok().is_some_and(|mut store| {
                store
                    .set_release_pointers(active_identity, previous_identity)
                    .is_ok()
            });
            if !pointers_persisted {
                self.mark_peer_manual_intervention(
                    store,
                    &mut active,
                    &mut record,
                    "peer-rollback-pointer-persist-failed",
                )
                .await;
                return Err(ManagerPeerProtocolError::LifecycleFailure.into());
            }
        }
        set_peer_phase(&mut record, Phase::RolledBack);
        let ack_id = manager_peer_ack_id(&node_id, request, ManagerPeerPhase::Rollback);
        set_peer_ack(&mut record, &node_id, ManagerPeerPhase::Rollback, &ack_id);
        let rolled_back_persisted = store
            .lock()
            .ok()
            .is_some_and(|mut store| store.advance_activation(record.clone()).is_ok());
        if !rolled_back_persisted {
            self.mark_peer_manual_intervention(
                store,
                &mut active,
                &mut record,
                "peer-rollback-ack-persist-failed",
            )
            .await;
            return Err(ManagerPeerProtocolError::LifecycleFailure.into());
        }
        self.inner
            .proxy
            .set_target(self.inner.mode.snapshot().target, true);
        self.inner.proxy.admission().start_serving();
        active.take();
        Ok(manager_peer_response(&node_id, request, ack_id))
    }
}

fn load_peer_record(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    request: &ManagerPeerRequest,
) -> Result<crate::manager::store::PersistedActivationRecord, super::ControlHttpError> {
    let store = store
        .lock()
        .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
    let record = store
        .snapshot()
        .activation_journals
        .get(&request.operation_id)
        .cloned()
        .ok_or(ManagerPeerProtocolError::NotReady)?;
    let participant = record
        .participants
        .values()
        .next()
        .ok_or(ManagerPeerProtocolError::NotReady)?;
    if record.expected_generation != request.expected_generation
        || record.policy_epoch != request.policy_epoch
        || participant.candidate_profile_id != request.profile_id
        || participant.candidate_digest != request.candidate_digest
        || participant.previous_digest != request.previous_digest
    {
        return Err(ManagerPeerProtocolError::Conflict.into());
    }
    Ok(record)
}

fn peer_participant<'a>(
    record: &'a crate::manager::store::PersistedActivationRecord,
    node_id: &str,
) -> Result<&'a crate::manager::store::PersistedParticipantRecord, ManagerPeerProtocolError> {
    record
        .participants
        .get(node_id)
        .ok_or(ManagerPeerProtocolError::NotReady)
}

fn store_node_id(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
) -> Result<String, ManagerPeerProtocolError> {
    store
        .lock()
        .map(|store| store.snapshot().node_id.clone())
        .map_err(|_| ManagerPeerProtocolError::Unavailable)
}

fn ensure_active_peer(
    active: &Option<ManagerPeerActive>,
    request: &ManagerPeerRequest,
) -> Result<(), ManagerPeerProtocolError> {
    if active
        .as_ref()
        .is_some_and(|current| current.matches(request))
    {
        Ok(())
    } else {
        Err(ManagerPeerProtocolError::NotReady)
    }
}

fn manager_peer_rollback_ack_exists(
    runtime: &super::ProductionClusterRuntime,
    request: &ManagerPeerRequest,
) -> bool {
    let store = runtime
        .inner
        .manager_store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(store) = store else {
        return false;
    };
    let Ok(store) = store.lock() else {
        return false;
    };
    let Some(record) = store
        .snapshot()
        .activation_journals
        .get(&request.operation_id)
    else {
        return false;
    };
    let node_id = &store.snapshot().node_id;
    record.expected_generation == request.expected_generation
        && record.policy_epoch == request.policy_epoch
        && record.participants.get(node_id).is_some_and(|participant| {
            participant.candidate_profile_id == request.profile_id
                && participant.candidate_digest == request.candidate_digest
                && participant.previous_digest == request.previous_digest
                && participant.rollback_ack.is_some()
        })
}

fn set_peer_phase(
    record: &mut crate::manager::store::PersistedActivationRecord,
    phase: crate::manager::store::PersistedActivationPhase,
) {
    record.phase = phase;
    for participant in record.participants.values_mut() {
        participant.phase = phase;
    }
}

fn set_peer_ack(
    record: &mut crate::manager::store::PersistedActivationRecord,
    node_id: &str,
    phase: ManagerPeerPhase,
    ack_id: &str,
) {
    let Some(participant) = record.participants.get_mut(node_id) else {
        return;
    };
    match phase {
        ManagerPeerPhase::ForwardActivate | ManagerPeerPhase::ForwardRollback => {}
        ManagerPeerPhase::Prepare => participant.prepare_ack = Some(ack_id.into()),
        ManagerPeerPhase::Drain => participant.drain_ack = Some(ack_id.into()),
        ManagerPeerPhase::Start => participant.ready_ack = Some(ack_id.into()),
        ManagerPeerPhase::Commit => participant.commit_ack = Some(ack_id.into()),
        ManagerPeerPhase::Rollback => participant.rollback_ack = Some(ack_id.into()),
        ManagerPeerPhase::Status => {}
    }
}

fn manager_peer_response(
    node_id: &str,
    request: &ManagerPeerRequest,
    ack_id: String,
) -> ManagerPeerResponse {
    ManagerPeerResponse {
        protocol_version: MANAGER_PEER_PROTOCOL_VERSION,
        node_id: node_id.into(),
        operation_id: request.operation_id.clone(),
        profile_id: request.profile_id.clone(),
        candidate_digest: request.candidate_digest.clone(),
        source_commit: request.source_commit.clone(),
        model_digest: request.model_digest.clone(),
        previous_digest: request.previous_digest.clone(),
        expected_generation: request.expected_generation,
        policy_epoch: request.policy_epoch,
        phase: request.phase,
        ack_id,
        profiles: Vec::new(),
        active_release_digest: None,
        active_model_digest: None,
        previous_profile_id: None,
        previous_model_digest: None,
        previous_release_ready: false,
        activation_phase: None,
        activation_failure_class: None,
    }
}

fn manager_release_model_digest(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    active: bool,
) -> Option<String> {
    use crate::manager::store::ReleaseIdentity;

    let store = store.lock().ok()?;
    let snapshot = store.snapshot();
    let identity = if active {
        snapshot.release_pointers.active.as_ref()
    } else {
        snapshot.release_pointers.previous.as_ref()
    }?;
    match identity {
        ReleaseIdentity::ManagedProfile(profile_id) => {
            let profile = snapshot.profiles.get(profile_id)?;
            let artifact = snapshot.artifacts.get(&profile.model_artifact_id)?;
            (artifact.validation_state == crate::manager::registry::ArtifactState::Verified)
                .then(|| artifact.sha256.clone())
        }
        ReleaseIdentity::ExternalBaseline { model_sha256, .. } => Some(model_sha256.clone()),
    }
}

impl super::ProductionClusterRuntime {
    /// Produce a sanitized peer snapshot for the Manager GUI. The request uses only locally
    /// verified candidate identities and the existing authenticated peer channel.
    pub async fn manager_peer_inventory(
        &self,
    ) -> Option<crate::manager::api::ManagerPeerInventoryDto> {
        let store = self
            .inner
            .manager_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let mode = self.inner.mode.snapshot();
        if mode.state != crate::target::ClusterState::PairedStandaloneReady
            || self.operator_policy() == crate::cluster::OperationPolicy::ForcedStandalone
            || !self.inner.lease.valid()
        {
            return None;
        }

        let profile_ids = store
            .lock()
            .ok()?
            .snapshot()
            .profiles
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut profiles = Vec::new();
        for profile_id in profile_ids {
            if let Ok(profile) = managed_profile_summary(&store, &profile_id) {
                profiles.push(profile);
            }
        }
        let has_external_baseline = store.lock().ok().is_some_and(|store| {
            let pointers = &store.snapshot().release_pointers;
            pointers
                .active
                .iter()
                .chain(pointers.previous.iter())
                .any(|identity| {
                    matches!(
                        identity,
                        crate::manager::store::ReleaseIdentity::ExternalBaseline { .. }
                    )
                })
        });
        if has_external_baseline
            && let Ok(profile) = activation_target_summary(
                &store,
                &self.inner.config,
                self.inner.role,
                "external-baseline",
            )
            .await
        {
            profiles.push(profile);
        }
        if profiles.is_empty() {
            return None;
        }

        let (expected_node_id, expected_role) = self.peer_identity().await;
        let node_role = match expected_role? {
            crate::cluster::ControlRole::Coordinator => "coordinator",
            crate::cluster::ControlRole::Worker => "worker",
        };
        let mut peer_node_id = None;
        let mut peer_profiles = std::collections::BTreeMap::new();
        let mut peer_active_digest = None;
        let mut peer_previous_profile_id = None;
        let mut peer_previous_digest = None;
        let mut peer_previous_ready = false;
        let mut peer_activation_phase = None;
        let mut peer_failure_class = None;
        for profile in profiles {
            let request = manager_peer_request(
                &uuid::Uuid::new_v4().simple().to_string(),
                &profile,
                None,
                mode.generation,
                self.policy_epoch(),
                ManagerPeerPhase::Status,
                None,
            );
            let response = self.inner.client.manager_peer(&request).await.ok()?;
            if expected_node_id.as_deref() != Some(response.node_id.as_str())
                || peer_node_id
                    .as_deref()
                    .is_some_and(|existing| existing != response.node_id)
            {
                return None;
            }
            peer_node_id = Some(response.node_id.clone());
            for profile in response.profiles {
                peer_profiles
                    .entry(profile.profile_id.clone())
                    .or_insert(profile);
            }
            peer_active_digest = response.active_model_digest;
            peer_previous_profile_id = response.previous_profile_id;
            peer_previous_digest = response.previous_model_digest;
            peer_previous_ready = response.previous_release_ready;
            peer_activation_phase = response.activation_phase;
            peer_failure_class = response
                .activation_failure_class
                .as_deref()
                .map(crate::manager::api::sanitize_activation_failure_class);
        }
        Some(crate::manager::api::ManagerPeerInventoryDto {
            node_id: peer_node_id?,
            node_role: node_role.into(),
            profiles: peer_profiles
                .into_values()
                .map(|profile| crate::manager::api::ManagerPeerProfileDto {
                    profile_id: profile.profile_id,
                    node_role: profile.node_role,
                    candidate_digest: profile.candidate_digest,
                    source_commit: profile.source_commit,
                    model_digest: profile.model_digest,
                    model_catalog_id: profile.model_catalog_id,
                    config_fingerprint: profile.config_fingerprint,
                })
                .collect(),
            active_digest: peer_active_digest,
            previous_profile_id: peer_previous_profile_id,
            previous_digest: peer_previous_digest,
            previous_release_ready: peer_previous_ready,
            activation_phase: peer_activation_phase,
            activation_failure_class: peer_failure_class,
        })
    }

    /// Return the local active model digest only while the durable pointer, verified command
    /// slot, and running child identity still agree.
    pub async fn manager_active_model_digest(&self) -> Option<String> {
        let store = self
            .inner
            .manager_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        if !manager_child_matches_current_release(self, &store)
            .await
            .ok()?
        {
            return None;
        }
        manager_release_model_digest(&store, true)
    }

    /// Return true only while the previous release can be revalidated against its current files.
    pub async fn manager_previous_release_ready(&self) -> bool {
        let Some(store) = self
            .inner
            .manager_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return false;
        };
        let Some(previous) = store
            .lock()
            .ok()
            .and_then(|store| store.snapshot().release_pointers.previous.clone())
        else {
            return false;
        };
        match previous {
            crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                verified_profile_command(&store, &self.inner.config, &profile_id)
                    .await
                    .is_ok()
            }
            identity @ crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                verify_external_baseline(&self.inner.config, &identity)
                    .await
                    .is_ok()
            }
        }
    }
}

fn manager_peer_profiles(
    store: &mut crate::manager::store::ManagerReleaseStore,
    request: &ManagerPeerRequest,
    local_role: crate::target::LocalRole,
) -> Result<Vec<ManagerPeerProfileSummary>, ManagerPeerProtocolError> {
    use crate::manager::store::{
        ArtifactKind, ArtifactProvenance, HardwareReadiness, ProfileCompatibility,
    };

    let profile_ids = store
        .snapshot()
        .profiles
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut matching = Vec::new();
    for profile_id in profile_ids {
        let profile = store
            .snapshot()
            .profiles
            .get(&profile_id)
            .cloned()
            .ok_or(ManagerPeerProtocolError::Unavailable)?;
        if profile.compatibility != ProfileCompatibility::Compatible
            || profile.hardware_readiness != HardwareReadiness::Ready
            || profile.role_artifact_ids.len() != 1
        {
            continue;
        }
        let build = match store
            .verify_managed_artifact(&profile.role_artifact_ids[0], ArtifactKind::Build)
        {
            Ok(value) => value,
            Err(_) => continue,
        };
        let model =
            match store.verify_managed_artifact(&profile.model_artifact_id, ArtifactKind::Model) {
                Ok(value) => value,
                Err(_) => continue,
            };
        let ArtifactProvenance::Build { record, .. } = &build.provenance else {
            continue;
        };
        let ArtifactProvenance::Model { catalog_id } = &model.provenance else {
            continue;
        };
        if record.source != request.source_commit
            || model.sha256 != request.model_digest
            || catalog_id.as_str() != profile.model_catalog_id
        {
            continue;
        }
        matching.push(ManagerPeerProfileSummary {
            profile_id: profile.profile_id,
            node_role: profile.node_role,
            candidate_digest: digest_text(&format!(
                "manager-release-v1\n{}\n{}",
                build.sha256, model.sha256
            )),
            source_commit: record.source.clone(),
            model_digest: model.sha256,
            model_catalog_id: profile.model_catalog_id,
            config_fingerprint: profile.config_fingerprint,
        });
    }
    let baseline_identity = {
        let snapshot = store.snapshot();
        snapshot
            .release_pointers
            .previous
            .as_ref()
            .filter(|identity| {
                matches!(
                    identity,
                    crate::manager::store::ReleaseIdentity::ExternalBaseline { .. }
                )
            })
            .or_else(|| {
                snapshot
                    .release_pointers
                    .active
                    .as_ref()
                    .filter(|identity| {
                        matches!(
                            identity,
                            crate::manager::store::ReleaseIdentity::ExternalBaseline { .. }
                        )
                    })
            })
    };
    if request.profile_id == "external-baseline"
        && request.source_commit == "0".repeat(40)
        && let Some(crate::manager::store::ReleaseIdentity::ExternalBaseline {
            config_fingerprint,
            executable_sha256,
            model_sha256,
        }) = baseline_identity
    {
        let baseline = crate::manager::store::ReleaseIdentity::ExternalBaseline {
            config_fingerprint: config_fingerprint.clone(),
            executable_sha256: executable_sha256.clone(),
            model_sha256: model_sha256.clone(),
        };
        matching.push(ManagerPeerProfileSummary {
            profile_id: "external-baseline".into(),
            node_role: match local_role {
                crate::target::LocalRole::Coordinator => "coordinator",
                crate::target::LocalRole::Worker => "worker",
                crate::target::LocalRole::Unknown => {
                    return Err(ManagerPeerProtocolError::WrongRole);
                }
            }
            .into(),
            // Baseline identity is deliberately node-local: executable/model bytes and the
            // validated config fingerprint can differ between the paired nodes.
            candidate_digest: release_identity_digest(&baseline),
            source_commit: request.source_commit.clone(),
            model_digest: model_sha256.clone(),
            model_catalog_id: "external-baseline".into(),
            config_fingerprint: config_fingerprint.clone(),
        });
    }
    Ok(matching)
}

fn manager_peer_ack_id(
    node_id: &str,
    request: &ManagerPeerRequest,
    phase: ManagerPeerPhase,
) -> String {
    let material = format!(
        "manager-peer-ack-v2\n{node_id}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        request.operation_id,
        request.profile_id,
        request.candidate_digest,
        request.source_commit,
        request.model_digest,
        request.expected_generation,
        request.policy_epoch,
        phase.as_str(),
    );
    format!("ack-{}", digest_text(&material))
}

#[cfg(test)]
fn validate_manager_peer_role(
    local_role: crate::target::LocalRole,
    peer_role: Option<crate::cluster::ControlRole>,
) -> Result<(), ManagerPeerProtocolError> {
    if local_role != crate::target::LocalRole::Worker
        || peer_role.is_some_and(|role| role != crate::cluster::ControlRole::Coordinator)
    {
        return Err(ManagerPeerProtocolError::WrongRole);
    }
    Ok(())
}

fn validate_manager_peer_epoch(
    request: &ManagerPeerRequest,
    current_generation: u64,
    current_policy_epoch: u64,
    policy_pending: bool,
) -> Result<(), ManagerPeerProtocolError> {
    if request.expected_generation != current_generation {
        return Err(ManagerPeerProtocolError::StaleGeneration);
    }
    if policy_pending || request.policy_epoch != current_policy_epoch {
        return Err(ManagerPeerProtocolError::StalePolicyEpoch);
    }
    Ok(())
}

fn validate_manager_peer_release_digests(
    request: &ManagerPeerRequest,
    actual_candidate_digest: &str,
    actual_source_commit: &str,
    actual_model_digest: &str,
    actual_previous_digest: &str,
) -> Result<(), ManagerPeerProtocolError> {
    if request.candidate_digest != actual_candidate_digest
        || request.source_commit != actual_source_commit
        || request.model_digest != actual_model_digest
        || request.previous_digest.as_deref() != Some(actual_previous_digest)
    {
        return Err(ManagerPeerProtocolError::Conflict);
    }
    Ok(())
}

#[cfg(test)]
mod manager_peer_protocol_tests {
    use super::*;

    fn request(phase: ManagerPeerPhase) -> ManagerPeerRequest {
        ManagerPeerRequest {
            operation_id: "8a5b9efb-1f7c-4c7d-a75b-2d0b26590819".into(),
            profile_id: "profile-local-01".into(),
            candidate_digest: "a".repeat(64),
            source_commit: "c".repeat(40),
            model_digest: "d".repeat(64),
            previous_digest: Some("b".repeat(64)),
            expected_generation: 7,
            policy_epoch: 11,
            phase,
            ack_id: None,
        }
    }

    #[test]
    fn peer_wire_types_only_serialize_the_minimal_transaction_envelope() {
        let request = request(ManagerPeerPhase::Prepare);
        let request_bytes = serde_json::to_vec(&request).expect("serialize request");
        let response = ManagerPeerResponse {
            protocol_version: MANAGER_PEER_PROTOCOL_VERSION,
            node_id: "worker-node".into(),
            operation_id: request.operation_id.clone(),
            profile_id: request.profile_id.clone(),
            candidate_digest: request.candidate_digest.clone(),
            source_commit: request.source_commit.clone(),
            model_digest: request.model_digest.clone(),
            previous_digest: request.previous_digest.clone(),
            expected_generation: request.expected_generation,
            policy_epoch: request.policy_epoch,
            phase: request.phase,
            ack_id: "worker-node:operation:prepare".into(),
            profiles: Vec::new(),
            active_release_digest: None,
            active_model_digest: None,
            previous_profile_id: None,
            previous_model_digest: None,
            previous_release_ready: false,
            activation_phase: None,
            activation_failure_class: None,
        };
        let response_bytes = serde_json::to_vec(&response).expect("serialize response");

        for forbidden in [
            "path",
            "url",
            "artifact_bytes",
            "runtime_lease",
            "secret",
            "bearer",
            "signature",
        ] {
            assert!(!String::from_utf8_lossy(&request_bytes).contains(forbidden));
            assert!(!String::from_utf8_lossy(&response_bytes).contains(forbidden));
        }
        assert_eq!(
            serde_json::from_slice::<ManagerPeerRequest>(&request_bytes).expect("round-trip"),
            request
        );
    }

    #[test]
    fn peer_request_rejects_unknown_fields_and_invalid_identity_values() {
        let mut json = serde_json::to_value(request(ManagerPeerPhase::Prepare)).unwrap();
        json["runtime_lease"] = serde_json::json!("must not cross the peer boundary");
        assert!(serde_json::from_value::<ManagerPeerRequest>(json).is_err());

        let mut invalid = request(ManagerPeerPhase::Prepare);
        invalid.profile_id = "../../outside".into();
        assert!(invalid.validate().is_err());
        invalid.profile_id = "profile-local-01".into();
        invalid.candidate_digest = "abc123".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn peer_phases_map_to_versioned_authenticated_control_paths() {
        let cases = [
            (ManagerPeerPhase::Status, "/v2/manager/status"),
            (
                ManagerPeerPhase::ForwardActivate,
                "/v2/manager/forward-activate",
            ),
            (
                ManagerPeerPhase::ForwardRollback,
                "/v2/manager/forward-rollback",
            ),
            (ManagerPeerPhase::Prepare, "/v2/manager/prepare"),
            (ManagerPeerPhase::Drain, "/v2/manager/drain"),
            (ManagerPeerPhase::Start, "/v2/manager/start"),
            (ManagerPeerPhase::Commit, "/v2/manager/commit"),
            (ManagerPeerPhase::Rollback, "/v2/manager/rollback"),
        ];
        for (phase, path) in cases {
            assert_eq!(phase.path(), path);
        }
    }

    #[test]
    fn peer_role_and_epoch_mismatches_are_rejected_before_preparation() {
        assert_eq!(
            validate_manager_peer_role(
                crate::target::LocalRole::Coordinator,
                Some(crate::cluster::ControlRole::Coordinator)
            ),
            Err(ManagerPeerProtocolError::WrongRole)
        );
        assert_eq!(
            validate_manager_peer_route_role(
                crate::target::LocalRole::Coordinator,
                Some(crate::cluster::ControlRole::Worker),
                ManagerPeerPhase::ForwardActivate,
            ),
            Ok(())
        );
        assert_eq!(
            validate_manager_peer_route_role(
                crate::target::LocalRole::Worker,
                Some(crate::cluster::ControlRole::Coordinator),
                ManagerPeerPhase::ForwardActivate,
            ),
            Err(ManagerPeerProtocolError::WrongRole)
        );
        assert_eq!(
            validate_manager_peer_role(
                crate::target::LocalRole::Worker,
                Some(crate::cluster::ControlRole::Worker)
            ),
            Err(ManagerPeerProtocolError::WrongRole)
        );

        let request = request(ManagerPeerPhase::Prepare);
        assert_eq!(
            validate_manager_peer_epoch(&request, 8, 11, false),
            Err(ManagerPeerProtocolError::StaleGeneration)
        );
        assert_eq!(
            validate_manager_peer_epoch(&request, 7, 12, false),
            Err(ManagerPeerProtocolError::StalePolicyEpoch)
        );
        assert_eq!(
            validate_manager_peer_epoch(&request, 7, 11, true),
            Err(ManagerPeerProtocolError::StalePolicyEpoch)
        );
    }

    #[test]
    fn peer_profile_digest_mismatch_fails_before_a_journal_can_be_created() {
        let request = request(ManagerPeerPhase::Prepare);
        assert_eq!(
            validate_manager_peer_release_digests(
                &request,
                &"c".repeat(64),
                &request.source_commit,
                &request.model_digest,
                &"b".repeat(64),
            ),
            Err(ManagerPeerProtocolError::Conflict)
        );
        assert_eq!(
            validate_manager_peer_release_digests(
                &request,
                &request.candidate_digest,
                &request.source_commit,
                &request.model_digest,
                &"c".repeat(64)
            ),
            Err(ManagerPeerProtocolError::Conflict)
        );
        assert!(
            validate_manager_peer_release_digests(
                &request,
                &request.candidate_digest,
                &request.source_commit,
                &request.model_digest,
                request.previous_digest.as_deref().unwrap()
            )
            .is_ok()
        );
        assert_eq!(
            manager_peer_ack_id("worker", &request, ManagerPeerPhase::Prepare),
            manager_peer_ack_id("worker", &request, ManagerPeerPhase::Prepare)
        );
    }
}

fn validate_manager_peer_route_role(
    local_role: crate::target::LocalRole,
    peer_role: Option<crate::cluster::ControlRole>,
    phase: ManagerPeerPhase,
) -> Result<(), ManagerPeerProtocolError> {
    let (required_local, required_peer) = match phase {
        ManagerPeerPhase::ForwardActivate | ManagerPeerPhase::ForwardRollback => (
            crate::target::LocalRole::Coordinator,
            crate::cluster::ControlRole::Worker,
        ),
        _ => (
            crate::target::LocalRole::Worker,
            crate::cluster::ControlRole::Coordinator,
        ),
    };
    if local_role == required_local && peer_role == Some(required_peer) {
        Ok(())
    } else {
        Err(ManagerPeerProtocolError::WrongRole)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerActivationRequest {
    pub operation_id: String,
    pub profile_id: String,
    pub expected_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerRollbackRequest {
    pub operation_id: String,
    pub expected_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerRuntimeOperation {
    Activate(ManagerActivationRequest),
    Rollback(ManagerRollbackRequest),
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerRuntimeError {
    Unavailable,
    Busy,
    StaleGeneration,
    LeaseUnavailable,
    StalePolicyEpoch,
    UnpairedPeer,
    NotReady,
    ManualIntervention,
}

impl std::fmt::Display for ManagerRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "manager runtime unavailable",
            Self::Busy => "lifecycle operation in progress",
            Self::StaleGeneration => "manager request generation is stale",
            Self::LeaseUnavailable => "manager peer lease is unavailable",
            Self::StalePolicyEpoch => "manager policy is not committed",
            Self::UnpairedPeer => "manager activation requires a paired peer",
            Self::NotReady => "manager profile is not activation-ready",
            Self::ManualIntervention => "manager activation requires manual intervention",
        })
    }
}

impl std::error::Error for ManagerRuntimeError {}

/// The single-node lifecycle owner. It runs inside the runtime actor and is the only Manager
/// path allowed to select a verified command or ask `ModeRuntime` to stop/start the DS4 child.
pub struct StandaloneManagerRuntimeOwner {
    store: Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    config: Arc<crate::config::ModeAwareConfig>,
    runtime: Arc<crate::cluster::ModeRuntime>,
    supervisor: Arc<crate::cluster::StandaloneSupervisor>,
    lifecycle_lease: crate::cluster::OperationLease,
}

impl StandaloneManagerRuntimeOwner {
    pub fn new(
        store: Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        config: Arc<crate::config::ModeAwareConfig>,
        runtime: Arc<crate::cluster::ModeRuntime>,
        supervisor: Arc<crate::cluster::StandaloneSupervisor>,
        lifecycle_lease: crate::cluster::OperationLease,
    ) -> Self {
        Self {
            store,
            config,
            runtime,
            supervisor,
            lifecycle_lease,
        }
    }

    pub async fn handle(
        &self,
        operation: ManagerRuntimeOperation,
    ) -> Result<Value, ManagerRuntimeError> {
        match operation {
            ManagerRuntimeOperation::Snapshot => Ok(self.snapshot()),
            ManagerRuntimeOperation::Activate(request) => {
                self.activate(
                    request.operation_id,
                    request.profile_id,
                    request.expected_generation,
                )
                .await
            }
            ManagerRuntimeOperation::Rollback(request) => {
                let previous = self
                    .store
                    .lock()
                    .map_err(|_| ManagerRuntimeError::Unavailable)?
                    .snapshot()
                    .release_pointers
                    .previous
                    .clone()
                    .ok_or(ManagerRuntimeError::NotReady)?;
                let profile_id = match &previous {
                    crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                        Some(profile_id.clone())
                    }
                    crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                        Some("external-baseline".into())
                    }
                };
                self.activate_to_previous(
                    request.operation_id,
                    profile_id,
                    previous,
                    request.expected_generation,
                )
                .await
            }
        }
    }

    fn snapshot(&self) -> Value {
        let runtime = self.runtime.snapshot();
        let release = self
            .store
            .lock()
            .map(|store| {
                let snapshot = store.snapshot();
                serde_json::json!({
                    "active": snapshot.release_pointers.active,
                    "previous": snapshot.release_pointers.previous,
                    "activation_phase": snapshot.activation_journals.values().next_back().map(|record| record.phase),
                })
            })
            .unwrap_or_else(|_| serde_json::json!({"store_available": false}));
        serde_json::json!({
            "generation": runtime.generation,
            "state": runtime.state.name(),
            "role": format!("{:?}", runtime.role).to_lowercase(),
            "release": release,
        })
    }

    async fn activate(
        &self,
        operation_id: String,
        profile_id: String,
        expected_generation: u64,
    ) -> Result<Value, ManagerRuntimeError> {
        let target = crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id.clone());
        self.activate_to_previous(operation_id, Some(profile_id), target, expected_generation)
            .await
    }

    async fn activate_to_previous(
        &self,
        operation_id: String,
        profile_id: Option<String>,
        target: crate::manager::store::ReleaseIdentity,
        expected_generation: u64,
    ) -> Result<Value, ManagerRuntimeError> {
        let current = self.runtime.snapshot();
        if current.generation != expected_generation {
            return Err(ManagerRuntimeError::StaleGeneration);
        }
        if current.state != crate::target::ClusterState::SoloStandaloneReady
            || current.stable_mode != crate::target::StableMode::SoloStandalone
        {
            return Err(ManagerRuntimeError::NotReady);
        }
        let operation_uuid = manager_operation_uuid(&operation_id);
        let _lease = self
            .lifecycle_lease
            .claim(
                crate::cluster::OperationKind::Activation,
                crate::cluster::OperationId(operation_uuid),
            )
            .map_err(|_| ManagerRuntimeError::Busy)?;

        let command = match &target {
            crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                Some(verified_profile_command(&self.store, &self.config, profile_id).await?)
            }
            crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                verify_external_baseline(&self.config, &target).await?;
                None
            }
        };

        let (previous, candidate_digest, node_id) = {
            let store = self
                .store
                .lock()
                .map_err(|_| ManagerRuntimeError::Unavailable)?;
            let node_id = store.snapshot().node_id.clone();
            let previous = store
                .snapshot()
                .release_pointers
                .active
                .clone()
                .ok_or(ManagerRuntimeError::NotReady)?;
            if previous == target {
                return Err(ManagerRuntimeError::NotReady);
            }
            let candidate_digest = match &target {
                crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                    let profile = store
                        .snapshot()
                        .profiles
                        .get(profile_id)
                        .ok_or(ManagerRuntimeError::NotReady)?;
                    let build = store
                        .snapshot()
                        .artifacts
                        .get(&profile.role_artifact_ids[0])
                        .ok_or(ManagerRuntimeError::NotReady)?;
                    let model = store
                        .snapshot()
                        .artifacts
                        .get(&profile.model_artifact_id)
                        .ok_or(ManagerRuntimeError::NotReady)?;
                    digest_text(&format!(
                        "manager-release-v1\n{}\n{}",
                        build.sha256, model.sha256
                    ))
                }
                crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                    release_identity_digest(&target)
                }
            };
            (previous, candidate_digest, node_id)
        };
        let previous_digest = release_identity_digest(&previous);
        let mut record = activation_record(
            operation_id.clone(),
            expected_generation,
            0,
            &node_id,
            profile_id.as_deref().unwrap_or("external-baseline"),
            candidate_digest,
            previous_digest,
        );
        self.store
            .lock()
            .map_err(|_| ManagerRuntimeError::Unavailable)?
            .record_activation(record.clone())
            .map_err(|_| ManagerRuntimeError::Unavailable)?;

        set_activation_phase(
            &mut record,
            crate::manager::store::PersistedActivationPhase::Draining,
        );
        self.advance_record(&record)?;
        let generation = match self
            .runtime
            .begin_manager_activation(expected_generation)
            .await
        {
            Ok(generation) => generation,
            Err(_) => {
                self.finish_rollback_record(&mut record, None, false)
                    .await?;
                return Err(ManagerRuntimeError::NotReady);
            }
        };

        set_activation_phase(
            &mut record,
            crate::manager::store::PersistedActivationPhase::Starting,
        );
        self.advance_record(&record)?;
        let selection = match command {
            Some(command) => self.supervisor.set_next_command(command).await,
            None => self.supervisor.restore_previous_command().await,
        };
        if selection.is_err() {
            self.finish_rollback_record(&mut record, Some(generation), false)
                .await?;
            return Err(ManagerRuntimeError::NotReady);
        }
        if self
            .runtime
            .start_manager_profile(generation)
            .await
            .is_err()
        {
            self.finish_rollback_record(&mut record, Some(generation), true)
                .await?;
            return Err(ManagerRuntimeError::NotReady);
        }
        set_activation_phase(
            &mut record,
            crate::manager::store::PersistedActivationPhase::Ready,
        );
        self.advance_record(&record)?;
        let ready_generation = self
            .runtime
            .mark_manager_profile_ready(generation)
            .await
            .map_err(|_| ManagerRuntimeError::ManualIntervention)?;
        set_activation_phase(
            &mut record,
            crate::manager::store::PersistedActivationPhase::Committing,
        );
        set_local_commit_ack(&mut record, &operation_id);
        self.advance_record(&record)?;

        let completed = complete_activation_record(record);
        self.store
            .lock()
            .map_err(|_| ManagerRuntimeError::Unavailable)?
            .finish_activation(completed, target.clone(), Some(previous))
            .map_err(|_| ManagerRuntimeError::ManualIntervention)?;
        self.runtime
            .reopen_manager_admission(ready_generation)
            .await
            .map_err(|_| ManagerRuntimeError::ManualIntervention)?;
        Ok(serde_json::json!({
            "operation_id": operation_id,
            "phase": "complete",
            "active": target,
            "generation": ready_generation,
        }))
    }

    fn advance_record(
        &self,
        record: &crate::manager::store::PersistedActivationRecord,
    ) -> Result<(), ManagerRuntimeError> {
        self.store
            .lock()
            .map_err(|_| ManagerRuntimeError::Unavailable)?
            .advance_activation(record.clone())
            .map_err(|_| ManagerRuntimeError::ManualIntervention)
    }

    async fn finish_rollback_record(
        &self,
        record: &mut crate::manager::store::PersistedActivationRecord,
        generation: Option<u64>,
        restore_command: bool,
    ) -> Result<(), ManagerRuntimeError> {
        set_activation_phase(
            record,
            crate::manager::store::PersistedActivationPhase::RollingBack,
        );
        record.failure_class = Some("candidate-activation-failed".into());
        self.advance_record(record)?;
        let current = self.runtime.snapshot();
        let previous_ready = current.generation == record.expected_generation
            && current.state == crate::target::ClusterState::SoloStandaloneReady;
        let restored = async {
            if previous_ready {
                return Ok(current.generation);
            }
            if restore_command {
                self.supervisor.restore_previous_command().await?;
            }
            let generation = generation.unwrap_or(current.generation);
            self.runtime.start_manager_profile(generation).await?;
            self.runtime.mark_manager_profile_ready(generation).await
        }
        .await;
        match restored {
            Ok(ready_generation) => {
                set_activation_phase(
                    record,
                    crate::manager::store::PersistedActivationPhase::RolledBack,
                );
                self.advance_record(record)?;
                self.runtime
                    .reopen_manager_admission(ready_generation)
                    .await
                    .map_err(|_| ManagerRuntimeError::ManualIntervention)
            }
            Err(_) => {
                set_activation_phase(
                    record,
                    crate::manager::store::PersistedActivationPhase::ManualIntervention,
                );
                record.failure_class = Some("previous-release-restoration-failed".into());
                self.advance_record(record)?;
                let _ = self.runtime.require_manager_manual_intervention().await;
                Err(ManagerRuntimeError::ManualIntervention)
            }
        }
    }
}

fn manager_operation_uuid(operation_id: &str) -> uuid::Uuid {
    let digest = Sha256::digest(operation_id.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

fn activation_record(
    operation_id: String,
    expected_generation: u64,
    policy_epoch: u64,
    node_id: &str,
    profile_id: &str,
    candidate_digest: String,
    previous_digest: String,
) -> crate::manager::store::PersistedActivationRecord {
    use crate::manager::store::{
        PersistedActivationPhase as Phase, PersistedActivationRecord, PersistedParticipantRecord,
    };
    let node_id = node_id.to_owned();
    PersistedActivationRecord {
        operation_id,
        expected_generation,
        policy_epoch,
        phase: Phase::Preparing,
        participants: std::collections::BTreeMap::from([(
            node_id.clone(),
            PersistedParticipantRecord {
                node_id,
                candidate_profile_id: profile_id.to_owned(),
                candidate_digest,
                previous_digest: Some(previous_digest),
                phase: Phase::Preparing,
                prepare_ack: Some("local-prepared".into()),
                drain_ack: None,
                ready_ack: None,
                commit_ack: None,
                rollback_ack: None,
            },
        )]),
        failure_class: None,
    }
}

fn set_activation_phase(
    record: &mut crate::manager::store::PersistedActivationRecord,
    phase: crate::manager::store::PersistedActivationPhase,
) {
    record.phase = phase;
    for participant in record.participants.values_mut() {
        participant.phase = phase;
        if phase == crate::manager::store::PersistedActivationPhase::Ready {
            participant.ready_ack = Some(format!("{}:local-ready", record.operation_id));
        }
    }
}

fn set_local_commit_ack(
    record: &mut crate::manager::store::PersistedActivationRecord,
    operation_id: &str,
) {
    for participant in record.participants.values_mut() {
        participant.commit_ack = Some(format!("{operation_id}:local-commit"));
    }
}

fn complete_activation_record(
    mut record: crate::manager::store::PersistedActivationRecord,
) -> crate::manager::store::PersistedActivationRecord {
    record.phase = crate::manager::store::PersistedActivationPhase::Complete;
    for participant in record.participants.values_mut() {
        participant.phase = crate::manager::store::PersistedActivationPhase::Complete;
    }
    record
}

fn digest_text(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn release_identity_digest(identity: &crate::manager::store::ReleaseIdentity) -> String {
    digest_text(&serde_json::to_string(identity).unwrap_or_default())
}

async fn verified_profile_command(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    config: &crate::config::ModeAwareConfig,
    profile_id: &str,
) -> Result<crate::cluster::VerifiedDs4Command, ManagerRuntimeError> {
    use crate::manager::store::{
        ArtifactKind, ArtifactProvenance, HardwareReadiness, ProfileCompatibility,
    };
    let (root, profile, executable, model) = {
        let mut store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
        let profile = store
            .snapshot()
            .profiles
            .get(profile_id)
            .cloned()
            .ok_or(ManagerRuntimeError::NotReady)?;
        if profile.compatibility != ProfileCompatibility::Compatible
            || profile.hardware_readiness != HardwareReadiness::Ready
            || profile.role_artifact_ids.len() != 1
        {
            return Err(ManagerRuntimeError::NotReady);
        }
        let expected_build_role = match profile.node_role.as_str() {
            "coordinator" | "ds4-server" => "ds4-server",
            "worker" | "ds4" => "ds4",
            _ => return Err(ManagerRuntimeError::NotReady),
        };
        let (build, executable) = store
            .verified_artifact_path(&profile.role_artifact_ids[0], ArtifactKind::Build)
            .map_err(|_| ManagerRuntimeError::NotReady)?;
        if !matches!(
            &build.provenance,
            ArtifactProvenance::Build { record, .. }
                if record.role == expected_build_role && record.target == expected_build_role
        ) {
            return Err(ManagerRuntimeError::NotReady);
        }
        let (model_record, model_path) = store
            .verified_artifact_path(&profile.model_artifact_id, ArtifactKind::Model)
            .map_err(|_| ManagerRuntimeError::NotReady)?;
        if !matches!(
            &model_record.provenance,
            ArtifactProvenance::Model { catalog_id } if catalog_id == &profile.model_catalog_id
        ) {
            return Err(ManagerRuntimeError::NotReady);
        }
        (
            store.root().root().to_path_buf(),
            profile,
            (build, executable),
            (model_record, model_path),
        )
    };
    let stage_config = crate::manager::stage::StageRuntimeConfig::from_validated_config(config);
    if profile.config_fingerprint != stage_config.config_fingerprint {
        return Err(ManagerRuntimeError::NotReady);
    }
    let mut command = crate::cluster::build_standalone_command(&config.ds4)
        .map_err(|_| ManagerRuntimeError::NotReady)?;
    command.executable = executable.1;
    command.working_directory = root.clone();
    command.profile.profile_id = profile.profile_id;
    let model_index = command
        .argv
        .iter()
        .position(|argument| argument == "-m")
        .and_then(|index| command.argv.get_mut(index + 1))
        .ok_or(ManagerRuntimeError::NotReady)?;
    *model_index = model.1.into_os_string();
    crate::cluster::VerifiedDs4Command::from_staged_profile(
        command,
        &root,
        &profile.config_fingerprint,
        &stage_config.config_fingerprint,
        &executable.0.sha256,
        &model.0.sha256,
        crate::cluster::process::Ds4CommandRole::Standalone,
    )
    .await
    .map_err(|_| ManagerRuntimeError::NotReady)
}

/// Resolve Manager journals and the committed active pointer before the app starts any DS4
/// child. `true` means the caller must boot in ManualIntervention with admission closed.
pub(crate) async fn prepare_manager_startup(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    config: &crate::config::ModeAwareConfig,
    supervisor: &crate::cluster::StandaloneSupervisor,
) -> bool {
    use crate::manager::store::ReleaseIdentity;
    let manual = match store.lock() {
        Ok(mut store) => match recover_manager_activation_journals(&mut store) {
            Ok(manual) => manual,
            Err(_) => return true,
        },
        Err(_) => return true,
    };
    if manual {
        return true;
    }

    let active = match store
        .lock()
        .map(|store| store.snapshot().release_pointers.active.clone())
    {
        Ok(active) => active,
        Err(_) => return true,
    };
    let active = match active {
        Some(active) => active,
        None => match external_baseline(config).await {
            Ok(baseline) => {
                let mut store = match store.lock() {
                    Ok(store) => store,
                    Err(_) => return true,
                };
                if store.snapshot().release_pointers.active.is_some()
                    || store.set_release_pointers(baseline.clone(), None).is_err()
                {
                    return true;
                }
                baseline
            }
            Err(_) => return true,
        },
    };
    match active {
        ReleaseIdentity::ManagedProfile(profile_id) => {
            let command = match verified_profile_command(store, config, &profile_id).await {
                Ok(command) => command,
                Err(_) => return true,
            };
            supervisor.set_next_command(command).await.is_err()
        }
        baseline @ ReleaseIdentity::ExternalBaseline { .. } => {
            verify_external_baseline(config, &baseline).await.is_err()
        }
    }
}

/// Recover the durable Manager transaction state before a caller allows any DS4 child to start.
/// Intent-only records are safe to close as rolled back; every later interrupted phase is
/// ambiguous and requires the caller to boot fail-closed in ManualIntervention.
pub(crate) fn recover_manager_activation_journals(
    store: &mut crate::manager::store::ManagerReleaseStore,
) -> Result<bool, crate::manager::store::StoreError> {
    use crate::manager::store::PersistedActivationPhase as Phase;
    let operation_ids = store
        .snapshot()
        .activation_journals
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut manual = false;
    for operation_id in operation_ids {
        let Some(mut record) = store
            .snapshot()
            .activation_journals
            .get(&operation_id)
            .cloned()
        else {
            continue;
        };
        match record.phase {
            Phase::Complete | Phase::RolledBack => continue,
            Phase::Preparing => {
                set_activation_phase(&mut record, Phase::RollingBack);
                store.advance_activation(record.clone())?;
                set_activation_phase(&mut record, Phase::RolledBack);
                store.advance_activation(record)?;
            }
            Phase::ManualIntervention => manual = true,
            _ => {
                set_activation_phase(&mut record, Phase::ManualIntervention);
                record.failure_class = Some("interrupted-activation".into());
                store.advance_activation(record)?;
                manual = true;
            }
        }
    }
    Ok(manual)
}

async fn external_baseline(
    config: &crate::config::ModeAwareConfig,
) -> Result<crate::manager::store::ReleaseIdentity, ManagerRuntimeError> {
    Ok(crate::manager::store::ReleaseIdentity::ExternalBaseline {
        config_fingerprint: crate::manager::stage::StageRuntimeConfig::from_validated_config(
            config,
        )
        .config_fingerprint,
        executable_sha256: digest_file(&config.ds4.binary).await?,
        model_sha256: digest_file(&config.ds4.standalone.model).await?,
    })
}

async fn verify_external_baseline(
    config: &crate::config::ModeAwareConfig,
    target: &crate::manager::store::ReleaseIdentity,
) -> Result<(), ManagerRuntimeError> {
    let crate::manager::store::ReleaseIdentity::ExternalBaseline {
        config_fingerprint,
        executable_sha256,
        model_sha256,
    } = target
    else {
        return Err(ManagerRuntimeError::NotReady);
    };
    let current = crate::manager::stage::StageRuntimeConfig::from_validated_config(config);
    if config_fingerprint != &current.config_fingerprint
        || executable_sha256 != &digest_file(&config.ds4.binary).await?
        || model_sha256 != &digest_file(&config.ds4.standalone.model).await?
    {
        return Err(ManagerRuntimeError::NotReady);
    }
    Ok(())
}

async fn manager_child_matches_profile(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    profile_id: &str,
) -> bool {
    let expected_profile_id = if profile_id == "external-baseline" {
        runtime.inner.config.ds4.standalone.profile_id.as_str()
    } else {
        profile_id
    };
    if !runtime.inner.standalone.is_running().await.unwrap_or(false)
        || !runtime
            .inner
            .standalone
            .child_identity()
            .await
            .is_some_and(|identity| identity.profile_id == expected_profile_id)
    {
        return false;
    }
    let Ok(command_snapshot) = runtime.inner.standalone.command_snapshot().await else {
        return false;
    };
    if command_snapshot.profile_id != expected_profile_id {
        return false;
    }
    if profile_id == "external-baseline" {
        let Ok(baseline) = activation_target_identity(store, profile_id) else {
            return false;
        };
        command_snapshot.command_sha256.is_empty()
            && verify_external_baseline(&runtime.inner.config, &baseline)
                .await
                .is_ok()
    } else {
        let Ok(command) = verified_profile_command(store, &runtime.inner.config, profile_id).await
        else {
            return false;
        };
        command_snapshot.command_sha256 == command.digest_hex()
    }
}

async fn manager_child_matches_current_release(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
) -> Result<bool, ManagerRuntimeError> {
    let profile_id = {
        let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
        match store
            .snapshot()
            .release_pointers
            .active
            .as_ref()
            .ok_or(ManagerRuntimeError::NotReady)?
        {
            crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                profile_id.clone()
            }
            crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                "external-baseline".into()
            }
        }
    };
    Ok(manager_child_matches_profile(runtime, store, &profile_id).await)
}

async fn digest_file(path: &Path) -> Result<String, ManagerRuntimeError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ManagerRuntimeError::NotReady)?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Cluster nodes use the same command actor, but the peer participant protocol is installed by
/// the next plan task. This boundary validates all runtime-owned preconditions and keeps the
/// child untouched until that authenticated protocol can collect both nodes' acknowledgements.
pub(crate) async fn handle_cluster_operation(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    operation: ManagerRuntimeOperation,
) -> Result<Value, ManagerRuntimeError> {
    match operation {
        ManagerRuntimeOperation::Snapshot => {
            let snapshot = runtime.inner.mode.snapshot();
            let peer_present = runtime.peer_present().await;
            let release = store
                .lock()
                .map_err(|_| ManagerRuntimeError::Unavailable)?
                .snapshot()
                .release_pointers
                .clone();
            Ok(serde_json::json!({
                "generation": snapshot.generation,
                "state": snapshot.state.name(),
                "role": runtime.inner.role.name(),
                "policy": runtime.operator_policy(),
                "policy_epoch": runtime.policy_epoch(),
                "peer_present": peer_present,
                "release": release,
            }))
        }
        ManagerRuntimeOperation::Activate(request) => {
            if runtime.inner.role == crate::target::LocalRole::Coordinator {
                activate_cluster_profile(
                    runtime,
                    store,
                    request.operation_id,
                    request.profile_id,
                    request.expected_generation,
                    false,
                    None,
                )
                .await
            } else {
                forward_worker_manager_operation(
                    runtime,
                    store,
                    request.operation_id,
                    request.profile_id,
                    request.expected_generation,
                    false,
                )
                .await
            }
        }
        ManagerRuntimeOperation::Rollback(request) => {
            let replay_profile_id = completed_activation_profile_id(store, &request.operation_id)?;
            if runtime.inner.role == crate::target::LocalRole::Worker
                && let Some(profile_id) = replay_profile_id.as_deref()
            {
                return completed_activation_replay(
                    runtime,
                    store,
                    &request.operation_id,
                    profile_id,
                    request.expected_generation,
                )
                .await?
                .ok_or(ManagerRuntimeError::ManualIntervention);
            }
            let profile_id = if let Some(profile_id) = replay_profile_id.as_ref() {
                profile_id.clone()
            } else {
                let previous = store
                    .lock()
                    .map_err(|_| ManagerRuntimeError::Unavailable)?
                    .snapshot()
                    .release_pointers
                    .previous
                    .clone()
                    .ok_or(ManagerRuntimeError::NotReady)?;
                match previous {
                    crate::manager::store::ReleaseIdentity::ManagedProfile(profile_id) => {
                        profile_id
                    }
                    crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => {
                        "external-baseline".into()
                    }
                }
            };
            if runtime.inner.role == crate::target::LocalRole::Coordinator {
                activate_cluster_profile(
                    runtime,
                    store,
                    request.operation_id,
                    profile_id,
                    request.expected_generation,
                    replay_profile_id.is_none(),
                    None,
                )
                .await
            } else {
                forward_worker_manager_operation(
                    runtime,
                    store,
                    request.operation_id,
                    profile_id,
                    request.expected_generation,
                    true,
                )
                .await
            }
        }
    }
}

async fn activate_cluster_profile(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    operation_id: String,
    profile_id: String,
    expected_generation: u64,
    require_peer_previous: bool,
    preferred_peer_request: Option<&ManagerPeerRequest>,
) -> Result<Value, ManagerRuntimeError> {
    use crate::manager::store::{
        PersistedActivationPhase as Phase, PersistedActivationRecord, PersistedParticipantRecord,
    };

    cluster_activation_preflight(runtime, expected_generation).await?;
    if !valid_peer_token(&operation_id) {
        return Err(ManagerRuntimeError::NotReady);
    }
    let _lease = runtime
        .claim_manager_activation(manager_operation_uuid(&operation_id))
        .map_err(|_| ManagerRuntimeError::Busy)?;

    if let Some(result) = completed_activation_replay(
        runtime,
        store,
        &operation_id,
        &profile_id,
        expected_generation,
    )
    .await?
    {
        return Ok(result);
    }

    let completed_replay = store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .snapshot()
        .activation_journals
        .get(&operation_id)
        .is_some_and(|record| record.phase == Phase::Complete);

    let local = activation_target_summary(
        store,
        &runtime.inner.config,
        runtime.inner.role,
        &profile_id,
    )
    .await?;
    let target = activation_target_identity(store, &profile_id)?;
    let local_command = if profile_id == "external-baseline" {
        None
    } else {
        Some(verified_profile_command(store, &runtime.inner.config, &profile_id).await?)
    };
    let local_node_id = store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .snapshot()
        .node_id
        .clone();
    let (local_previous, local_previous_digest) = {
        let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
        let previous = store
            .snapshot()
            .release_pointers
            .active
            .clone()
            .ok_or(ManagerRuntimeError::NotReady)?;
        let digest = release_identity_digest(&previous);
        (previous, digest)
    };

    // A completed replay returns the durable result; any interrupted replay is held for
    // reconciliation so it cannot issue a second child start under the same operation ID.
    {
        let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
        if let Some(existing) = store.snapshot().activation_journals.get(&operation_id) {
            return match existing.phase {
                Phase::Complete
                    if store.snapshot().release_pointers.active == Some(target.clone()) =>
                {
                    Ok(serde_json::json!({
                        "operation_id": operation_id,
                        "phase": "complete",
                        "active": target,
                        "generation": expected_generation,
                    }))
                }
                Phase::ManualIntervention => Err(ManagerRuntimeError::ManualIntervention),
                _ => Err(ManagerRuntimeError::Busy),
            };
        }
        if store
            .snapshot()
            .activation_journals
            .values()
            .any(|record| !matches!(record.phase, Phase::Complete | Phase::RolledBack))
        {
            return Err(ManagerRuntimeError::ManualIntervention);
        }
    }
    if local_previous == target {
        return Err(ManagerRuntimeError::NotReady);
    }

    let policy_epoch = runtime.policy_epoch();
    let status_request = manager_peer_request(
        &operation_id,
        &local,
        Some(local_previous_digest.clone()),
        expected_generation,
        policy_epoch,
        ManagerPeerPhase::Status,
        None,
    );
    let status = runtime
        .inner
        .client
        .manager_peer(&status_request)
        .await
        .map_err(|_| ManagerRuntimeError::UnpairedPeer)?;
    let (expected_peer_id, expected_peer_role) = runtime.peer_identity().await;
    if expected_peer_id.as_deref() != Some(status.node_id.as_str())
        || expected_peer_role != Some(crate::cluster::ControlRole::Worker)
    {
        return Err(ManagerRuntimeError::UnpairedPeer);
    }
    let remote_previous_profile_id = status.previous_profile_id.clone();
    let baseline_target = profile_id == "external-baseline";
    let mut compatible = status.profiles.into_iter().filter(|candidate| {
        matches!(candidate.node_role.as_str(), "worker" | "ds4")
            && (baseline_target && candidate.profile_id == "external-baseline"
                || !baseline_target
                    && candidate.model_catalog_id == local.model_catalog_id
                    && candidate.source_commit == local.source_commit
                    && candidate.model_digest == local.model_digest)
            && preferred_peer_request.is_none_or(|preferred| {
                candidate.profile_id == preferred.profile_id
                    && candidate.candidate_digest == preferred.candidate_digest
            })
            && (!require_peer_previous
                || completed_replay
                || remote_previous_profile_id.as_deref() == Some(candidate.profile_id.as_str()))
    });
    let peer = compatible.next().ok_or(ManagerRuntimeError::NotReady)?;
    if compatible.next().is_some() {
        return Err(ManagerRuntimeError::NotReady);
    }
    let peer_previous_digest = status
        .active_release_digest
        .filter(|digest| full_digest(digest))
        .ok_or(ManagerRuntimeError::NotReady)?;
    if preferred_peer_request.is_some_and(|preferred| {
        preferred.source_commit != peer.source_commit
            || preferred.model_digest != peer.model_digest
            || preferred.previous_digest.as_deref() != Some(peer_previous_digest.as_str())
    }) {
        return Err(ManagerRuntimeError::NotReady);
    }
    let peer_node_id = status.node_id;

    let local_prepare_request = manager_peer_request(
        &operation_id,
        &local,
        Some(local_previous_digest.clone()),
        expected_generation,
        policy_epoch,
        ManagerPeerPhase::Prepare,
        None,
    );
    let peer_prepare_request = manager_peer_request(
        &operation_id,
        &peer,
        Some(peer_previous_digest.clone()),
        expected_generation,
        policy_epoch,
        ManagerPeerPhase::Prepare,
        None,
    );
    let local_prepare_ack = manager_peer_ack_id(
        &local_node_id,
        &local_prepare_request,
        ManagerPeerPhase::Prepare,
    );
    let mut record = PersistedActivationRecord {
        operation_id: operation_id.clone(),
        expected_generation,
        policy_epoch,
        phase: Phase::Preparing,
        participants: std::collections::BTreeMap::from([
            (
                local_node_id.clone(),
                PersistedParticipantRecord {
                    node_id: local_node_id.clone(),
                    candidate_profile_id: local.profile_id.clone(),
                    candidate_digest: local.candidate_digest.clone(),
                    previous_digest: Some(local_previous_digest),
                    phase: Phase::Preparing,
                    prepare_ack: Some(local_prepare_ack),
                    drain_ack: None,
                    ready_ack: None,
                    commit_ack: None,
                    rollback_ack: None,
                },
            ),
            (
                peer_node_id.clone(),
                PersistedParticipantRecord {
                    node_id: peer_node_id.clone(),
                    candidate_profile_id: peer.profile_id.clone(),
                    candidate_digest: peer.candidate_digest.clone(),
                    previous_digest: Some(peer_previous_digest),
                    phase: Phase::Preparing,
                    prepare_ack: None,
                    drain_ack: None,
                    ready_ack: None,
                    commit_ack: None,
                    rollback_ack: None,
                },
            ),
        ]),
        failure_class: None,
    };
    store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .record_activation(record.clone())
        .map_err(|_| ManagerRuntimeError::Unavailable)?;

    let peer_prepare = match runtime
        .inner
        .client
        .manager_peer(&peer_prepare_request)
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return mark_cluster_manual(runtime, store, &mut record, "peer-prepare-ack-ambiguous")
                .await;
        }
    };
    set_cluster_ack(
        &mut record,
        &peer_node_id,
        ManagerPeerPhase::Prepare,
        &peer_prepare.ack_id,
    );
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "prepare-ack-persist-failed")
            .await;
    }

    set_cluster_phase(&mut record, Phase::Draining);
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "drain-intent-persist-failed")
            .await;
    }
    if runtime
        .inner
        .proxy
        .admission()
        .drain(
            expected_generation,
            runtime.inner.config.cluster.timeouts.drain,
        )
        .await
        .is_err()
    {
        return abort_cluster_activation(
            runtime,
            store,
            &mut record,
            &peer_prepare_request,
            &peer_prepare.ack_id,
            false,
            false,
            "local-drain-failed",
        )
        .await;
    }
    runtime.inner.proxy.set_target(
        crate::target::ProxyTarget::Unavailable {
            reason: crate::target::UnavailableReason::Transition,
        },
        false,
    );
    let stop_result = runtime.inner.standalone.stop().await;
    let local_running = runtime.inner.standalone.is_running().await;
    if stop_result.is_err() || !matches!(local_running, Ok(false)) {
        if matches!(local_running, Ok(false)) {
            return abort_cluster_activation(
                runtime,
                store,
                &mut record,
                &peer_prepare_request,
                &peer_prepare.ack_id,
                true,
                false,
                "local-drain-stop-unconfirmed",
            )
            .await;
        }
        return mark_cluster_manual(runtime, store, &mut record, "local-drain-ambiguous").await;
    }
    let local_drain_ack = manager_peer_ack_id(
        &local_node_id,
        &local_prepare_request,
        ManagerPeerPhase::Drain,
    );
    set_cluster_ack(
        &mut record,
        &local_node_id,
        ManagerPeerPhase::Drain,
        &local_drain_ack,
    );
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(
            runtime,
            store,
            &mut record,
            "local-drain-ack-persist-failed",
        )
        .await;
    }
    let peer_drain_request = with_peer_phase(
        &peer_prepare_request,
        ManagerPeerPhase::Drain,
        Some(peer_prepare.ack_id.clone()),
    );
    let peer_drain = match runtime.inner.client.manager_peer(&peer_drain_request).await {
        Ok(response) => response,
        Err(_) => {
            return abort_cluster_activation(
                runtime,
                store,
                &mut record,
                &peer_prepare_request,
                &peer_prepare.ack_id,
                true,
                false,
                "peer-drain-ack-ambiguous",
            )
            .await;
        }
    };
    set_cluster_ack(
        &mut record,
        &peer_node_id,
        ManagerPeerPhase::Drain,
        &peer_drain.ack_id,
    );
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "peer-drain-ack-persist-failed")
            .await;
    }

    set_cluster_phase(&mut record, Phase::Starting);
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "start-intent-persist-failed")
            .await;
    }
    let local_start = match local_command {
        Some(command) => runtime.inner.standalone.set_next_command(command).await,
        None => {
            if verify_external_baseline(&runtime.inner.config, &target)
                .await
                .is_err()
            {
                Err(anyhow::anyhow!(
                    "external baseline changed during activation"
                ))
            } else {
                runtime
                    .restore_manager_command(crate::cluster::process::Ds4CommandRole::Standalone)
                    .await
            }
        }
    };
    if local_start.is_err()
        || runtime
            .inner
            .standalone
            .start(expected_generation)
            .await
            .is_err()
        || !manager_child_matches_profile(runtime, store, &profile_id).await
    {
        return abort_cluster_activation(
            runtime,
            store,
            &mut record,
            &peer_prepare_request,
            &peer_drain.ack_id,
            true,
            local_start.is_ok(),
            "local-candidate-start-failed",
        )
        .await;
    }
    let local_ready_ack = manager_peer_ack_id(
        &local_node_id,
        &local_prepare_request,
        ManagerPeerPhase::Start,
    );
    set_cluster_ack(
        &mut record,
        &local_node_id,
        ManagerPeerPhase::Start,
        &local_ready_ack,
    );
    let peer_start_request = with_peer_phase(
        &peer_prepare_request,
        ManagerPeerPhase::Start,
        Some(peer_drain.ack_id.clone()),
    );
    let peer_ready = match runtime.inner.client.manager_peer(&peer_start_request).await {
        Ok(response) => response,
        Err(_) => {
            return abort_cluster_activation(
                runtime,
                store,
                &mut record,
                &peer_prepare_request,
                &peer_drain.ack_id,
                true,
                true,
                "peer-candidate-start-ambiguous",
            )
            .await;
        }
    };
    set_cluster_ack(
        &mut record,
        &peer_node_id,
        ManagerPeerPhase::Start,
        &peer_ready.ack_id,
    );
    set_cluster_phase(&mut record, Phase::Ready);
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "ready-ack-persist-failed").await;
    }

    set_cluster_phase(&mut record, Phase::Committing);
    set_cluster_ack(
        &mut record,
        &local_node_id,
        ManagerPeerPhase::Commit,
        &manager_peer_ack_id(
            &local_node_id,
            &local_prepare_request,
            ManagerPeerPhase::Commit,
        ),
    );
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(runtime, store, &mut record, "commit-intent-persist-failed")
            .await;
    }
    let peer_commit_request = with_peer_phase(
        &peer_prepare_request,
        ManagerPeerPhase::Commit,
        Some(peer_ready.ack_id.clone()),
    );
    let peer_commit = match runtime
        .inner
        .client
        .manager_peer(&peer_commit_request)
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return abort_cluster_activation(
                runtime,
                store,
                &mut record,
                &peer_prepare_request,
                &peer_ready.ack_id,
                true,
                true,
                "peer-commit-ack-ambiguous",
            )
            .await;
        }
    };
    set_cluster_ack(
        &mut record,
        &peer_node_id,
        ManagerPeerPhase::Commit,
        &peer_commit.ack_id,
    );
    if persist_cluster_record(store, &record).is_err() {
        return mark_cluster_manual(
            runtime,
            store,
            &mut record,
            "peer-commit-ack-persist-failed",
        )
        .await;
    }

    if validate_cluster_commit(
        runtime,
        store,
        expected_generation,
        policy_epoch,
        &profile_id,
    )
    .await
    .is_err()
    {
        return abort_cluster_activation(
            runtime,
            store,
            &mut record,
            &peer_prepare_request,
            &peer_commit.ack_id,
            true,
            true,
            "final-live-check-failed",
        )
        .await;
    }

    let completed = complete_activation_record(record.clone());
    if store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .finish_activation(completed, target.clone(), Some(local_previous))
        .is_err()
    {
        return mark_cluster_manual(
            runtime,
            store,
            &mut record,
            "global-complete-persist-failed",
        )
        .await;
    }

    // The peer's second Commit request is the finalize acknowledgement. The global journal is
    // already durable Complete, but coordinator admission stays closed until the peer confirms
    // its active pointer and admission boundary have both been published.
    let peer_finalize = with_peer_phase(
        &peer_prepare_request,
        ManagerPeerPhase::Commit,
        Some(peer_commit.ack_id.clone()),
    );
    if runtime
        .inner
        .client
        .manager_peer(&peer_finalize)
        .await
        .is_err()
    {
        let mut complete = store
            .lock()
            .map_err(|_| ManagerRuntimeError::Unavailable)?
            .snapshot()
            .activation_journals
            .get(&operation_id)
            .cloned()
            .ok_or(ManagerRuntimeError::Unavailable)?;
        return mark_cluster_manual(runtime, store, &mut complete, "peer-finalize-ack-ambiguous")
            .await;
    }

    if validate_cluster_commit(
        runtime,
        store,
        expected_generation,
        policy_epoch,
        &profile_id,
    )
    .await
    .is_err()
    {
        let mut complete = store
            .lock()
            .map_err(|_| ManagerRuntimeError::Unavailable)?
            .snapshot()
            .activation_journals
            .get(&operation_id)
            .cloned()
            .ok_or(ManagerRuntimeError::Unavailable)?;
        return mark_cluster_manual(
            runtime,
            store,
            &mut complete,
            "final-live-check-after-peer-finalize-failed",
        )
        .await;
    }

    runtime
        .inner
        .proxy
        .set_target(runtime.inner.mode.snapshot().target, true);
    runtime.inner.proxy.admission().start_serving();
    Ok(serde_json::json!({
        "operation_id": operation_id,
        "phase": "complete",
        "active": target,
        "generation": expected_generation,
    }))
}

async fn forward_worker_manager_operation(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    operation_id: String,
    profile_id: String,
    expected_generation: u64,
    rollback: bool,
) -> Result<Value, ManagerRuntimeError> {
    let snapshot = runtime.inner.mode.snapshot();
    if snapshot.generation != expected_generation {
        return Err(ManagerRuntimeError::StaleGeneration);
    }
    if snapshot.state != crate::target::ClusterState::PairedStandaloneReady {
        return Err(ManagerRuntimeError::NotReady);
    }
    if runtime.inner.policy_pending.load(Ordering::Acquire) {
        return Err(ManagerRuntimeError::StalePolicyEpoch);
    }
    if runtime.operator_policy() == crate::cluster::OperationPolicy::ForcedStandalone {
        return Err(ManagerRuntimeError::NotReady);
    }
    if !runtime.inner.lease.valid() {
        return Err(ManagerRuntimeError::LeaseUnavailable);
    }
    if !runtime.peer_present().await {
        return Err(ManagerRuntimeError::UnpairedPeer);
    }

    let profile = activation_target_summary(
        store,
        &runtime.inner.config,
        runtime.inner.role,
        &profile_id,
    )
    .await?;
    let target = activation_target_identity(store, &profile_id)?;
    let active_digest = store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .snapshot()
        .release_pointers
        .active
        .as_ref()
        .map(release_identity_digest)
        .ok_or(ManagerRuntimeError::NotReady)?;
    let phase = if rollback {
        ManagerPeerPhase::ForwardRollback
    } else {
        ManagerPeerPhase::ForwardActivate
    };
    let request = manager_peer_request(
        &operation_id,
        &profile,
        Some(active_digest),
        expected_generation,
        runtime.policy_epoch(),
        phase,
        None,
    );
    let response = runtime
        .inner
        .client
        .manager_peer(&request)
        .await
        .map_err(|_| ManagerRuntimeError::UnpairedPeer)?;
    Ok(serde_json::json!({
        "operation_id": operation_id,
        "phase": "complete",
        "active": target,
        "coordinator_node_id": response.node_id,
        "generation": expected_generation,
    }))
}

async fn forward_peer_manager_operation(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    request: &ManagerPeerRequest,
    rollback: bool,
) -> Result<ManagerPeerResponse, super::ControlHttpError> {
    use crate::manager::store::ReleaseIdentity;
    let replay_profile_id = if rollback {
        completed_activation_profile_id(store, &request.operation_id)
            .map_err(manager_runtime_protocol_error)?
    } else {
        None
    };
    if replay_profile_id
        .as_deref()
        .is_some_and(|profile_id| profile_id != request.profile_id)
    {
        return Err(ManagerPeerProtocolError::Conflict.into());
    }
    let completed_replay = replay_profile_id.is_some();
    let mut candidates = {
        let mut store = store
            .lock()
            .map_err(|_| ManagerPeerProtocolError::Unavailable)?;
        manager_peer_profiles(&mut store, request, runtime.inner.role)?
            .into_iter()
            .filter(|candidate| {
                matches!(candidate.node_role.as_str(), "coordinator" | "ds4-server")
            })
            .collect::<Vec<_>>()
    };
    if rollback {
        let previous_profile_id = match replay_profile_id {
            Some(profile_id) => profile_id,
            None => store
                .lock()
                .map_err(|_| ManagerPeerProtocolError::Unavailable)?
                .snapshot()
                .release_pointers
                .previous
                .as_ref()
                .map(|previous| match previous {
                    ReleaseIdentity::ManagedProfile(profile_id) => profile_id.clone(),
                    ReleaseIdentity::ExternalBaseline { .. } => "external-baseline".into(),
                })
                .ok_or(ManagerPeerProtocolError::NotReady)?,
        };
        candidates.retain(|candidate| candidate.profile_id == previous_profile_id);
    }
    let [local_profile] = candidates.as_slice() else {
        return Err(ManagerPeerProtocolError::NotReady.into());
    };
    let node_id = store
        .lock()
        .map_err(|_| ManagerPeerProtocolError::Unavailable)?
        .snapshot()
        .node_id
        .clone();
    activate_cluster_profile(
        runtime,
        store,
        request.operation_id.clone(),
        local_profile.profile_id.clone(),
        request.expected_generation,
        rollback && !completed_replay,
        Some(request),
    )
    .await
    .map_err(manager_runtime_protocol_error)?;
    Ok(manager_peer_response(
        &node_id,
        request,
        manager_peer_ack_id(&node_id, request, request.phase),
    ))
}

fn manager_runtime_protocol_error(error: ManagerRuntimeError) -> ManagerPeerProtocolError {
    match error {
        ManagerRuntimeError::Unavailable => ManagerPeerProtocolError::Unavailable,
        ManagerRuntimeError::Busy => ManagerPeerProtocolError::Conflict,
        ManagerRuntimeError::StaleGeneration => ManagerPeerProtocolError::StaleGeneration,
        ManagerRuntimeError::LeaseUnavailable => ManagerPeerProtocolError::Unavailable,
        ManagerRuntimeError::StalePolicyEpoch => ManagerPeerProtocolError::StalePolicyEpoch,
        ManagerRuntimeError::UnpairedPeer => ManagerPeerProtocolError::WrongPeer,
        ManagerRuntimeError::NotReady => ManagerPeerProtocolError::NotReady,
        ManagerRuntimeError::ManualIntervention => ManagerPeerProtocolError::LifecycleFailure,
    }
}

fn managed_profile_summary(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    profile_id: &str,
) -> Result<ManagerPeerProfileSummary, ManagerRuntimeError> {
    use crate::manager::store::{
        ArtifactKind, ArtifactProvenance, HardwareReadiness, ProfileCompatibility,
    };
    let mut store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
    let profile = store
        .snapshot()
        .profiles
        .get(profile_id)
        .cloned()
        .ok_or(ManagerRuntimeError::NotReady)?;
    if profile.compatibility != ProfileCompatibility::Compatible
        || profile.hardware_readiness != HardwareReadiness::Ready
        || profile.role_artifact_ids.len() != 1
    {
        return Err(ManagerRuntimeError::NotReady);
    }
    let build = store
        .verify_managed_artifact(&profile.role_artifact_ids[0], ArtifactKind::Build)
        .map_err(|_| ManagerRuntimeError::NotReady)?;
    let model = store
        .verify_managed_artifact(&profile.model_artifact_id, ArtifactKind::Model)
        .map_err(|_| ManagerRuntimeError::NotReady)?;
    let ArtifactProvenance::Build { record, .. } = &build.provenance else {
        return Err(ManagerRuntimeError::NotReady);
    };
    let ArtifactProvenance::Model { catalog_id } = &model.provenance else {
        return Err(ManagerRuntimeError::NotReady);
    };
    if catalog_id != &profile.model_catalog_id || !full_git_sha(&record.source) {
        return Err(ManagerRuntimeError::NotReady);
    }
    Ok(ManagerPeerProfileSummary {
        profile_id: profile.profile_id,
        node_role: profile.node_role,
        candidate_digest: digest_text(&format!(
            "manager-release-v1\n{}\n{}",
            build.sha256, model.sha256
        )),
        source_commit: record.source.clone(),
        model_digest: model.sha256,
        model_catalog_id: profile.model_catalog_id,
        config_fingerprint: profile.config_fingerprint,
    })
}

fn activation_target_identity(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    profile_id: &str,
) -> Result<crate::manager::store::ReleaseIdentity, ManagerRuntimeError> {
    use crate::manager::store::ReleaseIdentity;

    if profile_id != "external-baseline" {
        return Ok(ReleaseIdentity::ManagedProfile(profile_id.into()));
    }
    let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
    let snapshot = store.snapshot();
    let previous = snapshot.release_pointers.previous.clone();
    match previous {
        Some(identity @ ReleaseIdentity::ExternalBaseline { .. }) => Ok(identity),
        _ => match snapshot.release_pointers.active.clone() {
            Some(identity @ ReleaseIdentity::ExternalBaseline { .. }) => Ok(identity),
            _ => Err(ManagerRuntimeError::NotReady),
        },
    }
}

fn completed_activation_profile_id(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    operation_id: &str,
) -> Result<Option<String>, ManagerRuntimeError> {
    use crate::manager::store::{PersistedActivationPhase as Phase, ReleaseIdentity};

    let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
    let snapshot = store.snapshot();
    let Some(record) = snapshot.activation_journals.get(operation_id) else {
        return Ok(None);
    };
    if record.phase != Phase::Complete {
        return Ok(None);
    }
    let participant = record
        .participants
        .get(&snapshot.node_id)
        .ok_or(ManagerRuntimeError::ManualIntervention)?;
    let active = snapshot
        .release_pointers
        .active
        .as_ref()
        .ok_or(ManagerRuntimeError::ManualIntervention)?;
    let active_matches = match (&participant.candidate_profile_id[..], active) {
        ("external-baseline", ReleaseIdentity::ExternalBaseline { .. }) => {
            release_identity_digest(active) == participant.candidate_digest
        }
        (profile_id, ReleaseIdentity::ManagedProfile(active_profile_id)) => {
            profile_id == active_profile_id
        }
        _ => false,
    };
    if !active_matches {
        return Err(ManagerRuntimeError::Busy);
    }
    Ok(Some(participant.candidate_profile_id.clone()))
}

async fn completed_activation_replay(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    operation_id: &str,
    requested_profile_id: &str,
    expected_generation: u64,
) -> Result<Option<Value>, ManagerRuntimeError> {
    use crate::manager::store::PersistedActivationPhase as Phase;

    let (profile_id, candidate_digest, active) = {
        let store = store.lock().map_err(|_| ManagerRuntimeError::Unavailable)?;
        let snapshot = store.snapshot();
        let Some(record) = snapshot.activation_journals.get(operation_id) else {
            return Ok(None);
        };
        if record.phase == Phase::ManualIntervention {
            return Err(ManagerRuntimeError::ManualIntervention);
        }
        if record.phase != Phase::Complete {
            return Ok(None);
        }
        let participant = record
            .participants
            .get(&snapshot.node_id)
            .ok_or(ManagerRuntimeError::ManualIntervention)?;
        if participant.candidate_profile_id != requested_profile_id {
            return Err(ManagerRuntimeError::Busy);
        }
        (
            participant.candidate_profile_id.clone(),
            participant.candidate_digest.clone(),
            snapshot
                .release_pointers
                .active
                .clone()
                .ok_or(ManagerRuntimeError::ManualIntervention)?,
        )
    };
    if completed_activation_profile_id(store, operation_id)?.as_deref() != Some(&profile_id) {
        return Err(ManagerRuntimeError::ManualIntervention);
    }
    let summary = activation_target_summary(
        store,
        &runtime.inner.config,
        runtime.inner.role,
        &profile_id,
    )
    .await?;
    if summary.candidate_digest != candidate_digest {
        return Err(ManagerRuntimeError::ManualIntervention);
    }
    let current = runtime.inner.mode.snapshot();
    if current.generation != expected_generation {
        return Err(ManagerRuntimeError::StaleGeneration);
    }
    if current.state != crate::target::ClusterState::PairedStandaloneReady
        || runtime.inner.proxy.admission().snapshot().state
            != crate::admission::AdmissionState::Serving
    {
        return Err(ManagerRuntimeError::ManualIntervention);
    }
    if !manager_child_matches_profile(runtime, store, &profile_id).await {
        return Err(ManagerRuntimeError::ManualIntervention);
    }
    Ok(Some(serde_json::json!({
        "operation_id": operation_id,
        "phase": "complete",
        "active": active,
        "generation": expected_generation,
    })))
}

async fn activation_target_summary(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    config: &crate::config::ModeAwareConfig,
    local_role: crate::target::LocalRole,
    profile_id: &str,
) -> Result<ManagerPeerProfileSummary, ManagerRuntimeError> {
    use crate::manager::store::ReleaseIdentity;

    if profile_id != "external-baseline" {
        return managed_profile_summary(store, profile_id);
    }

    let baseline = activation_target_identity(store, profile_id)?;
    verify_external_baseline(config, &baseline).await?;
    let ReleaseIdentity::ExternalBaseline {
        config_fingerprint,
        model_sha256,
        ..
    } = &baseline
    else {
        return Err(ManagerRuntimeError::NotReady);
    };
    let node_role = match local_role {
        crate::target::LocalRole::Coordinator => "coordinator",
        crate::target::LocalRole::Worker => "worker",
        crate::target::LocalRole::Unknown => return Err(ManagerRuntimeError::NotReady),
    };
    Ok(ManagerPeerProfileSummary {
        profile_id: "external-baseline".into(),
        node_role: node_role.into(),
        candidate_digest: release_identity_digest(&baseline),
        source_commit: "0".repeat(40),
        model_digest: model_sha256.clone(),
        model_catalog_id: "external-baseline".into(),
        config_fingerprint: config_fingerprint.clone(),
    })
}

fn manager_peer_request(
    operation_id: &str,
    profile: &ManagerPeerProfileSummary,
    previous_digest: Option<String>,
    expected_generation: u64,
    policy_epoch: u64,
    phase: ManagerPeerPhase,
    ack_id: Option<String>,
) -> ManagerPeerRequest {
    ManagerPeerRequest {
        operation_id: operation_id.into(),
        profile_id: profile.profile_id.clone(),
        candidate_digest: profile.candidate_digest.clone(),
        source_commit: profile.source_commit.clone(),
        model_digest: profile.model_digest.clone(),
        previous_digest,
        expected_generation,
        policy_epoch,
        phase,
        ack_id,
    }
}

fn with_peer_phase(
    request: &ManagerPeerRequest,
    phase: ManagerPeerPhase,
    ack_id: Option<String>,
) -> ManagerPeerRequest {
    let mut request = request.clone();
    request.phase = phase;
    request.ack_id = ack_id;
    request
}

fn set_cluster_phase(
    record: &mut crate::manager::store::PersistedActivationRecord,
    phase: crate::manager::store::PersistedActivationPhase,
) {
    record.phase = phase;
    for participant in record.participants.values_mut() {
        participant.phase = phase;
    }
}

fn set_cluster_ack(
    record: &mut crate::manager::store::PersistedActivationRecord,
    node_id: &str,
    phase: ManagerPeerPhase,
    ack_id: &str,
) {
    set_peer_ack(record, node_id, phase, ack_id);
}

fn persist_cluster_record(
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    record: &crate::manager::store::PersistedActivationRecord,
) -> Result<(), ManagerRuntimeError> {
    store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .advance_activation(record.clone())
        .map_err(|_| ManagerRuntimeError::ManualIntervention)
}

async fn validate_cluster_commit(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    expected_generation: u64,
    policy_epoch: u64,
    profile_id: &str,
) -> Result<(), ManagerRuntimeError> {
    let snapshot = runtime.inner.mode.snapshot();
    validate_cluster_activation_context(
        expected_generation,
        snapshot.generation,
        runtime.inner.role,
        snapshot.state,
        runtime.inner.lease.valid(),
        runtime.peer_present().await,
        runtime.inner.policy_pending.load(Ordering::Acquire),
        runtime.operator_policy(),
    )?;
    if runtime.policy_epoch() != policy_epoch
        || !manager_child_matches_profile(runtime, store, profile_id).await
    {
        return Err(ManagerRuntimeError::StalePolicyEpoch);
    }
    Ok(())
}

async fn mark_cluster_manual<T>(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    record: &mut crate::manager::store::PersistedActivationRecord,
    reason: &str,
) -> Result<T, ManagerRuntimeError> {
    set_cluster_phase(
        record,
        crate::manager::store::PersistedActivationPhase::ManualIntervention,
    );
    record.failure_class = Some(reason.into());
    let persisted = persist_cluster_record(store, record).is_ok();
    runtime.inner.proxy.admission().block();
    runtime.inner.proxy.set_target(
        crate::target::ProxyTarget::Unavailable {
            reason: crate::target::UnavailableReason::Transition,
        },
        false,
    );
    let _ = runtime
        .inner
        .mode
        .require_manager_manual_intervention()
        .await;
    if persisted {
        Err(ManagerRuntimeError::ManualIntervention)
    } else {
        Err(ManagerRuntimeError::Unavailable)
    }
}

#[allow(clippy::too_many_arguments)]
async fn abort_cluster_activation(
    runtime: &super::ProductionClusterRuntime,
    store: &Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
    record: &mut crate::manager::store::PersistedActivationRecord,
    peer_request: &ManagerPeerRequest,
    peer_ack: &str,
    local_drained: bool,
    local_candidate_selected: bool,
    reason: &str,
) -> Result<Value, ManagerRuntimeError> {
    use crate::manager::store::PersistedActivationPhase as Phase;
    set_cluster_phase(record, Phase::RollingBack);
    record.failure_class = Some(reason.into());
    if persist_cluster_record(store, record).is_err() {
        return mark_cluster_manual(runtime, store, record, "rollback-intent-persist-failed").await;
    }

    let rollback_request = with_peer_phase(
        peer_request,
        ManagerPeerPhase::Rollback,
        Some(peer_ack.into()),
    );
    let peer_rollback_result = runtime.inner.client.manager_peer(&rollback_request).await;
    let local_node_id = store
        .lock()
        .map_err(|_| ManagerRuntimeError::Unavailable)?
        .snapshot()
        .node_id
        .clone();
    let local_restored = if local_drained {
        let stop = if runtime.inner.standalone.is_running().await.unwrap_or(false) {
            runtime.inner.standalone.stop().await.is_ok()
                && !runtime.inner.standalone.is_running().await.unwrap_or(true)
        } else {
            true
        };
        let command = !local_candidate_selected
            || runtime
                .restore_manager_command(crate::cluster::process::Ds4CommandRole::Standalone)
                .await
                .is_ok();
        stop && command
            && runtime
                .inner
                .standalone
                .start(runtime.inner.mode.snapshot().generation)
                .await
                .is_ok()
            && manager_child_matches_current_release(runtime, store)
                .await
                .unwrap_or(false)
    } else {
        manager_child_matches_current_release(runtime, store)
            .await
            .unwrap_or(false)
    };
    if !local_restored {
        return mark_cluster_manual(runtime, store, record, "local-previous-restore-failed").await;
    }
    let peer_rollback = match peer_rollback_result {
        Ok(response) => response,
        Err(_) => {
            return mark_cluster_manual(runtime, store, record, "peer-rollback-unconfirmed").await;
        }
    };
    let local_participant = record
        .participants
        .get(&local_node_id)
        .ok_or(ManagerRuntimeError::Unavailable)?;
    let local_rollback_request = with_peer_phase(
        peer_request,
        ManagerPeerPhase::Rollback,
        Some(peer_ack.into()),
    );
    let local_rollback_request = ManagerPeerRequest {
        profile_id: local_participant.candidate_profile_id.clone(),
        candidate_digest: local_participant.candidate_digest.clone(),
        previous_digest: local_participant.previous_digest.clone(),
        ..local_rollback_request
    };
    let local_rollback_ack = manager_peer_ack_id(
        &local_node_id,
        &local_rollback_request,
        ManagerPeerPhase::Rollback,
    );
    set_cluster_ack(
        record,
        &local_node_id,
        ManagerPeerPhase::Rollback,
        &local_rollback_ack,
    );
    set_cluster_ack(
        record,
        &peer_rollback.node_id,
        ManagerPeerPhase::Rollback,
        &peer_rollback.ack_id,
    );
    set_cluster_phase(record, Phase::RolledBack);
    if persist_cluster_record(store, record).is_err() {
        return mark_cluster_manual(runtime, store, record, "rollback-result-persist-failed").await;
    }
    runtime
        .inner
        .proxy
        .set_target(runtime.inner.mode.snapshot().target, true);
    runtime.inner.proxy.admission().start_serving();
    Err(ManagerRuntimeError::NotReady)
}

async fn cluster_activation_preflight(
    runtime: &super::ProductionClusterRuntime,
    expected_generation: u64,
) -> Result<(), ManagerRuntimeError> {
    let snapshot = runtime.inner.mode.snapshot();
    validate_cluster_activation_context(
        expected_generation,
        snapshot.generation,
        runtime.inner.role,
        snapshot.state,
        runtime.inner.lease.valid(),
        runtime.peer_present().await,
        runtime.inner.policy_pending.load(Ordering::Acquire),
        runtime.operator_policy(),
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_cluster_activation_context(
    expected_generation: u64,
    current_generation: u64,
    role: crate::target::LocalRole,
    state: crate::target::ClusterState,
    lease_valid: bool,
    peer_present: bool,
    policy_pending: bool,
    policy: crate::cluster::OperationPolicy,
) -> Result<(), ManagerRuntimeError> {
    if current_generation != expected_generation {
        return Err(ManagerRuntimeError::StaleGeneration);
    }
    if policy_pending {
        return Err(ManagerRuntimeError::StalePolicyEpoch);
    }
    if policy == crate::cluster::OperationPolicy::ForcedStandalone {
        return Err(ManagerRuntimeError::NotReady);
    }
    if !lease_valid {
        return Err(ManagerRuntimeError::LeaseUnavailable);
    }
    if !peer_present {
        return Err(ManagerRuntimeError::UnpairedPeer);
    }
    if role != crate::target::LocalRole::Coordinator
        || state != crate::target::ClusterState::PairedStandaloneReady
    {
        return Err(ManagerRuntimeError::NotReady);
    }
    Ok(())
}

enum ManagerRuntimeCommand {
    Activate(
        ManagerActivationRequest,
        oneshot::Sender<Result<Value, ManagerRuntimeError>>,
    ),
    Rollback(
        ManagerRollbackRequest,
        oneshot::Sender<Result<Value, ManagerRuntimeError>>,
    ),
    Snapshot(oneshot::Sender<Result<Value, ManagerRuntimeError>>),
}

#[derive(Clone)]
pub struct ManagerRuntimeHandle {
    sender: mpsc::Sender<ManagerRuntimeCommand>,
    available: Arc<AtomicBool>,
}

pub struct ManagerRuntimeReceiver {
    receiver: mpsc::Receiver<ManagerRuntimeCommand>,
    available: Arc<AtomicBool>,
}

pub fn manager_runtime_channel() -> (ManagerRuntimeHandle, ManagerRuntimeReceiver) {
    let (sender, receiver) = mpsc::channel(16);
    let available = Arc::new(AtomicBool::new(false));
    (
        ManagerRuntimeHandle {
            sender,
            available: available.clone(),
        },
        ManagerRuntimeReceiver {
            receiver,
            available,
        },
    )
}

impl ManagerRuntimeHandle {
    pub fn activate_blocking(
        &self,
        request: ManagerActivationRequest,
    ) -> Result<Value, ManagerRuntimeError> {
        self.request_blocking(|reply| ManagerRuntimeCommand::Activate(request, reply))
    }

    pub fn rollback_blocking(
        &self,
        request: ManagerRollbackRequest,
    ) -> Result<Value, ManagerRuntimeError> {
        self.request_blocking(|reply| ManagerRuntimeCommand::Rollback(request, reply))
    }

    pub fn snapshot_blocking(&self) -> Result<Value, ManagerRuntimeError> {
        self.request_blocking(ManagerRuntimeCommand::Snapshot)
    }

    fn request_blocking(
        &self,
        make_command: impl FnOnce(
            oneshot::Sender<Result<Value, ManagerRuntimeError>>,
        ) -> ManagerRuntimeCommand,
    ) -> Result<Value, ManagerRuntimeError> {
        if !self.available.load(Ordering::Acquire) {
            return Err(ManagerRuntimeError::Unavailable);
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| ManagerRuntimeError::Unavailable)?;
        // ManagerExecutor calls the synchronous backend on its spawn_blocking worker.
        runtime.block_on(async {
            let (reply, response) = oneshot::channel();
            self.sender
                .send(make_command(reply))
                .await
                .map_err(|_| ManagerRuntimeError::Unavailable)?;
            response
                .await
                .map_err(|_| ManagerRuntimeError::Unavailable)?
        })
    }
}

impl ManagerRuntimeReceiver {
    pub fn spawn<F, Fut>(mut self, handler: F) -> tokio::task::JoinHandle<()>
    where
        F: Fn(ManagerRuntimeOperation) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ManagerRuntimeError>> + Send + 'static,
    {
        self.available.store(true, Ordering::Release);
        tokio::spawn(async move {
            while let Some(command) = self.receiver.recv().await {
                let (operation, reply) = match command {
                    ManagerRuntimeCommand::Activate(request, reply) => {
                        (ManagerRuntimeOperation::Activate(request), reply)
                    }
                    ManagerRuntimeCommand::Rollback(request, reply) => {
                        (ManagerRuntimeOperation::Rollback(request), reply)
                    }
                    ManagerRuntimeCommand::Snapshot(reply) => {
                        (ManagerRuntimeOperation::Snapshot, reply)
                    }
                };
                let _ = reply.send(handler(operation).await);
            }
            self.available.store(false, Ordering::Release);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn standalone_owner_for_test() -> (
        StandaloneManagerRuntimeOwner,
        Arc<crate::cluster::ModeRuntime>,
        Arc<crate::proxy::ModeAwareProxyState>,
        crate::cluster::OperationLease,
        Arc<std::sync::Mutex<crate::manager::store::ManagerReleaseStore>>,
        Arc<crate::config::ModeAwareConfig>,
    ) {
        let mut config =
            crate::config::ModeAwareConfig::parse(include_str!("../../../siderostat.example.toml"))
                .expect("parse example config");
        config.cluster.enabled = false;
        let external_root = std::env::temp_dir().join(format!(
            "siderostat-manager-baseline-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&external_root).expect("baseline directory");
        let external_executable = external_root.join("ds4-server");
        let external_model = external_root.join("model.gguf");
        std::fs::write(&external_executable, b"external ds4 binary").expect("baseline binary");
        std::fs::write(&external_model, b"external model").expect("baseline model");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&external_executable, std::fs::Permissions::from_mode(0o700))
                .expect("baseline executable mode");
        }
        config.ds4.binary = external_executable;
        config.ds4.standalone.model = external_model;
        let store_root = std::env::temp_dir().join(format!(
            "siderostat-manager-owner-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let store = Arc::new(std::sync::Mutex::new(
            crate::manager::store::ManagerReleaseStore::open(
                crate::manager::registry::ManagerRoot::explicit(store_root),
                config.cluster.node_id.clone(),
            )
            .expect("manager store"),
        ));
        let proxy = Arc::new(
            crate::proxy::ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:8000").unwrap(),
                url::Url::parse("http://10.99.0.1:18082").unwrap(),
                crate::proxy::ModeAwareProxyOptions {
                    max_in_flight: 2,
                    request_body_limit_bytes: 4096,
                    response_header_timeout: std::time::Duration::from_secs(1),
                    first_body_byte_timeout: std::time::Duration::from_secs(1),
                    stream_idle_timeout: std::time::Duration::from_secs(1),
                    connect_timeout: std::time::Duration::from_secs(1),
                },
            )
            .unwrap(),
        );
        let supervisor = Arc::new(crate::cluster::StandaloneSupervisor::new_dry_run(
            crate::cluster::build_standalone_command(&config.ds4).unwrap(),
            url::Url::parse("http://127.0.0.1:8000/v1/models").unwrap(),
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(10),
            std::time::Duration::from_secs(1),
            false,
            Arc::new(crate::metrics::Metrics::default()),
        ));
        let runtime = Arc::new(
            crate::cluster::ModeRuntime::spawn_ready(
                crate::target::LocalRole::Unknown,
                proxy.clone(),
                supervisor.clone(),
                std::time::Duration::from_secs(1),
            )
            .await
            .expect("standalone runtime"),
        );
        let lease = crate::cluster::OperationLease::new();
        let owner = StandaloneManagerRuntimeOwner::new(
            store.clone(),
            Arc::new(config.clone()),
            runtime.clone(),
            supervisor,
            lease.clone(),
        );
        (owner, runtime, proxy, lease, store, Arc::new(config))
    }

    #[test]
    fn missing_runtime_owner_fails_closed() {
        let (handle, receiver) = manager_runtime_channel();
        drop(receiver);
        let result = std::thread::spawn(move || {
            handle.activate_blocking(ManagerActivationRequest {
                operation_id: "activate-1".into(),
                profile_id: "profile-1".into(),
                expected_generation: 1,
            })
        })
        .join()
        .expect("request thread");
        assert_eq!(result, Err(ManagerRuntimeError::Unavailable));
    }

    #[tokio::test]
    async fn typed_request_reaches_the_runtime_actor_and_returns_its_result() {
        let (handle, receiver) = manager_runtime_channel();
        let actor = receiver.spawn(|operation| async move {
            let ManagerRuntimeOperation::Activate(request) = operation else {
                return Err(ManagerRuntimeError::Unavailable);
            };
            Ok(serde_json::json!({
                "operation_id": request.operation_id,
                "profile_id": request.profile_id,
                "generation": request.expected_generation,
            }))
        });
        let response_handle = handle.clone();
        let response = tokio::task::spawn_blocking(move || {
            response_handle.activate_blocking(ManagerActivationRequest {
                operation_id: "activate-2".into(),
                profile_id: "profile-2".into(),
                expected_generation: 9,
            })
        })
        .await
        .expect("blocking bridge")
        .expect("actor response");
        assert_eq!(response["operation_id"], "activate-2");
        assert_eq!(response["profile_id"], "profile-2");
        assert_eq!(response["generation"], 9);
        actor.abort();
    }

    #[tokio::test]
    async fn standalone_owner_rejects_stale_generation_before_admission_drain() {
        let (owner, runtime, proxy, _, _, _) = standalone_owner_for_test().await;
        let current = runtime.snapshot();
        let response = owner
            .handle(ManagerRuntimeOperation::Activate(
                ManagerActivationRequest {
                    operation_id: "stale-generation".into(),
                    profile_id: format!("profile-{}", "a".repeat(64)),
                    expected_generation: current.generation.saturating_sub(1),
                },
            ))
            .await;
        assert_eq!(response, Err(ManagerRuntimeError::StaleGeneration));
        assert_eq!(runtime.snapshot().generation, current.generation);
        assert_eq!(
            proxy.admission().snapshot().state,
            crate::admission::AdmissionState::Serving
        );
    }

    #[tokio::test]
    async fn standalone_owner_rejects_conflicting_lifecycle_operation_before_drain() {
        let (owner, runtime, proxy, lease, _, _) = standalone_owner_for_test().await;
        let current = runtime.snapshot();
        let _restart = lease
            .claim(
                crate::cluster::OperationKind::Restart,
                crate::cluster::OperationId(uuid::Uuid::new_v4()),
            )
            .expect("restart owns lifecycle gate");
        let response = owner
            .handle(ManagerRuntimeOperation::Activate(
                ManagerActivationRequest {
                    operation_id: "blocked-by-restart".into(),
                    profile_id: format!("profile-{}", "a".repeat(64)),
                    expected_generation: current.generation,
                },
            ))
            .await;
        assert_eq!(response, Err(ManagerRuntimeError::Busy));
        assert_eq!(runtime.snapshot().generation, current.generation);
        assert_eq!(
            proxy.admission().snapshot().state,
            crate::admission::AdmissionState::Serving
        );
    }

    #[test]
    fn cluster_preflight_rejects_stale_generation_lease_policy_and_pair_state() {
        use crate::{
            cluster::OperationPolicy,
            target::{ClusterState, LocalRole},
        };
        let check = |generation, lease, peer, policy_pending, role, state, policy| {
            validate_cluster_activation_context(
                8,
                generation,
                role,
                state,
                lease,
                peer,
                policy_pending,
                policy,
            )
        };
        let ready = ClusterState::PairedStandaloneReady;
        let automatic = OperationPolicy::Automatic;
        assert_eq!(
            check(
                7,
                true,
                true,
                false,
                LocalRole::Coordinator,
                ready,
                automatic
            ),
            Err(ManagerRuntimeError::StaleGeneration)
        );
        assert_eq!(
            check(
                8,
                false,
                true,
                false,
                LocalRole::Coordinator,
                ready,
                automatic
            ),
            Err(ManagerRuntimeError::LeaseUnavailable)
        );
        assert_eq!(
            check(
                8,
                true,
                true,
                true,
                LocalRole::Coordinator,
                ready,
                automatic
            ),
            Err(ManagerRuntimeError::StalePolicyEpoch)
        );
        assert_eq!(
            check(
                8,
                true,
                false,
                false,
                LocalRole::Coordinator,
                ready,
                automatic
            ),
            Err(ManagerRuntimeError::UnpairedPeer)
        );
        assert_eq!(
            check(
                8,
                true,
                true,
                false,
                LocalRole::Coordinator,
                ClusterState::SoloStandaloneReady,
                automatic,
            ),
            Err(ManagerRuntimeError::NotReady)
        );
        assert_eq!(
            check(
                8,
                true,
                true,
                false,
                LocalRole::Coordinator,
                ready,
                OperationPolicy::ForcedStandalone,
            ),
            Err(ManagerRuntimeError::NotReady)
        );
    }

    #[tokio::test]
    async fn single_node_activation_commits_only_after_candidate_readiness() {
        use crate::manager::store::{
            ArtifactDraft, ArtifactKind, ArtifactProvenance, HardwareReadiness,
            PersistedActivationPhase, ProfileCompatibility, ReleaseIdentity, StagedProfileRecord,
        };
        let (owner, runtime, proxy, _, store, config) = standalone_owner_for_test().await;
        let profile_id = format!("profile-{}", "a".repeat(64));
        let (candidate, baseline) = {
            let mut store = store.lock().unwrap();
            let root = store.root().root().to_path_buf();
            let workspace = store.create_build_workspace().expect("workspace");
            let executable_source = workspace.join("ds4-server");
            let model_source = workspace.join("model.gguf");
            let executable_bytes = b"candidate ds4 server";
            let model_bytes = b"candidate model";
            std::fs::write(&executable_source, executable_bytes).expect("candidate binary");
            std::fs::write(&model_source, model_bytes).expect("candidate model");
            let executable_sha256 = digest_text(std::str::from_utf8(executable_bytes).unwrap());
            let model_sha256 = digest_text(std::str::from_utf8(model_bytes).unwrap());
            let source = crate::manager::registry::SourceRecord {
                remote: "https://github.com/antirez/ds4.git".into(),
                full_commit: "c".repeat(40),
                main_proof: "c".repeat(40),
                fetched_at: 1_758_795_200,
            };
            let source_id = store.record_source(source.clone()).expect("source receipt");
            let build = store
                .publish_artifact(
                    &executable_source,
                    ArtifactDraft {
                        kind: ArtifactKind::Build,
                        expected_sha256: executable_sha256.clone(),
                        expected_size: executable_bytes.len() as u64,
                        provenance: ArtifactProvenance::Build {
                            source_receipt_id: source_id,
                            record: crate::manager::registry::BuildRecord {
                                source: source.full_commit,
                                flags: "release".into(),
                                toolchain: "rustc-test".into(),
                                arch: "arm64".into(),
                                role: "ds4-server".into(),
                                target: "ds4-server".into(),
                                digest: executable_sha256,
                                help_digest: "d".repeat(64),
                            },
                        },
                    },
                )
                .expect("publish build");
            let model = store
                .publish_artifact(
                    &model_source,
                    ArtifactDraft {
                        kind: ArtifactKind::Model,
                        expected_sha256: model_sha256,
                        expected_size: model_bytes.len() as u64,
                        provenance: ArtifactProvenance::Model {
                            catalog_id: "test-model".into(),
                        },
                    },
                )
                .expect("publish model");
            let fingerprint =
                crate::manager::stage::StageRuntimeConfig::from_validated_config(&config)
                    .config_fingerprint;
            store
                .record_profile(StagedProfileRecord {
                    profile_id: profile_id.clone(),
                    node_role: "coordinator".into(),
                    role_artifact_ids: vec![build.id],
                    model_artifact_id: model.id,
                    model_catalog_id: "test-model".into(),
                    config_fingerprint: fingerprint,
                    compatibility: ProfileCompatibility::Compatible,
                    hardware_readiness: HardwareReadiness::Ready,
                })
                .expect("stage profile");
            let baseline = crate::manager::store::ReleaseIdentity::ExternalBaseline {
                config_fingerprint:
                    crate::manager::stage::StageRuntimeConfig::from_validated_config(&config)
                        .config_fingerprint,
                executable_sha256: digest_text("external ds4 binary"),
                model_sha256: digest_text("external model"),
            };
            store
                .set_release_pointers(baseline.clone(), None)
                .expect("record baseline");
            let _ = root;
            (profile_id.clone(), baseline)
        };
        let expected_generation = runtime.snapshot().generation;
        let result = owner
            .handle(ManagerRuntimeOperation::Activate(
                ManagerActivationRequest {
                    operation_id: "activation-local-1".into(),
                    profile_id: candidate.clone(),
                    expected_generation,
                },
            ))
            .await
            .expect("activation completes");
        assert_eq!(result["phase"], "complete");
        assert_eq!(
            runtime.snapshot().state,
            crate::target::ClusterState::SoloStandaloneReady
        );
        assert_eq!(
            proxy.admission().snapshot().state,
            crate::admission::AdmissionState::Serving
        );
        let store = store.lock().unwrap();
        assert_eq!(
            store.snapshot().release_pointers.active,
            Some(ReleaseIdentity::ManagedProfile(candidate.clone()))
        );
        assert_eq!(store.snapshot().release_pointers.previous, Some(baseline));
        assert_eq!(
            store.snapshot().activation_journals["activation-local-1"].phase,
            PersistedActivationPhase::Complete
        );
    }

    fn activation_record_at(
        operation_id: &str,
        phase: crate::manager::store::PersistedActivationPhase,
    ) -> crate::manager::store::PersistedActivationRecord {
        use crate::manager::store::{PersistedActivationRecord, PersistedParticipantRecord};
        PersistedActivationRecord {
            operation_id: operation_id.into(),
            expected_generation: 5,
            policy_epoch: 1,
            phase,
            participants: std::collections::BTreeMap::from([(
                "node-a".into(),
                PersistedParticipantRecord {
                    node_id: "node-a".into(),
                    candidate_profile_id: "profile-aaaaaaaa".into(),
                    candidate_digest: "a".repeat(64),
                    previous_digest: Some("b".repeat(64)),
                    phase,
                    prepare_ack: Some("prepared".into()),
                    drain_ack: None,
                    ready_ack: None,
                    commit_ack: None,
                    rollback_ack: None,
                },
            )]),
            failure_class: None,
        }
    }

    #[test]
    fn startup_closes_intent_and_fails_closed_after_drain_may_have_started() {
        use crate::manager::registry::ManagerRoot;
        use crate::manager::store::{ManagerReleaseStore, PersistedActivationPhase as Phase};

        let root = std::env::temp_dir().join(format!(
            "siderostat-manager-recovery-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut store = ManagerReleaseStore::open(ManagerRoot::explicit(root), "node-a")
            .expect("manager store");
        store
            .record_activation(activation_record_at("intent", Phase::Preparing))
            .expect("persist intent");
        assert!(!recover_manager_activation_journals(&mut store).expect("recover intent"));
        assert_eq!(
            store.snapshot().activation_journals["intent"].phase,
            Phase::RolledBack
        );

        for (index, phase) in [
            Phase::Draining,
            Phase::Starting,
            Phase::Ready,
            Phase::Committing,
            Phase::RollingBack,
        ]
        .into_iter()
        .enumerate()
        {
            let operation_id = format!("interrupted-{index}");
            store
                .record_activation(activation_record_at(&operation_id, phase))
                .expect("persist interrupted phase");
            assert!(recover_manager_activation_journals(&mut store).expect("manual recovery"));
            assert_eq!(
                store.snapshot().activation_journals[&operation_id].phase,
                Phase::ManualIntervention
            );
            assert_eq!(
                store.snapshot().activation_journals[&operation_id]
                    .failure_class
                    .as_deref(),
                Some("interrupted-activation")
            );
        }
    }
}
