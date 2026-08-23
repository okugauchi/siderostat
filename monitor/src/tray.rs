//! tray-icon based menu bar UI for the siderostat monitor.
//!
//! The tray icon is created and updated on the main thread (macOS AppKit
//! requirement). The menu reflects the current `DisplayState`; the polling
//! task shares state through the mutex passed to `update`. The registration
//! status display and action enablement are refreshed from the platform service
//! state on the AppKit main thread.
#![allow(dead_code)]

use crate::{
    config::LiveMetric,
    localization::{app_metadata, text},
    metrics::MonitorStatus,
    operation::OperationState,
    service_management::ServiceStatus,
    state::{DisplayState, FirstLaunchState},
};
use anyhow::{Context, Result};
use std::cell::Cell;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const MENU_QUIT: &str = "quit";
const MENU_RUNTIME_RESTART: &str = "runtime-restart";
const MENU_BG_TOGGLE: &str = "bg-toggle";
const MENU_OPEN_CONFIG: &str = "open-config";
const MENU_OPEN_LOGIN_ITEMS: &str = "open-login-items";

/// Icon drawing colors.
const GREEN: [u8; 4] = [0x2e, 0xcc, 0x71, 0xff]; // operating state
const RED: [u8; 4] = [0xe7, 0x4c, 0x3c, 0xff]; // non-operating state
const LINE: [u8; 4] = [0x3f, 0x3f, 0x3f, 0xff]; // connector line

const ICON_SIZE: u32 = 18;
const ICON_SUB: f32 = 4.0; // supersample factor for smooth circles
const PROGRESS_STALL_AGE_SECS: f64 = 60.0;

pub struct MonitorTray {
    _tray: TrayIcon,
    header: MenuItem,
    mode: MenuItem,
    state: MenuItem,
    generation: MenuItem,
    target: MenuItem,
    prefill: MenuItem,
    kv_cache: MenuItem,
    decode: MenuItem,
    runtime_status: MenuItem,
    login_status: MenuItem,
    first_launch: MenuItem,
    operation: MenuItem,
    show_decode_tps: bool,
    live_metric: LiveMetric,
    _separator: PredefinedMenuItem,
    _open_config: MenuItem,
    _open_login_items: MenuItem,
    _runtime_restart: MenuItem,
    _bg_toggle: MenuItem,
    _quit: MenuItem,
    runtime_service_status: Cell<ServiceStatus>,
    operation_busy: Cell<bool>,
}

impl MonitorTray {
    /// Build the tray icon and its static menu structure.
    pub fn new(show_decode_tps: bool, live_metric: LiveMetric) -> Result<Self> {
        let icon = icon_for(None, true)?;
        let header = MenuItem::new(text("app.name", "Siderostat"), false, None);
        let mode = MenuItem::new("Mode: --", false, None);
        let state = MenuItem::new("State: --", false, None);
        let generation = MenuItem::new("Gen: --", false, None);
        let target = MenuItem::new("Target: --", false, None);
        let prefill = MenuItem::new("Prefill: --", false, None);
        let kv_cache = MenuItem::new("KV cache: --", false, None);
        let decode = MenuItem::new("Decode: --", false, None);
        let runtime_status = MenuItem::new(
            text(
                "status.runtime_autostart.pending",
                "siderostat-runtime自動起動: --",
            ),
            false,
            None,
        );
        let login_status = MenuItem::new(
            text(
                "status.menu_bar_autostart.pending",
                "Siderostat メニューバー自動起動: --",
            ),
            false,
            None,
        );
        let first_launch = MenuItem::new(
            first_launch_status_text(&FirstLaunchState::VersionShown),
            false,
            None,
        );
        let operation = MenuItem::new(text("operation.idle", "操作: 待機中"), false, None);
        let open_config = MenuItem::with_id(
            MENU_OPEN_CONFIG,
            text("menu.settings", "設定ファイルを開く"),
            true,
            None,
        );
        let open_login_items = MenuItem::with_id(
            MENU_OPEN_LOGIN_ITEMS,
            text("menu.login_items", "ログイン項目を開く"),
            true,
            None,
        );
        let runtime_restart = MenuItem::with_id(
            MENU_RUNTIME_RESTART,
            text("menu.restart_runtime", "siderostat-runtimeを再起動"),
            true,
            None,
        );
        let bg_toggle = MenuItem::with_id(
            MENU_BG_TOGGLE,
            text(
                "menu.background_enable",
                "siderostat-runtimeを起動して自動起動を有効化",
            ),
            true,
            None,
        );
        let quit = MenuItem::with_id(MENU_QUIT, text("menu.quit", "Siderostatを終了"), true, None);

        let menu = Menu::new();
        menu.append(&header)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&mode)?;
        menu.append(&state)?;
        menu.append(&generation)?;
        menu.append(&target)?;
        let separator = PredefinedMenuItem::separator();
        menu.append(&separator)?;
        menu.append(&prefill)?;
        menu.append(&kv_cache)?;
        menu.append(&decode)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&runtime_status)?;
        menu.append(&login_status)?;
        menu.append(&first_launch)?;
        menu.append(&operation)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&open_config)?;
        menu.append(&runtime_restart)?;
        menu.append(&bg_toggle)?;
        menu.append(&open_login_items)?;
        menu.append(&quit)?;

        let tray = TrayIconBuilder::new()
            .with_tooltip(text("app.name", "Siderostat"))
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .context("build tray icon")?;

        Ok(Self {
            _tray: tray,
            header,
            mode,
            state,
            generation,
            target,
            prefill,
            kv_cache,
            decode,
            runtime_status,
            login_status,
            first_launch,
            operation,
            show_decode_tps,
            live_metric,
            _separator: separator,
            _open_config: open_config,
            _open_login_items: open_login_items,
            _runtime_restart: runtime_restart,
            _bg_toggle: bg_toggle,
            _quit: quit,
            runtime_service_status: Cell::new(ServiceStatus::NotRegistered),
            operation_busy: Cell::new(false),
        })
    }

    /// Update the tray icon, title, tooltip, and menu texts from the display
    /// state. The cluster mode is conveyed by the icon drawing (two circles and
    /// an optional connector), never by a text abbreviation.
    pub fn update(&self, display: &DisplayState) {
        self.header.set_text(display.node_id.as_ref().map_or_else(
            || "node_id: siderostat".to_string(),
            |node_id| format!("node_id: {node_id}"),
        ));

        if let Ok(icon) = icon_for(
            display.cluster_mode.as_deref(),
            display.status == MonitorStatus::Offline,
        ) {
            let _ = self._tray.set_icon(Some(icon));
        }

        if display.status == MonitorStatus::Offline {
            self.mode.set_text("Mode: --");
            self.state
                .set_text(text("status.offline", "siderostat-runtimeに接続できません"));
            self.generation.set_text("Gen: --");
            self.target.set_text("Target: --");
            self.prefill.set_text("Prefill: --");
            self.kv_cache.set_text("KV cache: --");
            self.decode.set_text("Decode: --");
            self._tray.set_title(Some("offline"));
            let _ = self._tray.set_tooltip(Some(text(
                "status.offline",
                "siderostat-runtimeに接続できません",
            )));
            return;
        }

        self.mode.set_text(format!(
            "Mode: {}",
            mode_display_name(display.cluster_mode.as_deref())
        ));
        let state_text = state_display_name(display.cluster_state.as_deref());
        self.state
            .set_text(if display.status == MonitorStatus::Degraded {
                format!(
                    "State: {state_text} / {}",
                    text(
                        "status.metrics_unavailable",
                        "siderostat-runtimeに接続済み、metricsを取得できません",
                    )
                )
            } else {
                format!("State: {state_text}")
            });
        self.generation.set_text(format!(
            "Gen: {}",
            display
                .generation
                .map_or("--".to_string(), |g| g.to_string())
        ));
        self.target.set_text(format!(
            "Target: {}",
            display
                .target_ready
                .map_or("--".to_string(), |ready| if ready {
                    "ready".to_string()
                } else {
                    "not-ready".to_string()
                })
        ));

        self.prefill.set_text(prefill_detail_text(display));

        if display.kv_hits_total > 0 {
            self.kv_cache.set_text(format!(
                "KV cache: hits={} tokens={} load={:.1}ms",
                display.kv_hits_total, display.kv_hit_tokens, display.kv_load_ms
            ));
        } else {
            self.kv_cache.set_text("KV cache: --");
        }

        if self.show_decode_tps && has_decode_values(display) {
            self.decode.set_text(decode_detail_text(display));
        } else {
            self.decode.set_text("Decode: --");
        }
        self._tray.set_title(Some(menu_bar_title(
            display,
            self.show_decode_tps,
            self.live_metric,
        )));
        let tooltip = if display.status == MonitorStatus::Degraded {
            format!(
                "siderostat node={} state={} / {}",
                display.node_id.as_deref().unwrap_or("--"),
                state_text,
                text(
                    "status.metrics_unavailable",
                    "siderostat-runtimeに接続済み、metricsを取得できません",
                )
            )
        } else {
            format!(
                "siderostat node={} state={state_text}",
                display.node_id.as_deref().unwrap_or("--")
            )
        };
        let _ = self._tray.set_tooltip(Some(tooltip));
    }

    /// Check whether a menu event requests the monitor to quit.
    pub fn is_quit_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_QUIT)
    }

    /// Check whether a menu event requests a siderostat runtime restart.
    pub fn is_runtime_restart_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_RUNTIME_RESTART)
    }

    /// Check whether a menu event requests toggling background execution.
    pub fn is_bg_toggle_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_BG_TOGGLE)
    }

    /// Check whether a menu event requests opening the runtime configuration.
    pub fn is_open_config_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_OPEN_CONFIG)
    }

    /// Check whether a menu event requests opening System Settings Login Items
    /// (approval affordance, C-05c).
    pub fn is_open_login_items_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_OPEN_LOGIN_ITEMS)
    }

    /// Update the background-service registration status display. The runtime
    /// LaunchAgent helper and the main-app login item are independent settings
    /// (C-03): updating one must never change the other.
    pub fn update_registration(&self, runtime: ServiceStatus, login_item: ServiceStatus) {
        // The menu item occupies one fixed position; only its action changes.
        self._bg_toggle.set_text(background_action_text(runtime));
        self.runtime_status.set_text(registration_status_text(
            &text("status.runtime_autostart", "siderostat-runtime自動起動"),
            runtime,
        ));
        self.login_status.set_text(registration_status_text(
            &text("status.menu_bar_autostart", "Siderostat自動起動"),
            login_item,
        ));
        self.runtime_service_status.set(runtime);
        self.refresh_action_state();
    }

    /// Update the user-visible status and prevent overlapping mutating actions.
    pub fn update_operation(&self, operation: &OperationState) {
        self.operation.set_text(operation.menu_text());
        self.operation_busy.set(operation.is_busy());
        self.refresh_action_state();
    }

    /// Update the first-launch progress line. This is a non-interactive menu
    /// item so the AppKit loop remains usable while registration and readiness
    /// checks continue asynchronously.
    pub fn update_first_launch(&self, state: &FirstLaunchState) {
        self.first_launch.set_text(first_launch_status_text(state));
    }

    /// Enable only the actions that are valid for the current service state.
    /// `SMAppService` status is copied into this tray object on the main thread,
    /// so this method only performs menu-item updates.
    fn refresh_action_state(&self) {
        let runtime_status = self.runtime_service_status.get();
        let actions = action_state(runtime_status, self.operation_busy.get());

        self._open_config.set_enabled(actions.open_config);
        self._runtime_restart.set_enabled(actions.runtime_restart);
        self._bg_toggle.set_enabled(actions.background_toggle);
        self._open_login_items.set_enabled(actions.open_login_items);
    }
}

/// Human-readable menu text for a registration status. The status name is
/// stable snake_case; the label prefix is a fixed UI noun.
fn registration_status_text(label: &str, status: ServiceStatus) -> String {
    format!("{label}: {}", service_status_text(status))
}

fn service_status_text(status: ServiceStatus) -> String {
    match status {
        ServiceStatus::Enabled => text("status.enabled", "有効"),
        ServiceStatus::NotRegistered => text("status.off", "無効"),
        ServiceStatus::RequiresApproval => text("status.approval_needed", "承認が必要"),
        ServiceStatus::NotFound => text("status.not_found", "未検出"),
        ServiceStatus::Error => text("status.error", "エラー"),
    }
}

fn first_launch_status_text(state: &FirstLaunchState) -> String {
    match state {
        FirstLaunchState::VersionShown => format!(
            "{} ({})",
            text(
                "first_launch.version",
                "初回起動: Siderostatのバージョン情報を確認しました",
            ),
            app_metadata()
        ),
        FirstLaunchState::InventoryChecked { legacy_present } => {
            let key = if *legacy_present {
                "first_launch.inventory_legacy"
            } else {
                "first_launch.inventory_clean"
            };
            let fallback = if *legacy_present {
                "初回起動: 旧インストールを検出しました"
            } else {
                "初回起動: 旧インストールはありません"
            };
            text(key, fallback)
        }
        FirstLaunchState::ConfigValidated { valid: true } => text(
            "first_launch.config_valid",
            "初回起動: 設定・秘密情報・manifestを確認しました",
        ),
        FirstLaunchState::ConfigValidated { valid: false } => text(
            "first_launch.config_invalid",
            "初回起動: 設定の検証に失敗しました",
        ),
        FirstLaunchState::ServiceStatusesChecked {
            runtime_status,
            main_app_login_status,
        } => format!(
            "{} (siderostat-runtime: {}; Siderostat: {})",
            text(
                "first_launch.service_statuses",
                "初回起動: 自動起動の状態を確認しました",
            ),
            service_status_text(*runtime_status),
            service_status_text(*main_app_login_status)
        ),
        FirstLaunchState::Registering => text(
            "first_launch.registering",
            "初回起動: siderostat-runtimeとSiderostatの自動起動を登録中…",
        ),
        FirstLaunchState::Registered => text(
            "first_launch.registered",
            "初回起動: siderostat-runtimeとSiderostatの自動起動を登録しました",
        ),
        FirstLaunchState::RequiresApproval => text(
            "first_launch.approval",
            "初回起動: siderostat-runtimeまたはSiderostatのログイン項目の承認が必要です",
        ),
        FirstLaunchState::RegisterFailed => text(
            "first_launch.register_failed",
            "初回起動: siderostat-runtimeまたはSiderostatの自動起動の登録に失敗しました",
        ),
        FirstLaunchState::MonitorLoginChecked => text(
            "first_launch.monitor_login",
            "初回起動: Siderostatの自動起動を確認しました",
        ),
        FirstLaunchState::RuntimeAdminReady => text(
            "first_launch.runtime_admin",
            "初回起動: siderostat-runtime APIの準備を確認しました",
        ),
        FirstLaunchState::ModelReady => text(
            "first_launch.model_ready",
            "初回起動: ds4-serverのモデル準備を確認しました",
        ),
    }
}

fn background_action_text(status: ServiceStatus) -> String {
    if status == ServiceStatus::Enabled {
        text(
            "menu.background_disable",
            "siderostat-runtimeを停止して自動起動を無効化",
        )
    } else {
        text(
            "menu.background_enable",
            "siderostat-runtimeを起動して自動起動を有効化",
        )
    }
}

/// Convert stable-mode machine names into the canonical user-visible names.
/// Unknown values remain visible for forward compatibility and diagnostics.
fn mode_display_name(mode: Option<&str>) -> String {
    match mode {
        Some("solo-standalone") => "Solo Standalone".to_string(),
        Some("paired-standalone") => "Paired Standalone".to_string(),
        Some("distributed-layer-parallel") | Some("distributed-mxfp4") => {
            "Distributed (layer-parallel)".to_string()
        }
        Some(other) => other.to_string(),
        None => "--".to_string(),
    }
}

/// Convert lifecycle-state machine names into concise descriptions that keep
/// the current execution topology explicit.
fn state_display_name(state: Option<&str>) -> String {
    match state {
        Some("booting") => "Booting".to_string(),
        Some("solo-standalone-starting") => "Solo Standalone starting".to_string(),
        Some("solo-standalone-ready") => "Solo Standalone ready".to_string(),
        Some("pairing") => "Pairing".to_string(),
        Some("paired-standalone-ready") => "Paired Standalone ready".to_string(),
        Some("awaiting-worker-hello") => "Awaiting worker hello".to_string(),
        Some("promoting") => "Promoting to Distributed (layer-parallel)".to_string(),
        Some("distributed-starting") => "Distributed (layer-parallel) starting".to_string(),
        Some("distributed-ready") => "Distributed (layer-parallel) ready".to_string(),
        Some("demoting") => "Demoting to Paired Standalone".to_string(),
        Some("backoff") => "Backoff".to_string(),
        Some("manual-intervention-required") => "Manual intervention required".to_string(),
        Some(other) => other.to_string(),
        None => "--".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActionState {
    open_config: bool,
    runtime_restart: bool,
    background_toggle: bool,
    open_login_items: bool,
}

fn action_state(runtime_status: ServiceStatus, operation_busy: bool) -> ActionState {
    ActionState {
        open_config: !operation_busy,
        runtime_restart: !operation_busy && runtime_status == ServiceStatus::Enabled,
        background_toggle: !operation_busy
            && matches!(
                runtime_status,
                ServiceStatus::NotRegistered
                    | ServiceStatus::Enabled
                    | ServiceStatus::RequiresApproval
            ),
        open_login_items: !operation_busy,
    }
}

fn selected_metric_title(
    display: &DisplayState,
    show_decode_tps: bool,
    live_metric: LiveMetric,
) -> String {
    match live_metric {
        LiveMetric::PrefillPercent if display.prefill_active && display.prefill_total > 0 => {
            format!("prefill {:.0}%", display.prefill_percent)
        }
        LiveMetric::PrefillChunkTps if display.prefill_active && display.prefill_total > 0 => {
            prefill_tps_title(display, false)
        }
        LiveMetric::PrefillAvgTps if display.prefill_active && display.prefill_total > 0 => {
            prefill_tps_title(display, true)
        }
        LiveMetric::PrefillElapsed
            if display.prefill_active
                && display.prefill_total > 0
                && display.prefill_elapsed_secs > 0.0 =>
        {
            format!("prefill {:.1}s", display.prefill_elapsed_secs)
        }
        LiveMetric::DecodeChunkTps if show_decode_tps && has_decode_values(display) => {
            decode_tps_title(display, false)
        }
        LiveMetric::DecodeAvgTps if show_decode_tps && has_decode_values(display) => {
            decode_tps_title(display, true)
        }
        LiveMetric::DecodeElapsed
            if show_decode_tps && display.decode_active && display.decode_elapsed_secs > 0.0 =>
        {
            format!("decode {:.1}s", display.decode_elapsed_secs)
        }
        LiveMetric::KvCache if display.kv_hits_total > 0 => {
            format!("KV {}t/{:.1}ms", display.kv_hit_tokens, display.kv_load_ms)
        }
        LiveMetric::None
        | LiveMetric::PrefillPercent
        | LiveMetric::PrefillChunkTps
        | LiveMetric::PrefillAvgTps
        | LiveMetric::PrefillElapsed
        | LiveMetric::DecodeChunkTps
        | LiveMetric::DecodeAvgTps
        | LiveMetric::DecodeElapsed
        | LiveMetric::KvCache => String::new(),
    }
}

fn menu_bar_title(
    display: &DisplayState,
    show_decode_tps: bool,
    live_metric: LiveMetric,
) -> String {
    let selected = selected_metric_title(display, show_decode_tps, live_metric);
    if !selected.is_empty() {
        return selected;
    }

    // Keep the menu bar useful when the configured prefill metric is no longer
    // active: switch to the currently active decode throughput instead of
    // leaving the last prefill-only title or an empty title on screen.
    if !matches!(live_metric, LiveMetric::None) && show_decode_tps && has_decode_values(display) {
        return decode_throughput_title(display);
    }
    String::new()
}

fn has_decode_values(display: &DisplayState) -> bool {
    // An active generation with no observed token is still meaningful: the
    // user needs to see that the first-token wait is in progress rather than
    // inherit a stale value from the previous request.
    display.decode_active
}

fn decode_throughput_title(display: &DisplayState) -> String {
    if !display.decode_active {
        String::new()
    } else if !display.decode_progress_observed {
        "decode waiting".to_string()
    } else if progress_stalled(display.decode_progress_age_secs) {
        "decode stalled".to_string()
    } else if !progress_age_available(display.decode_progress_age_secs) {
        "decode progress age unavailable".to_string()
    } else if display.decode_chunk_tps > 0.0 {
        format!("decode {:.1}t/s", display.decode_chunk_tps)
    } else if display.decode_avg_tps > 0.0 {
        format!("decode avg {:.1}t/s", display.decode_avg_tps)
    } else {
        String::new()
    }
}

fn progress_stalled(age_secs: Option<f64>) -> bool {
    age_secs.is_some_and(|age| age.is_finite() && age >= PROGRESS_STALL_AGE_SECS)
}

fn progress_age_available(age_secs: Option<f64>) -> bool {
    age_secs.is_some_and(|age| age.is_finite() && age >= 0.0)
}

fn prefill_tps_title(display: &DisplayState, average: bool) -> String {
    if progress_stalled(display.prefill_progress_age_secs) {
        return "prefill stalled".to_string();
    }
    if !progress_age_available(display.prefill_progress_age_secs) {
        return "prefill progress age unavailable".to_string();
    }

    if average {
        if display.prefill_avg_tps > 0.0 {
            format!("prefill avg {:.1}t/s", display.prefill_avg_tps)
        } else {
            String::new()
        }
    } else {
        if display.prefill_chunk_tps > 0.0 {
            format!("prefill {:.1}t/s", display.prefill_chunk_tps)
        } else {
            String::new()
        }
    }
}

fn decode_tps_title(display: &DisplayState, average: bool) -> String {
    if !display.decode_progress_observed {
        return "decode waiting".to_string();
    }
    if progress_stalled(display.decode_progress_age_secs) {
        return "decode stalled".to_string();
    }
    if !progress_age_available(display.decode_progress_age_secs) {
        return "decode progress age unavailable".to_string();
    }

    if average {
        if display.decode_avg_tps > 0.0 {
            format!("decode avg {:.1}t/s", display.decode_avg_tps)
        } else {
            String::new()
        }
    } else {
        if display.decode_chunk_tps > 0.0 {
            format!("decode {:.1}t/s", display.decode_chunk_tps)
        } else {
            String::new()
        }
    }
}

fn prefill_detail_text(display: &DisplayState) -> String {
    if !display.prefill_active {
        return "Prefill: --".to_string();
    }
    if progress_stalled(display.prefill_progress_age_secs) {
        return format!(
            "Prefill: stalled age={:.1}s",
            display.prefill_progress_age_secs.unwrap_or_default()
        );
    }
    let Some(age_secs) = display
        .prefill_progress_age_secs
        .filter(|age| progress_age_available(Some(*age)))
    else {
        return "Prefill: progress age unavailable".to_string();
    };
    format!(
        "Prefill: {}/{} ({:.1}%) chunk={:.1}t/s avg={:.1}t/s elapsed={:.1}s cached={} age={:.1}s",
        display.prefill_current,
        display.prefill_total,
        display.prefill_percent,
        display.prefill_chunk_tps,
        display.prefill_avg_tps,
        display.prefill_elapsed_secs,
        display.prefill_cached,
        age_secs
    )
}

fn decode_detail_text(display: &DisplayState) -> String {
    if !display.decode_active {
        return "Decode: --".to_string();
    }
    if !display.decode_progress_observed {
        return "Decode: first-token waiting".to_string();
    }
    if progress_stalled(display.decode_progress_age_secs) {
        return format!(
            "Decode: stalled age={:.1}s",
            display.decode_progress_age_secs.unwrap_or_default()
        );
    }
    let Some(age_secs) = display
        .decode_progress_age_secs
        .filter(|age| progress_age_available(Some(*age)))
    else {
        return "Decode: progress age unavailable".to_string();
    };
    format!(
        "Decode: completion={} chunk={:.1}t/s avg={:.1}t/s age={:.1}s",
        display.decode_completion, display.decode_chunk_tps, display.decode_avg_tps, age_secs
    )
}

/// Draw the menu bar icon from the cluster mode and online status.
fn icon_for(mode: Option<&str>, offline: bool) -> Result<Icon> {
    let rgba = icon_rgba(mode, offline);
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).context("create mode icon")
}

/// Render the icon pixels.
///
/// Two status circles are always drawn: green when the corresponding node is
/// operating, red otherwise. The `distributed-layer-parallel` mode additionally draws a
/// connector line between the two circles.
fn icon_rgba(mode: Option<&str>, offline: bool) -> Vec<u8> {
    let (top, bottom, connected) = if offline {
        (false, false, false)
    } else {
        match mode {
            Some("solo-standalone") => (true, false, false),
            Some("paired-standalone") => (true, true, false),
            Some("distributed-layer-parallel") | Some("distributed-mxfp4") => (true, true, true),
            _ => (true, false, false),
        }
    };
    let top_color = if top { GREEN } else { RED };
    let bottom_color = if bottom { GREEN } else { RED };

    let cx = 9.0;
    let top_cy = 4.5;
    let bottom_cy = 13.5;
    let radius = 3.25;

    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let inv = 1.0 / ICON_SUB;
    let n = ICON_SUB * ICON_SUB;
    for py in 0..ICON_SIZE {
        for px in 0..ICON_SIZE {
            let mut acc = [0f32; 4];
            for sy in 0..ICON_SUB as i32 {
                for sx in 0..ICON_SUB as i32 {
                    let x = px as f32 + (sx as f32 + 0.5) * inv;
                    let y = py as f32 + (sy as f32 + 0.5) * inv;
                    let color: [u8; 4] = if in_circle(x, y, cx, top_cy, radius) {
                        top_color
                    } else if in_circle(x, y, cx, bottom_cy, radius) {
                        bottom_color
                    } else if connected && on_connector(x, y, top_cy, bottom_cy, cx) {
                        LINE
                    } else {
                        [0, 0, 0, 0]
                    };
                    for (a, c) in acc.iter_mut().zip(color) {
                        *a += c as f32;
                    }
                }
            }
            let idx = ((py * ICON_SIZE + px) as usize) * 4;
            for (out, a) in rgba[idx..idx + 4].iter_mut().zip(acc) {
                *out = (a / n).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    rgba
}

fn in_circle(x: f32, y: f32, cx: f32, cy: f32, radius: f32) -> bool {
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}

/// The connector line spans between the two circle centers. Parts that fall
/// inside a filled circle are covered by the circle, so only the gap between
/// the circles is visible.
fn on_connector(x: f32, y: f32, top_cy: f32, bottom_cy: f32, cx: f32) -> bool {
    const HALF_THICK: f32 = 1.5;
    y >= top_cy && y <= bottom_cy && (x - cx).abs() <= HALF_THICK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], px: u32, py: u32) -> [u8; 4] {
        let idx = ((py * ICON_SIZE + px) as usize) * 4;
        rgba[idx..idx + 4].try_into().unwrap()
    }

    /// Center pixels of the top and bottom circles, and the gap between them.
    fn key_pixels(rgba: &[u8]) -> ([u8; 4], [u8; 4], [u8; 4]) {
        (pixel(rgba, 9, 4), pixel(rgba, 9, 13), pixel(rgba, 9, 9))
    }

    #[test]
    fn solo_renders_one_green_and_one_red_without_connector() {
        let rgba = icon_rgba(Some("solo-standalone"), false);
        let (left, right, gap) = key_pixels(&rgba);
        assert_eq!(left, GREEN);
        assert_eq!(right, RED);
        assert_eq!(gap[3], 0, "solo must not draw a connector");
    }

    #[test]
    fn paired_renders_two_green_circles_without_connector() {
        let rgba = icon_rgba(Some("paired-standalone"), false);
        let (left, right, gap) = key_pixels(&rgba);
        assert_eq!(left, GREEN);
        assert_eq!(right, GREEN);
        assert_eq!(gap[3], 0, "paired must not draw a connector");
    }

    #[test]
    fn dist_renders_two_circles_connected_by_a_line() {
        let rgba = icon_rgba(Some("distributed-layer-parallel"), false);
        let (left, right, gap) = key_pixels(&rgba);
        assert_eq!(left, GREEN);
        assert_eq!(right, GREEN);
        assert_eq!(gap, LINE, "dist must draw a connector between the circles");
    }

    #[test]
    fn offline_renders_two_red_circles_without_connector() {
        let rgba = icon_rgba(None, true);
        let (left, right, gap) = key_pixels(&rgba);
        assert_eq!(left, RED);
        assert_eq!(right, RED);
        assert_eq!(gap[3], 0, "offline must not draw a connector");
    }

    #[test]
    fn mode_and_state_display_names_are_canonical() {
        assert_eq!(
            mode_display_name(Some("solo-standalone")),
            "Solo Standalone"
        );
        assert_eq!(
            mode_display_name(Some("paired-standalone")),
            "Paired Standalone"
        );
        assert_eq!(
            mode_display_name(Some("distributed-layer-parallel")),
            "Distributed (layer-parallel)"
        );
        assert_eq!(
            mode_display_name(Some("distributed-mxfp4")),
            "Distributed (layer-parallel)"
        );
        assert_eq!(mode_display_name(Some("future-mode")), "future-mode");
        assert_eq!(mode_display_name(None), "--");

        assert_eq!(
            state_display_name(Some("distributed-ready")),
            "Distributed (layer-parallel) ready"
        );
        assert_eq!(
            state_display_name(Some("demoting")),
            "Demoting to Paired Standalone"
        );
        assert_eq!(state_display_name(Some("future-state")), "future-state");
        assert_eq!(state_display_name(None), "--");
    }

    #[test]
    fn title_shows_the_selected_metric() {
        let display = DisplayState {
            prefill_active: true,
            prefill_total: 9005,
            prefill_percent: 45.5,
            prefill_chunk_tps: 123.4,
            prefill_avg_tps: 100.0,
            prefill_elapsed_secs: 10.0,
            prefill_progress_age_secs: Some(1.0),
            kv_hits_total: 7,
            kv_hit_tokens: 9005,
            kv_load_ms: 12.3,
            decode_active: true,
            decode_progress_observed: true,
            decode_completion: 42,
            decode_chunk_tps: 32.1,
            decode_avg_tps: 28.5,
            decode_elapsed_secs: 1.5,
            decode_progress_age_secs: Some(1.0),
            ..DisplayState::default()
        };

        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::PrefillPercent),
            "prefill 46%"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::PrefillChunkTps),
            "prefill 123.4t/s"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::PrefillAvgTps),
            "prefill avg 100.0t/s"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::PrefillElapsed),
            "prefill 10.0s"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::KvCache),
            "KV 9005t/12.3ms"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::DecodeChunkTps),
            "decode 32.1t/s"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::DecodeAvgTps),
            "decode avg 28.5t/s"
        );
        assert_eq!(
            selected_metric_title(&display, true, LiveMetric::DecodeElapsed),
            "decode 1.5s"
        );
        assert_eq!(
            selected_metric_title(&display, false, LiveMetric::DecodeAvgTps),
            ""
        );
        assert_eq!(selected_metric_title(&display, true, LiveMetric::None), "");

        let mut completed = display;
        completed.prefill_active = false;
        assert_eq!(
            selected_metric_title(&completed, true, LiveMetric::PrefillAvgTps),
            ""
        );
    }

    #[test]
    fn title_falls_back_to_decode_throughput_after_prefill() {
        let mut display = DisplayState {
            prefill_active: true,
            prefill_total: 9005,
            prefill_avg_tps: 100.0,
            prefill_progress_age_secs: Some(1.0),
            decode_active: true,
            decode_completion: 42,
            decode_progress_observed: true,
            decode_progress_age_secs: Some(1.0),
            decode_chunk_tps: 32.1,
            decode_avg_tps: 28.5,
            ..DisplayState::default()
        };

        assert_eq!(
            menu_bar_title(&display, true, LiveMetric::PrefillAvgTps),
            "prefill avg 100.0t/s"
        );

        display.prefill_active = false;
        assert_eq!(
            menu_bar_title(&display, true, LiveMetric::PrefillAvgTps),
            "decode 32.1t/s"
        );

        display.decode_active = false;
        assert_eq!(
            menu_bar_title(&display, true, LiveMetric::PrefillAvgTps),
            ""
        );
    }

    #[test]
    fn stale_progress_never_displays_old_chunk_as_current() {
        let prefill = DisplayState {
            prefill_active: true,
            prefill_total: 9005,
            prefill_chunk_tps: 123.4,
            prefill_avg_tps: 100.0,
            prefill_progress_age_secs: Some(60.0),
            ..DisplayState::default()
        };
        assert_eq!(
            selected_metric_title(&prefill, true, LiveMetric::PrefillChunkTps),
            "prefill stalled"
        );
        assert_eq!(prefill_detail_text(&prefill), "Prefill: stalled age=60.0s");

        let decode = DisplayState {
            decode_active: true,
            decode_progress_observed: true,
            decode_completion: 42,
            decode_chunk_tps: 32.1,
            decode_avg_tps: 28.5,
            decode_progress_age_secs: Some(60.0),
            ..DisplayState::default()
        };
        assert_eq!(
            menu_bar_title(&decode, true, LiveMetric::PrefillChunkTps),
            "decode stalled"
        );
        assert_eq!(decode_detail_text(&decode), "Decode: stalled age=60.0s");
    }

    #[test]
    fn first_token_waiting_is_visible_without_a_stale_tps_value() {
        let display = DisplayState {
            decode_active: true,
            decode_progress_observed: false,
            decode_progress_age_secs: None,
            ..DisplayState::default()
        };
        assert_eq!(
            menu_bar_title(&display, true, LiveMetric::DecodeChunkTps),
            "decode waiting"
        );
        assert_eq!(decode_detail_text(&display), "Decode: first-token waiting");
    }

    #[test]
    fn missing_progress_age_never_displays_a_tps_as_current() {
        let prefill = DisplayState {
            prefill_active: true,
            prefill_total: 9005,
            prefill_chunk_tps: 123.4,
            prefill_avg_tps: 100.0,
            ..DisplayState::default()
        };
        assert_eq!(
            selected_metric_title(&prefill, true, LiveMetric::PrefillChunkTps),
            "prefill progress age unavailable"
        );
        assert_eq!(
            prefill_detail_text(&prefill),
            "Prefill: progress age unavailable"
        );

        let decode = DisplayState {
            decode_active: true,
            decode_progress_observed: true,
            decode_chunk_tps: 32.1,
            decode_avg_tps: 28.5,
            ..DisplayState::default()
        };
        assert_eq!(
            menu_bar_title(&decode, true, LiveMetric::PrefillChunkTps),
            "decode progress age unavailable"
        );
        assert_eq!(
            decode_detail_text(&decode),
            "Decode: progress age unavailable"
        );
    }

    // ---- C-03: independent login-start registration display ----

    #[test]
    fn registration_status_text_is_stable_per_status() {
        assert_eq!(
            registration_status_text("siderostat-runtime自動起動", ServiceStatus::Enabled),
            "siderostat-runtime自動起動: 有効"
        );
        assert_eq!(
            registration_status_text("siderostat-runtime自動起動", ServiceStatus::NotRegistered),
            "siderostat-runtime自動起動: 無効"
        );
        assert_eq!(
            registration_status_text(
                "siderostat-runtime自動起動",
                ServiceStatus::RequiresApproval
            ),
            "siderostat-runtime自動起動: 承認が必要"
        );
        assert_eq!(
            registration_status_text("siderostat-runtime自動起動", ServiceStatus::NotFound),
            "siderostat-runtime自動起動: 未検出"
        );
        assert_eq!(
            registration_status_text("siderostat-runtime自動起動", ServiceStatus::Error),
            "siderostat-runtime自動起動: エラー"
        );
    }

    #[test]
    fn registration_2x2_matrix_is_independent() {
        // runtime background service と main app login start は独立設定。
        // 2 x 2 の登録状態 matrix が、label と状態の組み合わせで区別される。
        let cases = [
            (ServiceStatus::Enabled, ServiceStatus::Enabled),
            (ServiceStatus::Enabled, ServiceStatus::NotRegistered),
            (ServiceStatus::NotRegistered, ServiceStatus::Enabled),
            (ServiceStatus::NotRegistered, ServiceStatus::NotRegistered),
        ];
        let mut seen = std::collections::HashSet::new();
        for (runtime, login) in cases {
            let runtime_text = registration_status_text("siderostat-runtime自動起動", runtime);
            let login_text = registration_status_text("Siderostat自動起動", login);
            // runtime の文言は login の文言と区別され、逆も同様。
            assert_ne!(runtime_text, login_text);
            // 各組み合わせの (runtime, login) 表示対が一意。
            let pair = (runtime_text.clone(), login_text.clone());
            assert!(seen.insert(pair), "duplicate 2x2 matrix entry");
        }
        assert_eq!(seen.len(), 4, "2x2 matrix must yield 4 distinct pairs");
    }

    #[test]
    fn background_action_uses_one_position_for_start_and_stop() {
        assert_eq!(
            background_action_text(ServiceStatus::NotRegistered),
            "siderostat-runtimeを起動して自動起動を有効化"
        );
        assert_eq!(
            background_action_text(ServiceStatus::Enabled),
            "siderostat-runtimeを停止して自動起動を無効化"
        );
    }

    #[test]
    fn menu_actions_are_gated_by_service_state_and_operation_state() {
        let stopped = action_state(ServiceStatus::NotRegistered, false);
        assert!(stopped.open_config);
        assert!(!stopped.runtime_restart);
        assert!(stopped.background_toggle);
        assert!(stopped.open_login_items);

        let running = action_state(ServiceStatus::Enabled, false);
        assert!(running.runtime_restart);
        assert!(running.open_login_items);

        let approval = action_state(ServiceStatus::RequiresApproval, false);
        assert!(approval.background_toggle);
        assert!(approval.open_login_items);

        let busy = action_state(ServiceStatus::Enabled, true);
        assert!(!busy.open_config);
        assert!(!busy.runtime_restart);
        assert!(!busy.background_toggle);
        assert!(!busy.open_login_items);
    }

    #[test]
    fn first_launch_status_distinguishes_approval_and_completion() {
        assert!(
            first_launch_status_text(&FirstLaunchState::RequiresApproval).contains("承認が必要")
        );
        assert!(first_launch_status_text(&FirstLaunchState::ModelReady).contains("モデル"));
    }

    // ---- C-05a: menu ids are distinct and each dispatch helper matches only
    // its own id ----

    fn event(id: &str) -> MenuEvent {
        MenuEvent {
            id: MenuId::new(id),
        }
    }

    #[test]
    fn menu_ids_are_distinct() {
        let ids = [
            MENU_QUIT,
            MENU_RUNTIME_RESTART,
            MENU_BG_TOGGLE,
            MENU_OPEN_CONFIG,
            MENU_OPEN_LOGIN_ITEMS,
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(id), "duplicate menu id: {id}");
        }
        assert_eq!(seen.len(), 5);
    }

    #[test]
    fn each_menu_event_matches_only_its_own_dispatch_helper() {
        let cases = [
            (
                MENU_QUIT,
                MonitorTray::is_quit_event as fn(&MenuEvent) -> bool,
            ),
            (
                MENU_RUNTIME_RESTART,
                MonitorTray::is_runtime_restart_event as fn(&MenuEvent) -> bool,
            ),
            (
                MENU_BG_TOGGLE,
                MonitorTray::is_bg_toggle_event as fn(&MenuEvent) -> bool,
            ),
            (
                MENU_OPEN_CONFIG,
                MonitorTray::is_open_config_event as fn(&MenuEvent) -> bool,
            ),
            (
                MENU_OPEN_LOGIN_ITEMS,
                MonitorTray::is_open_login_items_event as fn(&MenuEvent) -> bool,
            ),
        ];
        let all_ids = [
            MENU_QUIT,
            MENU_RUNTIME_RESTART,
            MENU_BG_TOGGLE,
            MENU_OPEN_CONFIG,
            MENU_OPEN_LOGIN_ITEMS,
        ];
        for (target_id, helper) in cases {
            // 自分の id には真を返す。
            assert!(helper(&event(target_id)), "helper must match {target_id}");
            // 他方の id には偽を返す（一意性）。
            for other in all_ids {
                if other != target_id {
                    assert!(
                        !helper(&event(other)),
                        "helper for {target_id} must not match {other}"
                    );
                }
            }
        }
    }
}
