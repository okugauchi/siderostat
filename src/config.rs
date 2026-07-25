use axum::http::{HeaderName, HeaderValue};
use serde::Deserialize;
use std::{
    collections::HashSet,
    env,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

const DEFAULT_CONFIG_FILE: &str = "ds4-smart-proxy.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_admin_listen")]
    pub admin_listen: SocketAddr,
    #[serde(default = "default_body_limit")]
    pub request_body_limit_bytes: usize,
    #[serde(default = "default_body_limit")]
    pub max_replayable_body_bytes: usize,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub active_probe: ActiveProbeConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub cooldown: CooldownConfig,
    #[serde(default)]
    pub affinity: AffinityConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub mode: RoutingMode,
    pub affinity_busy_policy: AffinityBusyPolicy,
    #[serde(deserialize_with = "deserialize_duration")]
    pub affinity_busy_grace: Duration,
    pub prefer_distributed_for_affinity: bool,
    pub max_attempts: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: RoutingMode::LocalFirst,
            affinity_busy_policy: AffinityBusyPolicy::Fail,
            affinity_busy_grace: Duration::from_secs(2),
            prefer_distributed_for_affinity: false,
            max_attempts: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingMode {
    LocalFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AffinityBusyPolicy {
    Fail,
    Spill,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeartbeatConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub interval: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub timeout: Duration,
    pub failure_threshold: u32,
    pub jitter_ratio: f64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(3),
            failure_threshold: 2,
            jitter_ratio: 0.20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActiveProbeConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_duration")]
    pub timeout: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub minimum_interval: Duration,
    pub model: String,
}

impl Default for ActiveProbeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(5),
            minimum_interval: Duration::from_secs(300),
            model: "deepseek-v4-flash".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub connect: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub response_headers: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub first_body_byte: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub stream_idle: Duration,
    #[serde(default, deserialize_with = "deserialize_optional_duration")]
    pub total: Option<Duration>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            response_headers: Duration::from_secs(60),
            first_body_byte: Duration::from_secs(300),
            stream_idle: Duration::from_secs(300),
            total: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CooldownConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub duration: Duration,
    pub consecutive_failure_threshold: u32,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(300),
            consecutive_failure_threshold: 2,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AffinityConfig {
    pub enabled: bool,
    pub secret_env: String,
    pub database_path: Option<PathBuf>,
    pub compute_prefix_affinity: bool,
    pub minimum_prefix_bytes: usize,
    pub maximum_prefix_hash_bytes: usize,
    #[serde(deserialize_with = "deserialize_duration")]
    pub default_sliding_ttl: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub default_absolute_ttl: Duration,
    pub max_entries: usize,
    pub allow_body_ids: bool,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            secret_env: "DS4_SMART_PROXY_AFFINITY_SECRET".to_string(),
            database_path: None,
            compute_prefix_affinity: false,
            minimum_prefix_bytes: 4096,
            maximum_prefix_hash_bytes: 1_048_576,
            default_sliding_ttl: Duration::from_secs(7 * 86_400),
            default_absolute_ttl: Duration::from_secs(30 * 86_400),
            max_entries: 100_000,
            allow_body_ids: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub format: LogFormat,
    pub level: String,
    pub timezone: Option<String>,
    pub redact_headers: Vec<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: "info".to_string(),
            timezone: None,
            redact_headers: vec![
                "authorization".into(),
                "proxy-authorization".into(),
                "x-api-key".into(),
                "x-ds4-affinity-key".into(),
                "x-hermes-session-id".into(),
                "x-hermes-session-key".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    pub id: String,
    pub url: Url,
    #[serde(default)]
    pub kind: BackendKind,
    #[serde(default)]
    pub local: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    #[serde(default = "default_heartbeat_path")]
    pub heartbeat_path: String,
    #[serde(default)]
    pub static_headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub danger_accept_invalid_certs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Standalone,
    Distributed,
}

impl Config {
    pub async fn load(explicit: Option<&Path>) -> anyhow::Result<(Self, PathBuf)> {
        let path = resolve_config_path(explicit)?;
        let contents = tokio::fs::read_to_string(&path).await?;
        let config: Self = toml::from_str(&contents)?;
        config.validate()?;
        Ok((config, path))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.backends.is_empty(), "backends must not be empty");
        anyhow::ensure!(
            self.request_body_limit_bytes > 0,
            "request_body_limit_bytes must be greater than zero"
        );
        anyhow::ensure!(
            self.max_replayable_body_bytes <= self.request_body_limit_bytes,
            "max_replayable_body_bytes must not exceed request_body_limit_bytes"
        );
        anyhow::ensure!(
            (1..=2).contains(&self.routing.max_attempts),
            "routing.max_attempts must be 1 or 2"
        );
        anyhow::ensure!(
            self.heartbeat.failure_threshold > 0,
            "heartbeat.failure_threshold must be greater than zero"
        );
        anyhow::ensure!(
            !self.heartbeat.interval.is_zero() && !self.heartbeat.timeout.is_zero(),
            "heartbeat interval and timeout must be greater than zero"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.heartbeat.jitter_ratio),
            "heartbeat.jitter_ratio must be between 0 and 1"
        );
        anyhow::ensure!(
            !self.timeouts.connect.is_zero()
                && !self.timeouts.response_headers.is_zero()
                && !self.timeouts.first_body_byte.is_zero()
                && !self.timeouts.stream_idle.is_zero(),
            "all configured timeouts must be greater than zero"
        );
        anyhow::ensure!(
            self.affinity.max_entries > 0,
            "affinity.max_entries must be greater than zero"
        );

        let mut ids = HashSet::new();
        let mut local_count = 0;
        for backend in &self.backends {
            anyhow::ensure!(
                !backend.id.trim().is_empty(),
                "backend id must not be empty"
            );
            anyhow::ensure!(
                ids.insert(&backend.id),
                "duplicate backend id: {}",
                backend.id
            );
            anyhow::ensure!(
                backend.max_in_flight > 0,
                "backend {} max_in_flight must be greater than zero",
                backend.id
            );
            anyhow::ensure!(
                backend.url.username().is_empty() && backend.url.password().is_none(),
                "backend {} URL must not contain userinfo",
                backend.id
            );
            anyhow::ensure!(
                matches!(backend.url.scheme(), "http" | "https"),
                "backend {} URL scheme must be http or https",
                backend.id
            );
            anyhow::ensure!(
                backend.heartbeat_path.starts_with('/'),
                "backend {} heartbeat_path must start with /",
                backend.id
            );
            for (name, value) in &backend.static_headers {
                HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                    anyhow::anyhow!(
                        "backend {} has invalid static header name {}: {}",
                        backend.id,
                        name,
                        error
                    )
                })?;
                HeaderValue::from_str(value).map_err(|error| {
                    anyhow::anyhow!(
                        "backend {} has invalid static header value for {}: {}",
                        backend.id,
                        name,
                        error
                    )
                })?;
            }
            local_count += usize::from(backend.local);
        }
        anyhow::ensure!(local_count <= 1, "at most one backend may be local");

        if self.affinity.enabled {
            let secret = env::var(&self.affinity.secret_env).unwrap_or_default();
            anyhow::ensure!(
                secret.len() >= 32,
                "{} must contain at least 32 bytes when affinity is enabled",
                self.affinity.secret_env
            );
        }
        ensure_loopback_or_explicit(self.admin_listen.ip())?;
        Ok(())
    }
}

fn resolve_config_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("DS4_SMART_PROXY_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let current = PathBuf::from(DEFAULT_CONFIG_FILE);
    if current.exists() {
        return Ok(current);
    }
    let platform = platform_default_path();
    anyhow::ensure!(
        platform.exists(),
        "configuration not found; tried {}, DS4_SMART_PROXY_CONFIG, {}, and {}",
        "--config",
        current.display(),
        platform.display()
    );
    Ok(platform)
}

fn platform_default_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Library/Application Support/ds4-smart-proxy/config.toml")
    } else {
        PathBuf::from("/etc/ds4-smart-proxy/config.toml")
    }
}

fn ensure_loopback_or_explicit(ip: IpAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        ip.is_loopback(),
        "admin_listen must use a loopback address until admin authentication is configured"
    );
    Ok(())
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:18080".parse().expect("valid default listen")
}
fn default_admin_listen() -> SocketAddr {
    "127.0.0.1:18081"
        .parse()
        .expect("valid default admin listen")
}
fn default_body_limit() -> usize {
    32 * 1024 * 1024
}
fn default_true() -> bool {
    true
}
fn default_max_in_flight() -> usize {
    1
}
fn default_heartbeat_path() -> String {
    "/v1/models".to_string()
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_duration(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid duration: {value}")))
}

fn deserialize_optional_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            parse_duration(&value)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid duration: {value}")))
        })
        .transpose()
}

fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (number, multiplier) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1_u64)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60_000)
    } else if let Some(v) = value.strip_suffix('h') {
        (v, 3_600_000)
    } else if let Some(v) = value.strip_suffix('d') {
        (v, 86_400_000)
    } else {
        return None;
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> &'static str {
        r#"
listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"

[affinity]
enabled = false

[[backends]]
id = "local"
url = "http://127.0.0.1:8000"
local = true
"#
    }

    #[test]
    fn parses_defaults_and_new_schema() {
        let config: Config = toml::from_str(valid_config()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.backends[0].kind, BackendKind::Standalone);
        assert_eq!(config.heartbeat.interval, Duration::from_secs(10));
        assert_eq!(
            config.affinity.default_sliding_ttl,
            Duration::from_secs(7 * 86_400)
        );
    }

    #[test]
    fn rejects_duplicate_backend_ids() {
        let input = format!(
            "{}\n[[backends]]\nid = \"local\"\nurl = \"http://127.0.0.1:8001\"",
            valid_config()
        );
        let config: Config = toml::from_str(&input).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3_600)));
    }

    #[test]
    fn parses_repository_example() {
        let config: Config =
            toml::from_str(include_str!("../ds4-smart-proxy.example.toml")).unwrap();
        assert_eq!(config.backends.len(), 2);
        assert!(config.backends[0].local);
    }
}
