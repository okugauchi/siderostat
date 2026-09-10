//! W01 — Bridge config と独立モジュール境界。受入 matrix。
//!
//! 本ファイルは W01 カードの target_commands にある `v040_web_config.rs` に
//! 対応する。公開 API（`siderostat::websearch::BridgeConfig`）を介して
//! 受入 case を検証する。本番 config validate を直接駆動し、実プロセス
//! 起動は行わない（dry-run / fake 境界）。
//!
//! 受入 case（全て必須）:
//! - 入力: external=false → 検索通信0（既定 disabled を保持）
//! - 入力: backend=self → 拒否
//! - 入力: 非loopback無認証 → 拒否
//! - 入力: URLにuserinfo → 拒否
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。

use siderostat::websearch::{BridgeConfig, config::SearxngConfig};

fn base() -> BridgeConfig {
    BridgeConfig {
        enabled: true,
        listen: "127.0.0.1:18081".into(),
        backend_url: "http://127.0.0.1:8080/v1/chat/completions"
            .parse()
            .expect("url"),
        bearer_token: "bridge-secret".into(),
        searxng: Some(SearxngConfig {
            endpoint: "http://127.0.0.1:8888/search".parse().expect("url"),
            api_key: None,
        }),
        ..BridgeConfig::default()
    }
}

/// 受入 case 1: external=false → 検索通信0。
/// Bridge 既定 disabled（external=false）では validate が要求せず、検索を開始しない。
#[test]
fn w01_external_false_keeps_search_off() {
    // 既定は disabled（検索通信0）。
    let default_cfg = BridgeConfig::default();
    assert!(!default_cfg.enabled);
    assert!(!default_cfg.external_access);
    default_cfg
        .validate()
        .expect("default disabled bridge is valid");

    // 無効 Bridge は不正 listen でも validate を通す（設定だけで停止しない）。
    let cfg = BridgeConfig {
        enabled: false,
        listen: "not-a-valid-addr".into(),
        ..base()
    };
    cfg.validate()
        .expect("disabled bridge must not require validation");
}

/// 受入 case 2: backend=self → 拒否。
#[test]
fn w01_backend_self_is_rejected() {
    let cfg = BridgeConfig {
        backend_url: "http://127.0.0.1:18081".parse().expect("url"),
        ..base()
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(
        err.contains("backend=self"),
        "must reject backend=self: {err}"
    );
}

/// backend が localhost で自身を指す場合も拒否。
#[test]
fn w01_backend_self_localhost_is_rejected() {
    let cfg = BridgeConfig {
        backend_url: "http://localhost:18081".parse().expect("url"),
        ..base()
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(
        err.contains("backend=self"),
        "must reject localhost self: {err}"
    );
}

/// 受入 case 3: 非loopback無認証 → 拒否。
#[test]
fn w01_non_loopback_without_auth_is_rejected() {
    let cfg = BridgeConfig {
        listen: "0.0.0.0:18081".into(),
        bearer_token: String::new(),
        ..base()
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(
        err.contains("bearer_token"),
        "must require auth on non-loopback: {err}"
    );
}

/// 非loopbackでも認証があれば許容（external access opt-in）。
#[test]
fn w01_non_loopback_with_auth_is_allowed() {
    let cfg = BridgeConfig {
        listen: "0.0.0.0:18081".into(),
        bearer_token: "bridge-secret".into(),
        ..base()
    };
    cfg.validate().expect("non-loopback with auth is valid");
}

/// 受入 case 4: URL に userinfo → 拒否（backend）。
#[test]
fn w01_backend_userinfo_is_rejected() {
    let cfg = BridgeConfig {
        backend_url: "http://user:pass@127.0.0.1:8080".parse().expect("url"),
        ..base()
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(
        err.contains("userinfo"),
        "backend userinfo must be rejected: {err}"
    );
}

/// 受入 case 4: URL に userinfo → 拒否（SearXNG endpoint）。
#[test]
fn w01_searxng_userinfo_is_rejected() {
    let cfg = BridgeConfig {
        searxng: Some(SearxngConfig {
            endpoint: "http://user:pass@127.0.0.1:8888/search"
                .parse()
                .expect("url"),
            api_key: None,
        }),
        ..base()
    };
    let err = cfg.validate().unwrap_err().to_string();
    assert!(
        err.contains("userinfo"),
        "searxng userinfo must be rejected: {err}"
    );
}

/// 未知フィールドは deny_unknown_fields で拒否。
#[test]
fn w01_unknown_field_is_rejected() {
    let toml = r#"
        enabled = true
        listen = "127.0.0.1:18081"
        backend_url = "http://127.0.0.1:8080/v1/chat/completions"
        bearer_token = "bridge-secret"
        bogus_field = "nope"
    "#;
    assert!(toml::from_str::<BridgeConfig>(toml).is_err());
}

/// 有効 Bridge を TOML からパースして validate。SearXNG credential は Bearer と分離。
#[test]
fn w01_valid_toml_parses_and_validates() {
    let toml = r#"
        enabled = true
        listen = "127.0.0.1:18081"
        backend_url = "http://127.0.0.1:8080/v1/chat/completions"
        bearer_token = "bridge-secret"

        [searxng]
        endpoint = "http://127.0.0.1:8888/search"
        api_key = "searxng-key"
    "#;
    let cfg: BridgeConfig = toml::from_str(toml).expect("valid toml");
    cfg.validate().expect("valid config");
    assert_eq!(
        cfg.searxng.as_ref().unwrap().api_key.as_deref(),
        Some("searxng-key")
    );
}
