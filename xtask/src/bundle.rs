//! Deterministic `Siderostat.app` bundle builder (B-03) and `.pkg` args (E-01).
//!
//! `cargo xtask app-dev --version <semver> --build-number <integer>` builds an
//! ad-hoc signed application bundle with the production layout, without touching
//! user data or `/Applications`. The staging directory is rebuilt from an empty
//! state on every run so two builds from identical inputs produce an identical
//! file list, plist values, and unsigned content digest.

use crate::util;
use anyhow::{Context, Result};
use clap::Args;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// Default staging root under the repository (not user data).
const DEFAULT_STAGING: &str = "build/app-dev";

/// 1024x1024 opaque deep-blue PNG placeholder icon, base64. The real icon is
/// provided by the user before release; this placeholder keeps the bundle layout
/// complete without committing a binary asset to the repository.
const PLACEHOLDER_ICON_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAABAAAAAQACAYAAAB/HSuDAAAWEElEQVR42u3YQREAAAQAQUkEEVFpCkhg9rEF7nmR1QMAAAD8FiIAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGgAgAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABIAIAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAACAEAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAiAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAIgAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAaACAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEgBAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEgAgAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYACIAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAiAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABoAQAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABoAIAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAACAASACAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAIgAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAAAYAEIAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAAAGAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAABgAAAAAgAEAAAAABgAAAABgAAAAAAAGAAAAAGAAAAAAAAYAAAAAYAAAAAAABgAAAABgAAAAAIABAAAAABgAAAAAgAEAAAAAGAAAAACAAQAAAAAYAAAAAIABAAAAAFwWbibDACqwRYsAAAAASUVORK5CYII=";

/// `app-dev` bundle builder arguments.
#[derive(Args, Clone)]
pub struct AppDevArgs {
    /// Release semver version written to CFBundleShortVersionString.
    #[arg(long)]
    pub version: String,
    /// Monotonic build number written to CFBundleVersion.
    #[arg(long)]
    pub build_number: u32,
    /// Staging directory (default build/app-dev). Rebuilt from empty each run.
    #[arg(long)]
    pub staging: Option<PathBuf>,
    /// Optional existing AppIcon.icns; when absent a placeholder icns is generated.
    #[arg(long)]
    pub icon: Option<PathBuf>,
    /// Run the static bundle verification after building.
    #[arg(long)]
    pub verify: bool,
}

/// `pkg-dev` arguments (E-01); wired here so the CLI is stable.
#[derive(Args, Clone)]
pub struct PkgDevArgs {
    /// AppDev staging directory containing the built Siderostat.app.
    #[arg(long)]
    pub app_dir: PathBuf,
    /// Release semver version for the package receipt.
    #[arg(long)]
    pub version: String,
    /// Output directory for the .pkg.
    #[arg(long, default_value = "dist")]
    pub output_dir: PathBuf,
}

/// Build the application bundle at `staging/Siderostat.app`.
pub fn app_dev(args: &AppDevArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("app-dev requires macOS (codesign/plutil/sips/iconutil)");
    }
    let root = repo_root()?;
    let staging = args
        .staging
        .clone()
        .unwrap_or_else(|| root.join(DEFAULT_STAGING));
    let app = staging.join("Siderostat.app");
    let contents = app.join("Contents");

    // 1. Rebuild staging from an empty state every run.
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("clean staging {}", staging.display()))?;
    }
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Helpers"))?;
    std::fs::create_dir_all(contents.join("Library/LaunchAgents"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;

    // 2. Fixed-order placement of template, binaries, and resources.
    let info = render_info_plist(&root, &args.version, args.build_number)?;
    std::fs::write(contents.join("Info.plist"), info)?;

    let monitor_bin = root.join("target/release/siderostat-monitor");
    anyhow::ensure!(
        monitor_bin.is_file(),
        "monitor binary missing; run `cargo build --release -p siderostat-monitor` first: {}",
        monitor_bin.display()
    );
    std::fs::copy(&monitor_bin, contents.join("MacOS/Siderostat"))?;

    let runtime_bin = root.join("target/release/siderostat");
    anyhow::ensure!(
        runtime_bin.is_file(),
        "runtime binary missing; run `cargo build --release` first: {}",
        runtime_bin.display()
    );
    std::fs::copy(&runtime_bin, contents.join("Helpers/siderostat-runtime"))?;

    // Bundle-internal LaunchAgent plist (bundle-relative helper path).
    let runtime_plist = root.join("contrib/macos/dev.siderostat-ds4-proxy.runtime.plist");
    std::fs::copy(
        &runtime_plist,
        contents.join("Library/LaunchAgents/dev.siderostat-ds4-proxy.runtime.plist"),
    )?;

    // Resources.
    let resources = root.join("contrib/macos/Resources");
    for name in ["LICENSE", "THIRD-PARTY-NOTICES.md", "default-config.toml"] {
        std::fs::copy(resources.join(name), contents.join("Resources").join(name))?;
    }
    let icon_dest = contents.join("Resources/AppIcon.icns");
    match &args.icon {
        Some(explicit) => {
            anyhow::ensure!(explicit.is_file(), "icon not found: {}", explicit.display());
            std::fs::copy(explicit, &icon_dest)?;
        }
        None => generate_placeholder_icon(&staging, &icon_dest)?,
    }

    // 3. Ad-hoc sign inside-out: helper first, then the app. Never --deep for signing.
    adhoc_sign(&contents.join("Helpers/siderostat-runtime"))?;
    adhoc_sign(&app)?;

    // 4. Verification.
    verify_bundle(&app)?;

    // 5. Report output and generated files.
    println!("bundle: {}", app.display());
    for entry in walk(&app) {
        println!("  {}", entry.display());
    }
    if args.verify {
        verify_bundle_strict(&app)?;
        println!("verification: PASS (plutil, codesign --verify --deep --strict)");
    }
    Ok(())
}

fn repo_root() -> Result<PathBuf> {
    // xtask runs from the repository root via `cargo xtask`.
    std::env::current_dir().context("resolve repository root")
}

/// Render Info.plist from the template, substituting version and build number.
fn render_info_plist(root: &Path, version: &str, build_number: u32) -> Result<Vec<u8>> {
    let template = std::fs::read_to_string(root.join("contrib/macos/Info.plist.in"))
        .context("read Info.plist.in")?;
    let rendered = template
        .replace("@VERSION@", version)
        .replace("@BUILD_NUMBER@", &build_number.to_string());
    Ok(rendered.into_bytes())
}

/// Generate a placeholder AppIcon.icns from the embedded PNG using sips + iconutil.
fn generate_placeholder_icon(staging: &Path, dest: &Path) -> Result<()> {
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD
        .decode(PLACEHOLDER_ICON_PNG_BASE64)
        .context("decode placeholder icon")?;
    let work = staging.join("icon-work");
    std::fs::create_dir_all(&work)?;
    let master = work.join("master.png");
    std::fs::write(&master, &png)?;

    let iconset = work.join("AppIcon.iconset");
    std::fs::create_dir_all(&iconset)?;
    // iconutil expects these named files (unscaled + @2x variants).
    let sizes: &[(&str, u32, bool)] = &[
        ("icon_16x16.png", 16, false),
        ("icon_16x16@2x.png", 32, false),
        ("icon_32x32.png", 32, false),
        ("icon_32x32@2x.png", 64, false),
        ("icon_128x128.png", 128, false),
        ("icon_128x128@2x.png", 256, false),
        ("icon_256x256.png", 256, false),
        ("icon_256x256@2x.png", 512, false),
        ("icon_512x512.png", 512, false),
        ("icon_512x512@2x.png", 1024, false),
    ];
    for (name, size, _) in sizes {
        let out = iconset.join(name);
        util::run_live(
            "sips",
            &[
                OsStr::new("-z"),
                OsStr::new(&size.to_string()),
                OsStr::new(&size.to_string()),
                OsStr::new(&master),
                OsStr::new("--out"),
                OsStr::new(&out),
            ],
        )?;
    }
    util::run_live(
        "iconutil",
        &[
            OsStr::new("-c"),
            OsStr::new("icns"),
            OsStr::new(&iconset),
            OsStr::new("-o"),
            OsStr::new(dest),
        ],
    )?;
    util::tracing_log(&format!("generated placeholder icon {}", dest.display()));
    Ok(())
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
    )?;
    Ok(())
}

fn verify_bundle(app: &Path) -> Result<()> {
    // plutil -lint on Info.plist and the nested LaunchAgent plist.
    for plist in [
        app.join("Contents/Info.plist"),
        app.join("Contents/Library/LaunchAgents/dev.siderostat-ds4-proxy.runtime.plist"),
    ] {
        util::run("plutil", &[OsStr::new("-lint"), OsStr::new(&plist)])?;
    }
    // Nested identifiers and bundle version from the built Info.plist.
    let plist_out = util::run(
        "/usr/libexec/PlistBuddy",
        &[
            OsStr::new("-c"),
            OsStr::new("Print :CFBundleIdentifier"),
            OsStr::new(&app.join("Contents/Info.plist")),
        ],
    )?;
    anyhow::ensure!(
        String::from_utf8_lossy(&plist_out).trim() == "dev.siderostat-ds4-proxy",
        "unexpected CFBundleIdentifier: {}",
        String::from_utf8_lossy(&plist_out)
    );
    // Signature of nested helper and app.
    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new(&app.join("Contents/Helpers/siderostat-runtime")),
        ],
    )?;
    Ok(())
}

fn verify_bundle_strict(app: &Path) -> Result<()> {
    util::run_live(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--deep"),
            OsStr::new("--strict"),
            OsStr::new("--verbose=4"),
            OsStr::new(app),
        ],
    )?;
    Ok(())
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_info_plist_substitutes_version_and_build_number() {
        let dir =
            std::env::temp_dir().join(format!("siderostat-bundle-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("contrib/macos")).unwrap();
        std::fs::write(
            dir.join("contrib/macos/Info.plist.in"),
            "<string>@VERSION@</string>\n<string>@BUILD_NUMBER@</string>\n",
        )
        .unwrap();
        let out = render_info_plist(&dir, "0.3.0", 42).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("<string>0.3.0</string>"));
        assert!(text.contains("<string>42</string>"));
        assert!(!text.contains("@VERSION@"));
        assert!(!text.contains("@BUILD_NUMBER@"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_icon_png_decodes_to_png_signature() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(PLACEHOLDER_ICON_PNG_BASE64)
            .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.len() < 100_000);
    }

    #[test]
    fn app_dev_reports_missing_runtime_binary_clearly() {
        // A missing release binary must fail fast with an actionable message.
        // This mirrors the exact check app_dev performs, isolated from any repo
        // state, and must not touch /Applications or user data.
        let root = std::env::temp_dir().join(format!(
            "siderostat-appdev-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let missing = root.join("target/release/siderostat");
        let check = || -> Result<()> {
            if missing.is_file() {
                Ok(())
            } else {
                anyhow::bail!(
                    "runtime binary missing; run `cargo build --release` first: {}",
                    missing.display()
                )
            }
        };
        let result = check();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("runtime binary missing")
        );
        // No user data or /Applications paths are created.
        assert!(!Path::new("/Applications/Siderostat.app").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
