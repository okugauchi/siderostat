//! Developer ID signing and notarization pipeline (E-02).
//!
//! The pipeline accepts certificate identity names and a notarytool Keychain
//! profile name, but never accepts or writes credential contents. `--dry-run`
//! renders the fixed command order with the profile redacted so the release
//! gate can be reviewed before a signed artifact is produced.

use crate::{package, util};
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// Main application bundle identifier (spec §5.2).
pub const APP_IDENTIFIER: &str = "dev.siderostat-ds4-proxy";
/// Nested runtime helper identifier (spec §5.2/§8.2).
pub const RUNTIME_IDENTIFIER: &str = "dev.siderostat-ds4-proxy.runtime";
/// Default signed-package staging directory.
const SIGNING_STAGING_DIR: &str = "build/signing";

/// Arguments for the signed Developer ID package pipeline.
#[derive(Args, Clone)]
pub struct SignArgs {
    /// App-dev staging directory containing Siderostat.app.
    #[arg(long)]
    pub app_dir: PathBuf,
    /// Release semver version written to the package and metadata.
    #[arg(long)]
    pub version: String,
    /// Monotonic app build number written to metadata.
    #[arg(long)]
    pub build_number: u32,
    /// Exact Developer ID Application identity from the login Keychain.
    #[arg(long)]
    pub application_identity: String,
    /// Exact Developer ID Installer identity from the login Keychain.
    #[arg(long)]
    pub installer_identity: String,
    /// notarytool Keychain profile name; credential contents are never accepted.
    #[arg(long)]
    pub notary_profile: String,
    /// Output directory for the signed package and build metadata.
    #[arg(long, default_value = "dist")]
    pub output_dir: PathBuf,
    /// Directory for the redacted notary log.
    #[arg(long)]
    pub notary_log_dir: Option<PathBuf>,
    /// Print the fixed command order without touching the filesystem or network.
    #[arg(long)]
    pub dry_run: bool,
}

/// Run the signed app/package/notarization pipeline.
pub fn sign(args: &SignArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("sign requires macOS (codesign/productbuild/notarytool/stapler)");
    }
    validate_args(args)?;
    let root = std::env::current_dir().context("resolve repository root")?;
    let app = args.app_dir.join("Siderostat.app");
    let package = args
        .output_dir
        .join(format!("Siderostat-{}.pkg", args.version));
    let log_dir = args
        .notary_log_dir
        .clone()
        .unwrap_or_else(|| args.output_dir.join("notary"));
    let log_path = log_dir.join(format!("Siderostat-{}-notary.json", args.version));

    if args.dry_run {
        for line in dry_run_plan(&app, &package, &log_path, args) {
            println!("dry-run: {line}");
        }
        return Ok(());
    }

    anyhow::ensure!(app.is_dir(), "app bundle missing: {}", app.display());
    std::fs::create_dir_all(&args.output_dir)
        .with_context(|| format!("create output directory {}", args.output_dir.display()))?;

    let staging = root.join(SIGNING_STAGING_DIR);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("create signing staging {}", staging.display()))?;
    let component = staging.join(format!("Siderostat-{}-component.pkg", args.version));

    // 1. Inside-out Developer ID Application signing. Never use --deep to sign.
    sign_code_item(
        &app.join("Contents/Helpers/siderostat-runtime"),
        &args.application_identity,
        RUNTIME_IDENTIFIER,
    )?;
    sign_code_item(&app, &args.application_identity, APP_IDENTIFIER)?;
    verify_app(&app)?;

    // 2. Build and sign the final product archive with Developer ID Installer.
    package::build_component(&app, &component, &args.version)?;
    package::build_product(
        &component,
        &package,
        &args.version,
        Some(&args.installer_identity),
    )?;
    verify_package_signature(&package)?;

    // 3. Submit, save the notary log, staple, and validate in this order.
    let submission = submit_for_notarization(&package, &args.notary_profile)?;
    let notary_log = fetch_notary_log(&submission, &args.notary_profile)?;
    util::write(&log_path, &notary_log)
        .with_context(|| format!("save notary log {}", log_path.display()))?;
    util::run(
        "xcrun",
        &[
            OsStr::new("stapler"),
            OsStr::new("staple"),
            OsStr::new(&package),
        ],
    )?;
    util::run(
        "xcrun",
        &[
            OsStr::new("stapler"),
            OsStr::new("validate"),
            OsStr::new(&package),
        ],
    )?;
    util::run(
        "spctl",
        &[
            OsStr::new("--assess"),
            OsStr::new("--type"),
            OsStr::new("install"),
            OsStr::new("--verbose=4"),
            OsStr::new(&package),
        ],
    )?;

    let metadata = build_metadata(&app, &package, &submission, args)?;
    let metadata_path = args
        .output_dir
        .join(format!("Siderostat-{}.metadata.json", args.version));
    let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
    util::write(&metadata_path, &metadata_bytes)?;

    println!("app: {}", app.display());
    println!("package: {}", package.display());
    println!("notary_log: {}", log_path.display());
    println!("metadata: {}", metadata_path.display());
    println!("notary_submission_id: {submission}");
    println!("app_sha256: {}", metadata["app_sha256"]);
    println!("pkg_sha256: {}", metadata["pkg_sha256"]);
    Ok(())
}

fn validate_args(args: &SignArgs) -> Result<()> {
    for (label, value) in [
        ("version", args.version.as_str()),
        ("application identity", args.application_identity.as_str()),
        ("installer identity", args.installer_identity.as_str()),
        ("notary profile", args.notary_profile.as_str()),
    ] {
        anyhow::ensure!(!value.trim().is_empty(), "{label} must not be empty");
        anyhow::ensure!(
            !value.chars().any(char::is_control),
            "{label} must not contain control characters"
        );
    }
    Ok(())
}

fn sign_code_item(path: &Path, identity: &str, identifier: &str) -> Result<()> {
    anyhow::ensure!(path.exists(), "code item missing: {}", path.display());
    util::run(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--options"),
            OsStr::new("runtime"),
            OsStr::new("--timestamp"),
            OsStr::new("--sign"),
            OsStr::new(identity),
            OsStr::new("--identifier"),
            OsStr::new(identifier),
            OsStr::new(path),
        ],
    )
    .with_context(|| format!("codesign {identifier}"))?;
    Ok(())
}

fn verify_app(app: &Path) -> Result<()> {
    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("--verbose=4"),
            OsStr::new(app),
        ],
    )?;
    Ok(())
}

fn verify_package_signature(package: &Path) -> Result<()> {
    util::run(
        "pkgutil",
        &[OsStr::new("--check-signature"), OsStr::new(package)],
    )?;
    Ok(())
}

fn submit_for_notarization(package: &Path, profile: &str) -> Result<String> {
    let output = util::run(
        "xcrun",
        &[
            OsStr::new("notarytool"),
            OsStr::new("submit"),
            OsStr::new(package),
            OsStr::new("--keychain-profile"),
            OsStr::new(profile),
            OsStr::new("--wait"),
            OsStr::new("--output-format"),
            OsStr::new("json"),
        ],
    )?;
    let value: Value = serde_json::from_slice(&output).context("parse notarytool submit JSON")?;
    value
        .get("id")
        .or_else(|| value.get("submissionId"))
        .or_else(|| value.get("submission_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("notarytool submit response has no submission ID")
}

fn fetch_notary_log(submission: &str, profile: &str) -> Result<Vec<u8>> {
    util::run(
        "xcrun",
        &[
            OsStr::new("notarytool"),
            OsStr::new("log"),
            OsStr::new(submission),
            OsStr::new("--keychain-profile"),
            OsStr::new(profile),
            OsStr::new("--output-format"),
            OsStr::new("json"),
        ],
    )
}

fn build_metadata(app: &Path, package: &Path, submission: &str, args: &SignArgs) -> Result<Value> {
    let rustc = String::from_utf8(util::run("rustc", &[OsStr::new("-vV")])?)?;
    let git_commit = String::from_utf8(util::run(
        "git",
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
    )?)?;
    let rust_version = field(&rustc, "release")?;
    let target = field(&rustc, "host")?;
    Ok(json!({
        "artifact": package.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
        "version": args.version.as_str(),
        "build_number": args.build_number,
        "app_sha256": util::sha256_hex(app)?,
        "pkg_sha256": util::sha256_hex(package)?,
        "git_commit": git_commit.trim(),
        "rust_version": rust_version,
        "target": target,
        "notary_submission_id": submission,
    }))
}

fn field(output: &str, name: &str) -> Result<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .map(str::to_owned)
        .with_context(|| format!("rustc -vV has no {name} field"))
}

fn dry_run_plan(app: &Path, package: &Path, log_path: &Path, args: &SignArgs) -> Vec<String> {
    vec![
        format!(
            "codesign --force --options runtime --timestamp --sign <application identity> --identifier {RUNTIME_IDENTIFIER} {}",
            app.join("Contents/Helpers/siderostat-runtime").display()
        ),
        format!(
            "codesign --force --options runtime --timestamp --sign <application identity> --identifier {APP_IDENTIFIER} {}",
            app.display()
        ),
        format!("codesign --verify --strict --verbose=4 {}", app.display()),
        format!(
            "pkgbuild --component {} --install-location /Applications --identifier {} --version {} <component package>",
            app.display(),
            package::COMPONENT_IDENTIFIER,
            args.version
        ),
        format!("expected package payload: {}", package::PAYLOAD_PATH),
        format!(
            "productbuild --package <component package> --identifier {} --version {} --sign <installer identity> --timestamp {}",
            package::PRODUCT_IDENTIFIER,
            args.version,
            package.display()
        ),
        format!("pkgutil --check-signature {}", package.display()),
        format!(
            "xcrun notarytool submit {} --keychain-profile <redacted> --wait --output-format json",
            package.display()
        ),
        "xcrun notarytool log <submission-id> --keychain-profile <redacted> --output-format json"
            .into(),
        format!("xcrun stapler staple {}", package.display()),
        format!("xcrun stapler validate {}", package.display()),
        format!(
            "spctl --assess --type install --verbose=4 {}",
            package.display()
        ),
        format!("write notary log {}", log_path.display()),
        format!("write checksum/build metadata beside {}", package.display()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(profile: &str) -> SignArgs {
        SignArgs {
            app_dir: PathBuf::from("build/app-dev"),
            version: "0.3.0".into(),
            build_number: 7,
            application_identity: "Developer ID Application: Example (TEAMID)".into(),
            installer_identity: "Developer ID Installer: Example (TEAMID)".into(),
            notary_profile: profile.into(),
            output_dir: PathBuf::from("dist"),
            notary_log_dir: None,
            dry_run: true,
        }
    }

    #[test]
    fn dry_run_has_fixed_inside_out_and_notarization_order() {
        let input = args("private-profile-name");
        let app = input.app_dir.join("Siderostat.app");
        let package = input.output_dir.join("Siderostat-0.3.0.pkg");
        let log = input.output_dir.join("notary/Siderostat-0.3.0-notary.json");
        let plan = dry_run_plan(&app, &package, &log, &input).join("\n");
        let helper = plan.find("siderostat-runtime").expect("helper signing");
        let app_sign = plan.find("--identifier dev.siderostat-ds4-proxy ").unwrap();
        let submit = plan.find("notarytool submit").unwrap();
        let staple = plan.find("stapler staple").unwrap();
        let validate = plan.find("stapler validate").unwrap();
        assert!(helper < app_sign);
        assert!(app_sign < submit);
        assert!(submit < staple);
        assert!(staple < validate);
        assert!(plan.contains("dev.siderostat-ds4-proxy.runtime"));
        assert!(plan.contains("dev.siderostat-ds4-proxy.pkg"));
        assert!(plan.contains("dev.siderostat-ds4-proxy.product"));
    }

    #[test]
    fn dry_run_never_renders_keychain_profile() {
        let input = args("private-profile-name");
        let plan = dry_run_plan(
            &input.app_dir.join("Siderostat.app"),
            &input.output_dir.join("Siderostat-0.3.0.pkg"),
            &input.output_dir.join("notary/log.json"),
            &input,
        )
        .join("\n");
        assert!(!plan.contains(&input.notary_profile));
        assert!(plan.contains("--keychain-profile <redacted>"));
    }

    #[test]
    fn profile_and_identity_validation_rejects_control_characters() {
        let mut input = args("profile\nwith-newline");
        assert!(validate_args(&input).is_err());
        input.notary_profile = "valid-profile".into();
        input.application_identity = "identity\0with-nul".into();
        assert!(validate_args(&input).is_err());
    }
}
