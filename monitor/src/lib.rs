//! siderostat-monitor: macOS menu bar monitor for siderostat (library crate).
//!
//! The binary (`main.rs`) drives the AppKit tray; this library exposes the
//! pollable, stateful, and service-management logic so it is unit-testable
//! from integration tests (e.g. `tests/v040_monitor_contract.rs`). G01。

pub mod client;
pub mod config;
pub mod connection_mode;
pub mod jobs;
pub mod launchd;
pub mod localization;
pub mod metrics;
pub mod migration;
pub mod operation;
pub mod service_management;
pub mod settings;
pub mod state;
pub mod tray;
pub mod uninstaller;
