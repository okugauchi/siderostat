//! Open Siderostat's runtime configuration from the monitor menu.

use anyhow::{Context, Result};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

const RUNTIME_CONFIG_RELATIVE_PATH: &str = "Library/Application Support/siderostat/config.toml";
const LOGIN_ITEMS_SCHEME: &str =
    "x-apple.systempreferences:com.apple.LoginItems-Settings.extension";

/// Open the runtime configuration in its default application. If installation
/// has not created the file yet, open the nearest existing parent directory so
/// the user can see where the file belongs.
pub fn open_runtime_config() -> Result<()> {
    let config_path = runtime_config_path()?;
    let target = open_target(&config_path).with_context(|| {
        format!(
            "find existing configuration file or parent for {}",
            config_path.display()
        )
    })?;
    if target != config_path {
        tracing::warn!(
            config_path = %config_path.display(),
            opened_path = %target.display(),
            "runtime configuration does not exist; opening its parent directory"
        );
    }

    let status = Command::new("/usr/bin/open")
        .arg(&target)
        .status()
        .with_context(|| format!("open {}", target.display()))?;
    anyhow::ensure!(
        status.success(),
        "open exited with status {:?} for {}",
        status.code(),
        target.display()
    );
    Ok(())
}

fn runtime_config_path() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(config_path_for_home(Path::new(&home)))
}

fn config_path_for_home(home: &Path) -> PathBuf {
    home.join(RUNTIME_CONFIG_RELATIVE_PATH)
}

fn open_target(config_path: &Path) -> Option<PathBuf> {
    if config_path.is_file() {
        return Some(config_path.to_path_buf());
    }
    config_path
        .parent()?
        .ancestors()
        .find(|candidate| candidate.is_dir())
        .map(Path::to_path_buf)
}

/// Open System Settings at the Login Items pane so the user can approve the
/// Siderostat background service. Shown only when the runtime registration
/// reports `requiresApproval` (C-05b/C-05c).
pub fn open_login_items() -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .arg(LOGIN_ITEMS_SCHEME)
        .status()
        .context("open Login Items settings")?;
    anyhow::ensure!(
        status.success(),
        "open Login Items exited with status {:?}",
        status.code()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_uses_the_siderostat_application_support_directory() {
        assert_eq!(
            config_path_for_home(Path::new("/Users/tester")),
            PathBuf::from("/Users/tester/Library/Application Support/siderostat/config.toml")
        );
    }

    #[test]
    fn missing_config_falls_back_to_an_existing_parent() {
        let root = env::temp_dir().join(format!(
            "siderostat-monitor-settings-{}",
            std::process::id()
        ));
        let config_dir = root.join("Library/Application Support/siderostat");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        assert_eq!(open_target(&config_path), Some(config_dir.clone()));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn login_items_scheme_targets_the_system_settings_login_items_pane() {
        // C-05c: approval 導線は System Settings のログイン項目 pane を開く。
        assert_eq!(
            LOGIN_ITEMS_SCHEME,
            "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"
        );
    }
}
