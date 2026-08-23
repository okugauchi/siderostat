//! Release DMG and Finder-facing Uninstaller.app builder (E-06).

use crate::util;
use anyhow::{Context, Result, bail};
use clap::Args;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub const UNINSTALLER_IDENTIFIER: &str = "dev.siderostat-ds4-proxy.uninstaller";
pub const DMG_IDENTIFIER: &str = "dev.siderostat-ds4-proxy.dmg";
pub const UNINSTALLER_EXECUTABLE: &str = "Siderostat Uninstaller";
pub const README_FILENAME: &str = "README.html";

#[derive(Args, Clone)]
pub struct DmgDevArgs {
    /// Existing Siderostat.app bundle used as the source for the monitor binary/resources.
    #[arg(long)]
    pub app_dir: PathBuf,
    /// Existing .pkg to place beside the uninstaller.
    #[arg(long)]
    pub package: PathBuf,
    /// Release version used in the DMG volume and filename.
    #[arg(long)]
    pub version: String,
    /// Build number written to the Uninstaller.app Info.plist.
    #[arg(long)]
    pub build_number: u32,
    /// Output directory for the DMG and intermediate Uninstaller.app.
    #[arg(long, default_value = "dist")]
    pub output_dir: PathBuf,
    /// Staging directory for the mounted-DMG source tree.
    #[arg(long)]
    pub staging: Option<PathBuf>,
    /// Verify the generated bundle and DMG source file list.
    #[arg(long)]
    pub verify: bool,
    /// Mark the DMG as a rollback artifact.
    #[arg(long)]
    pub rollback: bool,
}

/// Build an ad-hoc Uninstaller.app and a development DMG.
pub fn dmg_dev(args: &DmgDevArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("dmg-dev requires macOS (codesign/hdiutil)");
    }
    anyhow::ensure!(!args.version.trim().is_empty(), "version must not be empty");
    let root = std::env::current_dir().context("resolve repository root")?;
    let app = args.app_dir.join("Siderostat.app");
    let output_dir = &args.output_dir;
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("create output directory {}", output_dir.display()))?;
    let uninstaller = build_uninstaller_bundle(
        &root,
        &app,
        &args.version,
        args.build_number,
        &output_dir.join("Siderostat Uninstaller.app"),
    )?;
    let dmg = build_dmg(
        &args.package,
        &uninstaller,
        &args.version,
        args.rollback,
        output_dir,
        args.staging.as_deref(),
    )?;
    if args.verify {
        verify_uninstaller_bundle(&uninstaller)?;
        verify_mounted_dmg(
            &dmg,
            &expected_dmg_entries(
                args.package
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("package filename is not UTF-8")?,
            ),
        )?;
    }
    println!("uninstaller: {}", uninstaller.display());
    println!("dmg: {}", dmg.display());
    Ok(())
}

/// Build the Finder-launched application from the already built app's monitor
/// binary and localized resources.
pub fn build_uninstaller_bundle(
    root: &Path,
    app: &Path,
    version: &str,
    build_number: u32,
    destination: &Path,
) -> Result<PathBuf> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)
            .with_context(|| format!("clean {}", destination.display()))?;
    }
    let contents = destination.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;

    let monitor = app.join("Contents/MacOS/Siderostat");
    anyhow::ensure!(
        monitor.is_file(),
        "monitor binary missing: {}",
        monitor.display()
    );
    let executable = contents.join("MacOS").join(UNINSTALLER_EXECUTABLE);
    std::fs::copy(&monitor, &executable)
        .with_context(|| format!("copy monitor to {}", executable.display()))?;

    let template = std::fs::read_to_string(root.join("contrib/macos/Uninstaller-Info.plist.in"))
        .context("read Uninstaller-Info.plist.in")?;
    let info = template
        .replace("@VERSION@", version)
        .replace("@BUILD_NUMBER@", &build_number.to_string());
    std::fs::write(contents.join("Info.plist"), info.as_bytes())?;

    for locale in ["en.lproj", "ja.lproj"] {
        let source = app.join("Contents/Resources").join(locale);
        let destination_locale = contents.join("Resources").join(locale);
        std::fs::create_dir_all(&destination_locale)?;
        std::fs::copy(
            source.join("Localizable.strings"),
            destination_locale.join("Localizable.strings"),
        )?;
    }
    adhoc_sign(&executable)?;
    adhoc_sign(destination)?;
    Ok(destination.to_path_buf())
}

/// Create a read-only DMG source directory containing exactly the three public
/// distribution entries, then convert it to a compressed disk image.
pub fn build_dmg(
    package: &Path,
    uninstaller: &Path,
    version: &str,
    rollback: bool,
    output_dir: &Path,
    requested_staging: Option<&Path>,
) -> Result<PathBuf> {
    anyhow::ensure!(package.is_file(), "package missing: {}", package.display());
    anyhow::ensure!(
        uninstaller.is_dir(),
        "uninstaller missing: {}",
        uninstaller.display()
    );
    let staging = requested_staging
        .map(PathBuf::from)
        .unwrap_or_else(|| output_dir.join(format!(".Siderostat-dmg-staging-{version}")));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clean DMG staging {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)?;
    let package_name = package
        .file_name()
        .context("package has no filename")?
        .to_owned();
    std::fs::copy(package, staging.join(&package_name))?;
    copy_directory(uninstaller, &staging.join(UNINSTALLER_BUNDLE_NAME))?;
    std::fs::write(
        staging.join(README_FILENAME),
        readme_html(version, rollback).as_bytes(),
    )?;

    let dmg = output_dir.join(dmg_filename(version, rollback));
    util::run_live(
        "hdiutil",
        &[
            OsStr::new("create"),
            OsStr::new("-quiet"),
            OsStr::new("-volname"),
            OsStr::new(&format!("Siderostat {version}")),
            OsStr::new("-srcfolder"),
            OsStr::new(&staging),
            OsStr::new("-format"),
            OsStr::new("UDZO"),
            OsStr::new("-ov"),
            OsStr::new(&dmg),
        ],
    )?;
    Ok(dmg)
}

/// Wrap an app in a temporary zip because notarytool accepts app bundles only
/// through a supported container format. The zip is a signing/notarization
/// input and is not included in the public DMG.
pub fn zip_for_notarization(app: &Path, destination: &Path) -> Result<PathBuf> {
    anyhow::ensure!(app.is_dir(), "Uninstaller.app missing: {}", app.display());
    if destination.exists() {
        std::fs::remove_file(destination)
            .with_context(|| format!("clean notarization zip {}", destination.display()))?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    util::run_live(
        "ditto",
        &[
            OsStr::new("-c"),
            OsStr::new("-k"),
            OsStr::new("--keepParent"),
            OsStr::new(app),
            OsStr::new(destination),
        ],
    )?;
    Ok(destination.to_path_buf())
}

const UNINSTALLER_BUNDLE_NAME: &str = "Siderostat Uninstaller.app";

pub fn dmg_filename(version: &str, rollback: bool) -> String {
    if rollback {
        format!("Siderostat-{version}-rollback.dmg")
    } else {
        format!("Siderostat-{version}.dmg")
    }
}

pub fn expected_dmg_entries(package_filename: &str) -> Vec<String> {
    vec![
        package_filename.to_owned(),
        UNINSTALLER_BUNDLE_NAME.to_owned(),
        README_FILENAME.to_owned(),
    ]
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else {
            std::fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn readme_html(version: &str, rollback: bool) -> String {
    let mode = if rollback {
        "rollback package"
    } else {
        "release package"
    };
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Siderostat {version}</title><h1>Siderostat {version}</h1><p>Install the {mode} with the package installer. To remove Siderostat, open <strong>Siderostat Uninstaller.app</strong>.</p><p>The uninstaller removes the application and its services but preserves configuration, secrets, manifests, cluster state, models, and KV cache.</p>"
    )
}

fn adhoc_sign(path: &Path) -> Result<()> {
    util::run_live(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--sign"),
            OsStr::new("-"),
            OsStr::new("--timestamp"),
            OsStr::new(path),
        ],
    )
}

pub fn verify_uninstaller_bundle(app: &Path) -> Result<()> {
    util::run(
        "plutil",
        &[
            OsStr::new("-lint"),
            OsStr::new(&app.join("Contents/Info.plist")),
        ],
    )?;
    let identifier = util::run(
        "/usr/libexec/PlistBuddy",
        &[
            OsStr::new("-c"),
            OsStr::new("Print :CFBundleIdentifier"),
            OsStr::new(&app.join("Contents/Info.plist")),
        ],
    )?;
    anyhow::ensure!(
        String::from_utf8_lossy(&identifier).trim() == UNINSTALLER_IDENTIFIER,
        "unexpected Uninstaller bundle identifier: {}",
        String::from_utf8_lossy(&identifier).trim()
    );
    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--deep"),
            OsStr::new("--strict"),
            OsStr::new(app),
        ],
    )?;
    Ok(())
}

pub fn verify_mounted_dmg(dmg: &Path, expected: &[String]) -> Result<()> {
    let mount = std::env::temp_dir().join(format!("siderostat-dmg-mount-{}", std::process::id()));
    if mount.exists() {
        std::fs::remove_dir_all(&mount)
            .with_context(|| format!("clean DMG mount point {}", mount.display()))?;
    }
    std::fs::create_dir_all(&mount)?;
    util::run_live(
        "hdiutil",
        &[
            OsStr::new("attach"),
            OsStr::new("-readonly"),
            OsStr::new("-nobrowse"),
            OsStr::new("-mountpoint"),
            OsStr::new(&mount),
            OsStr::new(dmg),
        ],
    )?;
    let mut actual = std::fs::read_dir(&mount)
        .with_context(|| format!("read mounted DMG {}", mount.display()))?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    actual.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    let detach = util::run_live("hdiutil", &[OsStr::new("detach"), OsStr::new(&mount)]);
    detach?;
    anyhow::ensure!(
        actual == expected,
        "DMG entries differ: actual={actual:?} expected={expected:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dmg_filename_marks_rollback_without_changing_release_name() {
        assert_eq!(dmg_filename("0.3.0", false), "Siderostat-0.3.0.dmg");
        assert_eq!(dmg_filename("0.3.0", true), "Siderostat-0.3.0-rollback.dmg");
    }

    #[test]
    fn dmgs_have_only_package_uninstaller_and_readme_entries() {
        assert_eq!(
            expected_dmg_entries("Siderostat-0.3.0.pkg"),
            vec![
                "Siderostat-0.3.0.pkg",
                "Siderostat Uninstaller.app",
                "README.html"
            ]
        );
    }

    #[test]
    fn readme_states_data_preservation_contract() {
        let readme = readme_html("0.3.0", false);
        assert!(readme.contains("Siderostat Uninstaller.app"));
        assert!(readme.contains("preserves configuration"));
    }

    #[test]
    fn notarization_zip_is_not_a_public_dmg_entry() {
        assert!(
            !expected_dmg_entries("Siderostat-0.3.0.pkg")
                .iter()
                .any(|entry| entry.ends_with(".zip"))
        );
    }
}
