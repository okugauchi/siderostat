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
    _quit: MenuItem,
}

impl MonitorTray {
    /// Build the tray icon and its static menu structure.
    pub fn new() -> Result<Self> {
        let icon = simple_icon()?;
        let header = MenuItem::new("siderostat", false, None);
        let mode = MenuItem::new("Mode: --", false, None);
        let state = MenuItem::new("State: --", false, None);
        let generation = MenuItem::new("Gen: --", false, None);
        let target = MenuItem::new("Target: --", false, None);
        let prefill = MenuItem::new("Prefill: --", false, None);
        let kv_cache = MenuItem::new("KV cache: --", false, None);
        let decode = MenuItem::new("Decode: --", false, None);
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
            _quit: quit,
        })
    }

    /// Update the tray title, tooltip, and menu texts from the display state.
    pub fn update(&self, display: &DisplayState) {
        self.header.set_text(
            display
                .node_id
                .clone()
                .unwrap_or_else(|| "siderostat".into()),
        );
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
            self._tray.set_title(Some(display.status_abbreviation()));
        }

        if display.kv_hits_total > 0 {
            self.kv_cache.set_text(format!(
                "KV cache: hits={} tokens={} load={:.1}ms",
                display.kv_hits_total, display.kv_hit_tokens, display.kv_load_ms
            ));
        } else {
            self.kv_cache.set_text("KV cache: --");
        }

        self.decode
            .set_text(format!("Decode: {}", display.status_abbreviation()));
        let _ = self._tray.set_tooltip(Some(format!(
            "siderostat {}",
            display.status_abbreviation()
        )));
    }

    /// Check whether a menu event requests the monitor to quit.
    pub fn is_quit_event(event: &MenuEvent) -> bool {
        *event.id() == MenuId::new(MENU_QUIT)
    }
}

/// Create a small solid menu bar icon.
fn simple_icon() -> Result<Icon> {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x2d, 0x6c, 0xdf, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("create menu bar icon")
}
