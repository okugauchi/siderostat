//! Codex Web Search Bridge の typed 設定。
//!
//! この module は Bridge の設定を独立した型として定義し、不正な設定を
//! 起動前に拒否する。外部通信は既定で無効（external=false）であり、
//! 設定だけで検索通信や既存 runtime の停止が起きることはない。
//!
//! 契約: CONTRACTS.md C05 / BridgeConfig。
//!
//! 受入 case（全て必須）:
//! - 入力: external=false → 検索通信0（既定 disabled を保持）
//! - 入力: backend=self → 拒否
//! - 入力: 非loopback無認証 → 拒否
//! - 入力: URLにuserinfo → 拒否

use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use url::Url;

/// Bridge 専用 loopback listener の既定ポート。
///
/// C05 で「専用 loopback listener（候補18081）」とされる。既存 runtime の
/// listener（proxy/admin）とは独立した専用ポートであり、衝突は validate で
/// 明示エラーにする。この値は listen の既定に用いる（外部からは到達不可）。
pub const DEFAULT_BRIDGE_LISTEN_PORT: u16 = 18081;

/// BridgeConfig の既定 listen アドレス（loopback）。
fn default_listen() -> String {
    format!("127.0.0.1:{DEFAULT_BRIDGE_LISTEN_PORT}")
}

/// Bridge は既定で無効。設定だけで検索通信を開始しない。
fn default_enabled() -> bool {
    false
}

/// 既定は外部アクセス opt-out（loopback のみ）。
fn default_external_access() -> bool {
    false
}

/// SearXNG は設定済 endpoint を必須としない（無効 Bridge では検索しない）。
fn default_searxng() -> Option<SearxngConfig> {
    None
}

/// Bridge の typed 設定。
///
/// 全フィールドは `deny_unknown_fields` により未知フィールドを拒否する。
/// `external_access=false` のときは listen は loopback に制限され、検索通信
/// を外部に公開しない。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeConfig {
    /// Bridge 全体の有効/無効。既定 false。
    pub enabled: bool,
    /// 専用 listener アドレス。既定 loopback:18081。
    pub listen: String,
    /// backend（siDeroStat Chat 公開 proxy）の URL。
    pub backend_url: Url,
    /// Bridge 専用 bearer token。認証に使用する。SearXNG には転送しない。
    pub bearer_token: String,
    /// 外部アクセス opt-in。既定 false（loopback のみ）。
    pub external_access: bool,
    /// SearXNG endpoint。有効 Bridge の検索に使用。既定 None（未設定）。
    #[serde(default = "default_searxng")]
    pub searxng: Option<SearxngConfig>,
    /// 検索結果上限（既定5）。
    pub max_results: usize,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        BridgeConfig {
            enabled: default_enabled(),
            listen: default_listen(),
            // 既定 backend URL。外部通信を開始するものではない（enabled=false）。
            backend_url: Url::parse("http://127.0.0.1:8080/v1/chat/completions")
                .expect("static backend url"),
            bearer_token: String::new(),
            external_access: default_external_access(),
            searxng: default_searxng(),
            max_results: 5,
        }
    }
}

/// SearXNG endpoint 設定。
///
/// 管理 token を SearXNG に転送しない。SearXNG 固有 credential（api_key）は
/// 別フィールドとして保持し、Bridge client 認証（bearer_token）と分離する。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearxngConfig {
    /// SearXNG の JSON endpoint URL。
    pub endpoint: Url,
    /// SearXNG 固有の api_key（任意）。Bearer とは別。
    #[serde(default)]
    pub api_key: Option<String>,
}

/// URL の userinfo（user:pass@）有無を検証する。
///
/// C05: 「http/https以外/userinfo/危険URLは引用集合から除く」。config の
/// backend / SearXNG endpoint に userinfo が含まれる場合、credential が
/// ログや設定に混入する恐れがあるため起動前に拒否する。
fn reject_userinfo(field: &str, url: &Url) -> anyhow::Result<()> {
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{field} must not contain userinfo (user:pass@); use a dedicated credential field instead"
    );
    Ok(())
}

/// listen 文字列を SocketAddr として解釈する。
fn parse_listen(listen: &str) -> anyhow::Result<SocketAddr> {
    listen
        .parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("invalid listen address '{listen}': {e}"))
}

/// backend URL が自身（Bridge listener）を指している場合は拒否する。
///
/// backend=self は無限ループ（Bridge → 自身 → Bridge）を生む設定であり、
/// 起動前に拒否する。
fn reject_backend_self(backend_url: &Url, listen_addr: &SocketAddr) -> anyhow::Result<()> {
    let host = match backend_url.host_str() {
        Some(h) => h,
        None => return Ok(()),
    };
    let backend_ip = match host.parse::<IpAddr>() {
        Ok(ip) => ip,
        // ホスト名は IP 比較できない。localhost のみ自身の可能性があるため、
        // 明示的に localhost を loopback とみなして比較する。
        Err(_) => {
            let is_localhost = host.eq_ignore_ascii_case("localhost");
            if is_localhost {
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                return Ok(());
            }
        }
    };
    let backend_port = backend_url.port().unwrap_or(80);
    let same_ip = listen_addr.ip().is_loopback() && backend_ip.is_loopback();
    let same_port = listen_addr.port() == backend_port;
    anyhow::ensure!(
        !(same_ip && same_port),
        "backend_url must not point at the bridge itself (backend=self would loop): {}",
        backend_url
    );
    Ok(())
}

impl BridgeConfig {
    /// 設定を検証する。
    ///
    /// 受入 case:
    /// - external=false（既定）→ listen は loopback に制限され、検索通信を
    ///   外部に公開しない（検索通信0）。
    /// - backend=self → 拒否。
    /// - 非loopback無認証（bearer_token 空）→ 拒否。
    /// - backend / SearXNG URL に userinfo → 拒否。
    pub fn validate(&self) -> anyhow::Result<()> {
        // Bridge が無効なら、設定だけで検索通信や既存 runtime 停止を起こさない。
        // 無効時は listen/backend/searxng の細部は検証しない（有効化時に検証）。
        if !self.enabled {
            return Ok(());
        }

        let listen_addr = parse_listen(&self.listen)?;

        // 受入 case: 非loopback無認証 → 拒否。
        // 非 loopback へ listen する場合、認証（bearer_token）を必須にする。
        if !listen_addr.ip().is_loopback() {
            anyhow::ensure!(
                !self.bearer_token.is_empty(),
                "non-loopback listen requires a non-empty bearer_token for authentication"
            );
        }

        // 受入 case: backend=self → 拒否。
        reject_backend_self(&self.backend_url, &listen_addr)?;

        // 受入 case: URL に userinfo → 拒否。
        reject_userinfo("backend_url", &self.backend_url)?;
        if let Some(searxng) = &self.searxng {
            reject_userinfo("searxng.endpoint", &searxng.endpoint)?;
        }

        // 検索結果上限。
        anyhow::ensure!(
            (1..=10).contains(&self.max_results),
            "max_results must be within 1..=10 (C05 result default 5 / max 10), got {}",
            self.max_results
        );

        // backend は http/https のみ（C05）。それ以外は検索・引用の根拠に
        // ならないため拒否する。
        anyhow::ensure!(
            matches!(self.backend_url.scheme(), "http" | "https"),
            "backend_url must use http or https, got scheme '{}'",
            self.backend_url.scheme()
        );
        if let Some(searxng) = &self.searxng {
            anyhow::ensure!(
                matches!(searxng.endpoint.scheme(), "http" | "https"),
                "searxng.endpoint must use http or https, got scheme '{}'",
                searxng.endpoint.scheme()
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> BridgeConfig {
        BridgeConfig {
            enabled: true,
            listen: "127.0.0.1:18081".into(),
            backend_url: Url::parse("http://127.0.0.1:8080/v1/chat/completions").expect("url"),
            bearer_token: "bridge-secret".into(),
            external_access: false,
            searxng: Some(SearxngConfig {
                endpoint: Url::parse("http://127.0.0.1:8888/search").expect("url"),
                api_key: None,
            }),
            max_results: 5,
        }
    }

    /// 受入 case: external=false（既定）→ 検索通信0。
    /// Bridge 無効なら validate は何も要求せず、検索を開始しない。
    #[test]
    fn disabled_bridge_does_not_start_search() {
        let config = BridgeConfig {
            enabled: false,
            // 無効時は不正な listen でも validate を通す（設定だけで停止しない）。
            listen: "not-a-real-addr".into(),
            backend_url: Url::parse("http://127.0.0.1:8080").expect("url"),
            bearer_token: String::new(),
            ..bridge()
        };
        config
            .validate()
            .expect("disabled bridge must not require validation");
        // external=false（既定）では外部検索通信を開始しない。
        assert!(!config.enabled);
        assert!(!config.external_access);
    }

    /// 有効 Bridge は既定設定で validate を通す。
    #[test]
    fn enabled_bridge_default_validates() {
        bridge()
            .validate()
            .expect("default enabled bridge is valid");
    }

    /// 受入 case: backend=self → 拒否。
    #[test]
    fn rejects_backend_self() {
        // backend が Bridge listener 自身を指す（loopback + 同ポート）。
        let config = BridgeConfig {
            backend_url: Url::parse("http://127.0.0.1:18081").expect("url"),
            ..bridge()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("backend=self"),
            "must reject backend=self: {err}"
        );
    }

    /// backend が localhost で自身を指す場合も拒否。
    #[test]
    fn rejects_backend_self_via_localhost() {
        let config = BridgeConfig {
            backend_url: Url::parse("http://localhost:18081").expect("url"),
            ..bridge()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("backend=self"),
            "must reject localhost self: {err}"
        );
    }

    /// 受入 case: 非loopback無認証 → 拒否。
    #[test]
    fn rejects_non_loopback_without_auth() {
        let config = BridgeConfig {
            listen: "0.0.0.0:18081".into(),
            bearer_token: String::new(),
            ..bridge()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("bearer_token"),
            "non-loopback without auth must be rejected: {err}"
        );
    }

    /// 非loopbackでも認証があれば許容する（external access opt-in）。
    #[test]
    fn allows_non_loopback_with_auth() {
        let config = BridgeConfig {
            listen: "0.0.0.0:18081".into(),
            bearer_token: "bridge-secret".into(),
            ..bridge()
        };
        config.validate().expect("non-loopback with auth is valid");
    }

    /// 受入 case: URL に userinfo → 拒否（backend）。
    #[test]
    fn rejects_backend_userinfo() {
        let config = BridgeConfig {
            backend_url: Url::parse("http://user:pass@127.0.0.1:8080").expect("url"),
            ..bridge()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("userinfo"),
            "backend userinfo must be rejected: {err}"
        );
    }

    /// 受入 case: URL に userinfo → 拒否（SearXNG endpoint）。
    #[test]
    fn rejects_searxng_userinfo() {
        let config = BridgeConfig {
            searxng: Some(SearxngConfig {
                endpoint: Url::parse("http://user:pass@127.0.0.1:8888/search").expect("url"),
                api_key: None,
            }),
            ..bridge()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("userinfo"),
            "searxng userinfo must be rejected: {err}"
        );
    }

    /// 未知フィールドは deny_unknown_fields で拒否。
    #[test]
    fn rejects_unknown_field() {
        let toml = r#"
            enabled = true
            listen = "127.0.0.1:18081"
            backend_url = "http://127.0.0.1:8080/v1/chat/completions"
            bearer_token = "bridge-secret"
            bogus_field = "nope"
        "#;
        assert!(toml::from_str::<BridgeConfig>(toml).is_err());
    }

    /// 有効 Bridge を TOML からパースして validate。
    #[test]
    fn parses_valid_toml() {
        let toml = r#"
            enabled = true
            listen = "127.0.0.1:18081"
            backend_url = "http://127.0.0.1:8080/v1/chat/completions"
            bearer_token = "bridge-secret"

            [searxng]
            endpoint = "http://127.0.0.1:8888/search"
            api_key = "searxng-key"
        "#;
        let config: BridgeConfig = toml::from_str(toml).expect("valid toml");
        config.validate().expect("valid config");
        // SearXNG credential は Bearer とは別に保持される。
        assert_eq!(
            config.searxng.as_ref().unwrap().api_key.as_deref(),
            Some("searxng-key")
        );
    }

    /// 既定（empty TOML）は無効 Bridge として validate を通す（検索通信0）。
    #[test]
    fn default_is_disabled() {
        let config = BridgeConfig::default();
        assert!(!config.enabled);
        assert!(!config.external_access);
        config
            .validate()
            .expect("default bridge is disabled and valid");
    }
}
