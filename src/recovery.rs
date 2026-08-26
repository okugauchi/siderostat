use crate::{
    canary::CanaryReason,
    target::{ClusterState, LocalRole, StableMode},
};
use serde::{Serialize, Serializer};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const RECOVERY_HISTORY_LIMIT: usize = 32;
pub const RECOVERY_COOLDOWN_MILLIS: u64 = 60 * 60 * 1000;
pub const RECOVERY_WINDOW_MILLIS: u64 = 12 * 60 * 60 * 1000;
pub const RECOVERY_MAX_ATTEMPTS: usize = 2;
pub const RECOVERY_IDEMPOTENCY_KEY_MAX_BYTES: usize = 128;
pub const RECOVERY_ADMISSION_DRAIN_TIMEOUT_MILLIS: u64 = 60 * 1000;
pub const RECOVERY_LOW_TPS_SUSTAINED_MILLIS: u64 = 30 * 1000;
pub const RECOVERY_FIRST_TOKEN_TIMEOUT_MILLIS: u64 = 30 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryReason {
    ThroughputDegraded,
}

impl RecoveryReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThroughputDegraded => "throughput-degraded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryTrigger {
    ManualCanaryFailure,
    ProgressStall,
    LowDecodeTps,
    FirstTokenTimeout,
}

impl RecoveryTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManualCanaryFailure => "manual-canary-failure",
            Self::ProgressStall => "progress-stall",
            Self::LowDecodeTps => "low-decode-tps",
            Self::FirstTokenTimeout => "first-token-timeout",
        }
    }
}

#[cfg(test)]
mod detector_tests {
    use super::*;

    fn active_low_tps() -> RecoveryObservation {
        RecoveryObservation::Active {
            first_progress_observed: true,
            active_age_millis: 10_000,
            progress_age_millis: Some(100),
            chunk_tps: Some(4.9),
        }
    }

    #[test]
    fn one_low_sample_does_not_start_recovery() {
        let mut detector = RecoveryDetector::default();

        assert_eq!(detector.observe_at(0, active_low_tps()), None);
        assert_eq!(detector.observe_at(29_999, active_low_tps()), None);
        assert_eq!(
            detector.observe_at(30_000, active_low_tps()),
            Some(RecoveryTrigger::LowDecodeTps)
        );
    }

    #[test]
    fn idle_zero_tps_is_healthy_and_resets_incident() {
        let mut detector = RecoveryDetector::default();

        assert_eq!(
            detector.observe_at(
                0,
                RecoveryObservation::Active {
                    first_progress_observed: true,
                    active_age_millis: 1_000,
                    progress_age_millis: Some(0),
                    chunk_tps: Some(0.0),
                },
            ),
            None
        );
        assert_eq!(detector.observe_at(1_000, RecoveryObservation::Idle), None);
        assert_eq!(detector.observe_at(2_000, active_low_tps()), None);
    }

    #[test]
    fn progress_stall_has_priority_over_low_tps_and_is_emitted_once() {
        let mut detector = RecoveryDetector::default();
        let input = RecoveryObservation::Active {
            first_progress_observed: true,
            active_age_millis: 90_000,
            progress_age_millis: Some(60_000),
            chunk_tps: Some(1.0),
        };

        assert_eq!(
            detector.observe_at(0, input),
            Some(RecoveryTrigger::ProgressStall)
        );
        assert_eq!(detector.observe_at(1, input), None);
    }

    #[test]
    fn first_token_timeout_is_distinct_from_progress_stall() {
        let mut detector = RecoveryDetector::default();
        let input = RecoveryObservation::Active {
            first_progress_observed: false,
            active_age_millis: RECOVERY_FIRST_TOKEN_TIMEOUT_MILLIS,
            progress_age_millis: None,
            chunk_tps: None,
        };

        assert_eq!(
            detector.observe_at(0, input),
            Some(RecoveryTrigger::FirstTokenTimeout)
        );
    }

    #[test]
    fn canary_failure_is_mapped_once_and_healthy_canary_clears_incident() {
        let mut detector = RecoveryDetector::default();

        assert_eq!(
            detector.observe_canary_at(0, CanaryReason::Deadline),
            Some(RecoveryTrigger::FirstTokenTimeout)
        );
        assert_eq!(detector.observe_canary_at(1, CanaryReason::Deadline), None);
        assert_eq!(detector.observe_canary_at(2, CanaryReason::Healthy), None);
        assert_eq!(
            detector.observe_canary_at(3, CanaryReason::HttpError),
            Some(RecoveryTrigger::ManualCanaryFailure)
        );
    }

    #[test]
    fn clock_does_not_move_low_tps_window_backwards() {
        let mut detector = RecoveryDetector::default();
        let input = active_low_tps();

        assert_eq!(detector.observe_at(10_000, input), None);
        assert_eq!(detector.observe_at(9_000, input), None);
        assert_eq!(
            detector.observe_at(40_000, input),
            Some(RecoveryTrigger::LowDecodeTps)
        );
    }
}

/// Inputs consumed by the automatic throughput detector. The timestamps are
/// monotonic elapsed milliseconds supplied by the caller; wall-clock time is
/// intentionally not part of this type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecoveryObservation {
    Idle,
    Active {
        first_progress_observed: bool,
        active_age_millis: u64,
        progress_age_millis: Option<u64>,
        chunk_tps: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecoveryDetectorPolicy {
    pub low_decode_tps: f64,
    pub low_decode_duration_millis: u64,
    pub progress_stall_millis: u64,
    pub first_token_timeout_millis: u64,
}

impl Default for RecoveryDetectorPolicy {
    fn default() -> Self {
        Self {
            low_decode_tps: 5.0,
            low_decode_duration_millis: RECOVERY_LOW_TPS_SUSTAINED_MILLIS,
            progress_stall_millis: RECOVERY_ADMISSION_DRAIN_TIMEOUT_MILLIS,
            first_token_timeout_millis: RECOVERY_FIRST_TOKEN_TIMEOUT_MILLIS,
        }
    }
}

/// Deterministic, one-shot detector for a single degradation incident.
///
/// A detector does not own recovery lifecycle state and does not retry a
/// recovery job. The caller must pass its result through RecoveryService,
/// whose cooldown, attempt limit, role, cluster-state, and owner gates remain
/// authoritative.
#[derive(Debug, Clone)]
pub struct RecoveryDetector {
    policy: RecoveryDetectorPolicy,
    low_tps_since_millis: Option<u64>,
    incident_active: bool,
    last_observed_millis: u64,
}

impl RecoveryDetector {
    pub fn new(policy: RecoveryDetectorPolicy) -> anyhow::Result<Self> {
        anyhow::ensure!(
            policy.low_decode_tps.is_finite() && policy.low_decode_tps >= 0.0,
            "recovery detector low decode TPS must be finite and non-negative"
        );
        anyhow::ensure!(
            policy.low_decode_duration_millis > 0,
            "recovery detector low decode duration must be positive"
        );
        anyhow::ensure!(
            policy.progress_stall_millis > 0,
            "recovery detector progress stall must be positive"
        );
        anyhow::ensure!(
            policy.first_token_timeout_millis > 0,
            "recovery detector first-token timeout must be positive"
        );
        Ok(Self {
            policy,
            low_tps_since_millis: None,
            incident_active: false,
            last_observed_millis: 0,
        })
    }

    pub fn observe_at(
        &mut self,
        now_millis: u64,
        observation: RecoveryObservation,
    ) -> Option<RecoveryTrigger> {
        let now_millis = now_millis.max(self.last_observed_millis);
        self.last_observed_millis = now_millis;
        match observation {
            RecoveryObservation::Idle => {
                self.reset_incident();
                None
            }
            RecoveryObservation::Active {
                first_progress_observed,
                active_age_millis,
                progress_age_millis,
                chunk_tps,
            } => {
                if !first_progress_observed {
                    self.low_tps_since_millis = None;
                    if active_age_millis >= self.policy.first_token_timeout_millis {
                        return self.emit_once(RecoveryTrigger::FirstTokenTimeout);
                    }
                    return None;
                }

                if progress_age_millis.is_some_and(|age| age >= self.policy.progress_stall_millis) {
                    return self.emit_once(RecoveryTrigger::ProgressStall);
                }

                let low_tps = chunk_tps.is_some_and(|tps| {
                    tps.is_finite() && tps >= 0.0 && tps < self.policy.low_decode_tps
                });
                if low_tps {
                    let since = *self.low_tps_since_millis.get_or_insert(now_millis);
                    if now_millis.saturating_sub(since) >= self.policy.low_decode_duration_millis {
                        return self.emit_once(RecoveryTrigger::LowDecodeTps);
                    }
                } else {
                    self.low_tps_since_millis = None;
                }
                None
            }
        }
    }

    pub fn observe_canary_at(
        &mut self,
        now_millis: u64,
        reason: CanaryReason,
    ) -> Option<RecoveryTrigger> {
        let now_millis = now_millis.max(self.last_observed_millis);
        self.last_observed_millis = now_millis;
        match reason {
            CanaryReason::Healthy => {
                self.reset_incident();
                None
            }
            CanaryReason::Deadline => self.emit_once(RecoveryTrigger::FirstTokenTimeout),
            CanaryReason::LowDecodeTps => self.emit_once(RecoveryTrigger::LowDecodeTps),
            CanaryReason::ProgressStall => self.emit_once(RecoveryTrigger::ProgressStall),
            CanaryReason::HttpError => self.emit_once(RecoveryTrigger::ManualCanaryFailure),
        }
    }

    fn emit_once(&mut self, trigger: RecoveryTrigger) -> Option<RecoveryTrigger> {
        if self.incident_active {
            return None;
        }
        self.incident_active = true;
        Some(trigger)
    }

    fn reset_incident(&mut self) {
        self.low_tps_since_millis = None;
        self.incident_active = false;
    }
}

impl Default for RecoveryDetector {
    fn default() -> Self {
        Self::new(RecoveryDetectorPolicy::default())
            .expect("default recovery detector policy must be valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    Running,
    Succeeded,
    Failed,
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Accepted,
    Snapshot,
    AdmissionBlocked,
    Draining,
    Demoting,
    PairedStandalone,
    Promoting,
    PostRecoveryCanary,
    Serving,
    Completed,
    Failed,
    Suppressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFailureReason {
    NotCoordinator,
    NotDistributedReady,
    Cooldown,
    AttemptLimit,
    SnapshotWrite,
    UnknownRecoveryId,
    RecoveryOwnerBusy,
    DrainTimeout,
    DemotionFailed,
    PromotionFailed,
    ChildIdentityMismatch,
    PeerLoss,
    CanaryFailed,
}

impl RecoveryFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotCoordinator => "not-coordinator",
            Self::NotDistributedReady => "not-distributed-ready",
            Self::Cooldown => "cooldown",
            Self::AttemptLimit => "attempt-limit",
            Self::SnapshotWrite => "snapshot-write",
            Self::UnknownRecoveryId => "unknown-recovery-id",
            Self::RecoveryOwnerBusy => "recovery-owner-busy",
            Self::DrainTimeout => "drain-timeout",
            Self::DemotionFailed => "demotion-failed",
            Self::PromotionFailed => "promotion-failed",
            Self::ChildIdentityMismatch => "child-identity-mismatch",
            Self::PeerLoss => "peer-loss",
            Self::CanaryFailed => "canary-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryGate {
    pub role: LocalRole,
    pub mode: StableMode,
    pub state: ClusterState,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRequest {
    pub reason: RecoveryReason,
    pub trigger: RecoveryTrigger,
    pub idempotency_key: String,
}

impl RecoveryRequest {
    pub fn new(
        reason: RecoveryReason,
        trigger: RecoveryTrigger,
        idempotency_key: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let idempotency_key = idempotency_key.into();
        anyhow::ensure!(
            !idempotency_key.is_empty()
                && idempotency_key.len() <= RECOVERY_IDEMPOTENCY_KEY_MAX_BYTES,
            "idempotency_key must be between 1 and {RECOVERY_IDEMPOTENCY_KEY_MAX_BYTES} bytes"
        );
        anyhow::ensure!(
            !idempotency_key.chars().any(char::is_control),
            "idempotency_key must not contain control characters"
        );
        Ok(Self {
            reason,
            trigger,
            idempotency_key,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub history_limit: usize,
    pub cooldown: u64,
    pub window: u64,
    pub max_attempts: usize,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            history_limit: RECOVERY_HISTORY_LIMIT,
            cooldown: RECOVERY_COOLDOWN_MILLIS,
            window: RECOVERY_WINDOW_MILLIS,
            max_attempts: RECOVERY_MAX_ATTEMPTS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecoveryJob {
    pub recovery_id: String,
    pub operation: &'static str,
    pub state: RecoveryState,
    pub phase: RecoveryPhase,
    pub reason: RecoveryReason,
    pub trigger: RecoveryTrigger,
    pub owner: &'static str,
    #[serde(rename = "started_at", serialize_with = "serialize_timestamp_millis")]
    pub started_at_millis: u64,
    #[serde(
        rename = "finished_at",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_timestamp_millis"
    )]
    pub finished_at_millis: Option<u64>,
    #[serde(rename = "old_cluster_generation")]
    pub cluster_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_cluster_generation: Option<u64>,
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<RecoveryFailureReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_recovery_canary: Option<CanaryReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryStart {
    Created(RecoveryJob),
    Existing(RecoveryJob),
    Suppressed(RecoveryJob),
}

#[derive(Clone)]
pub struct RecoveryService {
    inner: Arc<Mutex<RecoveryInner>>,
}

struct RecoveryInner {
    policy: RecoveryPolicy,
    active: Option<RecoveryJob>,
    history: VecDeque<RecoveryJob>,
}

impl RecoveryService {
    pub fn new() -> Self {
        Self::with_policy(RecoveryPolicy::default())
    }

    pub fn with_policy(mut policy: RecoveryPolicy) -> Self {
        policy.history_limit = policy.history_limit.max(1);
        Self {
            inner: Arc::new(Mutex::new(RecoveryInner {
                policy,
                active: None,
                history: VecDeque::new(),
            })),
        }
    }

    pub fn begin(&self, gate: RecoveryGate, request: RecoveryRequest) -> RecoveryStart {
        self.begin_at(gate, request, now_millis())
    }

    pub fn begin_at(
        &self,
        gate: RecoveryGate,
        request: RecoveryRequest,
        now: u64,
    ) -> RecoveryStart {
        let mut inner = self.lock();
        if let Some(active) = inner.active.as_ref() {
            return RecoveryStart::Existing(active.clone());
        }
        if let Some(existing) = inner
            .history
            .iter()
            .rev()
            .find(|job| job.idempotency_key == request.idempotency_key)
        {
            return RecoveryStart::Existing(existing.clone());
        }

        if let Some(failure_reason) = gate_failure(gate) {
            let job = new_job(
                gate,
                request,
                now,
                RecoveryState::Suppressed,
                RecoveryPhase::Suppressed,
            );
            let job = with_failure(job, failure_reason, now);
            push_history(&mut inner, job.clone());
            return RecoveryStart::Suppressed(job);
        }
        if in_cooldown(&inner, now) {
            let job = new_job(
                gate,
                request,
                now,
                RecoveryState::Suppressed,
                RecoveryPhase::Suppressed,
            );
            let job = with_failure(job, RecoveryFailureReason::Cooldown, now);
            push_history(&mut inner, job.clone());
            return RecoveryStart::Suppressed(job);
        }
        if attempts_in_window(&inner, now) >= inner.policy.max_attempts {
            let job = new_job(
                gate,
                request,
                now,
                RecoveryState::Suppressed,
                RecoveryPhase::Suppressed,
            );
            let job = with_failure(job, RecoveryFailureReason::AttemptLimit, now);
            push_history(&mut inner, job.clone());
            return RecoveryStart::Suppressed(job);
        }

        let job = new_job(
            gate,
            request,
            now,
            RecoveryState::Running,
            RecoveryPhase::Snapshot,
        );
        inner.active = Some(job.clone());
        RecoveryStart::Created(job)
    }

    pub fn mark_snapshot_succeeded(
        &self,
        recovery_id: &str,
        snapshot_path: impl Into<String>,
    ) -> Option<RecoveryJob> {
        let mut inner = self.lock();
        let job = inner.active.as_mut()?;
        if job.recovery_id != recovery_id {
            return None;
        }
        job.snapshot_path = Some(snapshot_path.into());
        Some(job.clone())
    }

    pub fn mark_phase(
        &self,
        recovery_id: &str,
        phase: RecoveryPhase,
        admission: Option<String>,
    ) -> Option<RecoveryJob> {
        let mut inner = self.lock();
        let job = inner.active.as_mut()?;
        if job.recovery_id != recovery_id {
            return None;
        }
        job.phase = phase;
        job.admission = admission;
        Some(job.clone())
    }

    pub fn mark_post_recovery_canary(
        &self,
        recovery_id: &str,
        result: CanaryReason,
    ) -> Option<RecoveryJob> {
        let mut inner = self.lock();
        let job = inner.active.as_mut()?;
        if job.recovery_id != recovery_id {
            return None;
        }
        job.post_recovery_canary = Some(result);
        Some(job.clone())
    }

    pub fn mark_snapshot_failed(
        &self,
        recovery_id: &str,
        failure_reason: RecoveryFailureReason,
        finished_at_millis: u64,
    ) -> Option<RecoveryJob> {
        let mut inner = self.lock();
        let mut job = inner.active.take()?;
        if job.recovery_id != recovery_id {
            inner.active = Some(job);
            return None;
        }
        job = with_failure(job, failure_reason, finished_at_millis);
        push_history(&mut inner, job.clone());
        Some(job)
    }

    pub fn mark_succeeded(
        &self,
        recovery_id: &str,
        finished_at_millis: u64,
        new_cluster_generation: u64,
    ) -> Option<RecoveryJob> {
        self.mark_terminal(
            recovery_id,
            finished_at_millis,
            RecoveryState::Succeeded,
            RecoveryPhase::Completed,
            None,
            Some(new_cluster_generation),
        )
    }

    pub fn mark_failed(
        &self,
        recovery_id: &str,
        finished_at_millis: u64,
        failure_reason: RecoveryFailureReason,
    ) -> Option<RecoveryJob> {
        self.mark_terminal(
            recovery_id,
            finished_at_millis,
            RecoveryState::Failed,
            RecoveryPhase::Failed,
            Some(failure_reason),
            None,
        )
    }

    pub fn status(&self, recovery_id: &str) -> Option<RecoveryJob> {
        let inner = self.lock();
        inner
            .active
            .as_ref()
            .filter(|job| job.recovery_id == recovery_id)
            .cloned()
            .or_else(|| {
                inner
                    .history
                    .iter()
                    .rev()
                    .find(|job| job.recovery_id == recovery_id)
                    .cloned()
            })
    }

    pub fn has_active_job(&self) -> bool {
        self.lock().active.is_some()
    }

    pub fn history_len(&self) -> usize {
        self.lock().history.len()
    }

    fn mark_terminal(
        &self,
        recovery_id: &str,
        finished_at_millis: u64,
        state: RecoveryState,
        phase: RecoveryPhase,
        failure_reason: Option<RecoveryFailureReason>,
        new_cluster_generation: Option<u64>,
    ) -> Option<RecoveryJob> {
        let mut inner = self.lock();
        let mut job = inner.active.take()?;
        if job.recovery_id != recovery_id {
            inner.active = Some(job);
            return None;
        }
        job.state = state;
        job.phase = phase;
        job.finished_at_millis = Some(finished_at_millis);
        job.failure_reason = failure_reason;
        job.new_cluster_generation = new_cluster_generation;
        push_history(&mut inner, job.clone());
        Some(job)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RecoveryInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RecoveryService {
    fn default() -> Self {
        Self::new()
    }
}

fn gate_failure(gate: RecoveryGate) -> Option<RecoveryFailureReason> {
    if gate.role != LocalRole::Coordinator {
        return Some(RecoveryFailureReason::NotCoordinator);
    }
    if gate.mode != StableMode::DistributedLayerParallel
        || gate.state != ClusterState::DistributedReady
    {
        return Some(RecoveryFailureReason::NotDistributedReady);
    }
    None
}

fn in_cooldown(inner: &RecoveryInner, now: u64) -> bool {
    inner
        .history
        .iter()
        .filter(|job| matches!(job.state, RecoveryState::Succeeded | RecoveryState::Failed))
        .filter_map(|job| job.finished_at_millis)
        .max()
        .is_some_and(|finished| now < finished.saturating_add(inner.policy.cooldown))
}

fn attempts_in_window(inner: &RecoveryInner, now: u64) -> usize {
    inner
        .history
        .iter()
        .filter(|job| matches!(job.state, RecoveryState::Succeeded | RecoveryState::Failed))
        .filter(|job| now.saturating_sub(job.started_at_millis) <= inner.policy.window)
        .count()
}

fn new_job(
    gate: RecoveryGate,
    request: RecoveryRequest,
    started_at_millis: u64,
    state: RecoveryState,
    phase: RecoveryPhase,
) -> RecoveryJob {
    RecoveryJob {
        recovery_id: Uuid::new_v4().to_string(),
        operation: "recover-degraded",
        state,
        phase,
        reason: request.reason,
        trigger: request.trigger,
        owner: "coordinator",
        started_at_millis,
        finished_at_millis: None,
        cluster_generation: gate.generation,
        new_cluster_generation: None,
        idempotency_key: request.idempotency_key,
        failure_reason: None,
        snapshot_path: None,
        post_recovery_canary: None,
        admission: None,
    }
}

fn with_failure(
    mut job: RecoveryJob,
    failure_reason: RecoveryFailureReason,
    finished_at_millis: u64,
) -> RecoveryJob {
    job.state = if matches!(
        failure_reason,
        RecoveryFailureReason::NotCoordinator
            | RecoveryFailureReason::NotDistributedReady
            | RecoveryFailureReason::Cooldown
            | RecoveryFailureReason::AttemptLimit
            | RecoveryFailureReason::UnknownRecoveryId
    ) {
        RecoveryState::Suppressed
    } else {
        RecoveryState::Failed
    };
    job.phase = if job.state == RecoveryState::Failed {
        RecoveryPhase::Failed
    } else {
        RecoveryPhase::Suppressed
    };
    job.failure_reason = Some(failure_reason);
    job.finished_at_millis = Some(finished_at_millis);
    job
}

fn push_history(inner: &mut RecoveryInner, job: RecoveryJob) {
    inner.history.push_back(job);
    while inner.history.len() > inner.policy.history_limit {
        inner.history.pop_front();
    }
}

fn serialize_timestamp_millis<S>(millis: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format_timestamp_millis(*millis))
}

fn serialize_optional_timestamp_millis<S>(
    millis: &Option<u64>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match millis {
        Some(millis) => serializer.serialize_some(&format_timestamp_millis(*millis)),
        None => serializer.serialize_none(),
    }
}

fn format_timestamp_millis(millis: u64) -> String {
    let seconds = millis / 1_000;
    let milliseconds = millis % 1_000;
    let days = (seconds / 86_400) as i64;
    let seconds_in_day = seconds % 86_400;
    let (year, month, day) = civil_date_from_days(days);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

// Howard Hinnant's proleptic Gregorian calendar conversion, expressed without
// an external time dependency because recovery timestamps are display-only.
fn civil_date_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let shifted_days = days_since_unix_epoch + 719_468;
    let era = if shifted_days >= 0 {
        shifted_days / 146_097
    } else {
        (shifted_days - 146_096) / 146_097
    };
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u64::MAX as u128) as u64
        })
}

#[cfg(test)]
mod service_tests {
    use super::*;

    fn gate(generation: u64) -> RecoveryGate {
        RecoveryGate {
            role: LocalRole::Coordinator,
            mode: StableMode::DistributedLayerParallel,
            state: ClusterState::DistributedReady,
            generation,
        }
    }

    fn request(key: &str) -> RecoveryRequest {
        RecoveryRequest::new(
            RecoveryReason::ThroughputDegraded,
            RecoveryTrigger::LowDecodeTps,
            key,
        )
        .unwrap()
    }

    #[test]
    fn cooldown_and_attempt_limit_are_evaluated_with_deterministic_time() {
        let service = RecoveryService::with_policy(RecoveryPolicy {
            history_limit: 8,
            cooldown: 100,
            window: 1_000,
            max_attempts: 2,
        });

        let first = match service.begin_at(gate(1), request("first"), 0) {
            RecoveryStart::Created(job) => job,
            other => panic!("expected created recovery, got {other:?}"),
        };
        service.mark_failed("does-not-match", 1, RecoveryFailureReason::CanaryFailed);
        service
            .mark_failed(&first.recovery_id, 10, RecoveryFailureReason::CanaryFailed)
            .unwrap();

        let cooldown = service.begin_at(gate(2), request("cooldown"), 50);
        assert!(matches!(
            cooldown,
            RecoveryStart::Suppressed(RecoveryJob {
                failure_reason: Some(RecoveryFailureReason::Cooldown),
                ..
            })
        ));

        let second = match service.begin_at(gate(2), request("second"), 200) {
            RecoveryStart::Created(job) => job,
            other => panic!("expected second recovery, got {other:?}"),
        };
        service
            .mark_failed(
                &second.recovery_id,
                210,
                RecoveryFailureReason::CanaryFailed,
            )
            .unwrap();

        let limit = service.begin_at(gate(3), request("limit"), 400);
        assert!(matches!(
            limit,
            RecoveryStart::Suppressed(RecoveryJob {
                failure_reason: Some(RecoveryFailureReason::AttemptLimit),
                ..
            })
        ));
    }

    #[test]
    fn duplicate_active_event_returns_the_original_owner() {
        let service = RecoveryService::new();
        let first = match service.begin_at(gate(7), request("same"), 100) {
            RecoveryStart::Created(job) => job,
            other => panic!("expected created recovery, got {other:?}"),
        };
        let duplicate = service.begin_at(gate(7), request("different"), 101);
        assert!(matches!(
            duplicate,
            RecoveryStart::Existing(RecoveryJob { recovery_id, .. })
                if recovery_id == first.recovery_id
        ));
    }
}
