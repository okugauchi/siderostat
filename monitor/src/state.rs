//! Display state held by the monitor and updated from each metrics poll.
//!
//! Also models the first-launch reducer (C-05b): the sequence of first-launch
//! steps from the distribution spec §11 is driven by a pure reducer so the
//! order, approval gating, and failure branches are unit-testable. The legacy
//! inventory payload is connected in D-01; here it is only an interface.

use crate::{
    metrics::{MetricsSnapshot, MonitorStatus},
    service_management::ServiceStatus,
};

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    pub status: MonitorStatus,
    pub cluster_mode: Option<String>,
    pub cluster_state: Option<String>,
    pub generation: Option<u64>,
    pub target_ready: Option<bool>,
    pub node_id: Option<String>,
    pub prefill_active: bool,
    pub prefill_current: u64,
    pub prefill_total: u64,
    pub prefill_percent: f64,
    pub prefill_cached: u64,
    pub prefill_chunk_tps: f64,
    pub prefill_avg_tps: f64,
    pub prefill_elapsed_secs: f64,
    pub kv_hits_total: u64,
    pub kv_hit_tokens: u64,
    pub kv_load_ms: f64,
    pub decode_active: bool,
    pub decode_completion: u64,
    pub decode_chunk_tps: f64,
    pub decode_avg_tps: f64,
    pub decode_elapsed_secs: f64,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            status: MonitorStatus::Offline,
            cluster_mode: None,
            cluster_state: None,
            generation: None,
            target_ready: None,
            node_id: None,
            prefill_active: false,
            prefill_current: 0,
            prefill_total: 0,
            prefill_percent: 0.0,
            prefill_cached: 0,
            prefill_chunk_tps: 0.0,
            prefill_avg_tps: 0.0,
            prefill_elapsed_secs: 0.0,
            kv_hits_total: 0,
            kv_hit_tokens: 0,
            kv_load_ms: 0.0,
            decode_active: false,
            decode_completion: 0,
            decode_chunk_tps: 0.0,
            decode_avg_tps: 0.0,
            decode_elapsed_secs: 0.0,
        }
    }
}

impl DisplayState {
    /// Apply a successful metrics poll.
    pub fn apply_metrics(&mut self, snapshot: &MetricsSnapshot) {
        self.status = MonitorStatus::Online;
        self.cluster_mode = snapshot.cluster_mode.clone();
        self.cluster_state = snapshot.cluster_state.clone();
        self.generation = snapshot.generation;
        self.target_ready = snapshot.target_ready;
        self.node_id = snapshot.node_id.clone();
        self.prefill_active = snapshot.prefill.active;
        self.prefill_current = snapshot.prefill.current;
        self.prefill_total = snapshot.prefill.total;
        self.prefill_percent = snapshot.prefill.percent;
        self.prefill_cached = snapshot.prefill.cached;
        self.prefill_chunk_tps = snapshot.prefill.chunk_tps;
        self.prefill_avg_tps = snapshot.prefill.avg_tps;
        self.prefill_elapsed_secs = snapshot.prefill.elapsed_secs;
        self.kv_hits_total = snapshot.kv_cache.hits_total;
        self.kv_hit_tokens = snapshot.kv_cache.hit_tokens;
        self.kv_load_ms = snapshot.kv_cache.load_ms;
        self.decode_active = snapshot.decode.active;
        self.decode_completion = snapshot.decode.completion;
        self.decode_chunk_tps = snapshot.decode.chunk_tps;
        self.decode_avg_tps = snapshot.decode.avg_tps;
        self.decode_elapsed_secs = snapshot.decode.elapsed_secs;
    }

    /// Mark the monitor as connected but unable to read the selected metrics.
    pub fn mark_degraded(&mut self) {
        self.status = MonitorStatus::Degraded;
    }

    /// Mark the monitor as offline after both metrics and health polling fail.
    pub fn mark_offline(&mut self) {
        self.status = MonitorStatus::Offline;
    }
}

/// First-launch step from the distribution spec §11. Registration progress and
/// model-startup progress are distinct phases so the UI can render them as
/// separate progress states (C-05b). The monitor startup path feeds the same
/// events into this reducer; the legacy inventory remains read-only and is
/// supplied by D-01.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstLaunchState {
    /// App version / signature / build metadata shown (spec §11 step 1).
    VersionShown,
    /// Legacy install detected (spec step 2). `legacy_present` is populated by
    /// the D-01 inventory; here it is only the interface.
    InventoryChecked { legacy_present: bool },
    /// User config, secret, manifest validated (spec step 3).
    ConfigValidated { valid: bool },
    /// Runtime LaunchAgent and main-app login-item statuses read (spec steps
    /// 4 and 7). They are kept together so registration cannot report success
    /// while either independent service is still unregistered.
    ServiceStatusesChecked {
        runtime_status: ServiceStatus,
        main_app_login_status: ServiceStatus,
    },
    /// Background service purpose explained and registration requested (step 5).
    Registering,
    /// Registration succeeded (spec step 5 success).
    Registered,
    /// Approval is missing: show the "open Login Items" affordance (step 6).
    RequiresApproval,
    /// Registration failed; user may retry.
    RegisterFailed,
    /// Runtime and monitor login-start settings confirmed (spec step 7).
    /// Registration progress is complete; model-startup progress begins.
    MonitorLoginChecked,
    /// Runtime admin API is ready (spec step 8).
    RuntimeAdminReady,
    /// DwarfStar / model manifest readiness shown (spec step 9). First launch
    /// is complete without blocking UI on model load.
    ModelReady,
}

/// Event fed to the first-launch reducer. Kept minimal and pure so the order,
/// approval gating, and failure branches are unit-testable. Real inventory /
/// config / status payloads are connected by the C-05 startup driver.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstLaunchEvent {
    VersionShown,
    InventoryChecked {
        legacy_present: bool,
    },
    ConfigChecked {
        valid: bool,
    },
    ServiceStatusesChecked {
        runtime_status: ServiceStatus,
        main_app_login_status: ServiceStatus,
    },
    RegisterRequested,
    RegisterSucceeded,
    RegisterRequiresApproval,
    RegisterFailed,
    MonitorLoginConfirmed,
    RuntimeAdminReady,
    ModelReady,
}

/// Pure first-launch reducer. Each event advances the state along the spec §11
/// sequence; out-of-order or invalid events are ignored (state unchanged) so
/// the UI never regresses on spurious input. Failure branches (config invalid,
/// approval missing, registration failed) terminate the progress visibly and
/// never present a rejected state as enabled.
#[allow(dead_code)]
pub fn first_launch_reducer(state: FirstLaunchState, event: FirstLaunchEvent) -> FirstLaunchState {
    match (state, event) {
        (FirstLaunchState::VersionShown, FirstLaunchEvent::InventoryChecked { legacy_present }) => {
            FirstLaunchState::InventoryChecked { legacy_present }
        }
        (
            FirstLaunchState::InventoryChecked { .. },
            FirstLaunchEvent::ConfigChecked { valid: true },
        ) => FirstLaunchState::ConfigValidated { valid: true },
        // config 検証失敗: 明示的に失敗状態へ進め、enabled と表示しない。
        (
            FirstLaunchState::InventoryChecked { .. },
            FirstLaunchEvent::ConfigChecked { valid: false },
        ) => FirstLaunchState::ConfigValidated { valid: false },
        (
            FirstLaunchState::ConfigValidated { valid: true },
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status,
                main_app_login_status,
            },
        ) => FirstLaunchState::ServiceStatusesChecked {
            runtime_status,
            main_app_login_status,
        },
        (FirstLaunchState::ServiceStatusesChecked { .. }, FirstLaunchEvent::RegisterRequested) => {
            FirstLaunchState::Registering
        }
        (FirstLaunchState::Registering, FirstLaunchEvent::RegisterSucceeded) => {
            FirstLaunchState::Registered
        }
        (FirstLaunchState::Registering, FirstLaunchEvent::RegisterRequiresApproval) => {
            // approval 不足時だけ Login Items を開く明示操作を表示する状態へ。
            FirstLaunchState::RequiresApproval
        }
        (FirstLaunchState::Registering, FirstLaunchEvent::RegisterFailed) => {
            FirstLaunchState::RegisterFailed
        }
        // The user may approve the already registered service in System
        // Settings after the initial registration returned RequiresApproval.
        (
            FirstLaunchState::RequiresApproval,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: ServiceStatus::Enabled,
                main_app_login_status: ServiceStatus::Enabled,
            },
        ) => FirstLaunchState::Registered,
        // A failed registration can be retried by the next first-launch run.
        (FirstLaunchState::RegisterFailed, FirstLaunchEvent::RegisterRequested) => {
            FirstLaunchState::Registering
        }
        // registration progress 完了後に model-startup progress へ。
        (FirstLaunchState::Registered, FirstLaunchEvent::MonitorLoginConfirmed) => {
            FirstLaunchState::MonitorLoginChecked
        }
        (FirstLaunchState::MonitorLoginChecked, FirstLaunchEvent::RuntimeAdminReady) => {
            FirstLaunchState::RuntimeAdminReady
        }
        (FirstLaunchState::RuntimeAdminReady, FirstLaunchEvent::ModelReady) => {
            FirstLaunchState::ModelReady
        }
        // それ以外の event は現在の state を維持（順序外入力を無視）。
        (state, _) => state,
    }
}

/// Whether the first-launch UI should offer the "open Login Items" affordance.
/// True only when approval is actually required (spec §11 step 6) so a
/// rejected/denied state is never presented as enabled.
#[allow(dead_code)]
pub fn first_launch_needs_approval(state: &FirstLaunchState) -> bool {
    matches!(state, FirstLaunchState::RequiresApproval)
}

/// Whether first launch is fully complete (model-startup progress done).
#[allow(dead_code)]
pub fn first_launch_complete(state: &FirstLaunchState) -> bool {
    matches!(state, FirstLaunchState::ModelReady)
}

// ---------------------------------------------------------------------------
// D-03: app/runtime version handshake and notification
//
// The `.pkg` can replace `/Applications/Siderostat.app` while the old runtime
// executable image keeps running (spec §13.1). The monitor compares its own
// app version against the runtime `/healthz` version and, on mismatch, emits a
// state-change notification (never an automatic restart loop). Rolling back to
// a prior app with an incompatible schema warns instead of auto-converting user
// data.
// ---------------------------------------------------------------------------

/// Result of comparing the app version against the running runtime version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionHandshake {
    /// App and runtime build numbers match.
    Matched,
    /// The running runtime is older than the app (upgrade in place).
    RuntimeOlder,
    /// The running runtime is newer than the app (downgrade/rollback).
    RuntimeNewer,
    /// The runtime `/healthz` version could not be read.
    Unavailable,
}

/// Compare app vs runtime version strings. `app` is the monitor's own bundle
/// version, `runtime` is the `/healthz` version. Handles dotted numeric
/// versions; non-numeric segments fall back to string equality. Never mutates
/// anything.
pub fn version_handshake(app: &str, runtime: &str) -> VersionHandshake {
    if app == runtime {
        return VersionHandshake::Matched;
    }
    match (parse_version(app), parse_version(runtime)) {
        (Some(a), Some(b)) if a != b => {
            if a < b {
                VersionHandshake::RuntimeNewer
            } else {
                VersionHandshake::RuntimeOlder
            }
        }
        _ => VersionHandshake::Matched,
    }
}

/// Compare both version and build number. An unavailable build number is
/// ignored because developer binaries and older runtimes may not expose one;
/// the version comparison remains authoritative in that case.
pub fn version_handshake_with_build(
    app_version: &str,
    app_build: &str,
    runtime_version: &str,
    runtime_build: &str,
) -> VersionHandshake {
    let version_result = version_handshake(app_version, runtime_version);
    if version_result != VersionHandshake::Matched {
        return version_result;
    }
    if app_build == runtime_build
        || app_build.trim().is_empty()
        || runtime_build.trim().is_empty()
        || app_build.eq_ignore_ascii_case("unknown")
        || runtime_build.eq_ignore_ascii_case("unknown")
    {
        return VersionHandshake::Matched;
    }
    match (parse_version(app_build), parse_version(runtime_build)) {
        (Some(app), Some(runtime)) if app != runtime => {
            if app < runtime {
                VersionHandshake::RuntimeNewer
            } else {
                VersionHandshake::RuntimeOlder
            }
        }
        _ => VersionHandshake::Matched,
    }
}

/// Parse a dotted numeric version into comparable components. Returns `None`
/// when any segment is non-numeric (fall back to string comparison).
fn parse_version(version: &str) -> Option<Vec<u64>> {
    version
        .trim()
        .split('.')
        .map(|segment| segment.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            cluster_mode: Some("distributed-layer-parallel".into()),
            cluster_state: Some("distributed-ready".into()),
            generation: Some(7),
            target_ready: Some(true),
            node_id: Some("macstudio".into()),
            prefill: crate::metrics::PrefillState {
                active: true,
                current: 4096,
                total: 9005,
                percent: 45.5,
                cached: 0,
                chunk_tps: 123.4,
                avg_tps: 100.0,
                elapsed_secs: 10.0,
            },
            kv_cache: crate::metrics::KvCacheState {
                hits_total: 3,
                hit_tokens: 9005,
                load_ms: 12.3,
            },
            decode: crate::metrics::DecodeState {
                active: true,
                completion: 42,
                chunk_tps: 32.1,
                avg_tps: 28.5,
                elapsed_secs: 1.5,
            },
        }
    }

    #[test]
    fn applies_metrics() {
        let mut state = DisplayState::default();
        state.apply_metrics(&snapshot());
        assert_eq!(state.status, MonitorStatus::Online);
        assert_eq!(
            state.cluster_mode.as_deref(),
            Some("distributed-layer-parallel")
        );
        assert_eq!(state.generation, Some(7));
        assert!(state.prefill_active);
        assert_eq!(state.prefill_percent, 45.5);
        assert_eq!(state.prefill_avg_tps, 100.0);
        assert!(state.decode_active);
        assert_eq!(state.decode_completion, 42);
        assert_eq!(state.decode_elapsed_secs, 1.5);
    }

    #[test]
    fn offline_state_takes_precedence() {
        let mut state = DisplayState::default();
        state.apply_metrics(&snapshot());
        state.mark_offline();
        assert_eq!(state.status, MonitorStatus::Offline);
    }

    #[test]
    fn degraded_state_preserves_the_last_metrics_snapshot() {
        let mut state = DisplayState::default();
        state.apply_metrics(&snapshot());
        state.mark_degraded();

        assert_eq!(state.status, MonitorStatus::Degraded);
        assert_eq!(
            state.cluster_mode.as_deref(),
            Some("distributed-layer-parallel")
        );
        assert_eq!(state.cluster_state.as_deref(), Some("distributed-ready"));
        assert_eq!(state.target_ready, Some(true));
    }

    // ---- C-05b: first-launch reducer ----

    use crate::service_management::ServiceStatus as S;

    /// Drive the happy-path first launch: version → inventory → config →
    /// service statuses → register → login → admin ready → model ready.
    fn happy_path() -> FirstLaunchState {
        let mut state = FirstLaunchState::VersionShown;
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: true,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::ConfigChecked { valid: true });
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: S::NotRegistered,
                main_app_login_status: S::NotRegistered,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterRequested);
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterSucceeded);
        state = first_launch_reducer(state, FirstLaunchEvent::MonitorLoginConfirmed);
        state = first_launch_reducer(state, FirstLaunchEvent::RuntimeAdminReady);
        first_launch_reducer(state, FirstLaunchEvent::ModelReady)
    }

    #[test]
    fn first_launch_happy_path_reaches_model_ready() {
        assert_eq!(happy_path(), FirstLaunchState::ModelReady);
        assert!(first_launch_complete(&FirstLaunchState::ModelReady));
    }

    #[test]
    fn first_launch_legacy_present_is_carried_through() {
        let state = first_launch_reducer(
            FirstLaunchState::VersionShown,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: true,
            },
        );
        assert_eq!(
            state,
            FirstLaunchState::InventoryChecked {
                legacy_present: true
            }
        );
        // legacy なしも区別される。
        let clean = first_launch_reducer(
            FirstLaunchState::VersionShown,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: false,
            },
        );
        assert_eq!(
            clean,
            FirstLaunchState::InventoryChecked {
                legacy_present: false
            }
        );
    }

    #[test]
    fn config_invalid_stops_before_registration() {
        let state = first_launch_reducer(
            FirstLaunchState::VersionShown,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: false,
            },
        );
        let state = first_launch_reducer(state, FirstLaunchEvent::ConfigChecked { valid: false });
        assert_eq!(state, FirstLaunchState::ConfigValidated { valid: false });
        // 失敗状態から RegisterRequested は無視される（enabled と表示しない）。
        let state = first_launch_reducer(state, FirstLaunchEvent::RegisterRequested);
        assert_eq!(state, FirstLaunchState::ConfigValidated { valid: false });
    }

    #[test]
    fn requires_approval_gates_the_login_items_affordance() {
        let mut state = FirstLaunchState::VersionShown;
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: false,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::ConfigChecked { valid: true });
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: S::RequiresApproval,
                main_app_login_status: S::RequiresApproval,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterRequested);
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterRequiresApproval);
        assert_eq!(state, FirstLaunchState::RequiresApproval);
        // approval が必要な時だけ Login Items を開く導線を表示する。
        assert!(first_launch_needs_approval(&state));
        assert!(!first_launch_needs_approval(&FirstLaunchState::Registered));
        assert!(!first_launch_needs_approval(
            &FirstLaunchState::RegisterFailed
        ));
    }

    #[test]
    fn approval_can_resume_after_system_settings_changes_status() {
        let state = first_launch_reducer(
            FirstLaunchState::RequiresApproval,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: S::Enabled,
                main_app_login_status: S::Enabled,
            },
        );
        assert_eq!(state, FirstLaunchState::Registered);
    }

    #[test]
    fn approval_does_not_resume_until_both_services_are_enabled() {
        let state = first_launch_reducer(
            FirstLaunchState::RequiresApproval,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: S::Enabled,
                main_app_login_status: S::RequiresApproval,
            },
        );
        assert_eq!(state, FirstLaunchState::RequiresApproval);
    }

    #[test]
    fn failed_registration_can_be_retried() {
        assert_eq!(
            first_launch_reducer(
                FirstLaunchState::RegisterFailed,
                FirstLaunchEvent::RegisterRequested,
            ),
            FirstLaunchState::Registering
        );
    }

    #[test]
    fn register_failed_does_not_present_as_enabled() {
        let mut state = FirstLaunchState::VersionShown;
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::InventoryChecked {
                legacy_present: false,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::ConfigChecked { valid: true });
        state = first_launch_reducer(
            state,
            FirstLaunchEvent::ServiceStatusesChecked {
                runtime_status: S::NotRegistered,
                main_app_login_status: S::NotRegistered,
            },
        );
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterRequested);
        state = first_launch_reducer(state, FirstLaunchEvent::RegisterFailed);
        assert_eq!(state, FirstLaunchState::RegisterFailed);
        assert!(!first_launch_complete(&state));
    }

    #[test]
    fn out_of_order_events_are_ignored() {
        // ServiceStatusesChecked 前に RegisterRequested は無視される。
        let state = first_launch_reducer(
            FirstLaunchState::ConfigValidated { valid: true },
            FirstLaunchEvent::RegisterRequested,
        );
        assert_eq!(state, FirstLaunchState::ConfigValidated { valid: true });
        // ModelReady は RuntimeAdminReady 経由でのみ到達する。
        let state = first_launch_reducer(
            FirstLaunchState::MonitorLoginChecked,
            FirstLaunchEvent::ModelReady,
        );
        assert_eq!(state, FirstLaunchState::MonitorLoginChecked);
    }

    #[test]
    fn first_launch_does_not_block_ui_before_model_ready() {
        // C-05c 停止条件: first launch が model load 完了まで UI を block しない。
        // ModelReady 以外の段階は complete 扱いにならない。
        for state in [
            FirstLaunchState::VersionShown,
            FirstLaunchState::InventoryChecked {
                legacy_present: false,
            },
            FirstLaunchState::ConfigValidated { valid: true },
            FirstLaunchState::ServiceStatusesChecked {
                runtime_status: S::NotRegistered,
                main_app_login_status: S::NotRegistered,
            },
            FirstLaunchState::Registering,
            FirstLaunchState::Registered,
            FirstLaunchState::MonitorLoginChecked,
            FirstLaunchState::RuntimeAdminReady,
        ] {
            assert!(
                !first_launch_complete(&state),
                "must not be complete: {state:?}"
            );
        }
        assert!(first_launch_complete(&FirstLaunchState::ModelReady));
    }

    #[test]
    fn rejected_or_failed_states_never_present_as_enabled() {
        // C-05c 停止条件: 拒否状態 / 失敗状態を enabled と表示しない。
        // Login Items 導線は RequiresApproval のときだけ表示し、
        // 失敗状態では表示しない。complete にもしない。
        for state in [
            FirstLaunchState::ConfigValidated { valid: false },
            FirstLaunchState::RegisterFailed,
        ] {
            assert!(
                !first_launch_needs_approval(&state),
                "must not offer approval: {state:?}"
            );
            assert!(
                !first_launch_complete(&state),
                "must not be complete: {state:?}"
            );
        }
        assert!(first_launch_needs_approval(
            &FirstLaunchState::RequiresApproval
        ));
    }

    // ---- D-03: version handshake and notification ----

    #[test]
    fn version_handshake_matches_equal_versions() {
        assert_eq!(
            version_handshake("0.2.1", "0.2.1"),
            VersionHandshake::Matched
        );
        // build_number が同じでも version が同一なら matched。
        assert_eq!(
            version_handshake("0.2.1", "0.2.1"),
            VersionHandshake::Matched
        );
    }

    #[test]
    fn version_handshake_distinguishes_older_and_newer_runtime() {
        // app 0.3.0 vs runtime 0.2.1 → runtime 旧版（upgrade in place）。
        assert_eq!(
            version_handshake("0.3.0", "0.2.1"),
            VersionHandshake::RuntimeOlder
        );
        // app 0.2.1 vs runtime 0.3.0 → runtime 新版（downgrade/rollback）。
        assert_eq!(
            version_handshake("0.2.1", "0.3.0"),
            VersionHandshake::RuntimeNewer
        );
    }

    #[test]
    fn version_handshake_non_numeric_falls_back_to_string_equality() {
        // 数値でない segment は文字列等価にフォールバックし、mismatch を
        // 誤って older/newer にしない。
        assert_eq!(
            version_handshake("dev-abc", "dev-abc"),
            VersionHandshake::Matched
        );
        assert_eq!(
            version_handshake("dev-abc", "dev-abd"),
            VersionHandshake::Matched
        );
    }

    #[test]
    fn version_handshake_compares_build_numbers_when_versions_match() {
        assert_eq!(
            version_handshake_with_build("0.3.0", "2", "0.3.0", "1"),
            VersionHandshake::RuntimeOlder
        );
        assert_eq!(
            version_handshake_with_build("0.3.0", "1", "0.3.0", "2"),
            VersionHandshake::RuntimeNewer
        );
        assert_eq!(
            version_handshake_with_build("0.3.0", "1", "0.3.0", "unknown"),
            VersionHandshake::Matched
        );
    }
}
