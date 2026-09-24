//! Codex Web Search Bridge — 専用 binary。W08。
//!
//! C05 に基づき、Bridge 専用 loopback listener（既定 127.0.0.1:18081）で
//! BridgeServer を起動する。既存の wildcard proxy / Admin API は横取りせず、
//! 本 Bridge の専用 listener にのみバインドする。W08。
//!
//! 設定は BridgeConfig（toml / env）から読み、validate で不正設定を起動前に
//! 拒否する（backend=self・非loopback無認証・URL userinfo 等は W01 で拒否済み）。
//! 稼働中の ds4-server には非接触（enabled=false なら listen しない）。W08。
//!
//! 受入 case（全て必須）:
//! - 入力: client 切断 → 全 pending 解放
//! - 入力: 遅い client → 上限超えず timeout
//! - 入力: bad token → provider 通信 0
//! - 入力: DS4 配信中失敗 → failed terminal
//!
//! （実 HTTP 通信は dry-run 対象外。テストは fake 境界で検証する。）W08。

use std::sync::Arc;

use siderostat::websearch::backend::SearchBackend;
use siderostat::websearch::chat_client::ReqwestChatClient;
use siderostat::websearch::config::BridgeConfig;
use siderostat::websearch::searxng::{ReqwestTransport, SearxngBackend};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // tracing 初期化。W08。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // BridgeConfig を読み、validate で起動前に拒否する。W08。
    let config = load_config()?;
    if !config.enabled {
        // 既定 disabled。設定だけで検索通信や listen を開始しない。W08。
        tracing::info!("websearch bridge is disabled (enabled=false), not listening");
        return Ok(());
    }
    config.validate()?;

    // 専用 loopback listener を bind する。衝突は明示エラー。W08。
    let addr: std::net::SocketAddr = config
        .listen
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bridge listen address {:?}: {e}", config.listen))?;
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        anyhow::anyhow!(
            "bridge listen {} failed (address in use or not allowed): {e}",
            config.listen
        )
    })?;
    tracing::info!("websearch bridge listening on {}", listener.local_addr()?);

    // 実 DS4 Chat clientをbackend URLへ接続する。modelは環境変数で明示的に
    // 上書きできるが、既定はH02で配置したDeepSeek V4 Flash aliasとする。
    let model =
        std::env::var("SIDEROSTAT_BRIDGE_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let max_output_tokens = std::env::var("SIDEROSTAT_BRIDGE_MAX_OUTPUT_TOKENS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1024);
    let chat = ReqwestChatClient::new(config.backend_url.clone())?
        .with_model(model)
        .with_max_output_tokens(max_output_tokens);

    // SearXNG endpointが設定されていない場合は検索を実行しない。設定済みの場合は
    // adapterを本番HTTP backendとして接続する。Bridge tokenは転送しない。
    let search: Arc<dyn SearchBackend> = match config.searxng.clone() {
        Some(searxng) => Arc::new(
            SearxngBackend::new(
                searxng.endpoint,
                searxng.api_key,
                Box::new(ReqwestTransport::default()),
            )
            .with_max_results(config.max_results),
        ),
        None => siderostat::websearch::server::noop_search(),
    };
    let server = siderostat::websearch::server::BridgeServer::new(config, Arc::new(chat), search);
    let app = server.router();

    axum::serve(listener, app).await?;
    Ok(())
}

/// BridgeConfig を読み込む。W08。
///
/// 現状は既定値 + 環境変数 `SIDEROSTAT_BRIDGE_CONFIG`（toml ファイル path）を
/// 読む。ファイルが無ければ既定（disabled）を返す。W08。
fn load_config() -> anyhow::Result<BridgeConfig> {
    if let Ok(path) = std::env::var("SIDEROSTAT_BRIDGE_CONFIG") {
        let toml_str = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("read bridge config {path}: {e}"))?;
        let cfg: BridgeConfig = toml::from_str(&toml_str)
            .map_err(|e| anyhow::anyhow!("parse bridge config {path}: {e}"))?;
        return Ok(cfg);
    }
    Ok(BridgeConfig::default())
}
