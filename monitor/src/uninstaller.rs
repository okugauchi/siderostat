//! Safe uninstall workflow shared by the Finder-launched Uninstaller.app.

use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use crate::service_management::unregister_all_services;

pub const INSTALLED_APP_PATH: &str = "/Applications/Siderostat.app";
pub const INSTALLED_MONITOR_PATH: &str = "/Applications/Siderostat.app/Contents/MacOS/Siderostat";
pub const INSTALLED_RUNTIME_PATH: &str =
    "/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime";
pub const PACKAGE_RECEIPT_IDS: [&str; 2] = [
    "dev.siderostat-ds4-proxy.pkg",
    "dev.siderostat-ds4-proxy.product",
];
const SERVICE_UNREGISTER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub command: String,
}

pub fn is_uninstaller_bundle_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name == "Siderostat Uninstaller.app")
    })
}

pub fn parse_process_record(line: &str) -> Option<ProcessRecord> {
    let trimmed = line.trim_start();
    let (pid_text, command_text) = trimmed.split_once(char::is_whitespace)?;
    let pid = pid_text.parse().ok()?;
    let command = command_text.trim_start().to_owned();
    (!command.is_empty()).then_some(ProcessRecord { pid, command })
}

pub fn command_matches_exact_path(record: &ProcessRecord, executable: &Path) -> bool {
    let expected = executable.to_string_lossy();
    record.command == expected
        || record
            .command
            .strip_prefix(expected.as_ref())
            .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_whitespace))
}

/// True only when this executable was built into the DMG's uninstaller bundle.
pub fn is_uninstaller_process() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|path| is_uninstaller_bundle_path(&path))
}

/// Unregister both product-owned Service Management entries.
pub fn unregister_services_mode() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use crate::service_management::ServiceManagement;

        let adapter = ServiceManagement::new();
        unregister_all_services(&adapter).map_err(|error| anyhow::anyhow!(error))?;
        tracing::info!("Siderostat services unregistered");
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("service unregistration requires macOS")
    }
}

/// Run the Finder-facing uninstaller application.
pub fn run_uninstaller_app() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{
            NSAlert, NSAlertSecondButtonReturn, NSAlertStyle, NSApplication,
            NSApplicationActivationPolicy,
        };
        use objc2_foundation::NSString;

        let mtm =
            MainThreadMarker::new().context("uninstaller must run on the AppKit main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.activate();

        let confirmation = NSAlert::new(mtm);
        confirmation.setAlertStyle(NSAlertStyle::Warning);
        let title = crate::localization::text("uninstaller.confirm.title", "Uninstall Siderostat?");
        let message = crate::localization::text(
            "uninstaller.confirm.message",
            "Siderostat.app and its background services will be removed. User data will be kept.",
        );
        confirmation.setMessageText(&NSString::from_str(&title));
        confirmation.setInformativeText(&NSString::from_str(&message));
        confirmation.addButtonWithTitle(&NSString::from_str(&crate::localization::text(
            "uninstaller.confirm.cancel",
            "Cancel",
        )));
        confirmation.addButtonWithTitle(&NSString::from_str(&crate::localization::text(
            "uninstaller.confirm.remove",
            "Remove Siderostat",
        )));

        if confirmation.runModal() != NSAlertSecondButtonReturn {
            return Ok(());
        }

        let result = uninstall_product();
        let result_alert = NSAlert::new(mtm);
        match result {
            Ok(()) => {
                result_alert.setMessageText(&NSString::from_str(&crate::localization::text(
                    "uninstaller.result.title",
                    "Siderostat was removed",
                )));
                result_alert.setInformativeText(&NSString::from_str(&crate::localization::text(
                    "uninstaller.result.message",
                    "The application and its background services were removed. User data was kept.",
                )));
            }
            Err(error) => {
                result_alert.setAlertStyle(NSAlertStyle::Critical);
                result_alert.setMessageText(&NSString::from_str(&crate::localization::text(
                    "uninstaller.failure.title",
                    "Siderostat could not be fully removed",
                )));
                let details = crate::localization::text(
                    "uninstaller.failure.message",
                    "No user data was intentionally removed. Correct the reported condition and run the Uninstaller again.",
                );
                result_alert
                    .setInformativeText(&NSString::from_str(&format!("{details}\n\n{error}")));
            }
        }
        result_alert.addButtonWithTitle(&NSString::from_str(&crate::localization::text(
            "uninstaller.result.ok",
            "OK",
        )));
        result_alert.runModal();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("the GUI uninstaller requires macOS")
    }
}

#[derive(Debug, Clone)]
struct OwnedChildSnapshot {
    identity: siderostat_core::cluster::ChildIdentity,
    allow_sigkill: bool,
}

fn uninstall_product() -> Result<()> {
    let child = capture_owned_child()?;
    unregister_installed_services()?;
    stop_exact_path(Path::new(INSTALLED_MONITOR_PATH))?;
    stop_exact_path(Path::new(INSTALLED_RUNTIME_PATH))?;
    if let Some(child) = child {
        stop_owned_child(&child)?;
    }
    privileged_cleanup()?;
    Ok(())
}

fn unregister_installed_services() -> Result<()> {
    for monitor in unregister_service_helper_candidates() {
        if !monitor.is_file() {
            continue;
        }
        let mut child = Command::new(&monitor)
            .arg("--unregister-services")
            .spawn()
            .with_context(|| format!("run {} --unregister-services", monitor.display()))?;
        let deadline = Instant::now() + SERVICE_UNREGISTER_TIMEOUT;
        let status = loop {
            match child
                .try_wait()
                .with_context(|| format!("wait for {} --unregister-services", monitor.display()))?
            {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    child
                        .kill()
                        .with_context(|| format!("stop stalled {}", monitor.display()))?;
                    let _ = child.wait();
                    anyhow::bail!(
                        "{} --unregister-services did not finish within {} seconds",
                        monitor.display(),
                        SERVICE_UNREGISTER_TIMEOUT.as_secs()
                    );
                }
                None => thread::sleep(Duration::from_millis(100)),
            }
        };
        anyhow::ensure!(
            status.success(),
            "installed Siderostat could not unregister services: {}",
            "process exited with {status}"
        );
        return Ok(());
    }
    Ok(())
}

fn unregister_service_helper_candidates() -> Vec<PathBuf> {
    vec![PathBuf::from(INSTALLED_MONITOR_PATH)]
}

fn process_records() -> Result<Vec<ProcessRecord>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .output()
        .context("list processes")?;
    anyhow::ensure!(output.status.success(), "ps failed with {}", output.status);
    Ok(String::from_utf8(output.stdout)
        .context("decode process list")?
        .lines()
        .filter_map(parse_process_record)
        .collect())
}

fn matching_pids(executable: &Path) -> Result<Vec<u32>> {
    Ok(process_records()?
        .into_iter()
        .filter(|record| command_matches_exact_path(record, executable))
        .map(|record| record.pid)
        .collect())
}

fn send_signal(pid: u32, signal: &str) -> Result<()> {
    let output = Command::new("/bin/kill")
        .args([signal, &pid.to_string()])
        .output()
        .with_context(|| format!("send {signal} to pid {pid}"))?;
    anyhow::ensure!(
        output.status.success(),
        "kill {signal} {pid} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn stop_exact_path(executable: &Path) -> Result<()> {
    let initial = matching_pids(executable)?;
    for pid in &initial {
        send_signal(*pid, "-TERM")?;
    }
    if wait_for_no_matching_process(executable, Duration::from_secs(10))? {
        return Ok(());
    }

    for pid in matching_pids(executable)? {
        send_signal(pid, "-KILL")?;
    }
    anyhow::ensure!(
        wait_for_no_matching_process(executable, Duration::from_secs(10))?,
        "processes at {} did not stop",
        executable.display()
    );
    Ok(())
}

fn wait_for_no_matching_process(executable: &Path, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if matching_pids(executable)?.is_empty() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn capture_owned_child() -> Result<Option<OwnedChildSnapshot>> {
    let config_path = runtime_config_path()?;
    if !config_path.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("read runtime configuration {}", config_path.display()))?;
    let mut config = siderostat_core::config::ModeAwareConfig::parse(&contents)
        .context("parse runtime configuration")?;
    config.expand_paths().context("expand runtime paths")?;
    let state_path = config.cluster.state_path.clone();
    if !state_path.is_file() {
        return Ok(None);
    }
    let state_contents = std::fs::read_to_string(&state_path)
        .with_context(|| format!("read cluster state {}", state_path.display()))?;
    let state: siderostat_core::cluster::PersistentClusterState =
        serde_json::from_str(&state_contents).context("parse cluster state")?;
    state.validate().context("validate cluster state")?;
    let Some(child) = state.child else {
        return Ok(None);
    };
    let argv_sha256 = decode_sha256(&child.argv_sha256)?;
    Ok(Some(OwnedChildSnapshot {
        identity: siderostat_core::cluster::ChildIdentity {
            pid: child.pid,
            executable: child.executable,
            argv_sha256,
            profile_id: state
                .active_profile
                .unwrap_or_else(|| "uninstaller".to_owned()),
            generation: state.generation,
            spawned_at_millis: child.spawned_at_millis,
            process_start_micros: child.process_start_micros,
        },
        allow_sigkill: config.ds4.allow_sigkill,
    }))
}

fn runtime_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/siderostat/config.toml"))
}

fn decode_sha256(value: &str) -> Result<[u8; 32]> {
    anyhow::ensure!(value.len() == 64, "persisted child hash has invalid length");
    let mut output = [0_u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .with_context(|| format!("decode persisted child hash at byte {index}"))?;
    }
    Ok(output)
}

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn applescript_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn privileged_cleanup_script(app: &Path, trash_app: &Path) -> String {
    let trash_dir = trash_app.parent().unwrap_or_else(|| Path::new("/"));
    let app = shell_quote(app);
    let trash_dir = shell_quote(trash_dir);
    let trash_app = shell_quote(trash_app);
    let receipts = PACKAGE_RECEIPT_IDS
        .iter()
        .map(|receipt| format!("{receipt:?}"))
        .collect::<Vec<_>>()
        .join(" ");
    let shell = format!(
        "set -eu; trash_base={trash_app}; trash_target=\"$trash_base\"; if [ -e {app} ]; then /bin/mkdir -p {trash_dir}; while [ -e \"$trash_target\" ]; do suffix=1; while [ -e \"$trash_base.$suffix\" ]; do suffix=$((suffix + 1)); done; trash_target=\"$trash_base.$suffix\"; done; /bin/mv -f -- {app} \"$trash_target\"; fi; for receipt in {receipts}; do if /usr/sbin/pkgutil --pkg-info \"$receipt\" >/dev/null 2>&1; then /usr/sbin/pkgutil --forget \"$receipt\"; fi; done"
    );
    format!(
        "do shell script \"{}\" with administrator privileges",
        applescript_quote(&shell)
    )
}

fn receipt_is_installed(receipt: &str) -> Result<bool> {
    let output = Command::new("/usr/sbin/pkgutil")
        .args(["--pkg-info", receipt])
        .output()
        .with_context(|| format!("inspect package receipt {receipt}"))?;
    Ok(output.status.success())
}

fn privileged_cleanup() -> Result<()> {
    let app = Path::new(INSTALLED_APP_PATH);
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let trash_app = PathBuf::from(home).join(".Trash/Siderostat.app");
    let receipts_installed = PACKAGE_RECEIPT_IDS
        .iter()
        .map(|receipt| receipt_is_installed(receipt))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .any(|installed| installed);
    if !app.exists() && !receipts_installed {
        return Ok(());
    }

    let script = privileged_cleanup_script(app, &trash_app);
    let output = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .context("move Siderostat.app to Trash and forget package receipts")?;
    anyhow::ensure!(
        output.status.success(),
        "privileged Siderostat cleanup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    anyhow::ensure!(!app.exists(), "Siderostat.app remains in /Applications");
    Ok(())
}

fn stop_owned_child(child: &OwnedChildSnapshot) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use siderostat_core::cluster::platform_process_controller;

        let controller = platform_process_controller();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build child stop runtime")?;
        runtime
            .block_on(controller.stop_recovered_owned(
                &child.identity,
                Duration::from_secs(10),
                Duration::from_millis(200),
                child.allow_sigkill,
            ))
            .map_err(|error| anyhow::anyhow!("stop managed ds4-server child: {error}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = child;
        anyhow::bail!("managed child stop requires macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn uninstaller_bundle_detection_does_not_match_installed_app() {
        assert!(is_uninstaller_bundle_path(Path::new(
            "/Volumes/Siderostat/Siderostat Uninstaller.app/Contents/MacOS/Siderostat Uninstaller"
        )));
        assert!(!is_uninstaller_bundle_path(Path::new(
            "/Applications/Siderostat.app/Contents/MacOS/Siderostat"
        )));
    }

    #[test]
    fn process_parser_keeps_arguments_for_identity_check() {
        assert_eq!(
            parse_process_record(
                " 1234 /Applications/Siderostat.app/Contents/MacOS/Siderostat --unregister-services"
            ),
            Some(ProcessRecord {
                pid: 1234,
                command:
                    "/Applications/Siderostat.app/Contents/MacOS/Siderostat --unregister-services"
                        .into(),
            })
        );
        assert_eq!(parse_process_record("not-a-pid command"), None);
    }

    #[test]
    fn process_identity_requires_path_boundary() {
        let expected = PathBuf::from(INSTALLED_MONITOR_PATH);
        let exact = ProcessRecord {
            pid: 10,
            command: format!("{INSTALLED_MONITOR_PATH} --test"),
        };
        let prefix_collision = ProcessRecord {
            pid: 11,
            command: format!("{INSTALLED_MONITOR_PATH}-other"),
        };
        assert!(command_matches_exact_path(&exact, &expected));
        assert!(!command_matches_exact_path(&prefix_collision, &expected));
    }

    #[test]
    fn receipt_ids_are_fixed_to_product_receipts() {
        assert_eq!(
            PACKAGE_RECEIPT_IDS,
            [
                "dev.siderostat-ds4-proxy.pkg",
                "dev.siderostat-ds4-proxy.product"
            ]
        );
    }

    #[test]
    fn service_unregister_helper_never_uses_a_trash_copy() {
        assert_eq!(
            unregister_service_helper_candidates(),
            vec![PathBuf::from(INSTALLED_MONITOR_PATH)]
        );
    }

    #[test]
    fn privileged_cleanup_is_one_authorization_transaction() {
        let script = privileged_cleanup_script(
            Path::new("/Applications/Siderostat.app"),
            Path::new("/Users/test/.Trash/Siderostat.app"),
        );
        assert!(script.contains("pkgutil --forget"));
        assert!(script.contains("/Applications/Siderostat.app"));
        assert!(script.contains("/Users/test/.Trash/Siderostat.app"));
        assert!(!script.contains("chown"));
        assert!(script.contains("trash_base"));
        assert!(script.contains("trash_target"));
        assert!(script.contains("with administrator privileges"));
    }
}
