//! siderostat xtask runner: `cargo xtask <command>`.
//!
//! Subcommands:
//! - install: build, sign, and install the proxy, secrets, config, manifests,
//!   and LaunchAgent plist.
//! - verify:  check the installed LaunchAgent and admin API.
//! - uninstall: stop the LaunchAgent and disable its plist.
//! - app-dev: build a deterministic ad-hoc signed `Siderostat.app` bundle.
//! - pkg-dev: build a scriptless flat `.pkg` from an `app-dev` bundle.
//! - sign: sign, notarize, staple, and verify a release package.

mod bundle;
mod install;
mod manifest;
mod package;
mod signing;
mod util;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use install::{FingerprintModelsArgs, InstallArgs};
use std::ffi::OsStr;

#[derive(Parser)]
#[command(version, about = "siderostat install/verify automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build, sign, and install the proxy, secrets, config, manifests, and LaunchAgent plist.
    Install(InstallArgs),
    /// Compute and cache SHA-256 values for the GGUF model files.
    FingerprintModels(FingerprintModelsArgs),
    /// Verify the installed LaunchAgent and admin API.
    Verify,
    /// Stop the LaunchAgent and disable its plist.
    Uninstall,
    /// Build a deterministic ad-hoc signed Siderostat.app bundle (B-03).
    AppDev(bundle::AppDevArgs),
    /// Build a scriptless flat .pkg from an app-dev bundle (E-01).
    PkgDev(bundle::PkgDevArgs),
    /// Build, sign, notarize, staple, and verify a release package (E-02).
    Sign(signing::SignArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Install(args) => install::install(&args),
        Command::FingerprintModels(args) => install::fingerprint_models(&args),
        Command::Verify => verify(),
        Command::Uninstall => uninstall(),
        Command::AppDev(args) => bundle::app_dev(&args),
        Command::PkgDev(args) => package::pkg_dev(&args),
        Command::Sign(args) => signing::sign(&args),
    }
}

fn verify() -> Result<()> {
    let uid = current_uid()?;
    let job = format!("gui/{uid}/local.siderostat.runtime");
    util::tracing_log(&format!("launchctl print {job}"));
    match util::run("launchctl", &[OsStr::new("print"), OsStr::new(&job)]) {
        Ok(_) => util::tracing_log("LaunchAgent is loaded"),
        Err(error) => {
            util::tracing_log(&format!("LaunchAgent not loaded: {error}"));
        }
    }
    for endpoint in ["/healthz", "/readyz", "/cluster", "/metrics"] {
        let url = format!("http://127.0.0.1:18081{endpoint}");
        util::tracing_log(&format!("GET {url}"));
        match util::run(
            "curl",
            &[
                OsStr::new("--fail"),
                OsStr::new("--silent"),
                OsStr::new(&url),
            ],
        ) {
            Ok(out) => {
                let body = String::from_utf8_lossy(&out);
                util::tracing_log(&format!("  {endpoint}: {body}"));
            }
            Err(error) => {
                util::tracing_log(&format!("  {endpoint}: unreachable ({error})"));
            }
        }
    }
    Ok(())
}

fn uninstall() -> Result<()> {
    let uid = current_uid()?;
    let job = format!("gui/{uid}/local.siderostat.runtime");
    util::run_live("launchctl", &[OsStr::new("bootout"), OsStr::new(&job)])?;
    let home = util::home()?;
    let plist = home.join("Library/LaunchAgents/local.siderostat.runtime.plist");
    let disabled = home.join("Library/LaunchAgents/local.siderostat.runtime.plist.disabled");
    if plist.is_file() {
        std::fs::rename(&plist, &disabled)
            .with_context(|| format!("disable {}", plist.display()))?;
        util::tracing_log(&format!(
            "moved {} -> {}",
            plist.display(),
            disabled.display()
        ));
    }
    util::tracing_log("uninstall complete (models, KV cache, secrets, runtime state preserved)");
    Ok(())
}

fn current_uid() -> Result<u32> {
    let out = util::run("id", &[OsStr::new("-u")])?;
    Ok(String::from_utf8(out)?.trim().parse()?)
}
