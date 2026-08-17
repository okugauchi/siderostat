//! LaunchAgent controls used by the monitor menu.

use anyhow::{Context, Result};
use std::process::Command;

pub const RUNTIME_LABEL: &str = "local.siderostat.runtime";
pub const MONITOR_LABEL: &str = "local.siderostat.monitor";

pub fn kickstart(label: &str) -> Result<()> {
    let target = service_target(label)?;
    run_launchctl(["kickstart", "-k", &target])
        .with_context(|| format!("kickstart LaunchAgent {target}"))
}

/// Stop both jobs. The monitor job is stopped last because this process is
/// itself owned by that LaunchAgent.
pub fn bootout_runtime_and_monitor() -> Result<()> {
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
    // Keep the test focused on the stable LaunchAgent labels without invoking
    // launchctl or depending on a logged-in Aqua session.
    #[test]
    fn labels_are_the_standard_siderostat_jobs() {
        assert_eq!(super::RUNTIME_LABEL, "local.siderostat.runtime");
        assert_eq!(super::MONITOR_LABEL, "local.siderostat.monitor");
    }
}
