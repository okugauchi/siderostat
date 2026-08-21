//! tray-icon based menu bar UI for the siderostat monitor.
//!
//! The tray icon is created and updated on the main thread (macOS AppKit
//! requirement). The menu reflects the current `DisplayState`; the polling
//! task shares state through the mutex passed to `update`. The registration
//! status display (`update_registration`) is wired from the first-launch /
//! menu UI in C-05; until then it is dead from the bin crate's perspective.
#![allow(dead_code)]

use crate::{
    config::LiveMetric, metrics::MonitorStatus, service_management::ServiceStatus,
    state::DisplayState,
};
use anyhow::{Context, Result};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const MENU_QUIT: &str = "quit";
const MENU_RUNTIME_RESTART: &str = "runtime-restart";
const MENU_BG_START: &str = "bg-start";
const MENU_BG_STOP: &str = "bg-stop";
const MENU_OPEN_CONFIG: &str = "open-config";
const MENU_OPEN_LOGIN_ITEMS: &str = "open-login-items";

/// Icon drawing colors.
const GREEN: [u8; 4] = [0x2e, 0xcc, 0x71, 0xff]; // operating state
const RED: [u8; 4] = [0xe7, 0x4c, 0x3c, 0xff]; // non-operating state
const LINE: [u8; 4] = [0x3f, 0x3f, 0x3f, 0xff]; // connector line

const ICON_SIZE: u32 = 18;
const ICON_SUB: f32 = 4.0; // supersample factor for smooth circles

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
    show_decode_tps: bool,
    live_metric: LiveMetric,
    _separator: PredefinedMenuItem,
    _open_config: MenuItem,
    _open_login_items: MenuItem,
    _runtime_restart: MenuItem,
    _bg_start: MenuItem,
    _bg_stop: MenuItem,
    _quit: MenuItem,
}

impl MonitorTray {
    /// Build the tray icon and its static menu structure.
    pub fn new(show_decode_tps: bool, live_metric: LiveMetric) -> Result<Self> {
        let icon = icon_for(None, true)?;
        let header = MenuItem::new("siderostat", false, None);
        let mode = MenuItem::new("Mode: --", false, None);
        let state = MenuItem::new("State: --", false, None);
        let generation = MenuItem::new("Gen: --", false, None);
        let target = MenuItem::new("Target: --", false, None);
        let prefill = MenuItem::new("Prefill: --", false, None);
        let kv_cache = MenuItem::new("KV cache: --", false, None);
        let decode = MenuItem::new("Decode: --", false, None);
        let runtime_status = MenuItem::new("Runtime: --", false, None);
        let login_status = MenuItem::new("Login start: --", false, None);
        let open_config = MenuItem::with_id(MENU_OPEN_CONFIG, "設定ファイルを開く", true, None);
        let open_login_items =
            MenuItem::with_id(MENU_OPEN_LOGIN_ITEMS, "ログイン項目を開く", true, None);
        let runtime_restart =
            MenuItem::with_id(MENU_RUNTIME_RESTART, "Runtime を再起動", true, None);
        let bg_start = MenuItem::with_id(MENU_BG_START, "バックグラウンド実行を開始", true, None);
        let bg_stop = MenuItem::with_id(MENU_BG_STOP, "バックグラウンド実行を停止", true, None);
        let quit = MenuItem::with_id(MENU_QUIT, "終了", true, None);

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
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&open_config)?;
        menu.append(&runtime_restart)?;
        menu.append(&bg_start)?;
        menu.append(&bg_stop)?;
        menu.append(&open_login_items)?;
        menu.append(&quit)?;

        let tray = TrayIconBuilder::new()
            .with_tooltip("siderostat monitor")
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
            show_decode_tps,
            live_metric,
            _separator: separator,
            _open_config: open_config,
            _open_login_items: open_login_items,
            _runtime_restart: runtime_restart,
            _bg_start: bg_start,
            _bg_stop: bg_stop,
            _quit: quit,
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
            self.state.set_text("siderostat に接続できません");
            self.generation.set_text("Gen: --");
            self.target.set_text("Target: --");
            self.prefill.set_text("Prefill: --");
            self.kv_cache.set_text("KV cache: --");
            self.decode.set_text("Decode: --");
            self._tray.set_title(Some("offline"));
            let _ = self._tray.set_tooltip(Some("siderostat に接続できません"));
            return;
        }

        self.mode.set_text(format!(
            "Mode: {}",
            display.cluster_mode.as_deref().unwrap_or("--")
        ));
        self.state.set_text(format!(
            "State: {}",
            display.cluster_state.as_deref().unwrap_or("--")
        ));
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

        if display.prefill_active {
            self.prefill.set_text(format!(
                "Prefill: {}/{} ({:.1}%) chunk={:.1}t/s avg={:.1}t/s elapsed={:.1}s cached={}",
                display.prefill_current,
                display.prefill_total,
                display.prefill_percent,
                display.prefill_chunk_tps,
                display.prefill_avg_tps,
                display.prefill_elapsed_secs,
                display.prefill_cached
            ));
        } else {
            self.prefill.set_text("Prefill: --");
        }

        if display.kv_hits_total > 0 {
            self.kv_cache.set_text(format!(
                "KV cache: hits={} tokens={} load={:.1}ms",
                display.kv_hits_total, display.kv_hit_tokens, display.kv_load_ms
            ));
        } else {
            self.kv_cache.set_text("KV cache: --");
        }

        if self.show_decode_tps && has_decode_values(display) {
            self.decode.set_text(format!(
                "Decode: completion={} chunk={:.1}t/s avg={:.1}t/s",
                display.decode_completion, display.decode_chunk_tps, display.decode_avg_tps
            ));
        } else {
            self.decode.set_text("Decode: --");
        }
        self._tray.set_title(Some(menu_bar_title(
            display,
            self.show_decode_tps,
            self.live_metric,
        )));
        let _ = self._tray.set_tooltip(Some(format!(
            "siderostat node={} state={}",
            display.node_id.as_deref().unwrap_or("--"),
            display.cluster_state.as_deref().unwrap_or("--"),
        )));
    }

    /// Check whether a menu event requests the monitor to quit.
    pub fn is_quit_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_QUIT)
    }

    /// Check whether a menu event requests a siderostat runtime restart.
    pub fn is_runtime_restart_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_RUNTIME_RESTART)
    }

    /// Check whether a menu event requests starting background execution.
    pub fn is_bg_start_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_BG_START)
    }

    /// Check whether a menu event requests stopping background execution.
    pub fn is_bg_stop_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_BG_STOP)
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
        self.runtime_status
            .set_text(registration_status_text("Runtime", runtime));
        self.login_status
            .set_text(registration_status_text("Login start", login_item));
    }
}

/// Human-readable menu text for a registration status. The status name is
/// stable snake_case; the label prefix is a fixed UI noun.
fn registration_status_text(label: &str, status: ServiceStatus) -> String {
    let value = match status {
        ServiceStatus::Enabled => "enabled",
        ServiceStatus::NotRegistered => "off",
        ServiceStatus::RequiresApproval => "approval needed",
        ServiceStatus::NotFound => "not found",
        ServiceStatus::Error => "error",
    };
    format!("{label}: {value}")
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
        LiveMetric::PrefillChunkTps
            if display.prefill_active
                && display.prefill_total > 0
                && display.prefill_chunk_tps > 0.0 =>
        {
            format!("prefill {:.1}t/s", display.prefill_chunk_tps)
        }
        LiveMetric::PrefillAvgTps
            if display.prefill_active
                && display.prefill_total > 0
                && display.prefill_avg_tps > 0.0 =>
        {
            format!("prefill avg {:.1}t/s", display.prefill_avg_tps)
        }
        LiveMetric::PrefillElapsed
            if display.prefill_active
                && display.prefill_total > 0
                && display.prefill_elapsed_secs > 0.0 =>
        {
            format!("prefill {:.1}s", display.prefill_elapsed_secs)
        }
        LiveMetric::DecodeChunkTps if show_decode_tps && has_decode_values(display) => {
            format!("decode {:.1}t/s", display.decode_chunk_tps)
        }
        LiveMetric::DecodeAvgTps if show_decode_tps && has_decode_values(display) => {
            format!("decode avg {:.1}t/s", display.decode_avg_tps)
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
    display.decode_active
        && (display.decode_completion > 0
            || display.decode_chunk_tps > 0.0
            || display.decode_avg_tps > 0.0)
}

fn decode_throughput_title(display: &DisplayState) -> String {
    if display.decode_avg_tps > 0.0 {
        format!("decode avg {:.1}t/s", display.decode_avg_tps)
    } else if display.decode_chunk_tps > 0.0 {
        format!("decode {:.1}t/s", display.decode_chunk_tps)
    } else {
        String::new()
    }
}

/// Draw the menu bar icon from the cluster mode and online status.
fn icon_for(mode: Option<&str>, offline: bool) -> Result<Icon> {
    let rgba = icon_rgba(mode, offline);
    Icon::from_rgba(rgba, ICON_SIZE, ICON_SIZE).context("create mode icon")
}

/// Render the icon pixels.
///
/// Two status circles are always drawn: green when the corresponding node is
/// operating, red otherwise. The `distributed-mxfp4` mode additionally draws a
/// connector line between the two circles.
fn icon_rgba(mode: Option<&str>, offline: bool) -> Vec<u8> {
    let (top, bottom, connected) = if offline {
        (false, false, false)
    } else {
        match mode {
            Some("solo-standalone") => (true, false, false),
            Some("paired-standalone") => (true, true, false),
            Some("distributed-mxfp4") => (true, true, true),
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
        let rgba = icon_rgba(Some("distributed-mxfp4"), false);
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
    fn title_shows_the_selected_metric() {
        let display = DisplayState {
            prefill_active: true,
            prefill_total: 9005,
            prefill_percent: 45.5,
            prefill_chunk_tps: 123.4,
            prefill_avg_tps: 100.0,
            prefill_elapsed_secs: 10.0,
            kv_hits_total: 7,
            kv_hit_tokens: 9005,
            kv_load_ms: 12.3,
            decode_active: true,
            decode_completion: 42,
            decode_chunk_tps: 32.1,
            decode_avg_tps: 28.5,
            decode_elapsed_secs: 1.5,
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
            decode_active: true,
            decode_completion: 42,
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
            "decode avg 28.5t/s"
        );

        display.decode_active = false;
        assert_eq!(
            menu_bar_title(&display, true, LiveMetric::PrefillAvgTps),
            ""
        );
    }

    // ---- C-03: independent login-start registration display ----

    #[test]
    fn registration_status_text_is_stable_per_status() {
        assert_eq!(
            registration_status_text("Runtime", ServiceStatus::Enabled),
            "Runtime: enabled"
        );
        assert_eq!(
            registration_status_text("Runtime", ServiceStatus::NotRegistered),
            "Runtime: off"
        );
        assert_eq!(
            registration_status_text("Runtime", ServiceStatus::RequiresApproval),
            "Runtime: approval needed"
        );
        assert_eq!(
            registration_status_text("Runtime", ServiceStatus::NotFound),
            "Runtime: not found"
        );
        assert_eq!(
            registration_status_text("Runtime", ServiceStatus::Error),
            "Runtime: error"
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
            let runtime_text = registration_status_text("Runtime", runtime);
            let login_text = registration_status_text("Login start", login);
            // runtime の文言は login の文言と区別され、逆も同様。
            assert_ne!(runtime_text, login_text);
            // 各組み合わせの (runtime, login) 表示対が一意。
            let pair = (runtime_text.clone(), login_text.clone());
            assert!(seen.insert(pair), "duplicate 2x2 matrix entry");
        }
        assert_eq!(seen.len(), 4, "2x2 matrix must yield 4 distinct pairs");
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
            MENU_BG_START,
            MENU_BG_STOP,
            MENU_OPEN_CONFIG,
            MENU_OPEN_LOGIN_ITEMS,
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(id), "duplicate menu id: {id}");
        }
        assert_eq!(seen.len(), 6);
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
                MENU_BG_START,
                MonitorTray::is_bg_start_event as fn(&MenuEvent) -> bool,
            ),
            (
                MENU_BG_STOP,
                MonitorTray::is_bg_stop_event as fn(&MenuEvent) -> bool,
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
            MENU_BG_START,
            MENU_BG_STOP,
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
