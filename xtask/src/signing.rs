//! Developer ID signing and notarization pipeline (E-02).
//!
//! The pipeline accepts certificate identity names and a notarytool Keychain
//! profile name, but never accepts or writes credential contents. `--dry-run`
//! renders the fixed command order with the profile redacted so the release
//! gate can be reviewed before a signed artifact is produced.

use crate::{dmg, package, util};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
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

/// Trusted timestamp policy for release signing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TimestampMode {
    /// Use Apple's secure timestamp service and run the notarization gate.
    Apple,
    /// Produce an explicitly marked local diagnostic artifact without a trusted timestamp.
    None,
}

impl TimestampMode {
    fn is_no_timestamp(self) -> bool {
        matches!(self, Self::None)
    }

    fn includes_secure_timestamp(self) -> bool {
        !self.is_no_timestamp()
    }

    fn codesign_flag(self) -> &'static str {
        if self.is_no_timestamp() {
            "--timestamp=none"
        } else {
            "--timestamp"
        }
    }

    fn as_str(self) -> &'static str {
        if self.is_no_timestamp() {
            "none"
        } else {
            "apple"
        }
    }

    fn distribution_ready(self) -> bool {
        self.includes_secure_timestamp()
    }
}

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
    pub notary_profile: Option<String>,
    /// Output directory for the signed package and build metadata.
    #[arg(long, default_value = "dist")]
    pub output_dir: PathBuf,
    /// Directory for the redacted notary log.
    #[arg(long)]
    pub notary_log_dir: Option<PathBuf>,
    /// Print the fixed command order without touching the filesystem or network.
    #[arg(long)]
    pub dry_run: bool,
    /// Explicitly allow replacing a higher installed app bundle version.
    #[arg(long)]
    pub rollback: bool,
    /// Also build, sign, notarize, staple, and verify the release DMG.
    #[arg(long)]
    pub with_dmg: bool,
    /// Trusted timestamp policy. `none` is for explicitly marked local diagnostics only.
    #[arg(long, value_enum, default_value = "apple")]
    pub timestamp_mode: TimestampMode,
}

#[derive(Debug, Clone)]
struct DmgRelease {
    uninstaller: PathBuf,
    dmg: PathBuf,
    uninstaller_submission: Option<String>,
    dmg_submission: Option<String>,
}

/// Run the signed app/package/notarization pipeline.
pub fn sign(args: &SignArgs) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("sign requires macOS (codesign/productbuild/notarytool/stapler)");
    }
    validate_args(args)?;
    let root = std::env::current_dir().context("resolve repository root")?;
    let app = args.app_dir.join("Siderostat.app");
    let package = args.output_dir.join(package::package_filename_for_mode(
        &args.version,
        args.rollback,
        args.timestamp_mode.is_no_timestamp(),
    ));
    let log_dir = args
        .notary_log_dir
        .clone()
        .unwrap_or_else(|| args.output_dir.join("notary"));
    let log_path = log_dir.join(package::notary_log_filename_for_mode(
        &args.version,
        args.rollback,
        args.timestamp_mode.is_no_timestamp(),
    ));
    let dmg_log_path = log_dir.join(format!(
        "Siderostat-{}{}-dmg-notary.json",
        args.version,
        artifact_suffix(args)
    ));
    let uninstaller_log_path = log_dir.join(format!(
        "Siderostat-{}{}-uninstaller-notary.json",
        args.version,
        artifact_suffix(args)
    ));

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
    let component = staging.join(package::component_filename_for_mode(
        &args.version,
        args.rollback,
        args.timestamp_mode.is_no_timestamp(),
    ));

    // 1. Inside-out Developer ID Application signing. Never use --deep to sign.
    sign_code_item(
        &app.join("Contents/Helpers/siderostat-runtime"),
        &args.application_identity,
        RUNTIME_IDENTIFIER,
        args.timestamp_mode,
    )?;
    sign_code_item(
        &app,
        &args.application_identity,
        APP_IDENTIFIER,
        args.timestamp_mode,
    )?;
    verify_app(&app)?;

    // 2. Build and sign the final product archive with Developer ID Installer.
    package::build_component(&app, &component, &args.version, args.rollback)?;
    package::build_product(
        &component,
        &package,
        &args.version,
        Some(&args.installer_identity),
        args.timestamp_mode.includes_secure_timestamp(),
    )?;
    verify_package_signature(&package)?;

    // 3. Notarize only the production timestamp mode. No-timestamp artifacts
    // are intentionally limited to local diagnostics.
    let submission = if args.timestamp_mode.distribution_ready() {
        let profile = args
            .notary_profile
            .as_deref()
            .expect("validated Apple timestamp mode has a notary profile");
        let submission = submit_for_notarization(&package, profile)?;
        let notary_log = fetch_notary_log(&submission, profile)?;
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
        Some(submission)
    } else {
        None
    };

    let dmg_release = if args.with_dmg {
        let uninstaller = dmg::build_uninstaller_bundle(
            &root,
            &app,
            &args.version,
            args.build_number,
            &staging.join("Siderostat Uninstaller.app"),
        )?;
        sign_code_item(
            &uninstaller.join("Contents/MacOS/Siderostat Uninstaller"),
            &args.application_identity,
            dmg::UNINSTALLER_IDENTIFIER,
            args.timestamp_mode,
        )?;
        sign_code_item(
            &uninstaller,
            &args.application_identity,
            dmg::UNINSTALLER_IDENTIFIER,
            args.timestamp_mode,
        )?;
        dmg::verify_uninstaller_bundle(&uninstaller)?;
        let uninstaller_submission = if args.timestamp_mode.distribution_ready() {
            let profile = args
                .notary_profile
                .as_deref()
                .expect("validated Apple timestamp mode has a notary profile");
            let uninstaller_zip = dmg::zip_for_notarization(
                &uninstaller,
                &staging.join("Siderostat Uninstaller-notary.zip"),
            )?;
            let submission = submit_for_notarization(&uninstaller_zip, profile)?;
            let uninstaller_log = fetch_notary_log(&submission, profile)?;
            util::write(&uninstaller_log_path, &uninstaller_log).with_context(|| {
                format!(
                    "save Uninstaller notary log {}",
                    uninstaller_log_path.display()
                )
            })?;
            util::run(
                "xcrun",
                &[
                    OsStr::new("stapler"),
                    OsStr::new("staple"),
                    OsStr::new(&uninstaller),
                ],
            )?;
            util::run(
                "xcrun",
                &[
                    OsStr::new("stapler"),
                    OsStr::new("validate"),
                    OsStr::new(&uninstaller),
                ],
            )?;
            util::run(
                "spctl",
                &[
                    OsStr::new("--assess"),
                    OsStr::new("--type"),
                    OsStr::new("execute"),
                    OsStr::new("--verbose=4"),
                    OsStr::new(&uninstaller),
                ],
            )?;
            Some(submission)
        } else {
            None
        };

        let dmg = dmg::build_dmg(
            &package,
            &uninstaller,
            &args.version,
            args.rollback,
            args.timestamp_mode.is_no_timestamp(),
            &args.output_dir,
            Some(&staging.join("dmg-source")),
        )?;
        sign_code_item(
            &dmg,
            &args.application_identity,
            dmg::DMG_IDENTIFIER,
            args.timestamp_mode,
        )?;
        verify_code_item(&dmg)?;
        let dmg_submission = if args.timestamp_mode.distribution_ready() {
            let profile = args
                .notary_profile
                .as_deref()
                .expect("validated Apple timestamp mode has a notary profile");
            let submission = submit_for_notarization(&dmg, profile)?;
            let dmg_log = fetch_notary_log(&submission, profile)?;
            util::write(&dmg_log_path, &dmg_log)
                .with_context(|| format!("save DMG notary log {}", dmg_log_path.display()))?;
            util::run(
                "xcrun",
                &[
                    OsStr::new("stapler"),
                    OsStr::new("staple"),
                    OsStr::new(&dmg),
                ],
            )?;
            util::run(
                "xcrun",
                &[
                    OsStr::new("stapler"),
                    OsStr::new("validate"),
                    OsStr::new(&dmg),
                ],
            )?;
            util::run(
                "spctl",
                &[
                    OsStr::new("--assess"),
                    OsStr::new("--type"),
                    OsStr::new("open"),
                    OsStr::new("--context"),
                    OsStr::new("context:primary-signature"),
                    OsStr::new("--verbose=4"),
                    OsStr::new(&dmg),
                ],
            )?;
            Some(submission)
        } else {
            None
        };
        let package_filename = package
            .file_name()
            .and_then(|name| name.to_str())
            .context("package filename is not UTF-8")?;
        dmg::verify_mounted_dmg(&dmg, &dmg::expected_dmg_entries(package_filename))?;
        Some(DmgRelease {
            uninstaller,
            dmg,
            uninstaller_submission,
            dmg_submission,
        })
    } else {
        None
    };

    let metadata = build_metadata(&app, &package, &submission, dmg_release.as_ref(), args)?;
    let metadata_path = args.output_dir.join(package::metadata_filename_for_mode(
        &args.version,
        args.rollback,
        args.timestamp_mode.is_no_timestamp(),
    ));
    let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
    util::write(&metadata_path, &metadata_bytes)?;

    println!("app: {}", app.display());
    println!("package: {}", package.display());
    println!(
        "timestamp_mode: {} (distribution_ready={})",
        args.timestamp_mode.as_str(),
        args.timestamp_mode.distribution_ready()
    );
    if submission.is_some() {
        println!("notary_log: {}", log_path.display());
    }
    if let Some(release) = &dmg_release {
        println!("uninstaller: {}", release.uninstaller.display());
        if release.uninstaller_submission.is_some() {
            println!("uninstaller_notary_log: {}", uninstaller_log_path.display());
            println!(
                "uninstaller_notary_submission_id: {}",
                release
                    .uninstaller_submission
                    .as_deref()
                    .unwrap_or_default()
            );
        }
        println!("dmg: {}", release.dmg.display());
        if release.dmg_submission.is_some() {
            println!("dmg_notary_log: {}", dmg_log_path.display());
            println!(
                "dmg_notary_submission_id: {}",
                release.dmg_submission.as_deref().unwrap_or_default()
            );
        }
    }
    println!("metadata: {}", metadata_path.display());
    if let Some(submission) = &submission {
        println!("notary_submission_id: {submission}");
    }
    println!("app_sha256: {}", metadata["app_sha256"]);
    println!("pkg_sha256: {}", metadata["pkg_sha256"]);
    Ok(())
}

fn validate_args(args: &SignArgs) -> Result<()> {
    for (label, value) in [
        ("version", args.version.as_str()),
        ("application identity", args.application_identity.as_str()),
        ("installer identity", args.installer_identity.as_str()),
    ] {
        anyhow::ensure!(!value.trim().is_empty(), "{label} must not be empty");
        anyhow::ensure!(
            !value.chars().any(char::is_control),
            "{label} must not contain control characters"
        );
    }
    if let Some(profile) = args.notary_profile.as_deref() {
        anyhow::ensure!(
            !profile.trim().is_empty(),
            "notary profile must not be empty"
        );
        anyhow::ensure!(
            !profile.chars().any(char::is_control),
            "notary profile must not contain control characters"
        );
    }
    if args.timestamp_mode.distribution_ready() {
        anyhow::ensure!(
            args.notary_profile
                .as_deref()
                .is_some_and(|profile| !profile.trim().is_empty()),
            "--notary-profile is required when --timestamp-mode=apple"
        );
    }
    Ok(())
}

fn sign_code_item(
    path: &Path,
    identity: &str,
    identifier: &str,
    timestamp_mode: TimestampMode,
) -> Result<()> {
    anyhow::ensure!(path.exists(), "code item missing: {}", path.display());
    util::run(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--options"),
            OsStr::new("runtime"),
            OsStr::new(timestamp_mode.codesign_flag()),
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
    verify_code_item(app)
}

fn verify_code_item(path: &Path) -> Result<()> {
    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--strict"),
            OsStr::new("--verbose=4"),
            OsStr::new(path),
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

fn artifact_suffix(args: &SignArgs) -> String {
    let mut suffix = String::new();
    if args.rollback {
        suffix.push_str("-rollback");
    }
    if args.timestamp_mode.is_no_timestamp() {
        suffix.push_str("-no-timestamp");
    }
    suffix
}

fn build_metadata(
    app: &Path,
    package: &Path,
    submission: &Option<String>,
    dmg_release: Option<&DmgRelease>,
    args: &SignArgs,
) -> Result<Value> {
    let rustc = String::from_utf8(util::run("rustc", &[OsStr::new("-vV")])?)?;
    let git_commit = String::from_utf8(util::run(
        "git",
        &[OsStr::new("rev-parse"), OsStr::new("HEAD")],
    )?)?;
    let rust_version = field(&rustc, "release")?;
    let target = field(&rustc, "host")?;
    let mut metadata = json!({
        "artifact": package.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
        "version": args.version.as_str(),
        "build_number": args.build_number,
        "rollback": args.rollback,
        "install_mode": if args.rollback { "rollback" } else { "upgrade" },
        "app_sha256": util::sha256_tree(app)?,
        "pkg_sha256": util::sha256_hex(package)?,
        "git_commit": git_commit.trim(),
        "rust_version": rust_version,
        "target": target,
        "timestamp_mode": args.timestamp_mode.as_str(),
        "distribution_ready": args.timestamp_mode.distribution_ready(),
        "notarization": if args.timestamp_mode.distribution_ready() {
            "required"
        } else {
            "skipped"
        },
        "notary_submission_id": submission,
    });
    if let Some(release) = dmg_release {
        metadata["uninstaller_sha256"] = json!(util::sha256_tree(&release.uninstaller)?);
        metadata["uninstaller_notary_submission_id"] = json!(&release.uninstaller_submission);
        metadata["dmg_sha256"] = json!(util::sha256_hex(&release.dmg)?);
        metadata["dmg_notary_submission_id"] = json!(&release.dmg_submission);
    }
    Ok(metadata)
}

fn field(output: &str, name: &str) -> Result<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .map(str::to_owned)
        .with_context(|| format!("rustc -vV has no {name} field"))
}

fn dry_run_plan(app: &Path, package: &Path, log_path: &Path, args: &SignArgs) -> Vec<String> {
    let mut plan = vec![
        format!(
            "package mode: {} (BundleIsVersionChecked={})",
            if args.rollback { "rollback" } else { "upgrade" },
            if args.rollback { "false" } else { "true" }
        ),
        format!(
            "timestamp mode: {} (distribution_ready={})",
            args.timestamp_mode.as_str(),
            args.timestamp_mode.distribution_ready()
        ),
        format!(
            "codesign --force --options runtime {} --sign <application identity> --identifier {RUNTIME_IDENTIFIER} {}",
            args.timestamp_mode.codesign_flag(),
            app.join("Contents/Helpers/siderostat-runtime").display()
        ),
        format!(
            "codesign --force --options runtime {} --sign <application identity> --identifier {APP_IDENTIFIER} {}",
            args.timestamp_mode.codesign_flag(),
            app.display()
        ),
        format!("codesign --verify --strict --verbose=4 {}", app.display()),
        format!(
            "pkgbuild --root <payload root> --component-plist <component plist> --scripts <controlled preinstall> --install-location /Applications --identifier {} --version {} <component package>",
            package::COMPONENT_IDENTIFIER,
            args.version
        ),
        format!("expected package payload: {}", package::PAYLOAD_PATH),
        format!(
            "productbuild --package <component package> --identifier {} --version {} --sign <installer identity> {} {}",
            package::PRODUCT_IDENTIFIER,
            args.version,
            if args.timestamp_mode.includes_secure_timestamp() {
                "--timestamp"
            } else {
                "--timestamp=none"
            },
            package.display()
        ),
        format!("pkgutil --check-signature {}", package.display()),
    ];
    if args.timestamp_mode.distribution_ready() {
        plan.extend([
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
        ]);
    } else {
        plan.push("notarization/staple/Gatekeeper: skipped (no trusted timestamp)".into());
    }
    plan.push(format!(
        "write checksum/build metadata beside {}",
        package.display()
    ));
    if args.with_dmg {
        let dmg_name = format!("Siderostat-{}{}.dmg", args.version, artifact_suffix(args));
        plan.extend([
            format!(
                "build {}/Contents/MacOS/{} with bundle identifier {}",
                "Siderostat Uninstaller.app",
                dmg::UNINSTALLER_EXECUTABLE,
                dmg::UNINSTALLER_IDENTIFIER
            ),
            format!(
                "codesign --force --options runtime {} --sign <application identity> --identifier {} <uninstaller executable>",
                args.timestamp_mode.codesign_flag(),
                dmg::UNINSTALLER_IDENTIFIER
            ),
            format!(
                "codesign --force --options runtime {} --sign <application identity> --identifier {} <uninstaller.app>",
                args.timestamp_mode.codesign_flag(),
                dmg::UNINSTALLER_IDENTIFIER
            ),
            "codesign --verify --deep --strict Siderostat Uninstaller.app".into(),
            format!("hdiutil create {dmg_name}"),
            format!(
                "codesign --force --options runtime {} --sign <application identity> --identifier {} <dmg>",
                args.timestamp_mode.codesign_flag(),
                dmg::DMG_IDENTIFIER
            ),
            "codesign --verify --strict --verbose=4 <dmg>".into(),
            "hdiutil attach/detach and verify exactly pkg + Uninstaller.app + README.html".into(),
        ]);
        if args.timestamp_mode.distribution_ready() {
            plan.extend([
                "ditto -c -k --keepParent Siderostat Uninstaller.app <temporary-notary.zip>".into(),
                "xcrun notarytool submit <temporary-notary.zip> --keychain-profile <redacted> --wait --output-format json".into(),
                "xcrun stapler staple <uninstaller.app>".into(),
                "xcrun stapler validate <uninstaller.app>".into(),
                "spctl --assess --type execute --verbose=4 <uninstaller.app>".into(),
                "xcrun notarytool submit <dmg> --keychain-profile <redacted> --wait --output-format json".into(),
                "xcrun stapler staple <dmg>".into(),
                "xcrun stapler validate <dmg>".into(),
                "spctl --assess --type open --context context:primary-signature --verbose=4 <dmg>"
                    .into(),
                "write DMG/Uninstaller checksums and DMG notary metadata".into(),
            ]);
        } else {
            plan.push("DMG notarization/staple/Gatekeeper: skipped (no trusted timestamp)".into());
        }
    }
    plan
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
            notary_profile: Some(profile.into()),
            output_dir: PathBuf::from("dist"),
            notary_log_dir: None,
            dry_run: true,
            rollback: false,
            with_dmg: false,
            timestamp_mode: TimestampMode::Apple,
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
        assert!(!plan.contains(input.notary_profile.as_deref().unwrap()));
        assert!(plan.contains("--keychain-profile <redacted>"));
    }

    #[test]
    fn dry_run_with_dmg_includes_uninstaller_and_dmg_gate() {
        let mut input = args("private-profile-name");
        input.with_dmg = true;
        let plan = dry_run_plan(
            &input.app_dir.join("Siderostat.app"),
            &input.output_dir.join("Siderostat-0.3.0.pkg"),
            &input.output_dir.join("notary/log.json"),
            &input,
        )
        .join("\n");
        assert!(plan.contains(dmg::UNINSTALLER_IDENTIFIER));
        assert!(plan.contains("hdiutil create Siderostat-0.3.0.dmg"));
        assert!(plan.contains("spctl --assess --type open"));
    }

    #[test]
    fn profile_and_identity_validation_rejects_control_characters() {
        let mut input = args("profile\nwith-newline");
        assert!(validate_args(&input).is_err());
        input.notary_profile = Some("valid-profile".into());
        input.application_identity = "identity\0with-nul".into();
        assert!(validate_args(&input).is_err());
    }

    #[test]
    fn rollback_dry_run_is_explicit_and_uses_rollback_artifact_names() {
        let mut input = args("private-profile-name");
        input.rollback = true;
        let package = input
            .output_dir
            .join(package::package_filename(&input.version, input.rollback));
        let log = input
            .output_dir
            .join("notary")
            .join(package::notary_log_filename_for_mode(
                &input.version,
                input.rollback,
                false,
            ));
        let plan = dry_run_plan(
            &input.app_dir.join("Siderostat.app"),
            &package,
            &log,
            &input,
        )
        .join("\n");

        assert!(plan.contains("package mode: rollback (BundleIsVersionChecked=false)"));
        assert!(plan.contains("Siderostat-0.3.0-rollback.pkg"));
        assert!(plan.contains("Siderostat-0.3.0-rollback-notary.json"));
    }

    #[test]
    fn no_timestamp_dry_run_is_explicit_and_skips_notarization() {
        let mut input = args("");
        input.timestamp_mode = TimestampMode::None;
        let package = input.output_dir.join(package::package_filename_for_mode(
            &input.version,
            input.rollback,
            true,
        ));
        let log = input
            .output_dir
            .join("notary/Siderostat-0.3.0-no-timestamp-notary.json");
        let plan = dry_run_plan(
            &input.app_dir.join("Siderostat.app"),
            &package,
            &log,
            &input,
        )
        .join("\n");

        assert!(plan.contains("timestamp mode: none (distribution_ready=false)"));
        assert!(plan.contains("--timestamp=none"));
        assert!(plan.contains("Siderostat-0.3.0-no-timestamp.pkg"));
        assert!(!plan.contains("notarytool submit"));
        assert!(!plan.contains("stapler"));
        assert!(!plan.contains("private-profile-name"));
    }

    #[test]
    fn no_timestamp_dmg_dry_run_skips_all_notarization_steps() {
        let mut input = args("");
        input.timestamp_mode = TimestampMode::None;
        input.with_dmg = true;
        let plan = dry_run_plan(
            &input.app_dir.join("Siderostat.app"),
            &input.output_dir.join("Siderostat-0.3.0-no-timestamp.pkg"),
            &input.output_dir.join("notary/log.json"),
            &input,
        )
        .join("\n");

        assert!(plan.contains("Siderostat-0.3.0-no-timestamp.dmg"));
        assert!(plan.contains("--timestamp=none"));
        assert!(plan.contains("DMG notarization/staple/Gatekeeper: skipped"));
        assert!(!plan.contains("notarytool"));
        assert!(!plan.contains("stapler"));
    }

    #[test]
    fn apple_timestamp_mode_requires_a_notary_profile() {
        let mut input = args("");
        input.notary_profile = None;
        assert!(validate_args(&input).is_err());

        input.timestamp_mode = TimestampMode::None;
        assert!(validate_args(&input).is_ok());
    }
}
