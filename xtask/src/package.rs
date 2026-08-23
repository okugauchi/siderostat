//! Flat `.pkg` builder with controlled install hooks (E-01).
//!
//! `cargo xtask pkg-dev` turns an `app-dev` bundle into a component package
//! and product archive with a single `/Applications/Siderostat.app` payload.
//! The preinstall hook terminates only an existing Monitor executable at the
//! exact `/Applications/Siderostat.app` path before the bundle is replaced;
//! the postinstall hook only requests a launch in the active console user's GUI
//! session. Runtime, ds4-server, and user data are not touched. The expanded
//! package is inspected so unexpected scripts or forbidden payload paths fail
//! the build.
//!
//! Identifier / version are fixed here (spec §9.1) so a later signed release
//! shares the same receipt ID and version comparison for upgrade. A rollback
//! package is opt-in and disables only the bundle version check; normal package
//! generation remains version-checked.
//! This module never touches user data, LaunchAgents, or config.

use crate::bundle::PkgDevArgs;
use crate::util;
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Component package receipt identifier (spec §9.1).
pub const COMPONENT_IDENTIFIER: &str = "dev.siderostat-ds4-proxy.pkg";
/// Product archive identifier (spec §9.1).
pub const PRODUCT_IDENTIFIER: &str = "dev.siderostat-ds4-proxy.product";
/// Install location for the single payload.
pub const INSTALL_LOCATION: &str = "/Applications";
/// The single payload item.
pub const PAYLOAD_PATH: &str = "/Applications/Siderostat.app";
/// Staging directory for the intermediate component package.
const PKG_STAGING_DIR: &str = "build/pkg-dev";
const PREINSTALL_SCRIPT_NAME: &str = "preinstall";
const POSTINSTALL_SCRIPT_NAME: &str = "postinstall";

/// Installer runs this script as root before replacing the app bundle. It is
/// deliberately restricted to the exact installed Monitor executable path;
/// it does not stop the runtime helper or any ds4-server child.
const PREINSTALL_SCRIPT: &str = r##"#!/bin/sh
set -eu

APP_EXECUTABLE='/Applications/Siderostat.app/Contents/MacOS/Siderostat'
WAIT_STEPS=50

running_monitor_pids() {
    /bin/ps -axo pid=,command= 2>/dev/null | /usr/bin/awk -v expected="$APP_EXECUTABLE" '
        {
            pid = $1
            command = $0
            sub(/^[[:space:]]*[0-9]+[[:space:]]+/, "", command)
            if (command == expected || index(command, expected " ") == 1) {
                print pid
            }
        }
    '
}

log_message() {
    /usr/bin/logger -t SiderostatInstaller "$1" 2>/dev/null || true
}

pids=$(running_monitor_pids)
if [ -z "$pids" ]; then
    exit 0
fi

log_message 'terminating the existing Siderostat Monitor before upgrade'
for pid in $pids; do
    /bin/kill -TERM "$pid" 2>/dev/null || true
done

step=0
while [ "$step" -lt "$WAIT_STEPS" ]; do
    remaining=$(running_monitor_pids)
    [ -z "$remaining" ] && exit 0
    /bin/sleep 0.2
    step=$((step + 1))
done

remaining=$(running_monitor_pids)
if [ -n "$remaining" ]; then
    log_message 'Siderostat Monitor did not exit after TERM; forcing only the exact Monitor path'
    for pid in $remaining; do
        /bin/kill -KILL "$pid" 2>/dev/null || true
    done
fi

step=0
while [ "$step" -lt "$WAIT_STEPS" ]; do
    remaining=$(running_monitor_pids)
    [ -z "$remaining" ] && exit 0
    /bin/sleep 0.2
    step=$((step + 1))
done

log_message 'could not stop the existing Siderostat Monitor'
exit 1
"##;

/// Launch the newly installed app in the active console user's GUI session.
/// Registration and approval remain app-side, in the user's session; this
/// script only provides the natural first-launch handoff after installation.
const POSTINSTALL_SCRIPT: &str = r##"#!/bin/sh
set -eu

APP='/Applications/Siderostat.app'
CONSOLE_USER=$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || true)
if [ -z "$CONSOLE_USER" ] || [ "$CONSOLE_USER" = root ]; then
    /usr/bin/logger -t SiderostatInstaller 'no active GUI user; skipping first launch' 2>/dev/null || true
    exit 0
fi

CONSOLE_UID=$(/usr/bin/id -u "$CONSOLE_USER" 2>/dev/null || true)
if [ -z "$CONSOLE_UID" ]; then
    /usr/bin/logger -t SiderostatInstaller 'could not resolve active GUI user; skipping first launch' 2>/dev/null || true
    exit 0
fi

/usr/bin/logger -t SiderostatInstaller 'launching Siderostat after installation' 2>/dev/null || true
/bin/launchctl asuser "$CONSOLE_UID" /usr/bin/open -a "$APP" 2>/dev/null || \
    /usr/bin/logger -t SiderostatInstaller 'Siderostat first launch could not be requested' 2>/dev/null || true
exit 0
"##;

/// Bundle-specific Installer behavior for the fixed `/Applications` payload.
/// In particular, relocation must be disabled so an existing bundle with the
/// same identifier cannot redirect a clean install into another directory.
/// `{{VERSION_CHECKED}}` is replaced with `true` for normal packages and
/// `false` only for an explicitly requested rollback package.
const COMPONENT_PLIST_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
    <dict>
        <key>BundleHasStrictIdentifier</key>
        <true/>
        <key>BundleIsRelocatable</key>
        <false/>
        <key>BundleIsVersionChecked</key>
        <{{VERSION_CHECKED}}/>
        <key>BundleOverwriteAction</key>
        <string>upgrade</string>
        <key>RootRelativeBundlePath</key>
        <string>Siderostat.app</string>
    </dict>
</array>
</plist>
"#;

fn component_plist(rollback: bool) -> String {
    let version_checked = if rollback { "false" } else { "true" };
    COMPONENT_PLIST_TEMPLATE.replace("{{VERSION_CHECKED}}", version_checked)
}

pub(crate) fn artifact_suffix(rollback: bool) -> &'static str {
    if rollback { "-rollback" } else { "" }
}

pub(crate) fn package_filename(version: &str, rollback: bool) -> String {
    format!(
        "Siderostat-{}{suffix}.pkg",
        version,
        suffix = artifact_suffix(rollback)
    )
}

pub(crate) fn component_filename(version: &str, rollback: bool) -> String {
    format!(
        "Siderostat-{}{}-component.pkg",
        version,
        artifact_suffix(rollback)
    )
}

pub(crate) fn metadata_filename(version: &str, rollback: bool) -> String {
    format!(
        "Siderostat-{}{}.metadata.json",
        version,
        artifact_suffix(rollback)
    )
}

pub(crate) fn notary_log_filename(version: &str, rollback: bool) -> String {
    format!(
        "Siderostat-{}{}-notary.json",
        version,
        artifact_suffix(rollback)
    )
}

/// Build a flat `.pkg` from an app-dev bundle.
pub fn pkg_dev(args: &PkgDevArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("pkg-dev requires macOS (pkgbuild/productbuild/pkgutil)");
    }
    let root = std::env::current_dir().context("resolve repository root")?;
    let app = args.app_dir.join("Siderostat.app");
    anyhow::ensure!(app.is_dir(), "app bundle missing: {}", app.display());

    let staging = root.join(PKG_STAGING_DIR);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create staging {}", staging.display()))?;
    let component_pkg = staging.join(component_filename(&args.version, args.rollback));
    let product_pkg = args
        .output_dir
        .join(package_filename(&args.version, args.rollback));
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output {}", args.output_dir.display()))?;

    // 1. component package with the app as the single payload.
    build_component(&app, &component_pkg, &args.version, args.rollback)?;

    // 2. product archive wrapping the component.
    build_product(&component_pkg, &product_pkg, &args.version, None)?;

    // 3. expand and inspect: one payload item and the controlled install scripts,
    // and no forbidden paths.
    let expand_dir = staging.join("expand");
    if expand_dir.exists() {
        std::fs::remove_dir_all(&expand_dir)
            .with_context(|| format!("clean expand {}", expand_dir.display()))?;
    }
    util::run(
        "pkgutil",
        &[
            OsStr::new("--expand-full"),
            OsStr::new(&product_pkg),
            OsStr::new(&expand_dir),
        ],
    )?;
    let report = inspect_expanded(&expand_dir)?;
    anyhow::ensure!(
        report.payload == ["Siderostat.app"],
        "expected one Siderostat.app payload, found {:?}",
        report.payload
    );
    let mut scripts = report.scripts.clone();
    scripts.sort();
    let mut expected_scripts = vec![PREINSTALL_SCRIPT_NAME, POSTINSTALL_SCRIPT_NAME];
    expected_scripts.sort();
    anyhow::ensure!(
        scripts == expected_scripts,
        "unexpected installer scripts: {:?}",
        report.scripts
    );
    anyhow::ensure!(
        report.forbidden.is_empty(),
        "forbidden package paths: {:?}",
        report.forbidden
    );
    verify_bundle_version_mode(&expand_dir, args.rollback)?;
    util::tracing_log(&format!("package payload: {}", report.payload.join(", ")));
    util::tracing_log(&format!("package scripts: {:?}", report.scripts));
    util::tracing_log(&format!("wrote {}", product_pkg.display()));
    Ok(())
}

/// Run `pkgbuild` to create the component package with one app payload.
///
/// The app is copied under a temporary root and packaged with `--root` rather
/// than `--component`. `--component` adds a relocatable bundle declaration;
/// Installer may then move an existing bundle with the same identifier out of
/// `/Applications`, which is unsafe for a clean install and for upgrade
/// verification.
pub(crate) fn build_component(
    app: &Path,
    component_pkg: &Path,
    version: &str,
    rollback: bool,
) -> Result<()> {
    let staging = component_pkg
        .parent()
        .context("component package has no parent directory")?
        .join("payload-root");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clean package payload staging {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create package payload staging {}", staging.display()))?;
    let staged_app = staging.join("Siderostat.app");
    util::run("ditto", &[app.as_os_str(), staged_app.as_os_str()])
        .with_context(|| format!("stage app payload {}", staged_app.display()))?;

    let component_plist_path = component_pkg
        .parent()
        .context("component package has no parent directory")?
        .join("component-plist.xml");
    std::fs::write(&component_plist_path, component_plist(rollback))
        .with_context(|| format!("write component plist {}", component_plist_path.display()))?;

    let scripts = component_pkg
        .parent()
        .context("component package has no parent directory")?
        .join("scripts");
    if scripts.exists() {
        std::fs::remove_dir_all(&scripts)
            .with_context(|| format!("clean installer scripts {}", scripts.display()))?;
    }
    std::fs::create_dir_all(&scripts)
        .with_context(|| format!("create installer scripts {}", scripts.display()))?;
    let preinstall = scripts.join(PREINSTALL_SCRIPT_NAME);
    std::fs::write(&preinstall, PREINSTALL_SCRIPT)
        .with_context(|| format!("write installer script {}", preinstall.display()))?;
    std::fs::set_permissions(&preinstall, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("make installer script executable {}", preinstall.display()))?;
    let postinstall = scripts.join(POSTINSTALL_SCRIPT_NAME);
    std::fs::write(&postinstall, POSTINSTALL_SCRIPT)
        .with_context(|| format!("write installer script {}", postinstall.display()))?;
    std::fs::set_permissions(&postinstall, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("make installer script executable {}", postinstall.display()))?;

    let result = util::run(
        "pkgbuild",
        &[
            OsStr::new("--root"),
            OsStr::new(&staging),
            OsStr::new("--component-plist"),
            OsStr::new(&component_plist_path),
            OsStr::new("--scripts"),
            OsStr::new(&scripts),
            OsStr::new("--install-location"),
            OsStr::new(INSTALL_LOCATION),
            OsStr::new("--identifier"),
            OsStr::new(COMPONENT_IDENTIFIER),
            OsStr::new("--version"),
            OsStr::new(version),
            OsStr::new(component_pkg),
        ],
    )
    .with_context(|| format!("pkgbuild component {}", component_pkg.display()));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&scripts);
    let _ = std::fs::remove_file(&component_plist_path);
    result.map(|_| ())
}

/// Run `productbuild` to wrap the component into the final product archive.
pub(crate) fn build_product(
    component_pkg: &Path,
    product_pkg: &Path,
    version: &str,
    installer_identity: Option<&str>,
) -> Result<()> {
    let mut args = vec![
        OsString::from("--package"),
        component_pkg.as_os_str().to_os_string(),
        OsString::from("--identifier"),
        OsString::from(PRODUCT_IDENTIFIER),
        OsString::from("--version"),
        OsString::from(version),
    ];
    if let Some(identity) = installer_identity {
        args.extend([
            OsString::from("--sign"),
            OsString::from(identity),
            OsString::from("--timestamp"),
        ]);
    }
    args.push(product_pkg.as_os_str().to_os_string());
    let arg_refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    util::run("productbuild", &arg_refs)
        .with_context(|| format!("productbuild {}", product_pkg.display()))?;
    Ok(())
}

/// Result of inspecting an expanded flat package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedPackageReport {
    /// Top-level items in the `Payload` directory (install-time destinations).
    pub payload: Vec<String>,
    /// Installer script files found (must contain only the controlled hooks).
    pub scripts: Vec<String>,
    /// Forbidden paths found (must be empty).
    pub forbidden: Vec<String>,
}

/// Inspect an expanded flat package: verify exactly one payload item, the
/// controlled installer scripts, and no forbidden paths. Pure so it can be
/// unit-tested against a fixture. `scripts` is non-empty when a `Scripts`
/// directory or a `.postinstall`/`.preinstall` file is present; `forbidden` flags
/// `/usr/local/bin`, LaunchAgents, or LaunchDaemons payload entries.
pub fn inspect_expanded(expand_dir: &Path) -> Result<ExpandedPackageReport> {
    let mut report = ExpandedPackageReport {
        payload: Vec::new(),
        scripts: Vec::new(),
        forbidden: Vec::new(),
    };

    let mut payload_dirs = Vec::new();
    collect_dirs_named(expand_dir, "Payload", &mut payload_dirs)?;
    if let [payload_dir] = payload_dirs.as_slice() {
        for entry in std::fs::read_dir(payload_dir)
            .with_context(|| format!("read Payload {}", payload_dir.display()))?
        {
            let entry = entry.context("read Payload entry")?;
            let name = entry.file_name().to_string_lossy().into_owned();
            report.payload.push(name);
            // Payload 内 entry を payload_dir に対して相対化して
            // install-relative パス（例: Applications）を得る。
            let path = entry.path();
            let rel = path.strip_prefix(payload_dir).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().into_owned();
            // Forbidden install destinations: any non-Applications payload.
            if is_forbidden_payload(&rel_str) {
                report.forbidden.push(rel_str);
            }
        }
    } else {
        report.forbidden.push(format!(
            "expected one Payload directory, found {}",
            payload_dirs.len()
        ));
    }

    // Installer scripts: any Scripts directory or pre/postinstall file.
    let mut scripts_dirs = Vec::new();
    collect_dirs_named(expand_dir, "Scripts", &mut scripts_dirs)?;
    for scripts_dir in scripts_dirs {
        for entry in std::fs::read_dir(&scripts_dir)
            .with_context(|| format!("read Scripts {}", scripts_dir.display()))?
        {
            let entry = entry.context("read installer script entry")?;
            report
                .scripts
                .push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let mut script_files = Vec::new();
    collect_files_named(
        expand_dir,
        &[".preinstall", ".postinstall"],
        &mut script_files,
    )?;
    for script in script_files {
        report.scripts.push(
            script
                .strip_prefix(expand_dir)
                .unwrap_or(&script)
                .display()
                .to_string(),
        );
    }

    Ok(report)
}

fn verify_bundle_version_mode(expand_dir: &Path, rollback: bool) -> Result<()> {
    let mut package_infos = Vec::new();
    collect_files_named(expand_dir, &["PackageInfo"], &mut package_infos)?;
    anyhow::ensure!(
        package_infos.len() == 1,
        "expected one component PackageInfo, found {}",
        package_infos.len()
    );
    let package_info = std::fs::read_to_string(&package_infos[0])
        .with_context(|| format!("read component PackageInfo {}", package_infos[0].display()))?;
    anyhow::ensure!(
        bundle_version_mode_matches(&package_info, rollback),
        "component PackageInfo bundle-version mode does not match rollback={rollback}"
    );
    Ok(())
}

fn bundle_version_mode_matches(package_info: &str, rollback: bool) -> bool {
    let has_empty_version_element = package_info.contains("<bundle-version/>");
    let has_populated_version_element =
        package_info.contains("<bundle-version>") && package_info.contains("</bundle-version>");
    if rollback {
        has_empty_version_element && !has_populated_version_element
    } else {
        has_populated_version_element && !has_empty_version_element
    }
}

fn collect_dirs_named(root: &Path, name: &str, found: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(root).with_context(|| format!("read directory {}", root.display()))?
    {
        let entry = entry.context("read directory entry")?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|value| value.to_str()) == Some(name) {
                found.push(path.clone());
            }
            collect_dirs_named(&path, name, found)?;
        }
    }
    Ok(())
}

fn collect_files_named(
    root: &Path,
    names: &[&str],
    found: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(root).with_context(|| format!("read directory {}", root.display()))?
    {
        let entry = entry.context("read directory entry")?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_named(&path, names, found)?;
        } else if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| names.contains(&value))
        {
            found.push(path);
        }
    }
    Ok(())
}

/// Whether a payload entry path lands on a forbidden install destination.
/// Payload entries are relative to the install location. The only allowed
/// payload is `/Applications/Siderostat.app`, so the top-level `Applications`
/// directory (and anything beneath it) is allowed; any other top-level entry
/// is forbidden.
fn is_forbidden_payload(rel: &str) -> bool {
    let rel = rel.trim_start_matches('/');
    !(rel == "Siderostat.app"
        || rel.starts_with("Siderostat.app/")
        || rel == "Applications"
        || rel.starts_with("Applications/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_install_location_are_stable() {
        // 同一 receipt ID / version で upgrade 可能にする固定値（spec 9.1）。
        assert_eq!(COMPONENT_IDENTIFIER, "dev.siderostat-ds4-proxy.pkg");
        assert_eq!(PRODUCT_IDENTIFIER, "dev.siderostat-ds4-proxy.product");
        assert_eq!(INSTALL_LOCATION, "/Applications");
        assert_eq!(PAYLOAD_PATH, "/Applications/Siderostat.app");
    }

    #[test]
    fn component_plist_disables_bundle_relocation() {
        let plist = component_plist(false);
        assert!(plist.contains("<key>BundleIsRelocatable</key>\n        <false/>"));
        assert!(
            plist.contains("<key>BundleOverwriteAction</key>\n        <string>upgrade</string>")
        );
    }

    #[test]
    fn component_plist_version_check_is_disabled_only_for_rollback() {
        let normal = component_plist(false);
        let rollback = component_plist(true);

        assert!(normal.contains("<key>BundleIsVersionChecked</key>\n        <true/>"));
        assert!(rollback.contains("<key>BundleIsVersionChecked</key>\n        <false/>"));
        assert_ne!(normal, rollback);
    }

    #[test]
    fn package_info_bundle_version_mode_is_explicit() {
        let normal =
            "<bundle-version>\n  <bundle id=\"dev.siderostat-ds4-proxy\"/>\n</bundle-version>";
        let rollback = "<bundle-version/>";

        assert!(bundle_version_mode_matches(normal, false));
        assert!(!bundle_version_mode_matches(normal, true));
        assert!(bundle_version_mode_matches(rollback, true));
        assert!(!bundle_version_mode_matches(rollback, false));
    }

    #[test]
    fn preinstall_script_is_scoped_to_the_installed_monitor_path() {
        assert!(
            PREINSTALL_SCRIPT.contains("/Applications/Siderostat.app/Contents/MacOS/Siderostat")
        );
        assert!(PREINSTALL_SCRIPT.contains("/bin/kill -TERM"));
        assert!(PREINSTALL_SCRIPT.contains("/bin/kill -KILL"));
        assert!(!PREINSTALL_SCRIPT.contains("killall"));
        assert!(!PREINSTALL_SCRIPT.contains("pkill"));
        assert!(!PREINSTALL_SCRIPT.contains("/Library/LaunchAgents"));
    }

    #[test]
    fn postinstall_launches_the_app_in_the_console_user_session() {
        assert!(POSTINSTALL_SCRIPT.contains("launchctl asuser"));
        assert!(POSTINSTALL_SCRIPT.contains("/usr/bin/open"));
        assert!(POSTINSTALL_SCRIPT.contains("/Applications/Siderostat.app"));
        assert!(!POSTINSTALL_SCRIPT.contains("Library/Application Support"));
    }

    #[test]
    fn forbidden_payload_detection_rejects_non_app_paths() {
        // Payload は /Applications/Siderostat.app 一項目に限定。
        assert!(!is_forbidden_payload("Applications/Siderostat.app"));
        assert!(!is_forbidden_payload("Applications"));
        assert!(is_forbidden_payload("usr/local/bin/siderostat"));
        assert!(is_forbidden_payload(
            "Library/LaunchAgents/local.siderostat.runtime.plist"
        ));
        assert!(is_forbidden_payload("Library/LaunchDaemons/x.plist"));
        assert!(is_forbidden_payload("etc/something"));
    }

    #[test]
    fn inspect_expanded_detects_single_payload_and_no_scripts() {
        let dir =
            std::env::temp_dir().join(format!("siderostat-pkg-inspect-{}", std::process::id()));
        let payload = dir.join("Payload/Applications/Siderostat.app");
        std::fs::create_dir_all(&payload).unwrap();
        // Payload 直下の Applications ディレクトリを payload 一項目として数える。
        std::fs::create_dir_all(dir.join("Payload/Applications")).unwrap();

        let report = inspect_expanded(&dir).unwrap();
        // 一項目（Applications）。
        assert_eq!(report.payload, vec!["Applications"]);
        assert!(report.scripts.is_empty());
        assert!(report.forbidden.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn inspect_expanded_detects_installer_scripts() {
        let dir = std::env::temp_dir().join(format!(
            "siderostat-pkg-inspect-scripts-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("Payload/Applications")).unwrap();
        std::fs::create_dir_all(dir.join("Scripts")).unwrap();
        std::fs::write(dir.join("Scripts/postinstall"), b"#!/bin/sh").unwrap();

        let report = inspect_expanded(&dir).unwrap();
        assert_eq!(report.scripts, vec!["postinstall"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn inspect_expanded_detects_forbidden_payload_paths() {
        let dir = std::env::temp_dir().join(format!(
            "siderostat-pkg-inspect-forbidden-{}",
            std::process::id()
        ));
        // Payload 直下に /usr/local/bin を置く forbidden fixture。
        std::fs::create_dir_all(dir.join("Payload/usr/local/bin")).unwrap();
        std::fs::create_dir_all(dir.join("Payload/Applications")).unwrap();

        let report = inspect_expanded(&dir).unwrap();
        assert!(
            !report.forbidden.is_empty(),
            "must flag /usr/local/bin payload"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
