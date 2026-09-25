//! Runtime-owned command bridge for Manager activation and rollback.

use std::{
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

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
                    crate::manager::store::ReleaseIdentity::ExternalBaseline { .. } => None,
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
                ready_ack: None,
                commit_ack: None,
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
            || !matches!(profile.node_role.as_str(), "coordinator" | "ds4-server")
        {
            return Err(ManagerRuntimeError::NotReady);
        }
        let (build, executable) = store
            .verified_artifact_path(&profile.role_artifact_ids[0], ArtifactKind::Build)
            .map_err(|_| ManagerRuntimeError::NotReady)?;
        if !matches!(
            &build.provenance,
            ArtifactProvenance::Build { record, .. }
                if record.role == "ds4-server" && record.target == "ds4-server"
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
            cluster_activation_preflight(runtime, request.expected_generation).await?;
            let _lease = runtime
                .claim_manager_activation(manager_operation_uuid(&request.operation_id))
                .map_err(|_| ManagerRuntimeError::Busy)?;
            Err(ManagerRuntimeError::NotReady)
        }
        ManagerRuntimeOperation::Rollback(request) => {
            cluster_activation_preflight(runtime, request.expected_generation).await?;
            let _lease = runtime
                .claim_manager_activation(manager_operation_uuid(&request.operation_id))
                .map_err(|_| ManagerRuntimeError::Busy)?;
            Err(ManagerRuntimeError::NotReady)
        }
    }
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
                    ready_ack: None,
                    commit_ack: None,
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
