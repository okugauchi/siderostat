//! tray-icon based menu bar UI for the siderostat monitor.
//!
//! The tray icon is created and updated on the main thread (macOS AppKit
//! requirement). The menu reflects the current `DisplayState`; the polling
//! task shares state through the mutex passed to `update`.

use crate::{metrics::MonitorStatus, state::DisplayState};
use anyhow::{Context, Result};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

const MENU_QUIT: &str = "quit";
const MENU_RESTART: &str = "restart";

/// Icon drawing colors.
const GREEN: [u8; 4] = [0x2e, 0xcc, 0x71, 0xff]; // operating state
const RED: [u8; 4] = [0xe7, 0x4c, 0x3c, 0xff]; // non-operating state
const LINE: [u8; 4] = [0x3f, 0x3f, 0x3f, 0xff]; // connector line

const ICON_SIZE: u32 = 16;
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
    _separator: PredefinedMenuItem,
    _restart: MenuItem,
    _quit: MenuItem,
}

impl MonitorTray {
    /// Build the tray icon and its static menu structure.
    pub fn new() -> Result<Self> {
        let icon = icon_for(None, true)?;
        let header = MenuItem::new("siderostat", false, None);
        let mode = MenuItem::new("Mode: --", false, None);
        let state = MenuItem::new("State: --", false, None);
        let generation = MenuItem::new("Gen: --", false, None);
        let target = MenuItem::new("Target: --", false, None);
        let prefill = MenuItem::new("Prefill: --", false, None);
        let kv_cache = MenuItem::new("KV cache: --", false, None);
        let decode = MenuItem::new("Decode: --", false, None);
        let restart = MenuItem::with_id(MENU_RESTART, "siderostat(runtime) の再起動", true, None);
        let quit = MenuItem::with_id(MENU_QUIT, "Monitor を終了", true, None);

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
        menu.append(&restart)?;
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
            _separator: separator,
            _restart: restart,
            _quit: quit,
        })
    }

    /// Update the tray icon, title, tooltip, and menu texts from the display
    /// state. The cluster mode is conveyed by the icon drawing (two circles and
    /// an optional connector), never by a text abbreviation.
    pub fn update(&self, display: &DisplayState) {
        self.header.set_text(
            display
                .node_id
                .clone()
                .unwrap_or_else(|| "siderostat".into()),
        );

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
                "Prefill: {}/{} ({:.1}%)  cached={}",
                display.prefill_current,
                display.prefill_total,
                display.prefill_percent,
                display.prefill_cached
            ));
            self._tray
                .set_title(Some(format!("prefill {:.0}%", display.prefill_percent)));
        } else {
            self.prefill.set_text("Prefill: --");
            // The cluster mode is drawn by the icon, not shown as text.
            self._tray.set_title(Some(String::new()));
        }

        if display.kv_hits_total > 0 {
            self.kv_cache.set_text(format!(
                "KV cache: hits={} tokens={} load={:.1}ms",
                display.kv_hits_total, display.kv_hit_tokens, display.kv_load_ms
            ));
        } else {
            self.kv_cache.set_text("KV cache: --");
        }

        // Decode TPS is not available from the current metrics, so show a
        // placeholder instead of the cluster-mode abbreviation.
        self.decode.set_text("Decode: --");
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
    pub fn is_restart_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_RESTART)
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
    let (left, right, connected) = if offline {
        (false, false, false)
    } else {
        match mode {
            Some("solo-standalone") => (true, false, false),
            Some("paired-standalone") => (true, true, false),
            Some("distributed-mxfp4") => (true, true, true),
            _ => (true, false, false),
        }
    };
    let left_color = if left { GREEN } else { RED };
    let right_color = if right { GREEN } else { RED };

    let left_cx = 4.0;
    let right_cx = 12.0;
    let cy = 8.0;
    let radius = 2.5;

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
                    let color: [u8; 4] = if in_circle(x, y, left_cx, cy, radius) {
                        left_color
                    } else if in_circle(x, y, right_cx, cy, radius) {
                        right_color
                    } else if connected && on_connector(x, y, left_cx, right_cx, cy) {
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
fn on_connector(x: f32, y: f32, left_cx: f32, right_cx: f32, cy: f32) -> bool {
    const HALF_THICK: f32 = 1.0;
    x >= left_cx && x <= right_cx && (y - cy).abs() <= HALF_THICK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], px: u32, py: u32) -> [u8; 4] {
        let idx = ((py * ICON_SIZE + px) as usize) * 4;
        rgba[idx..idx + 4].try_into().unwrap()
    }

    /// Center pixels of the left and right circles, and the gap between them.
    fn key_pixels(rgba: &[u8]) -> ([u8; 4], [u8; 4], [u8; 4]) {
        (pixel(rgba, 4, 8), pixel(rgba, 12, 8), pixel(rgba, 8, 8))
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
}
