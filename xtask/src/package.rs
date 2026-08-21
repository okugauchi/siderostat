//! Scriptless flat `.pkg` builder (E-01).
//!
//! `cargo xtask pkg-dev` turns an `app-dev` bundle into a component package
//! and product archive with a single `/Applications/Siderostat.app` payload,
//! without certificate or notary credential. No `preinstall` / `postinstall`
//! script is generated, and the expanded package is inspected so a forbidden
//! path or installer script fails the build.
//!
//! Identifier / version are fixed here (spec §9.1) so a later signed release
//! shares the same receipt ID and version comparison for upgrade.
//! This module never touches user data, LaunchAgents, or config.

use crate::bundle::PkgDevArgs;
use crate::util;
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
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

/// Build a scriptless flat `.pkg` from an app-dev bundle.
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
    let component_pkg = staging.join(format!("Siderostat-{}-component.pkg", args.version));
    let product_pkg = args
        .output_dir
        .join(format!("Siderostat-{}.pkg", args.version));
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output {}", args.output_dir.display()))?;

    // 1. component package with the app as the single payload.
    build_component(&app, &component_pkg, &args.version)?;

    // 2. product archive wrapping the component.
    build_product(&component_pkg, &product_pkg, &args.version, None)?;

    // 3. expand and inspect: one payload item, no scripts, no forbidden paths.
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
    anyhow::ensure!(
        report.scripts.is_empty(),
        "installer scripts are forbidden: {:?}",
        report.scripts
    );
    anyhow::ensure!(
        report.forbidden.is_empty(),
        "forbidden package paths: {:?}",
        report.forbidden
    );
    util::tracing_log(&format!("package payload: {}", report.payload.join(", ")));
    util::tracing_log(&format!("package scripts: {:?}", report.scripts));
    util::tracing_log(&format!("wrote {}", product_pkg.display()));
    Ok(())
}

/// Run `pkgbuild` to create the component package with one app payload.
pub(crate) fn build_component(app: &Path, component_pkg: &Path, version: &str) -> Result<()> {
    util::run(
        "pkgbuild",
        &[
            OsStr::new("--component"),
            OsStr::new(app),
            OsStr::new("--install-location"),
            OsStr::new(INSTALL_LOCATION),
            OsStr::new("--identifier"),
            OsStr::new(COMPONENT_IDENTIFIER),
            OsStr::new("--version"),
            OsStr::new(version),
            OsStr::new(component_pkg),
        ],
    )
    .with_context(|| format!("pkgbuild component {}", component_pkg.display()))?;
    Ok(())
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
    /// Installer script files found (must be empty).
    pub scripts: Vec<String>,
    /// Forbidden paths found (must be empty).
    pub forbidden: Vec<String>,
}

/// Inspect an expanded flat package: verify exactly one payload item, no
/// installer scripts, and no forbidden paths. Pure so it can be unit-tested
/// against a fixture. `scripts` is non-empty when a `Scripts` directory or a
/// `.postinstall`/`.preinstall` file is present; `forbidden` flags
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
        for entry in std::fs::read_dir(&payload_dir)
            .with_context(|| format!("read Payload {}", payload_dir.display()))?
        {
            let entry = entry.context("read Payload entry")?;
            let name = entry.file_name().to_string_lossy().into_owned();
            report.payload.push(name);
            // Payload 内 entry を payload_dir に対して相対化して
            // install-relative パス（例: Applications）を得る。
            let path = entry.path();
            let rel = path.strip_prefix(&payload_dir).unwrap_or(&path);
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
