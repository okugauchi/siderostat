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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeAwareConfig {
    pub schema_version: u32,
    pub proxy: ProxyConfig,
    pub cluster: ClusterConfig,
    pub ds4: Ds4Config,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub public_listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub request_body_limit_bytes: usize,
    pub max_in_flight: usize,
    pub timeouts: ProxyTimeoutConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyTimeoutConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub connect: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub response_headers: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub first_body_byte: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub stream_idle: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub enabled: bool,
    pub node_id: String,
    pub interface: String,
    pub coordinator_address: IpAddr,
    pub worker_address: IpAddr,
    pub control_port: u16,
    pub ds4_distributed_port: u16,
    pub peer_ingress_port: u16,
    pub state_path: PathBuf,
    pub manifest_cache_dir: PathBuf,
    pub discovery: DiscoveryConfig,
    pub security: ClusterSecurityConfig,
    pub policy: ClusterPolicyConfig,
    pub timeouts: ClusterTimeoutConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub mode: DiscoveryMode,
    pub bonjour_service_type: String,
    pub bonjour_domain: String,
    #[serde(deserialize_with = "deserialize_duration")]
    pub event_debounce: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub reconcile_interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiscoveryMode {
    Bonjour,
    Static,
    BonjourWithStaticFallback,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSecurityConfig {
    pub control_secret_file: PathBuf,
    pub peer_proxy_token_file: PathBuf,
    pub admin_token_file: PathBuf,
    #[serde(deserialize_with = "deserialize_duration")]
    pub max_clock_skew: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub nonce_ttl: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterPolicyConfig {
    pub auto_pair: bool,
    pub auto_promote: bool,
    pub auto_demote: bool,
    #[serde(deserialize_with = "deserialize_duration")]
    pub required_peer_stability: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub route_loss_grace: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub promotion_backoff: Duration,
    pub max_consecutive_promotion_failures: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterTimeoutConfig {
    #[serde(deserialize_with = "deserialize_duration")]
    pub peer_connect: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub peer_request: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub control_lease: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub drain: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub stop: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub rendezvous_hello: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub worker_startup: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub coordinator_startup: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub complete_route: Duration,
    #[serde(deserialize_with = "deserialize_duration")]
    pub standalone_startup: Duration,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ds4Config {
    pub binary: PathBuf,
    pub working_directory: PathBuf,
    pub http_host: IpAddr,
    pub http_port: u16,
    pub allow_sigkill: bool,
    pub standalone: Ds4StandaloneConfig,
    pub mxfp4: Ds4Mxfp4Config,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ds4StandaloneConfig {
    pub profile_id: String,
    pub model: PathBuf,
    pub model_manifest: PathBuf,
    pub checkpoint: String,
    pub model_variant: ModelVariant,
    pub residency: Residency,
    pub context_size: u32,
    pub kv_disk_dir: PathBuf,
    pub kv_disk_space_mb: u64,
    #[serde(default)]
    pub ssd_cache_experts: Option<String>,
    #[serde(default)]
    pub ssd_full_layers: Option<u32>,
    #[serde(default)]
    pub ssd_preload_experts: Option<u32>,
    #[serde(default)]
    pub ssd_cold: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelVariant {
    Q2,
    Q2Q4,
    Mxfp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Residency {
    Resident,
    SsdStreaming,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ds4Mxfp4Config {
    pub model: PathBuf,
    pub model_manifest: PathBuf,
    pub checkpoint: String,
    pub context_size: u32,
    pub coordinator_layers: String,
    pub worker_layers: String,
    pub kv_disk_dir: PathBuf,
    pub kv_disk_space_mb: u64,
    #[serde(default)]
    pub extra_args: Vec<String>,
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

    fn mode_aware_config() -> &'static str {
        r#"
schema_version = 2

[proxy]
public_listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"
request_body_limit_bytes = 33554432
max_in_flight = 1

[proxy.timeouts]
connect = "5s"
response_headers = "60s"
first_body_byte = "300s"
stream_idle = "300s"

[cluster]
enabled = true
node_id = "macstudio-coordinator"
interface = "bridge0"
coordinator_address = "10.99.0.1"
worker_address = "10.99.0.2"
control_port = 9920
ds4_distributed_port = 9911
peer_ingress_port = 18082
state_path = "$HOME/Library/Application Support/ds4-smart-proxy/cluster-state.json"
manifest_cache_dir = "$HOME/Library/Application Support/ds4-smart-proxy/manifests"

[cluster.discovery]
mode = "bonjour-with-static-fallback"
bonjour_service_type = "_ds4cluster._tcp"
bonjour_domain = "local."
event_debounce = "500ms"
reconcile_interval = "30s"

[cluster.security]
control_secret_file = "$HOME/Library/Application Support/ds4-smart-proxy/cluster-control.key"
peer_proxy_token_file = "$HOME/Library/Application Support/ds4-smart-proxy/peer-proxy.key"
admin_token_file = "$HOME/Library/Application Support/ds4-smart-proxy/admin.key"
max_clock_skew = "30s"
nonce_ttl = "5m"

[cluster.policy]
auto_pair = true
auto_promote = true
auto_demote = true
required_peer_stability = "5s"
route_loss_grace = "15s"
promotion_backoff = "300s"
max_consecutive_promotion_failures = 3

[cluster.timeouts]
peer_connect = "1s"
peer_request = "3s"
control_lease = "15s"
drain = "180s"
stop = "180s"
rendezvous_hello = "900s"
worker_startup = "600s"
coordinator_startup = "600s"
complete_route = "180s"
standalone_startup = "900s"

[ds4]
binary = "$HOME/LLM/ds4/ds4-server"
working_directory = "$HOME/LLM/ds4"
http_host = "127.0.0.1"
http_port = 8000
allow_sigkill = false

[ds4.standalone]
profile_id = "flash-0731-q2-q4-ssd"
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-Layers37-42Q4KExperts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/ds4-smart-proxy/manifests/standalone-flash-0731-q2-q4-ssd.json"
checkpoint = "flash-0731"
model_variant = "q2-q4"
residency = "ssd-streaming"
context_size = 262144
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/standalone/flash-0731-q2-q4-ssd"
kv_disk_space_mb = 262144
ssd_cache_experts = "32GB"
extra_args = []

[ds4.mxfp4]
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-MXFP4Experts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/ds4-smart-proxy/manifests/mxfp4-0731.json"
checkpoint = "flash-0731"
context_size = 262144
coordinator_layers = "0:19"
worker_layers = "20:output"
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/distributed/mxfp4-0731"
kv_disk_space_mb = 262144
extra_args = ["--debug"]

[logging]
format = "json"
level = "info"
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

    #[test]
    fn parses_complete_mode_aware_schema_v2() {
        let config: ModeAwareConfig = toml::from_str(mode_aware_config()).unwrap();
        assert_eq!(config.schema_version, 2);
        assert_eq!(
            config.proxy.public_listen,
            "127.0.0.1:18080".parse().unwrap()
        );
        assert_eq!(
            config.cluster.discovery.mode,
            DiscoveryMode::BonjourWithStaticFallback
        );
        assert_eq!(config.ds4.standalone.model_variant, ModelVariant::Q2Q4);
        assert_eq!(config.ds4.standalone.residency, Residency::SsdStreaming);
        assert_eq!(
            config.cluster.timeouts.rendezvous_hello,
            Duration::from_secs(900)
        );
    }

    #[test]
    fn rejects_unknown_mode_aware_field() {
        let input = mode_aware_config().replace(
            "max_in_flight = 1",
            "max_in_flight = 1\nunknown_proxy_field = true",
        );
        let error = toml::from_str::<ModeAwareConfig>(&input).unwrap_err();
        assert!(error.to_string().contains("unknown_proxy_field"));
    }

    #[test]
    fn rejects_legacy_root_fields_in_mode_aware_schema() {
        let input =
            mode_aware_config().replace("schema_version = 2", "schema_version = 2\nbackends = []");
        let error = toml::from_str::<ModeAwareConfig>(&input).unwrap_err();
        assert!(error.to_string().contains("backends"));
    }
}
