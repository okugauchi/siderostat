//! LaunchAgent controls used by the monitor menu.
//!
//! In bundle mode (the monitor running from an `.app` bundle) the runtime and
//! monitor are managed through Service Management (`SMAppService`) and the
//! graceful-restart admin endpoint, never through `launchctl`. This module
//! keeps the raw launchctl path for the non-bundle developer workflow and
//! refuses to use it in bundle mode (C-05a).

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

pub const RUNTIME_LABEL: &str = "local.siderostat.runtime";
pub const MONITOR_LABEL: &str = "local.siderostat.monitor";

/// True when the monitor executable lives inside an `.app` bundle. In bundle
/// mode launchctl is not used for lifecycle operations (C-05a).
pub fn is_bundle_mode() -> bool {
    std::env::current_exe()
        .map(|path| is_bundle_path(&path))
        .unwrap_or(false)
}

/// Bundle detection as a pure function so it can be unit-tested.
/// An `.app` bundle has the executable under `<name>.app/Contents/MacOS/`.
pub fn is_bundle_path(exe_path: &Path) -> bool {
    let mut components = exe_path.components().rev();
    // .../Contents/MacOS/<executable>
    let _executable = components.next();
    let macos = components.next().map(|c| c.as_os_str() == "MacOS");
    let contents = components.next().map(|c| c.as_os_str() == "Contents");
    macos == Some(true) && contents == Some(true)
}

pub fn kickstart(label: &str) -> Result<()> {
    if is_bundle_mode() {
        bail!("launchctl kickstart is not used in bundle mode");
    }
    let target = service_target(label)?;
    run_launchctl(["kickstart", "-k", &target])
        .with_context(|| format!("kickstart LaunchAgent {target}"))
}

/// Stop both jobs. The monitor job is stopped last because this process is
/// itself owned by that LaunchAgent. Not available in bundle mode.
pub fn bootout_runtime_and_monitor() -> Result<()> {
    if is_bundle_mode() {
        bail!("launchctl bootout is not used in bundle mode");
    }
    let runtime = service_target(RUNTIME_LABEL)?;
    let monitor = service_target(MONITOR_LABEL)?;
    let mut errors = Vec::new();
    for target in [&runtime, &monitor] {
        if let Err(error) = run_launchctl(["bootout", target]) {
            errors.push(format!("{target}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("failed to stop LaunchAgents: {}", errors.join("; "))
    }
}

fn service_target(label: &str) -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .context("get current user id")?;
    if !output.status.success() {
        anyhow::bail!("id -u failed with status {:?}", output.status.code());
    }
    let uid = String::from_utf8(output.stdout)
        .context("decode current user id")?
        .trim()
        .to_string();
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("invalid current user id");
    }
    Ok(format!("gui/{uid}/{label}"))
}

fn run_launchctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("run launchctl")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "launchctl exited with status {:?}: {}",
        output.status.code(),
        stderr.trim()
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    // Keep the test focused on the stable LaunchAgent labels without invoking
    // launchctl or depending on a logged-in Aqua session.
    #[test]
    fn labels_are_the_standard_siderostat_jobs() {
        assert_eq!(super::RUNTIME_LABEL, "local.siderostat.runtime");
        assert_eq!(super::MONITOR_LABEL, "local.siderostat.monitor");
    }

    #[test]
    fn bundle_path_is_detected_under_contents_macos() {
        assert!(super::is_bundle_path(Path::new(
            "/Applications/Siderostat.app/Contents/MacOS/siderostat-monitor"
        )));
    }

    #[test]
    fn non_bundle_binary_is_not_a_bundle_path() {
        assert!(!super::is_bundle_path(Path::new(
            "/usr/local/bin/siderostat-monitor"
        )));
        assert!(!super::is_bundle_path(Path::new(
            "/Applications/Siderostat.app/Contents/Resources/siderostat-monitor"
        )));
        assert!(!super::is_bundle_path(Path::new("siderostat-monitor")));
    }
}
