//! `install` command: build, sign, and install the proxy, secrets, config,
//! manifests, and LaunchAgent plist.

use crate::{manifest, util};
use anyhow::{Context, Result};
use clap::Args;
use siderostat::config::ModeAwareConfig;
use std::process::Command;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const LAUNCH_AGENT_UNLOAD_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_AGENT_UNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Args, Clone)]
pub struct ModelSelectionArgs {
    /// Path to the ds4-server binary. Default: discovered under $HOME.
    #[arg(long)]
    pub ds4_server: Option<PathBuf>,
    /// Standalone model file (path or basename under DWARFSTAR_HOME/gguf).
    #[arg(long)]
    pub standalone_model: Option<PathBuf>,
    /// Model used by the distributed topology (path or basename under DWARFSTAR_HOME/gguf).
    /// `--mxfp4-model` remains accepted as a compatibility alias; MXFP4 is a
    /// quantization value, not the topology name.
    #[arg(long = "distributed-model", alias = "mxfp4-model")]
    pub distributed_model: Option<PathBuf>,
    /// DSpark support model file (path or basename under DWARFSTAR_HOME/gguf).
    #[arg(long)]
    pub dspark_support: Option<PathBuf>,
}

#[derive(Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub models: ModelSelectionArgs,
    /// node_id written to config.toml. Default: hostname.
    #[arg(long)]
    pub node_id: Option<String>,
    /// Directory containing shared cluster-control + peer-proxy secret files.
    #[arg(long)]
    pub shared_secret_dir: Option<PathBuf>,
    /// Verified DS4 source commit for the distributed manifest.
    #[arg(long)]
    pub ds4_source_commit: Option<String>,
    /// Approved ds4 binary digest(s) for the distributed manifest (repeatable).
    #[arg(long)]
    pub ds4_binary_digest: Vec<String>,
    /// Peer ds4 binary digest(s) for the distributed manifest (repeatable).
    /// The locally installed digest is computed and added automatically.
    #[arg(long)]
    pub peer_ds4_binary_digest: Vec<String>,
    /// Compute/update GGUF SHA-256 values without prompting.
    #[arg(long)]
    pub hash_models: bool,
    /// Accept same-size model metadata drift using the cached full digest.
    /// Use only when the operator knows the model content was not changed.
    #[arg(long, conflicts_with = "hash_models")]
    pub accept_model_metadata_change: bool,
    /// Run the full documented CI gates (fmt/clippy/test/diff-check) first.
    #[arg(long)]
    pub ci: bool,
    /// Bootstrap and kickstart the LaunchAgent now (restarts ds4-server).
    #[arg(long)]
    pub start: bool,
}

#[derive(Args)]
pub struct FingerprintModelsArgs {
    #[command(flatten)]
    pub models: ModelSelectionArgs,
}

pub fn install(args: &InstallArgs) -> Result<()> {
    if !args.ds4_binary_digest.is_empty() && !args.peer_ds4_binary_digest.is_empty() {
        anyhow::bail!("--ds4-binary-digest and --peer-ds4-binary-digest cannot be used together");
    }

    if args.ci {
        run_ci_gates()?;
    }

    let (home, dwfstar_home, gguf_dir) = discover_ds4(args.models.ds4_server.as_deref())?;
    let (standalone_model, distributed_model, dspark_support) = resolve_models(
        &gguf_dir,
        args.models.standalone_model.as_deref(),
        args.models.distributed_model.as_deref(),
        args.models.dspark_support.as_deref(),
    )?;

    let hash_models = args.hash_models
        || util::confirm_default_no("Compute SHA-256 for GGUF model weights now? [y/N] ")?;
    let cache_path = home
        .join("Library/Application Support/siderostat/manifests")
        .join(manifest::DIGEST_CACHE_FILE_NAME);
    if hash_models {
        manifest::fingerprint_models(
            &cache_path,
            &standalone_model,
            &distributed_model,
            Some(&dspark_support),
        )?;
    } else {
        manifest::verify_model_cache(
            &cache_path,
            &standalone_model,
            &distributed_model,
            Some(&dspark_support),
            args.accept_model_metadata_change,
        )?;
    }

    build_release()?;
    sign_and_install_proxy()?;

    ensure_secrets(&home, args.shared_secret_dir.as_deref())?;

    let config = write_config(
        &home,
        &dwfstar_home,
        &standalone_model,
        &distributed_model,
        &dspark_support,
        args.node_id.as_deref(),
    )?;

    // Resolve operator-only manifest inputs, preserving a prior install if it
    // matches the local ds4 binary.
    let (source_commit, approved) = resolve_manifest_inputs(&config, args)?;
    manifest::generate(&config, source_commit.as_deref(), &approved)?;

    config
        .validate()
        .context("generated config.toml failed validation")?;
    util::tracing_log("config.toml validation passed");

    install_launch_agent(&home, args.start)?;
    install_monitor_launch_agent(&home, args.start)?;

    util::tracing_log("install complete");
    Ok(())
}

/// Compute the GGUF digests once and store them beside the generated manifests.
pub fn fingerprint_models(args: &FingerprintModelsArgs) -> Result<()> {
    let (home, _dwfstar_home, gguf_dir) = discover_ds4(args.models.ds4_server.as_deref())?;
    let (standalone, distributed, dspark_support) = resolve_models(
        &gguf_dir,
        args.models.standalone_model.as_deref(),
        args.models.distributed_model.as_deref(),
        args.models.dspark_support.as_deref(),
    )?;
    let cache_path = home
        .join("Library/Application Support/siderostat/manifests")
        .join(manifest::DIGEST_CACHE_FILE_NAME);
    manifest::fingerprint_models(
        &cache_path,
        &standalone,
        &distributed,
        Some(&dspark_support),
    )
}

/// Required CI gates (docs/installation.md section 5). Only run with `--ci`.
fn run_ci_gates() -> Result<()> {
    let gates: &[&[&str]] = &[
        &["cargo", "fmt", "--check"],
        &[
            "cargo",
            "clippy",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
        &["cargo", "test", "--all-targets"],
        &["git", "diff", "--check"],
    ];
    for args in gates {
        let label = args.join(" ");
        let status = Command::new(args[0])
            .args(&args[1..])
            .status()
            .with_context(|| format!("spawn CI gate: {label}"))?;
        if !status.success() {
            anyhow::bail!("CI gate failed: {label}");
        }
        util::tracing_log(&format!("CI gate passed: {label}"));
    }
    Ok(())
}

fn build_release() -> Result<()> {
    util::tracing_log("cargo build --release");
    util::run_live("cargo", &[OsStr::new("build"), OsStr::new("--release")])?;
    // Explicitly build the monitor binary too. It is a workspace member, so the
    // full release build already covers it; this guarantees it exists before we
    // sign and install it.
    util::tracing_log("cargo build --release -p siderostat-monitor");
    util::run_live(
        "cargo",
        &[
            OsStr::new("build"),
            OsStr::new("--release"),
            OsStr::new("-p"),
            OsStr::new("siderostat-monitor"),
        ],
    )?;
    Ok(())
}

/// Sign a release binary (ad-hoc, not linker-signed) so launchd's launch
/// constraints accept it, then install it to /usr/local/bin.
fn sign_and_install(bin: &Path, dest: &Path) -> Result<()> {
    util::tracing_log(&format!("codesign --force --sign - {}", bin.display()));
    util::run(
        "codesign",
        &[
            OsStr::new("--force"),
            OsStr::new("--sign"),
            OsStr::new("-"),
            OsStr::new(bin),
        ],
    )?;
    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--verbose=4"),
            OsStr::new(bin),
        ],
    )?;

    // Idempotent install: if /usr/local/bin already holds the same bytes as the
    // freshly signed build, do not replace it (no sudo install needed).
    let unchanged = if dest.is_file() {
        let installed = util::sha256_hex(dest)?;
        let built = util::sha256_hex(bin)?;
        installed == built
    } else {
        false
    };

    if unchanged {
        util::tracing_log(&format!(
            "{} unchanged; keeping existing install",
            dest.display()
        ));
    } else {
        util::tracing_log(&format!("sudo install to {}", dest.display()));
        util::run_live(
            "sudo",
            &[
                OsStr::new("install"),
                OsStr::new("-d"),
                OsStr::new("-m"),
                OsStr::new("0755"),
                OsStr::new("/usr/local/bin"),
            ],
        )?;
        util::run_live(
            "sudo",
            &[
                OsStr::new("install"),
                OsStr::new("-m"),
                OsStr::new("0755"),
                OsStr::new(bin),
                OsStr::new(dest),
            ],
        )?;
    }

    util::run(
        "codesign",
        &[
            OsStr::new("--verify"),
            OsStr::new("--verbose=4"),
            OsStr::new(dest),
        ],
    )?;
    util::tracing_log(&format!("installed and verified {}", dest.display()));
    Ok(())
}

/// Sign and install the proxy binary, then the monitor binary.
fn sign_and_install_proxy() -> Result<()> {
    sign_and_install(
        &PathBuf::from("target/release/siderostat"),
        &PathBuf::from("/usr/local/bin/siderostat"),
    )?;
    sign_and_install(
        &PathBuf::from("target/release/siderostat-monitor"),
        &PathBuf::from("/usr/local/bin/siderostat-monitor"),
    )?;
    Ok(())
}

/// Locate the ds4-server binary and derive DWARFSTAR_HOME (its parent) and the
/// gguf model directory.
fn discover_ds4(explicit: Option<&Path>) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let home = util::home()?;
    let candidate = explicit.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let common = home.join("LLM/ds4/ds4-server");
        if common.is_file() {
            common
        } else {
            // Search under $HOME for a ds4-server executable.
            let out = util::run(
                "find",
                &[
                    OsStr::new(&home),
                    OsStr::new("-type"),
                    OsStr::new("f"),
                    OsStr::new("-name"),
                    OsStr::new("ds4-server"),
                ],
            )
            .ok()
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .ok()
                    .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            })
            .map(PathBuf::from)
            .unwrap_or_default();
            if out.is_file() { out } else { PathBuf::new() }
        }
    });
    if !candidate.is_file() {
        anyhow::bail!(
            "could not locate ds4-server; pass --ds4-server <path> or place it under $HOME"
        );
    }
    let ds4_bin = candidate
        .canonicalize()
        .with_context(|| format!("canonicalize {}", candidate.display()))?;
    let dwfstar_home = ds4_bin
        .parent()
        .context("ds4-server has no parent directory")?
        .to_path_buf();
    let gguf_dir = dwfstar_home.join("gguf");
    if !gguf_dir.is_dir() {
        anyhow::bail!("expected model directory {}", gguf_dir.display());
    }
    util::tracing_log(&format!(
        "DWARFSTAR_HOME={} ds4-server={}",
        dwfstar_home.display(),
        ds4_bin.display()
    ));
    Ok((home, dwfstar_home, gguf_dir))
}

/// Classify model files in DWARFSTAR_HOME/gguf by role: distributed model,
/// standalone model, and DSpark support model. The distributed model's
/// quantization is recorded separately in its manifest.
fn resolve_models(
    gguf_dir: &Path,
    standalone_flag: Option<&Path>,
    distributed_flag: Option<&Path>,
    dspark_flag: Option<&Path>,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let standalone = resolve_one(gguf_dir, standalone_flag, "standalone", |name| {
        let lower = name.to_lowercase();
        !lower.contains("mxfp4") && !lower.contains("dspark")
    })?;
    let distributed = resolve_one(gguf_dir, distributed_flag, "distributed", |name| {
        name.to_lowercase().contains("mxfp4")
    })?;
    let dspark = resolve_one(gguf_dir, dspark_flag, "dspark", |name| {
        name.to_lowercase().contains("dspark")
    })?;
    if standalone == distributed || standalone == dspark || distributed == dspark {
        anyhow::bail!(
            "model classification produced duplicate files; pass --standalone-model/--distributed-model/--dspark-support"
        );
    }
    Ok((standalone, distributed, dspark))
}

fn resolve_one(
    dir: &Path,
    explicit: Option<&Path>,
    kind: &str,
    matches: impl Fn(&str) -> bool,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            dir.join(path)
        };
        if !candidate.is_file() {
            anyhow::bail!("{kind} model not found at {}", candidate.display());
        }
        return candidate
            .canonicalize()
            .with_context(|| format!("canonicalize {kind} model"));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gguf") {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches(&name) {
                candidates.push(path);
            }
        }
    }
    if candidates.is_empty() {
        anyhow::bail!("no {kind} model found in {}", dir.display());
    }
    if candidates.len() > 1 {
        // Prefer the largest candidate.
        candidates.sort_by_key(|p| util::file_size(p).unwrap_or(0));
        let best = candidates.pop().unwrap();
        util::tracing_log(&format!(
            "multiple {kind} candidates; chose largest {}",
            best.display()
        ));
        return Ok(best);
    }
    candidates[0]
        .canonicalize()
        .with_context(|| format!("canonicalize {kind} model"))
}

/// Create extensionless secret files at 0600 with 32+ bytes. Existing secrets
/// are preserved; legacy `.key` files are copied to the new names so an
/// existing installation can migrate without losing its configured values.
fn ensure_secrets(home: &Path, shared_dir: Option<&Path>) -> Result<()> {
    let secret_dir = home.join("Library/Application Support/siderostat/secrets");
    std::fs::create_dir_all(&secret_dir)
        .with_context(|| format!("mkdir {}", secret_dir.display()))?;
    // Restrict the secrets directory to the owner.
    util::run("chmod", &[OsStr::new("700"), OsStr::new(&secret_dir)])?;

    let names = [
        ("cluster-control", "cluster-control.key"),
        ("peer-proxy", "peer-proxy.key"),
        ("admin", "admin.key"),
    ];
    for (name, legacy_name) in names {
        let path = secret_dir.join(name);
        if path.is_file() && util::file_size(&path)? >= 32 {
            util::tracing_log(&format!("preserving existing secret {}", path.display()));
            continue;
        }
        let legacy_path = secret_dir.join(legacy_name);
        if legacy_path.is_file() && util::file_size(&legacy_path)? >= 32 {
            std::fs::copy(&legacy_path, &path)
                .with_context(|| format!("migrate legacy secret {}", legacy_path.display()))?;
            util::run("chmod", &[OsStr::new("600"), OsStr::new(&path)])?;
            util::tracing_log(&format!(
                "migrated legacy secret {} -> {}",
                legacy_path.display(),
                path.display()
            ));
            continue;
        }
        util::tracing_log(&format!("creating secret {}", path.display()));
        util::run(
            "openssl",
            &[
                OsStr::new("rand"),
                OsStr::new("-out"),
                OsStr::new(&path),
                OsStr::new("32"),
            ],
        )?;
        util::run("chmod", &[OsStr::new("600"), OsStr::new(&path)])?;
    }

    if let Some(shared) = shared_dir {
        for (name, legacy_name) in [
            ("cluster-control", "cluster-control.key"),
            ("peer-proxy", "peer-proxy.key"),
        ] {
            let canonical_src = shared.join(name);
            let legacy_src = shared.join(legacy_name);
            let src = if canonical_src.is_file() && util::file_size(&canonical_src)? >= 32 {
                &canonical_src
            } else {
                &legacy_src
            };
            let dst = secret_dir.join(name);
            if !src.is_file() {
                anyhow::bail!(
                    "shared secret source missing {} or {}",
                    canonical_src.display(),
                    legacy_src.display()
                );
            }
            std::fs::copy(src, &dst).with_context(|| format!("copy shared secret {}", name))?;
            util::run("chmod", &[OsStr::new("600"), OsStr::new(&dst)])?;
            util::tracing_log(&format!("installed shared secret {}", name));
        }
    } else {
        util::tracing_log(
            "NOTE: cluster-control and peer-proxy are generated locally; for a 2-node cluster copy \
             these same files to the peer node via an approved secure path.",
        );
    }
    Ok(())
}

/// Write config.toml from the example, replacing every placeholder with a real
/// absolute path, and parse it (paths expanded). Manifests are generated after
/// this, so validation runs later once the manifest files exist.
fn write_config(
    home: &Path,
    dwfstar_home: &Path,
    standalone_model: &Path,
    distributed_model: &Path,
    dspark_support: &Path,
    node_id: Option<&str>,
) -> Result<ModeAwareConfig> {
    let example = PathBuf::from("siderostat.example.toml");
    let content = std::fs::read_to_string(&example)?;

    let manifest_dir = home.join("Library/Application Support/siderostat/manifests");
    let standalone_manifest = manifest_dir.join("standalone.json");
    let distributed_manifest = manifest_dir.join("distributed.json");

    let node_id = node_id.map(|s| s.to_string()).unwrap_or_else(|| {
        String::from_utf8(util::run("hostname", &[]).unwrap_or_default())
            .unwrap_or_default()
            .trim()
            .to_string()
    });

    let mut out = content.clone();
    let replacements: &[(&str, &str)] = &[
        (
            "binary = \"$HOME/LLM/ds4/PLACEHOLDER-ds4-server\"",
            &format!("binary = \"{}\"", dwfstar_home.join("ds4-server").display()),
        ),
        (
            "working_directory = \"$HOME/LLM/ds4\"",
            &format!("working_directory = \"{}\"", dwfstar_home.display()),
        ),
        (
            "support_model = \"$HOME/Library/Application Support/siderostat/models/PLACEHOLDER-dspark-support-0731.gguf\"",
            &format!("support_model = \"{}\"", dspark_support.display()),
        ),
        (
            "model = \"$HOME/Library/Application Support/siderostat/models/PLACEHOLDER-standalone.gguf\"",
            &format!("model = \"{}\"", standalone_model.display()),
        ),
        (
            "model_manifest = \"$HOME/Library/Application Support/siderostat/manifests/standalone-flash-0731-q2-q4-resident-dspark.json\"",
            &format!("model_manifest = \"{}\"", standalone_manifest.display()),
        ),
        (
            "model = \"$HOME/Library/Application Support/siderostat/models/PLACEHOLDER-mxfp4.gguf\"",
            &format!("model = \"{}\"", distributed_model.display()),
        ),
        (
            "model_manifest = \"$HOME/Library/Application Support/siderostat/manifests/mxfp4-0731.json\"",
            &format!("model_manifest = \"{}\"", distributed_manifest.display()),
        ),
        ("PLACEHOLDER-cluster-control", "cluster-control"),
        ("PLACEHOLDER-peer-proxy", "peer-proxy"),
        ("PLACEHOLDER-admin", "admin"),
        (
            "node_id = \"macstudio-coordinator\"",
            &format!("node_id = \"{node_id}\""),
        ),
    ];
    for (from, to) in replacements {
        if !out.contains(from) {
            anyhow::bail!(
                "example config is missing expected line {from:?}; example may have drifted from the install task"
            );
        }
        out = out.replace(from, to);
    }
    if out.contains("PLACEHOLDER") {
        anyhow::bail!("unresolved PLACEHOLDER remains in generated config.toml");
    }

    let config_path = home.join("Library/Application Support/siderostat/config.toml");
    // Idempotent: write the substituted content only when the deployed config
    // differs; otherwise leave it (and its backup) untouched.
    util::backup_and_write_if_changed(&config_path, out.as_bytes())?;
    util::tracing_log(&format!("wrote config -> {}", config_path.display()));

    let mut config = ModeAwareConfig::parse(&out).context("parse generated config.toml")?;
    config
        .expand_paths()
        .context("expand paths in config.toml")?;
    Ok(config)
}

/// Determine the distributed manifest's source commit and approved binary set.
/// Prefer explicit flags; otherwise preserve a prior distributed manifest that
/// matches the local ds4 binary.
fn resolve_manifest_inputs(
    config: &ModeAwareConfig,
    args: &InstallArgs,
) -> Result<(Option<String>, Vec<String>)> {
    let local_digest = util::sha256_hex_logged(&config.ds4.binary, "ds4 binary")?;
    let prior = manifest::read_existing_distributed(&config.ds4.distributed.model_manifest)?;

    let source_commit = match args.ds4_source_commit.as_deref() {
        Some(s) => Some(s.to_string()),
        None => prior
            .as_ref()
            .filter(|m| m.ds4_binary_sha256 == local_digest)
            .map(|m| m.ds4_source_commit.clone()),
    };

    let approved = if !args.peer_ds4_binary_digest.is_empty() {
        resolve_compatible_digest_inputs(&local_digest, &args.peer_ds4_binary_digest)
    } else if !args.ds4_binary_digest.is_empty() {
        args.ds4_binary_digest.clone()
    } else {
        prior
            .as_ref()
            .filter(|m| m.ds4_binary_sha256 == local_digest)
            .map(|m| m.compatible_ds4_binary_sha256.clone())
            .unwrap_or_else(|| vec![local_digest.clone()])
    };

    Ok((source_commit, approved))
}

/// Build the compatibility inputs for the peer-digest workflow. The manifest
/// generator sorts and deduplicates this list, so this helper only appends the
/// local digest to the operator-supplied peer digests.
fn resolve_compatible_digest_inputs(local: &str, peer_digests: &[String]) -> Vec<String> {
    let mut inputs = peer_digests.to_vec();
    inputs.push(local.to_string());
    inputs
}

/// Install the LaunchAgent plist. `start` controls whether the job is
/// bootstrapped/kickstarted now (restarts ds4-server).
fn install_launch_agent(home: &Path, start: bool) -> Result<()> {
    let user = util::current_user()?;
    let repo_plist = PathBuf::from("contrib/launchd/local.siderostat.runtime.plist");
    let content = std::fs::read_to_string(&repo_plist)?;
    let content = content.replace("USERNAME", &user);
    if content.contains("USERNAME") || content.contains("PLACEHOLDER") {
        anyhow::bail!("LaunchAgent plist still contains an unresolved placeholder");
    }

    let agent_dir = home.join("Library/LaunchAgents");
    let plist = agent_dir.join("local.siderostat.runtime.plist");
    // Idempotent: keep the deployed plist (and its backup) when unchanged.
    util::backup_and_write_if_changed(&plist, content.as_bytes())?;

    let config_path = home.join("Library/Application Support/siderostat/config.toml");
    util::run(
        "/usr/libexec/PlistBuddy",
        &[
            OsStr::new("-c"),
            OsStr::new(&format!(
                "Set :ProgramArguments:3 {}",
                config_path.display()
            )),
            OsStr::new(&plist),
        ],
    )?;

    let log_dir = home.join("Library/Logs/local.siderostat.runtime");
    let log_path = log_dir.join("ds4-server_siderostat.log");
    util::run(
        "/usr/libexec/PlistBuddy",
        &[
            OsStr::new("-c"),
            OsStr::new(&format!("Set :StandardOutPath {}", log_path.display())),
            OsStr::new(&plist),
        ],
    )?;
    util::run(
        "/usr/libexec/PlistBuddy",
        &[
            OsStr::new("-c"),
            OsStr::new(&format!("Set :StandardErrorPath {}", log_path.display())),
            OsStr::new(&plist),
        ],
    )?;
    util::run("plutil", &[OsStr::new("-lint"), OsStr::new(&plist)])?;
    std::fs::create_dir_all(&log_dir).with_context(|| format!("mkdir {}", log_dir.display()))?;
    util::tracing_log(&format!("plist installed -> {}", plist.display()));

    if start {
        start_launch_agent(&plist, "local.siderostat.runtime")?;
    } else {
        util::tracing_log(
            "LaunchAgent plist installed but NOT started (avoid restarting ds4-server). \
             Start it later with:\n  launchctl bootstrap \"gui/$(id -u)\" <plist>\n  launchctl kickstart -k \"gui/$(id -u)/local.siderostat.runtime\"",
        );
    }
    Ok(())
}

/// Install the monitor LaunchAgent plist. The monitor is a menu-bar GUI app
/// (gui/<uid> Aqua session), so RunAtLoad + KeepAlive(SuccessfulExit=false)
/// auto-start it at login and restart it on crash, but a manual quit from the
/// menu is honored. `start` bootstraps/kickstarts the job now.
fn install_monitor_launch_agent(home: &Path, start: bool) -> Result<()> {
    let user = util::current_user()?;
    let repo_plist = PathBuf::from("contrib/launchd/local.siderostat.monitor.plist");
    let content = std::fs::read_to_string(&repo_plist)?;
    let content = content.replace("USERNAME", &user);
    if content.contains("USERNAME") || content.contains("PLACEHOLDER") {
        anyhow::bail!("monitor LaunchAgent plist still contains an unresolved placeholder");
    }

    let agent_dir = home.join("Library/LaunchAgents");
    let plist = agent_dir.join("local.siderostat.monitor.plist");
    // Idempotent: keep the deployed plist (and its backup) when unchanged.
    util::backup_and_write_if_changed(&plist, content.as_bytes())?;

    let log_dir = home.join("Library/Logs/local.siderostat.monitor");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("mkdir {}", log_dir.display()))?;
    util::run("plutil", &[OsStr::new("-lint"), OsStr::new(&plist)])?;
    util::tracing_log(&format!("monitor plist installed -> {}", plist.display()));

    if start {
        start_launch_agent(&plist, "local.siderostat.monitor")?;
    } else {
        util::tracing_log(
            "monitor LaunchAgent plist installed but NOT started. It will auto-start at the \
             next login; start it now with:\n  launchctl bootstrap \"gui/$(id -u)\" <plist>\n  \
             launchctl kickstart -k \"gui/$(id -u)/local.siderostat.monitor\"",
        );
    }
    Ok(())
}

/// Start a LaunchAgent job. If launchd already has the job loaded (for example a
/// process running via the LaunchAgent from an earlier install), bootout it
/// first so `install --start` can re-bootstrap and restart cleanly. This keeps
/// repeated installs idempotent instead of failing on a second bootstrap.
fn start_launch_agent(plist: &Path, label: &str) -> Result<()> {
    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    let job = format!("gui/{uid}/{label}");
    // Ensure the job is enabled before bootstrap.
    let _ = util::run("launchctl", &[OsStr::new("enable"), OsStr::new(&job)]);
    // Detect whether launchd already owns the job. `launchctl print` succeeds
    // when the job is loaded into the gui domain.
    let loaded = util::run("launchctl", &[OsStr::new("print"), OsStr::new(&job)]).is_ok();
    if loaded {
        util::tracing_log(&format!("{job} already loaded; bootout before restart"));
        util::run_live("launchctl", &[OsStr::new("bootout"), OsStr::new(&job)])?;
        wait_for_launch_agent_unload(&job)?;
    }
    util::run_live(
        "launchctl",
        &[
            OsStr::new("bootstrap"),
            OsStr::new(&domain),
            OsStr::new(plist),
        ],
    )?;
    util::run_live(
        "launchctl",
        &[OsStr::new("kickstart"), OsStr::new("-k"), OsStr::new(&job)],
    )?;
    util::tracing_log(&format!("LaunchAgent started: {job}"));
    Ok(())
}

fn wait_for_launch_agent_unload(job: &str) -> Result<()> {
    wait_for_launch_agent_unload_with(
        job,
        LAUNCH_AGENT_UNLOAD_TIMEOUT,
        LAUNCH_AGENT_UNLOAD_POLL_INTERVAL,
        || util::run("launchctl", &[OsStr::new("print"), OsStr::new(job)]).is_ok(),
        thread::sleep,
    )
}

fn wait_for_launch_agent_unload_with(
    job: &str,
    timeout: Duration,
    poll_interval: Duration,
    mut is_loaded: impl FnMut() -> bool,
    mut sleep: impl FnMut(Duration),
) -> Result<()> {
    let started = Instant::now();
    loop {
        if !is_loaded() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            anyhow::bail!("timed out waiting for {job} to unload after bootout");
        }
        sleep(poll_interval);
    }
}

fn current_uid() -> Result<u32> {
    let out = util::run("id", &[OsStr::new("-u")])?;
    Ok(String::from_utf8(out)?.trim().parse()?)
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_secrets, resolve_compatible_digest_inputs, wait_for_launch_agent_unload_with,
    };
    use std::{cell::Cell, fs, time::Duration};

    #[test]
    fn waits_for_launch_agent_to_finish_unloading() {
        let polls = Cell::new(0);
        let sleeps = Cell::new(0);

        wait_for_launch_agent_unload_with(
            "gui/501/local.siderostat.runtime",
            Duration::from_secs(1),
            Duration::ZERO,
            || {
                let current = polls.get();
                polls.set(current + 1);
                current < 2
            },
            |_| sleeps.set(sleeps.get() + 1),
        )
        .unwrap();

        assert_eq!(polls.get(), 3);
        assert_eq!(sleeps.get(), 2);
    }

    #[test]
    fn reports_launch_agent_unload_timeout() {
        let error = wait_for_launch_agent_unload_with(
            "gui/501/local.siderostat.runtime",
            Duration::ZERO,
            Duration::ZERO,
            || true,
            |_| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("timed out waiting"));
    }

    #[test]
    fn migrates_legacy_secret_names_and_shared_inputs() {
        let root = std::env::temp_dir().join(format!(
            "siderostat-xtask-secret-migration-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let secret_dir = root.join("Library/Application Support/siderostat/secrets");
        let shared_dir = root.join("shared");
        fs::create_dir_all(&secret_dir).unwrap();
        fs::create_dir_all(&shared_dir).unwrap();

        fs::write(secret_dir.join("cluster-control.key"), [1u8; 32]).unwrap();
        fs::write(secret_dir.join("peer-proxy.key"), [2u8; 32]).unwrap();
        fs::write(secret_dir.join("admin.key"), [3u8; 32]).unwrap();
        fs::write(shared_dir.join("cluster-control.key"), [4u8; 32]).unwrap();
        fs::write(shared_dir.join("peer-proxy.key"), [5u8; 32]).unwrap();

        ensure_secrets(&root, Some(&shared_dir)).unwrap();

        assert_eq!(
            fs::read(secret_dir.join("cluster-control")).unwrap(),
            [4u8; 32]
        );
        assert_eq!(fs::read(secret_dir.join("peer-proxy")).unwrap(), [5u8; 32]);
        assert_eq!(fs::read(secret_dir.join("admin")).unwrap(), [3u8; 32]);
        assert!(secret_dir.join("cluster-control.key").is_file());
        assert!(secret_dir.join("peer-proxy.key").is_file());
        assert!(secret_dir.join("admin.key").is_file());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn peer_digest_inputs_include_the_local_digest() {
        let local = "11".repeat(32);
        let peer = "22".repeat(32);

        assert_eq!(
            resolve_compatible_digest_inputs(&local, std::slice::from_ref(&peer)),
            vec![peer.clone(), local]
        );
    }

    #[test]
    fn peer_digest_inputs_preserve_multiple_peer_builds() {
        let local = "11".repeat(32);
        let peer_a = "22".repeat(32);
        let peer_b = "33".repeat(32);

        assert_eq!(
            resolve_compatible_digest_inputs(&local, &[peer_a.clone(), peer_b.clone()]),
            vec![peer_a, peer_b, local]
        );
    }
}
