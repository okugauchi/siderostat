use anyhow::Result;
use clap::Parser;
use ds4_smart_proxy::{
    app,
    config::{Config, LogFormat},
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
    let (config, config_path) = Config::load(args.config.as_deref()).await?;
    initialize_logging(&config);
    info!(
        config_path = %config_path.display(),
        listen = %config.listen,
        admin_listen = %config.admin_listen,
        backends = ?config.backends.iter().map(|backend| &backend.id).collect::<Vec<_>>(),
        "configuration loaded"
    );

    app::serve(config).await
}

fn initialize_logging(config: &Config) {
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
