use crate::{
    app,
    cluster::encode_token,
    config::{LogFormat, ModeAwareConfig},
};
use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "siderostat")]
pub struct Args {
    /// Path to the TOML configuration file.
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the proxy and its single supervisor.
    Serve {
        /// Decline startup cleanup when stale siderostat/DS4 processes are detected.
        #[arg(long)]
        decline_startup_cleanup: bool,
    },
    /// Inspect or mutate the already-running process through its admin API.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ClusterCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Reconcile,
    Pair,
    Promote,
    Demote {
        #[arg(long)]
        reason: Option<String>,
    },
    Restart,
    Fingerprint {
        #[arg(long, value_enum)]
        profile: Profile,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Profile {
    Standalone,
    Distributed,
}

pub async fn run() -> anyhow::Result<()> {
    run_with(Args::parse()).await
}

async fn run_with(args: Args) -> anyhow::Result<()> {
    let (config, config_path) = ModeAwareConfig::load(args.config.as_deref()).await?;
    match args.command {
        None => {
            initialize_logging(&config);
            info!(
                config_path = %config_path.display(),
                public_listen = %config.proxy.public_listen,
                admin_listen = %config.proxy.admin_listen,
                node_id = %config.cluster.node_id,
                cluster_enabled = config.cluster.enabled,
                "configuration loaded"
            );
            app::serve(config).await
        }
        Some(Command::Serve {
            decline_startup_cleanup,
        }) => {
            initialize_logging(&config);
            info!(
                config_path = %config_path.display(),
                public_listen = %config.proxy.public_listen,
                admin_listen = %config.proxy.admin_listen,
                node_id = %config.cluster.node_id,
                cluster_enabled = config.cluster.enabled,
                "configuration loaded"
            );
            app::serve_with_options(
                config,
                app::ServeOptions {
                    decline_startup_cleanup,
                },
            )
            .await
        }
        Some(Command::Cluster { command }) => run_cluster(&config, command).await,
    }
}

async fn run_cluster(config: &ModeAwareConfig, command: ClusterCommand) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = format!("http://{}", config.proxy.admin_listen);
    let (method, path, body, output) = match command {
        ClusterCommand::Status { json } => (
            reqwest::Method::GET,
            "/cluster",
            None,
            Output::Status { json },
        ),
        ClusterCommand::Doctor { json } => (
            reqwest::Method::GET,
            "/cluster",
            None,
            Output::Doctor { json },
        ),
        ClusterCommand::Reconcile => (
            reqwest::Method::POST,
            "/cluster/reconcile",
            None,
            Output::Json,
        ),
        ClusterCommand::Pair => (reqwest::Method::POST, "/cluster/pair", None, Output::Json),
        ClusterCommand::Promote => (
            reqwest::Method::POST,
            "/cluster/promote",
            None,
            Output::Json,
        ),
        ClusterCommand::Demote { reason } => (
            reqwest::Method::POST,
            "/cluster/demote",
            Some(json!({"reason": reason})),
            Output::Json,
        ),
        ClusterCommand::Restart => (
            reqwest::Method::POST,
            "/cluster/restart",
            None,
            Output::Json,
        ),
        ClusterCommand::Fingerprint { profile } => (
            reqwest::Method::POST,
            "/cluster/fingerprint",
            Some(json!({"profile": match profile {
                Profile::Standalone => "standalone",
                Profile::Distributed => "distributed",
            }})),
            Output::Json,
        ),
    };
    let mutation = method == reqwest::Method::POST;
    let mut request = client.request(method, format!("{base}{path}"));
    if mutation {
        let token = tokio::fs::read(&config.cluster.security.admin_token_file)
            .await
            .context("failed to read admin token")?;
        request = request.bearer_auth(encode_token(&token));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.context("admin API request failed")?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .context("admin API returned invalid JSON")?;
    anyhow::ensure!(
        status.is_success() || status == StatusCode::ACCEPTED,
        "admin API returned {status}: {value}"
    );
    match output {
        Output::Json | Output::Status { json: true } => {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Output::Status { json: false } => print_human_status(&value),
        Output::Doctor { json } => {
            let report = doctor_report(value);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "doctor={} state={} target_ready={}",
                    if report["healthy"].as_bool() == Some(true) {
                        "ok"
                    } else {
                        "failed"
                    },
                    report["cluster"]["state"].as_str().unwrap_or("unknown"),
                    report["cluster"]["target_ready"].as_bool().unwrap_or(false),
                );
            }
        }
    }
    Ok(())
}

enum Output {
    Status { json: bool },
    Doctor { json: bool },
    Json,
}

fn doctor_report(cluster: Value) -> Value {
    let target_ready = cluster["target_ready"].as_bool() == Some(true);
    let state = cluster["state"].as_str().unwrap_or("unknown");
    let safe_state = state != "manual-intervention-required" && state != "booting";
    let admission_serving = cluster["admission"]["state"].as_str() == Some("serving");
    json!({
        "healthy": target_ready && safe_state && admission_serving,
        "checks": {
            "target_ready": target_ready,
            "safe_state": safe_state,
            "admission_serving": admission_serving,
        },
        "cluster": cluster,
    })
}

fn print_human_status(value: &Value) {
    println!(
        "node={} role={} mode={} state={} target={} ready={}",
        value["node_id"].as_str().unwrap_or("unknown"),
        value["role"].as_str().unwrap_or("unknown"),
        value["mode"].as_str().unwrap_or("unknown"),
        value["state"].as_str().unwrap_or("unknown"),
        value["target"].as_str().unwrap_or("unknown"),
        value["target_ready"].as_bool().unwrap_or(false),
    );
}

fn initialize_logging(config: &ModeAwareConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("siderostat={}", config.logging.level)));
    match config.logging.format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_target(false)
            .with_env_filter(filter)
            .init(),
        LogFormat::Text => tracing_subscriber::fmt()
            .with_ansi(false)
            .with_target(false)
            .with_env_filter(filter)
            .init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_explicit_serve_select_the_supervisor_path() {
        let legacy = Args::try_parse_from(["siderostat", "--config", "node.toml"]).unwrap();
        let explicit =
            Args::try_parse_from(["siderostat", "serve", "--config", "node.toml"]).unwrap();
        assert!(legacy.command.is_none());
        assert!(matches!(
            explicit.command,
            Some(Command::Serve {
                decline_startup_cleanup: false
            })
        ));
    }

    #[test]
    fn serve_can_explicitly_decline_startup_cleanup() {
        let args =
            Args::try_parse_from(["siderostat", "serve", "--decline-startup-cleanup"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Serve {
                decline_startup_cleanup: true
            })
        ));
    }

    #[test]
    fn every_cluster_command_selects_only_the_admin_client_path() {
        for args in [
            vec!["siderostat", "cluster", "status"],
            vec!["siderostat", "cluster", "doctor"],
            vec!["siderostat", "cluster", "reconcile"],
            vec!["siderostat", "cluster", "pair"],
            vec!["siderostat", "cluster", "promote"],
            vec!["siderostat", "cluster", "demote"],
            vec!["siderostat", "cluster", "restart"],
            vec![
                "siderostat",
                "cluster",
                "fingerprint",
                "--profile",
                "standalone",
            ],
        ] {
            let parsed = Args::try_parse_from(args).unwrap();
            assert!(matches!(parsed.command, Some(Command::Cluster { .. })));
        }
    }

    #[test]
    fn doctor_reports_unready_or_manual_state() {
        let report = doctor_report(json!({
            "state": "manual-intervention-required",
            "target_ready": true,
            "admission": {"state": "serving"},
        }));
        assert_eq!(report["healthy"], false);
        assert_eq!(report["checks"]["safe_state"], false);
    }
}
