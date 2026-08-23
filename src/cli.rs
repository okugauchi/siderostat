use crate::{
    app,
    canary::CanaryExecutor,
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
    /// Graceful runtime restart via `/admin/restart` (C-04c). Unlike the
    /// cluster `Restart` above, this drains in-flight requests and relaunches
    /// the whole runtime process through launchd.
    GracefulRestart,
    /// Run one bounded inference request against this node's local public endpoint.
    Canary {
        #[arg(long)]
        json: bool,
    },
    /// Start or inspect the single-owner degraded-throughput recovery job.
    RecoverDegraded {
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        trigger: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        json: bool,
    },
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

/// Resolve the admin API request (method, path, body, output) for a cluster
/// subcommand. Pure so tests can verify route selection (e.g. the graceful
/// runtime restart uses `/admin/restart`, distinct from the cluster restart's
/// `/cluster/restart`).
fn cluster_request(
    command: ClusterCommand,
) -> (reqwest::Method, &'static str, Option<Value>, Output) {
    match command {
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
        ClusterCommand::GracefulRestart => {
            (reqwest::Method::POST, "/admin/restart", None, Output::Json)
        }
        ClusterCommand::Canary { .. } => unreachable!("canary uses the public endpoint path"),
        ClusterCommand::RecoverDegraded { .. } => {
            unreachable!("recovery uses its dedicated admin request path")
        }
        ClusterCommand::Fingerprint { profile } => (
            reqwest::Method::POST,
            "/cluster/fingerprint",
            Some(json!({"profile": match profile {
                Profile::Standalone => "standalone",
                Profile::Distributed => "distributed",
            }})),
            Output::Json,
        ),
    }
}

async fn run_cluster(config: &ModeAwareConfig, command: ClusterCommand) -> anyhow::Result<()> {
    match command {
        ClusterCommand::Canary { json } => run_canary(config, json).await,
        ClusterCommand::RecoverDegraded {
            reason,
            trigger,
            idempotency_key,
            status,
            json,
        } => run_recovery_command(config, reason, trigger, idempotency_key, status, json).await,
        command => run_admin_command(config, command).await,
    }
}

async fn run_admin_command(
    config: &ModeAwareConfig,
    command: ClusterCommand,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = format!("http://{}", config.proxy.admin_listen);
    let (method, path, body, output) = cluster_request(command);
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

async fn run_recovery_command(
    config: &ModeAwareConfig,
    reason: Option<String>,
    trigger: Option<String>,
    idempotency_key: Option<String>,
    status_id: Option<String>,
    json_output: bool,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let token = tokio::fs::read(&config.cluster.security.admin_token_file)
        .await
        .context("failed to read admin token")?;
    let base = format!("http://{}", config.proxy.admin_listen);
    let (method, url, body) = if let Some(status_id) = status_id {
        anyhow::ensure!(
            reason.is_none() && trigger.is_none() && idempotency_key.is_none(),
            "--status cannot be combined with --reason, --trigger, or --idempotency-key"
        );
        let uuid = uuid::Uuid::parse_str(&status_id).context("--status must be a UUID")?;
        (
            reqwest::Method::GET,
            format!("{base}/cluster/recover-degraded/{uuid}"),
            None,
        )
    } else {
        let reason = reason.context("--reason is required when starting recovery")?;
        let trigger = trigger.context("--trigger is required when starting recovery")?;
        anyhow::ensure!(reason == "throughput-degraded", "unsupported --reason");
        anyhow::ensure!(
            matches!(
                trigger.as_str(),
                "manual-canary-failure"
                    | "progress-stall"
                    | "low-decode-tps"
                    | "first-token-timeout"
            ),
            "unsupported --trigger"
        );
        let idempotency_key = idempotency_key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        (
            reqwest::Method::POST,
            format!("{base}/cluster/recover-degraded"),
            Some(json!({
                "reason": reason,
                "trigger": trigger,
                "idempotency_key": idempotency_key,
            })),
        )
    };
    let mut request = client
        .request(method, url)
        .bearer_auth(encode_token(&token));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .context("recovery admin API request failed")?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .context("recovery admin API returned invalid JSON")?;
    anyhow::ensure!(
        status.is_success() || status == StatusCode::ACCEPTED,
        "recovery admin API returned {status}: {value}"
    );
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "operation={} recovery_id={} state={} phase={}",
            value["operation"].as_str().unwrap_or("recover-degraded"),
            value["recovery_id"].as_str().unwrap_or("unknown"),
            value["state"].as_str().unwrap_or("unknown"),
            value["phase"].as_str().unwrap_or("unknown"),
        );
    }
    Ok(())
}

async fn run_canary(config: &ModeAwareConfig, json_output: bool) -> anyhow::Result<()> {
    let result = CanaryExecutor::default_for(config.proxy.public_listen)?
        .execute()
        .await;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "status={} reason={:?} elapsed_ms={} ttfb_ms={} generated_tokens={} chunk_tps={} http_status={}",
            result.status,
            result.reason,
            result.elapsed_ms,
            result
                .ttfb_ms
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            result.generated_tokens,
            result
                .chunk_tps
                .map_or_else(|| "-".to_owned(), |value| format!("{value:.3}")),
            result
                .http_status
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
        );
    }
    anyhow::ensure!(
        result.reason == crate::canary::CanaryReason::Healthy,
        "canary failed: {:?}",
        result.reason
    );
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
            vec!["siderostat", "cluster", "graceful-restart"],
            vec!["siderostat", "cluster", "canary"],
            vec![
                "siderostat",
                "cluster",
                "recover-degraded",
                "--reason",
                "throughput-degraded",
                "--trigger",
                "manual-canary-failure",
            ],
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
    fn graceful_restart_selects_admin_restart_route_distinct_from_cluster_restart() {
        // C-04c: graceful runtime restart は `/admin/restart` を選び、
        // cluster 再構成用の `/cluster/restart` と区別される。
        let (method, path, body, output) = cluster_request(ClusterCommand::GracefulRestart);
        assert_eq!(method, reqwest::Method::POST);
        assert_eq!(path, "/admin/restart");
        assert!(body.is_none());
        assert!(matches!(output, Output::Json));

        let (cluster_method, cluster_path, _, _) = cluster_request(ClusterCommand::Restart);
        assert_eq!(cluster_method, reqwest::Method::POST);
        assert_eq!(cluster_path, "/cluster/restart");
        assert_ne!(path, cluster_path);
    }

    #[test]
    fn graceful_restart_is_a_mutating_admin_route_thus_bearer_authenticated() {
        // mutation (POST) なので `run_cluster` は admin token を読み、
        // bearer auth を付与する。unauthenticated endpoint は使われない。
        let (method, path, _, _) = cluster_request(ClusterCommand::GracefulRestart);
        assert_eq!(method, reqwest::Method::POST);
        assert!(path.starts_with("/admin/") || path.starts_with("/cluster/"));
        // POST は全て mutation 扱い。GET の status/doctor と区別される。
        let (status_method, _, _, _) = cluster_request(ClusterCommand::Status { json: true });
        assert_eq!(status_method, reqwest::Method::GET);
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
