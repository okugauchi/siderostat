//! DS4 Manager API — ManagerApi / ManagerJob DTO。M10。
//!
//! C04 に基づき、`GET /manager/status`、`GET /manager/inventory`、`POST /manager/jobs`、
//! `GET /manager/jobs/{id}`、`POST /manager/jobs/{id}/cancel` のロジックと
//! 公開 DTO を提供する。許可 job kind の厳密 payload と
//! generation/idempotency を検証する。secret と raw build log を公開 DTO に
//! 入れない。M10。
//!
//! 受入 case（全て必須）:
//! - 入力: fetch→build→download→verify→activate→rollback → 状態一致
//! - 入力: cancel 後 poll → terminal 保持
//! - 入力: 不正 job / unknown field → 400
//! - 入力: activate busy → 409
use crate::manager::executor::ManagerExecutionRequest;
use crate::manager::jobs::{JobJournal, JobKind, JobPhase, ManagerJobError};
use crate::manager::registry::ArtifactState;
use crate::manager::store::{
    ArtifactKind, ArtifactProvenance, HardwareReadiness, ManagerStoreSnapshot,
    PersistedActivationPhase, ProfileCompatibility, ReleaseIdentity,
};

/// 公開 job DTO。secret / raw build log を含まない。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerJobDto {
    pub id: String,
    pub kind: String,
    pub progress: u8,
    pub phase: String,
    pub error: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub cancel: bool,
}

/// JobPhase を snake_case 文字列にする。M10。
fn phase_name(phase: JobPhase) -> &'static str {
    match phase {
        JobPhase::Running => "running",
        JobPhase::Succeeded => "succeeded",
        JobPhase::Failed => "failed",
        JobPhase::Cancelling => "cancelling",
        JobPhase::Interrupted => "interrupted",
    }
}

impl From<&crate::manager::jobs::ManagerJob> for ManagerJobDto {
    fn from(job: &crate::manager::jobs::ManagerJob) -> Self {
        Self {
            id: job.id.clone(),
            kind: job.kind.to_string(),
            progress: job.progress,
            phase: phase_name(job.phase).to_string(),
            error: job.error.clone(),
            created_at: job.created_at,
            updated_at: job.updated_at,
            cancel: job.cancel,
        }
    }
}

/// `GET /manager/status` の応答。job 進捗と旧 active 維持を観測できる。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerStatusResponse {
    pub jobs: Vec<ManagerJobDto>,
    /// 旧 active artifact の digest（維持されていれば Some）。M10。
    pub active_digest: Option<String>,
    /// 進行中 job 数（queue depth）。M10。
    pub queue_depth: usize,
}

/// Sanitized source receipt projection. Remote URLs and local cache paths are omitted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerSourceReceiptDto {
    pub receipt_id: String,
    pub full_commit: String,
    pub main_proof: String,
    pub fetched_at: u64,
}

/// Sanitized artifact reference used by inventory profiles.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerArtifactReferenceDto {
    pub id: String,
    pub digest: Option<String>,
    pub verified: bool,
}

/// Sanitized artifact summary. It never contains a managed path or source URL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerArtifactDto {
    pub id: String,
    pub kind: String,
    pub digest: String,
    pub size: u64,
    pub verified: bool,
    pub source_commit: Option<String>,
    pub role: Option<String>,
    pub catalog_id: Option<String>,
}

/// Sanitized staged profile projection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerStagedProfileDto {
    pub profile_id: String,
    pub node_role: String,
    pub role_artifacts: Vec<ManagerArtifactReferenceDto>,
    pub model_artifact: ManagerArtifactReferenceDto,
    pub config_fingerprint: String,
    pub compatibility: ProfileCompatibility,
    pub hardware_readiness: HardwareReadiness,
    pub activation_ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerNodeReadinessDto {
    pub ready: bool,
    pub reason: Option<String>,
}

/// Durable, local-node inventory. Live process state is intentionally absent;
/// `/manager/status.active_digest` remains unknown until runtime observation is bound.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerInventoryResponse {
    pub node_id: String,
    /// Runtime-observed role for this local node. `None` means role is unknown.
    #[serde(default)]
    pub node_role: Option<String>,
    pub source_commits: Vec<ManagerSourceReceiptDto>,
    pub artifacts: Vec<ManagerArtifactDto>,
    pub profiles: Vec<ManagerStagedProfileDto>,
    pub node_readiness: ManagerNodeReadinessDto,
    /// Live-observed model digest, only when it matches a verified release pointer.
    pub active_digest: Option<String>,
    /// Verified stored previous release digest.
    pub previous_digest: Option<String>,
    /// Local previous release identity. The external baseline uses a fixed sentinel.
    #[serde(default)]
    pub previous_profile_id: Option<String>,
    /// Whether the previous release has a locally verified durable record.
    #[serde(default)]
    pub previous_release_ready: bool,
    pub activation_phase: Option<PersistedActivationPhase>,
    /// Fixed allowlisted transaction failure class, never a raw error message.
    #[serde(default)]
    pub activation_failure_class: Option<String>,
    /// Runtime-owned state used to gate an explicit activation request.
    #[serde(default)]
    pub runtime: Option<ManagerRuntimeReadinessDto>,
    /// Authenticated, sanitized peer inventory. Missing means peer status is unavailable.
    #[serde(default)]
    pub peer: Option<ManagerPeerInventoryDto>,
}

/// Sanitized runtime state required by the Manager action gate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerRuntimeReadinessDto {
    pub cluster_enabled: bool,
    pub generation: u64,
    pub state: String,
    pub desired_policy: String,
    pub applied_policy: String,
    pub policy_epoch: u64,
}

/// A peer's node-local compatible candidate summary. No paths or command data are exposed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerPeerProfileDto {
    pub profile_id: String,
    pub node_role: String,
    pub candidate_digest: String,
    pub source_commit: String,
    pub model_digest: String,
    pub model_catalog_id: String,
    pub config_fingerprint: String,
}

/// Sanitized peer readiness and durable release pointers from an authenticated peer status.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ManagerPeerInventoryDto {
    pub node_id: String,
    pub node_role: String,
    pub profiles: Vec<ManagerPeerProfileDto>,
    pub active_digest: Option<String>,
    pub previous_profile_id: Option<String>,
    pub previous_digest: Option<String>,
    pub previous_release_ready: bool,
    pub activation_phase: Option<PersistedActivationPhase>,
    /// Fixed allowlisted peer transaction failure class.
    pub activation_failure_class: Option<String>,
}

/// `POST /manager/jobs` の要求。kind の厳密 payload。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct JobSubmitRequest {
    /// 許可 job kind（fetch/build/download/verify/stage/activate/rollback）。M10。
    pub kind: String,
    /// 同一作業の正規化キー（idempotency）。M10。
    pub payload_key: String,
    /// activation/rollback に要求（C04）。M10。
    #[serde(default)]
    pub expected_generation: u64,
}

impl JobSubmitRequest {
    /// Preserve the submitted generation when handing a journaled request to the executor.
    /// Runtime lease material is resolved only by the runtime owner.
    pub fn execution_request(
        &self,
        id: String,
    ) -> Result<ManagerExecutionRequest, ManagerApiError> {
        let kind = parse_kind(&self.kind)
            .ok_or_else(|| ManagerApiError::BadRequest("unknown job kind".into()))?;
        Ok(ManagerExecutionRequest {
            id,
            kind,
            payload_key: self.payload_key.clone(),
            expected_generation: self.expected_generation,
        })
    }
}

/// `POST /manager/jobs` の応答。M10。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SubmitResponse {
    pub id: String,
}

/// manager API エラー。HTTP status に対応する。M10。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerApiError {
    /// 400: 不正 job / unknown field / unknown kind。M10。
    BadRequest(String),
    /// 404: 未知 job ID。M10。
    NotFound,
    /// 409: activate busy（同時 activation は 1）。M10。
    Conflict(String),
    /// 503: job journal の永続化に失敗。
    Persistence,
}

impl std::fmt::Display for ManagerApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerApiError::BadRequest(msg) => write!(f, "bad request: {msg}"),
            ManagerApiError::NotFound => write!(f, "job not found"),
            ManagerApiError::Conflict(msg) => write!(f, "conflict: {msg}"),
            ManagerApiError::Persistence => write!(f, "manager job storage unavailable"),
        }
    }
}

impl std::error::Error for ManagerApiError {}

impl From<ManagerJobError> for ManagerApiError {
    fn from(e: ManagerJobError) -> Self {
        match e {
            ManagerJobError::UnknownKind => {
                ManagerApiError::BadRequest("unknown job kind".to_string())
            }
            ManagerJobError::NotFound => ManagerApiError::NotFound,
            ManagerJobError::Persistence | ManagerJobError::IdExhausted => {
                ManagerApiError::Persistence
            }
            ManagerJobError::InvalidTransition => {
                ManagerApiError::Conflict("job is not in an active phase".into())
            }
        }
    }
}

/// kind 文字列 → JobKind。M10。
pub fn parse_kind(kind: &str) -> Option<JobKind> {
    match kind {
        "fetch" => Some(JobKind::Fetch),
        "build" => Some(JobKind::Build),
        "download" => Some(JobKind::Download),
        "verify" => Some(JobKind::Verify),
        "stage" => Some(JobKind::Stage),
        "activate" => Some(JobKind::Activate),
        "rollback" => Some(JobKind::Rollback),
        _ => None,
    }
}

/// job を submit する。activation/rollback は expected_generation を要求し、
/// runtime lease はruntime owner が実行時に解決する。activate busy → 409。M10。
pub fn submit(
    journal: &mut JobJournal,
    req: JobSubmitRequest,
) -> Result<SubmitResponse, ManagerApiError> {
    let kind = parse_kind(&req.kind)
        .ok_or_else(|| ManagerApiError::BadRequest(format!("unknown job kind: {}", req.kind)))?;
    // activation/rollback だけ generation を要求（C04）。M10。
    if matches!(kind, JobKind::Activate | JobKind::Rollback) {
        if req.expected_generation == 0 {
            return Err(ManagerApiError::BadRequest(
                "activate/rollback requires expected_generation".to_string(),
            ));
        }
        // activate busy → 409（同時 activation は 1、C04）。M10。
        if kind == JobKind::Activate && journal.has_running(JobKind::Activate) {
            return Err(ManagerApiError::Conflict(
                "an activation job is already running".to_string(),
            ));
        }
    }
    let id = journal.enqueue(kind, &req.payload_key)?;
    Ok(SubmitResponse { id })
}

/// JSON 文字列から submit する。unknown field / parse 失敗 → 400。M10。
pub fn submit_json(
    journal: &mut JobJournal,
    body: &str,
) -> Result<SubmitResponse, ManagerApiError> {
    let req: JobSubmitRequest =
        serde_json::from_str(body).map_err(|e| ManagerApiError::BadRequest(e.to_string()))?;
    submit(journal, req)
}

/// `GET /manager/status`。M10。
pub fn status(journal: &JobJournal, active_digest: Option<String>) -> ManagerStatusResponse {
    let jobs: Vec<ManagerJobDto> = journal
        .all()
        .iter()
        .map(|j| ManagerJobDto::from(*j))
        .collect();
    let queue_depth = journal
        .all()
        .iter()
        .filter(|j| matches!(j.phase, JobPhase::Running | JobPhase::Cancelling))
        .count();
    ManagerStatusResponse {
        jobs,
        active_digest,
        queue_depth,
    }
}

/// Project the durable store into an API-safe node-local inventory response.
pub fn inventory(snapshot: &ManagerStoreSnapshot) -> ManagerInventoryResponse {
    inventory_with_live_active_digest(snapshot, None)
}

/// Project inventory with a runtime-observed digest. A stored release pointer
/// alone is not evidence that the child loaded that release.
pub fn inventory_with_live_active_digest(
    snapshot: &ManagerStoreSnapshot,
    live_active_digest: Option<&str>,
) -> ManagerInventoryResponse {
    let source_commits = snapshot
        .source_receipts
        .iter()
        .map(|(receipt_id, receipt)| ManagerSourceReceiptDto {
            receipt_id: receipt_id.clone(),
            full_commit: receipt.full_commit.clone(),
            main_proof: receipt.main_proof.clone(),
            fetched_at: receipt.fetched_at,
        })
        .collect();

    let artifacts = snapshot
        .artifacts
        .values()
        .map(|artifact| {
            let (source_commit, role, catalog_id) = match &artifact.provenance {
                ArtifactProvenance::Build {
                    source_receipt_id,
                    record,
                } => (
                    snapshot
                        .source_receipts
                        .get(source_receipt_id)
                        .map(|source| source.full_commit.clone()),
                    super::build::is_approved_role(&record.role).then(|| record.role.clone()),
                    None,
                ),
                ArtifactProvenance::Model { catalog_id } => (None, None, Some(catalog_id.clone())),
            };
            ManagerArtifactDto {
                id: artifact.id.clone(),
                kind: match artifact.kind {
                    ArtifactKind::Build => "build".into(),
                    ArtifactKind::Model => "model".into(),
                },
                digest: artifact.sha256.clone(),
                size: artifact.size,
                verified: artifact.validation_state == ArtifactState::Verified,
                source_commit,
                role,
                catalog_id,
            }
        })
        .collect::<Vec<_>>();

    let profiles = snapshot
        .profiles
        .values()
        .map(|profile| {
            let role_artifacts = profile
                .role_artifact_ids
                .iter()
                .map(|id| artifact_reference(snapshot, id, ArtifactKind::Build))
                .collect::<Vec<_>>();
            let model_artifact =
                artifact_reference(snapshot, &profile.model_artifact_id, ArtifactKind::Model);
            let activation_ready = profile.compatibility == ProfileCompatibility::Compatible
                && profile.hardware_readiness == HardwareReadiness::Ready
                && !role_artifacts.is_empty()
                && role_artifacts
                    .iter()
                    .all(|artifact| artifact.verified && artifact.digest.is_some())
                && model_artifact.verified
                && model_artifact.digest.is_some();
            ManagerStagedProfileDto {
                profile_id: profile.profile_id.clone(),
                node_role: profile.node_role.clone(),
                role_artifacts,
                model_artifact,
                config_fingerprint: profile.config_fingerprint.clone(),
                compatibility: profile.compatibility,
                hardware_readiness: profile.hardware_readiness,
                activation_ready,
            }
        })
        .collect::<Vec<_>>();
    let ready = profiles.iter().any(|profile| profile.activation_ready);
    let reason = (!ready).then(|| {
        if profiles.is_empty() {
            "no staged profile".to_string()
        } else if profiles
            .iter()
            .all(|p| p.compatibility != ProfileCompatibility::Compatible)
        {
            "profile compatibility is not verified".to_string()
        } else if profiles
            .iter()
            .any(|p| p.hardware_readiness == HardwareReadiness::Pending)
        {
            "hardware readiness is pending".to_string()
        } else {
            "profile artifacts are missing or unverified".to_string()
        }
    });

    let recorded_active_digest =
        release_model_digest(snapshot, snapshot.release_pointers.active.as_ref());
    let active_digest = live_active_digest
        .filter(|observed| recorded_active_digest.as_deref() == Some(*observed))
        .map(str::to_string);

    ManagerInventoryResponse {
        node_id: snapshot.node_id.clone(),
        node_role: None,
        source_commits,
        artifacts,
        profiles,
        node_readiness: ManagerNodeReadinessDto { ready, reason },
        active_digest,
        previous_digest: release_model_digest(
            snapshot,
            snapshot.release_pointers.previous.as_ref(),
        ),
        previous_profile_id: snapshot
            .release_pointers
            .previous
            .as_ref()
            .map(release_identity_profile_id),
        // The store contains durable metadata but cannot prove that the files still exist or
        // match their recorded digest. The runtime owner overwrites this only after rechecking
        // the previous command/baseline against the filesystem.
        previous_release_ready: false,
        activation_phase: inventory_activation_phase(snapshot),
        activation_failure_class: inventory_activation_failure_class(snapshot),
        runtime: None,
        peer: None,
    }
}

pub fn release_identity_profile_id(identity: &ReleaseIdentity) -> String {
    match identity {
        ReleaseIdentity::ManagedProfile(profile_id) => profile_id.clone(),
        ReleaseIdentity::ExternalBaseline { .. } => "external-baseline".into(),
    }
}

pub fn inventory_activation_phase(
    snapshot: &ManagerStoreSnapshot,
) -> Option<PersistedActivationPhase> {
    let pending = snapshot
        .activation_journals
        .values()
        .filter(|journal| {
            !matches!(
                journal.phase,
                PersistedActivationPhase::Complete | PersistedActivationPhase::RolledBack
            )
        })
        .map(|journal| journal.phase)
        .collect::<Vec<_>>();
    match pending.as_slice() {
        [phase] => Some(*phase),
        [] => {
            if snapshot
                .activation_journals
                .values()
                .any(|journal| journal.phase == PersistedActivationPhase::ManualIntervention)
            {
                Some(PersistedActivationPhase::ManualIntervention)
            } else {
                snapshot
                    .activation_journals
                    .values()
                    .next_back()
                    .map(|journal| journal.phase)
            }
        }
        _ => Some(PersistedActivationPhase::ManualIntervention),
    }
}

pub fn inventory_activation_failure_class(snapshot: &ManagerStoreSnapshot) -> Option<String> {
    let pending = snapshot
        .activation_journals
        .values()
        .filter(|journal| {
            !matches!(
                journal.phase,
                PersistedActivationPhase::Complete | PersistedActivationPhase::RolledBack
            )
        })
        .collect::<Vec<_>>();
    if pending.len() > 1 {
        return Some("multiple-unresolved-transactions".into());
    }
    let journal = pending
        .first()
        .copied()
        .or_else(|| snapshot.activation_journals.values().next_back())?;
    journal
        .failure_class
        .as_deref()
        .map(sanitize_activation_failure_class)
}

pub fn sanitize_activation_failure_class(value: &str) -> String {
    const SAFE_CLASSES: &[&str] = &[
        "candidate-activation-failed",
        "interrupted-activation",
        "multiple-unresolved-transactions",
        "prepare-ack-persist-failed",
        "drain-intent-persist-failed",
        "local-drain-failed",
        "local-drain-ambiguous",
        "local-drain-stop-unconfirmed",
        "local-drain-ack-persist-failed",
        "local-drain-rollback-intent-persist-failed",
        "local-drain-rollback-ack-persist-failed",
        "local-drain-unconfirmed",
        "local-candidate-start-failed",
        "local-start-failed",
        "local-previous-restore-failed",
        "local-ready-ack-persist-failed",
        "ready-ack-persist-failed",
        "commit-intent-persist-failed",
        "start-intent-persist-failed",
        "global-complete-persist-failed",
        "final-live-check-failed",
        "final-live-check-after-peer-finalize-failed",
        "rollback-intent-persist-failed",
        "rollback-result-persist-failed",
        "rollback-ack-persist-failed",
        "rollback-drain-failed",
        "rollback-failed",
        "rollback-pointer-persist-failed",
        "rollback-ack-ambiguous",
        "rollback-start-failed",
        "peer-prepare-ack-persist-failed",
        "peer-prepare-ack-ambiguous",
        "peer-drain-effect-ambiguous",
        "peer-drain-ack-ambiguous",
        "peer-drain-ack-persist-failed",
        "peer-drain-failed",
        "peer-drain-unconfirmed",
        "peer-drain-failed-child-not-running",
        "peer-drain-rollback-intent-persist-failed",
        "peer-drain-rollback-ack-persist-failed",
        "peer-start-ack-ambiguous",
        "peer-start-ack-persist-failed",
        "peer-candidate-start-ambiguous",
        "peer-ready-ack-persist-failed",
        "peer-commit-ack-ambiguous",
        "peer-commit-ack-persist-failed",
        "peer-finalize-ack-ambiguous",
        "peer-rollback-before-drain",
        "peer-rollback-requested",
        "peer-rollback-drain-failed",
        "peer-rollback-unconfirmed",
        "peer-rollback-failed",
        "peer-rollback-pointer-persist-failed",
        "peer-rollback-ack-persist-failed",
        "peer-candidate-start-failed",
        "peer-previous-restore-failed",
        "peer-rollback-intent-persist-failed",
        "previous-release-restoration-failed",
        "peer-rollback-ack-ambiguous",
        "peer-manual-intervention",
    ];
    if SAFE_CLASSES.contains(&value) {
        value.to_string()
    } else {
        "runtime-transaction-failed".into()
    }
}

fn artifact_reference(
    snapshot: &ManagerStoreSnapshot,
    id: &str,
    expected_kind: ArtifactKind,
) -> ManagerArtifactReferenceDto {
    let artifact = snapshot.artifacts.get(id);
    let valid = artifact.is_some_and(|artifact| {
        artifact.kind == expected_kind && artifact.validation_state == ArtifactState::Verified
    });
    ManagerArtifactReferenceDto {
        id: id.to_string(),
        digest: artifact
            .filter(|artifact| artifact.kind == expected_kind)
            .map(|artifact| artifact.sha256.clone()),
        verified: valid,
    }
}

fn release_model_digest(
    snapshot: &ManagerStoreSnapshot,
    identity: Option<&ReleaseIdentity>,
) -> Option<String> {
    match identity? {
        ReleaseIdentity::ManagedProfile(profile_id) => {
            let profile = snapshot.profiles.get(profile_id)?;
            if profile.compatibility != ProfileCompatibility::Compatible
                || profile.hardware_readiness != HardwareReadiness::Ready
            {
                return None;
            }
            let model = snapshot.artifacts.get(&profile.model_artifact_id)?;
            (model.kind == ArtifactKind::Model && model.validation_state == ArtifactState::Verified)
                .then(|| model.sha256.clone())
        }
        ReleaseIdentity::ExternalBaseline { model_sha256, .. } => Some(model_sha256.clone()),
    }
}

/// `GET /manager/jobs/{id}`。M10。
pub fn get(journal: &JobJournal, id: &str) -> Result<ManagerJobDto, ManagerApiError> {
    let job = journal.get(id).ok_or(ManagerApiError::NotFound)?;
    Ok(ManagerJobDto::from(job))
}

/// `POST /manager/jobs/{id}/cancel`。M10。job phase を Cancelling にし、
/// terminal 状態を保持する（cancel 後 poll でも terminal）。M10。
pub fn cancel(journal: &mut JobJournal, id: &str) -> Result<(), ManagerApiError> {
    let phase = journal.get(id).ok_or(ManagerApiError::NotFound)?.phase;
    if matches!(phase, JobPhase::Running | JobPhase::Cancelling) {
        journal.request_cancel(id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: &str, key: &str) -> JobSubmitRequest {
        JobSubmitRequest {
            kind: kind.to_string(),
            payload_key: key.to_string(),
            expected_generation: 0,
        }
    }

    fn activate_req(key: &str) -> JobSubmitRequest {
        JobSubmitRequest {
            kind: "activate".to_string(),
            payload_key: key.to_string(),
            expected_generation: 3,
        }
    }

    #[test]
    fn activation_failure_class_is_restricted_to_fixed_safe_labels() {
        assert_eq!(
            sanitize_activation_failure_class("peer-drain-effect-ambiguous"),
            "peer-drain-effect-ambiguous"
        );
        assert_eq!(
            sanitize_activation_failure_class("/private/path token=private-secret"),
            "runtime-transaction-failed"
        );
    }

    /// 受入: fetch→build→download→verify→activate→rollback → 状態一致。M10。
    #[test]
    fn full_pipeline_states_match() {
        let mut journal = JobJournal::new();
        for (kind, key) in [
            ("fetch", "src-a"),
            ("build", "src-a"),
            ("download", "model-a"),
            ("verify", "model-a"),
            ("stage", "profile-a"),
        ] {
            let id = submit(&mut journal, req(kind, key)).expect("submit").id;
            assert!(journal.get(&id).expect("job").kind.as_str() == kind);
            journal.succeed(&id).expect("succeed");
        }
        // activate（generation 必須、runtime lease は runtime owner が解決）。M10。
        let id = submit(&mut journal, activate_req("profile-a"))
            .expect("activate")
            .id;
        journal.succeed(&id).expect("succeed");
        // rollback（generation 必須、runtime lease は runtime owner が解決）。M10。
        let mut rb = activate_req("profile-a");
        rb.kind = "rollback".to_string();
        let id = submit(&mut journal, rb).expect("rollback").id;
        journal.succeed(&id).expect("succeed");
        // 全 job が Succeeded。状態一致（全 kind が揃う）。M10。all() は
        // ID 順（アルファベット順）で返すため、集合で検証する。M10。
        let jobs = journal.all();
        assert_eq!(jobs.len(), 7);
        assert!(jobs.iter().all(|j| j.phase == JobPhase::Succeeded));
        let mut kinds: Vec<&str> = jobs.iter().map(|j| j.kind.as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            vec![
                "activate", "build", "download", "fetch", "rollback", "stage", "verify"
            ]
        );
    }

    /// 受入: cancel 後 poll → terminal 保持。M10。
    #[test]
    fn cancel_keeps_terminal_phase() {
        let mut journal = JobJournal::new();
        let id = submit(&mut journal, req("fetch", "src-b"))
            .expect("submit")
            .id;
        cancel(&mut journal, &id).expect("cancel");
        let job = journal.get(&id).expect("job");
        assert_eq!(job.phase, JobPhase::Cancelling);
        // poll しても terminal（Cancelling）を保持。M10。
        let dto = get(&journal, &id).expect("get");
        assert_eq!(dto.phase, "cancelling");
    }

    /// 受入: 不正 job / unknown field → 400。M10。
    #[test]
    fn invalid_job_and_unknown_field_rejected() {
        let mut journal = JobJournal::new();
        // unknown kind → 400。M10。
        let err = submit(&mut journal, req("publish", "x")).expect_err("unknown kind");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        // unknown field（deny_unknown_fields）→ 400。M10。
        let err = submit_json(
            &mut journal,
            r#"{"kind":"fetch","payload_key":"y","bogus":1}"#,
        )
        .expect_err("unknown field");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        // JSON 破損 → 400。M10。
        let err = submit_json(&mut journal, "not-json").expect_err("bad json");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
    }

    #[test]
    fn activation_request_uses_generation_without_accepting_a_caller_lease() {
        let mut journal = JobJournal::new();
        let without_lease = submit_json(
            &mut journal,
            r#"{"kind":"activate","payload_key":"profile-a","expected_generation":7}"#,
        );
        assert!(without_lease.is_ok(), "runtime lease is runtime-owned");

        let with_caller_lease = submit_json(
            &mut journal,
            r#"{"kind":"rollback","payload_key":"previous","expected_generation":7,"runtime_lease":"caller-controlled"}"#,
        );
        assert!(matches!(
            with_caller_lease,
            Err(ManagerApiError::BadRequest(_))
        ));
    }

    /// 受入: activate busy → 409。M10。
    #[test]
    fn activate_busy_conflicts() {
        let mut journal = JobJournal::new();
        let _ = submit(&mut journal, activate_req("profile-b")).expect("first activate");
        // 2 つ目の activate（進行中）→ 409。M10。
        let err = submit(&mut journal, activate_req("profile-c")).expect_err("busy");
        assert!(matches!(err, ManagerApiError::Conflict(_)));
    }

    /// activation/rollback は generation 必須。lease はruntime ownerが解決。M10。
    #[test]
    fn activate_requires_generation_only() {
        let mut journal = JobJournal::new();
        // generation 0 → 400。M10。
        let mut a = activate_req("profile-d");
        a.expected_generation = 0;
        let err = submit(&mut journal, a).expect_err("gen required");
        assert!(matches!(err, ManagerApiError::BadRequest(_)));
        let accepted = submit_json(
            &mut journal,
            r#"{"kind":"activate","payload_key":"profile-e","expected_generation":4}"#,
        );
        assert!(accepted.is_ok());
    }

    /// DTO は secret/raw build log を含まない。M10。
    #[test]
    fn dto_excludes_secret_and_raw_build_log() {
        let mut journal = JobJournal::new();
        let id = submit(&mut journal, req("build", "src-c"))
            .expect("submit")
            .id;
        let dto = get(&journal, &id).expect("get");
        let json = serde_json::to_string(&dto).expect("json");
        // DTO に secret / build log フィールドが無い。M10。
        assert!(!json.contains("secret"));
        assert!(!json.contains("api_key"));
        assert!(!json.contains("build_log"));
        assert!(!json.contains("token"));
    }
}
