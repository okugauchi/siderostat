use anyhow::Result;
use clap::Parser;
use ds4_smart_proxy::{
    app,
    config::{LogFormat, ModeAwareConfig},
};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(long, short)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (config, config_path) = ModeAwareConfig::load(args.config.as_deref()).await?;
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

fn initialize_logging(config: &ModeAwareConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("ds4_smart_proxy={}", config.logging.level)));
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
