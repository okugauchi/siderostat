use axum::http::{HeaderName, HeaderValue};
use serde::Deserialize;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::HashSet,
    env, fs,
    net::{IpAddr, SocketAddr},
    path::{Component, Path, PathBuf},
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

impl ModeAwareConfig {
    pub async fn load(explicit: Option<&Path>) -> anyhow::Result<(Self, PathBuf)> {
        let path = resolve_config_path(explicit)?;
        let contents = tokio::fs::read_to_string(&path).await?;
        let mut config = Self::parse(&contents)?;
        config.expand_paths()?;
        config.validate()?;
        Ok((config, path))
    }

    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        let value: toml::Value = toml::from_str(contents)?;
        if let Some(table) = value.as_table() {
            let legacy = [
                "backends",
                "routing",
                "affinity",
                "heartbeat",
                "active_probe",
                "cooldown",
            ]
            .into_iter()
            .filter(|field| table.contains_key(*field))
            .collect::<Vec<_>>();
            anyhow::ensure!(
                legacy.is_empty(),
                "legacy configuration fields are not supported by schema v2: {}; migrate to proxy/cluster/ds4 sections (the legacy affinity SQLite database is not deleted)",
                legacy.join(", ")
            );
        }
        toml::from_str(contents).map_err(Into::into)
    }

    pub fn expand_paths(&mut self) -> anyhow::Result<()> {
        self.cluster.state_path = expand_config_path(&self.cluster.state_path)?;
        self.cluster.manifest_cache_dir = expand_config_path(&self.cluster.manifest_cache_dir)?;
        self.cluster.security.control_secret_file =
            expand_config_path(&self.cluster.security.control_secret_file)?;
        self.cluster.security.peer_proxy_token_file =
            expand_config_path(&self.cluster.security.peer_proxy_token_file)?;
        self.cluster.security.admin_token_file =
            expand_config_path(&self.cluster.security.admin_token_file)?;
        self.ds4.binary = expand_config_path(&self.ds4.binary)?;
        self.ds4.working_directory = expand_config_path(&self.ds4.working_directory)?;
        self.ds4.standalone.model = expand_config_path(&self.ds4.standalone.model)?;
        self.ds4.standalone.model_manifest =
            expand_config_path(&self.ds4.standalone.model_manifest)?;
        self.ds4.standalone.kv_disk_dir = expand_config_path(&self.ds4.standalone.kv_disk_dir)?;
        self.ds4.mxfp4.model = expand_config_path(&self.ds4.mxfp4.model)?;
        self.ds4.mxfp4.model_manifest = expand_config_path(&self.ds4.mxfp4.model_manifest)?;
        self.ds4.mxfp4.kv_disk_dir = expand_config_path(&self.ds4.mxfp4.kv_disk_dir)?;
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == 2,
            "schema_version must be 2, got {}",
            self.schema_version
        );
        anyhow::ensure!(
            self.proxy.request_body_limit_bytes > 0,
            "proxy.request_body_limit_bytes must be greater than zero"
        );
        anyhow::ensure!(
            self.proxy.max_in_flight > 0,
            "proxy.max_in_flight must be greater than zero"
        );
        ensure_loopback_or_explicit(self.proxy.admin_listen.ip())?;
        validate_ports(self)?;
        validate_dns_sd_name(
            &self.cluster.discovery.bonjour_service_type,
            &self.cluster.discovery.bonjour_domain,
        )?;
        validate_durations(self)?;
        validate_paths(self)?;
        validate_ssd_streaming(&self.ds4.standalone)?;
        anyhow::ensure!(
            self.ds4.standalone.kv_disk_dir != self.ds4.mxfp4.kv_disk_dir,
            "standalone and distributed kv_disk_dir must be different"
        );
        validate_layer_split(
            &self.ds4.mxfp4.coordinator_layers,
            &self.ds4.mxfp4.worker_layers,
        )?;
        validate_secret_files(&self.cluster.security)?;
        validate_extra_args("ds4.standalone.extra_args", &self.ds4.standalone.extra_args)?;
        validate_extra_args("ds4.mxfp4.extra_args", &self.ds4.mxfp4.extra_args)?;
        Ok(())
    }
}

fn validate_ports(config: &ModeAwareConfig) -> anyhow::Result<()> {
    let ports = [
        ("proxy.public_listen", config.proxy.public_listen.port()),
        ("proxy.admin_listen", config.proxy.admin_listen.port()),
        ("cluster.control_port", config.cluster.control_port),
        (
            "cluster.ds4_distributed_port",
            config.cluster.ds4_distributed_port,
        ),
        (
            "cluster.peer_ingress_port",
            config.cluster.peer_ingress_port,
        ),
        ("ds4.http_port", config.ds4.http_port),
    ];
    let mut seen = std::collections::HashMap::new();
    for (name, port) in ports {
        anyhow::ensure!(port != 0, "{name} must not use port 0");
        if let Some(previous) = seen.insert(port, name) {
            anyhow::bail!("port collision: {previous} and {name} both use {port}");
        }
    }
    Ok(())
}

fn validate_dns_sd_name(service_type: &str, domain: &str) -> anyhow::Result<()> {
    let service_labels = service_type.split('.').collect::<Vec<_>>();
    anyhow::ensure!(
        service_labels.len() == 2
            && service_labels[0].starts_with('_')
            && matches!(service_labels[1], "_tcp" | "_udp")
            && service_labels.iter().all(|label| {
                (2..=63).contains(&label.len())
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            }),
        "cluster.discovery.bonjour_service_type is invalid: {service_type}"
    );
    anyhow::ensure!(
        domain.ends_with('.') && domain.len() <= 255,
        "cluster.discovery.bonjour_domain must be an absolute DNS name"
    );
    for label in domain.trim_end_matches('.').split('.') {
        anyhow::ensure!(
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "cluster.discovery.bonjour_domain contains an invalid label"
        );
    }
    Ok(())
}

fn validate_durations(config: &ModeAwareConfig) -> anyhow::Result<()> {
    let durations = [
        ("proxy.timeouts.connect", config.proxy.timeouts.connect),
        (
            "proxy.timeouts.response_headers",
            config.proxy.timeouts.response_headers,
        ),
        (
            "proxy.timeouts.first_body_byte",
            config.proxy.timeouts.first_body_byte,
        ),
        (
            "proxy.timeouts.stream_idle",
            config.proxy.timeouts.stream_idle,
        ),
        (
            "cluster.discovery.event_debounce",
            config.cluster.discovery.event_debounce,
        ),
        (
            "cluster.discovery.reconcile_interval",
            config.cluster.discovery.reconcile_interval,
        ),
        (
            "cluster.security.max_clock_skew",
            config.cluster.security.max_clock_skew,
        ),
        (
            "cluster.security.nonce_ttl",
            config.cluster.security.nonce_ttl,
        ),
        (
            "cluster.policy.required_peer_stability",
            config.cluster.policy.required_peer_stability,
        ),
        (
            "cluster.policy.route_loss_grace",
            config.cluster.policy.route_loss_grace,
        ),
        (
            "cluster.policy.promotion_backoff",
            config.cluster.policy.promotion_backoff,
        ),
        (
            "cluster.timeouts.peer_connect",
            config.cluster.timeouts.peer_connect,
        ),
        (
            "cluster.timeouts.peer_request",
            config.cluster.timeouts.peer_request,
        ),
        (
            "cluster.timeouts.control_lease",
            config.cluster.timeouts.control_lease,
        ),
        ("cluster.timeouts.drain", config.cluster.timeouts.drain),
        ("cluster.timeouts.stop", config.cluster.timeouts.stop),
        (
            "cluster.timeouts.rendezvous_hello",
            config.cluster.timeouts.rendezvous_hello,
        ),
        (
            "cluster.timeouts.worker_startup",
            config.cluster.timeouts.worker_startup,
        ),
        (
            "cluster.timeouts.coordinator_startup",
            config.cluster.timeouts.coordinator_startup,
        ),
        (
            "cluster.timeouts.complete_route",
            config.cluster.timeouts.complete_route,
        ),
        (
            "cluster.timeouts.standalone_startup",
            config.cluster.timeouts.standalone_startup,
        ),
    ];
    for (name, duration) in durations {
        anyhow::ensure!(!duration.is_zero(), "{name} must be greater than zero");
    }
    anyhow::ensure!(
        config.cluster.timeouts.rendezvous_hello >= config.cluster.timeouts.worker_startup,
        "cluster.timeouts.rendezvous_hello must not be shorter than worker_startup"
    );
    anyhow::ensure!(
        config.cluster.policy.max_consecutive_promotion_failures > 0,
        "cluster.policy.max_consecutive_promotion_failures must be greater than zero"
    );
    Ok(())
}

fn validate_paths(config: &ModeAwareConfig) -> anyhow::Result<()> {
    validate_regular_file("ds4.binary", &config.ds4.binary, true)?;
    validate_directory("ds4.working_directory", &config.ds4.working_directory)?;
    validate_regular_file("ds4.standalone.model", &config.ds4.standalone.model, false)?;
    validate_regular_file(
        "ds4.standalone.model_manifest",
        &config.ds4.standalone.model_manifest,
        false,
    )?;
    validate_regular_file("ds4.mxfp4.model", &config.ds4.mxfp4.model, false)?;
    validate_regular_file(
        "ds4.mxfp4.model_manifest",
        &config.ds4.mxfp4.model_manifest,
        false,
    )?;
    for (name, path) in [
        ("cluster.state_path", &config.cluster.state_path),
        (
            "cluster.manifest_cache_dir",
            &config.cluster.manifest_cache_dir,
        ),
        (
            "ds4.standalone.kv_disk_dir",
            &config.ds4.standalone.kv_disk_dir,
        ),
        ("ds4.mxfp4.kv_disk_dir", &config.ds4.mxfp4.kv_disk_dir),
    ] {
        validate_absolute_normalized(name, path)?;
    }
    Ok(())
}

fn validate_regular_file(name: &str, path: &Path, executable: bool) -> anyhow::Result<()> {
    validate_absolute_normalized(name, path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| anyhow::anyhow!("{name} {} is unavailable: {error}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "{name} must not be a symlink"
    );
    anyhow::ensure!(metadata.is_file(), "{name} must be a regular file");
    let canonical = fs::canonicalize(path)?;
    anyhow::ensure!(
        canonical == path,
        "{name} must use its canonical absolute path"
    );
    #[cfg(unix)]
    if executable {
        anyhow::ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "{name} must be executable"
        );
    }
    Ok(())
}

fn validate_directory(name: &str, path: &Path) -> anyhow::Result<()> {
    validate_absolute_normalized(name, path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| anyhow::anyhow!("{name} {} is unavailable: {error}", path.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "{name} must not be a symlink"
    );
    anyhow::ensure!(metadata.is_dir(), "{name} must be a directory");
    anyhow::ensure!(fs::canonicalize(path)? == path, "{name} must be canonical");
    Ok(())
}

fn validate_absolute_normalized(name: &str, path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(path.is_absolute(), "{name} must be an absolute path");
    anyhow::ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir)),
        "{name} must not contain '.' or '..' components"
    );
    Ok(())
}

fn validate_ssd_streaming(config: &Ds4StandaloneConfig) -> anyhow::Result<()> {
    let has_ssd_options = config.ssd_cache_experts.is_some()
        || config.ssd_full_layers.is_some()
        || config.ssd_preload_experts.is_some()
        || config.ssd_cold;
    anyhow::ensure!(
        config.residency != Residency::Resident || !has_ssd_options,
        "SSD streaming options require residency = 'ssd-streaming'"
    );
    if let Some(value) = &config.ssd_cache_experts {
        let number = value.strip_suffix("GB").unwrap_or(value);
        anyhow::ensure!(
            number.parse::<u64>().is_ok_and(|number| number > 0),
            "ds4.standalone.ssd_cache_experts must be a positive integer or <number>GB"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayerEnd {
    Number(u32),
    Output,
}

fn parse_layer_range(value: &str) -> anyhow::Result<(u32, LayerEnd)> {
    let (start, end) = value
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid layer range: {value}"))?;
    let start = start.parse::<u32>()?;
    let end = if end == "output" {
        LayerEnd::Output
    } else {
        LayerEnd::Number(end.parse::<u32>()?)
    };
    Ok((start, end))
}

fn validate_layer_split(coordinator: &str, worker: &str) -> anyhow::Result<()> {
    let (coordinator_start, coordinator_end) = parse_layer_range(coordinator)?;
    let (worker_start, worker_end) = parse_layer_range(worker)?;
    anyhow::ensure!(
        coordinator_start == 0,
        "coordinator layers must start at layer 0"
    );
    let LayerEnd::Number(coordinator_end) = coordinator_end else {
        anyhow::bail!("coordinator layers must not own output");
    };
    anyhow::ensure!(
        worker_start == coordinator_end.saturating_add(1),
        "layer split must not contain a gap or overlap"
    );
    anyhow::ensure!(
        worker_end == LayerEnd::Output,
        "worker layers must own output"
    );
    Ok(())
}

fn validate_secret_files(config: &ClusterSecurityConfig) -> anyhow::Result<()> {
    let files = [
        (
            "cluster.security.control_secret_file",
            &config.control_secret_file,
        ),
        (
            "cluster.security.peer_proxy_token_file",
            &config.peer_proxy_token_file,
        ),
        (
            "cluster.security.admin_token_file",
            &config.admin_token_file,
        ),
    ];
    let mut paths = HashSet::new();
    let mut values = HashSet::new();
    for (name, path) in files {
        validate_regular_file(name, path, false)?;
        anyhow::ensure!(
            paths.insert(path),
            "cluster secret/token paths must be different"
        );
        let metadata = fs::metadata(path)?;
        #[cfg(unix)]
        anyhow::ensure!(
            metadata.permissions().mode() & 0o777 == 0o600,
            "{name} must have mode 0600"
        );
        let value = fs::read(path)?;
        anyhow::ensure!(value.len() >= 32, "{name} must contain at least 32 bytes");
        anyhow::ensure!(
            values.insert(value),
            "cluster secret/token values must be different"
        );
    }
    Ok(())
}

fn validate_extra_args(name: &str, arguments: &[String]) -> anyhow::Result<()> {
    const FORBIDDEN: &[&str] = &[
        "-m",
        "--model",
        "--role",
        "--layers",
        "--coordinator",
        "--listen",
        "--host",
        "--port",
        "--ctx",
        "--kv-disk-dir",
        "--kv-disk-space-mb",
        "--ssd-streaming",
        "--ssd-streaming-cache-experts",
        "--ssd-streaming-full-layers",
        "--ssd-streaming-preload-experts",
        "--ssd-streaming-cold",
    ];
    for argument in arguments {
        if let Some(forbidden) = FORBIDDEN.iter().find(|forbidden| {
            argument == **forbidden
                || argument
                    .strip_prefix(**forbidden)
                    .is_some_and(|suffix| suffix.starts_with('='))
        }) {
            anyhow::bail!("{name} must not override generated option {forbidden}");
        }
    }
    Ok(())
}

fn expand_config_path(path: &Path) -> anyhow::Result<PathBuf> {
    let Some(value) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if value == "~" {
        return environment_config_path("HOME", "");
    }
    if let Some(suffix) = value.strip_prefix("~/") {
        return environment_config_path("HOME", suffix);
    }
    if let Some(variable) = value.strip_prefix('$') {
        let (name, suffix) = if let Some(variable) = variable.strip_prefix('{') {
            let end = variable.find('}').ok_or_else(|| {
                anyhow::anyhow!("invalid environment variable expression in config path: {value}")
            })?;
            (&variable[..end], &variable[end + 1..])
        } else {
            let end = variable
                .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .unwrap_or(variable.len());
            (&variable[..end], &variable[end..])
        };
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_'),
            "invalid environment variable name in config path: {value}"
        );
        anyhow::ensure!(
            suffix.is_empty() || suffix.starts_with('/'),
            "environment variable in config path must be followed by '/': {value}"
        );
        return environment_config_path(name, suffix.trim_start_matches('/'));
    }
    anyhow::ensure!(
        !value.starts_with('~'),
        "only '~/' is supported for home expansion in config path: {value}"
    );
    Ok(path.to_path_buf())
}

fn environment_config_path(name: &str, suffix: &str) -> anyhow::Result<PathBuf> {
    let base = env::var_os(name).ok_or_else(|| {
        anyhow::anyhow!("environment variable {name} referenced by config path is not set")
    })?;
    anyhow::ensure!(
        !base.is_empty(),
        "environment variable {name} referenced by config path is empty"
    );
    let mut path = PathBuf::from(base);
    if !suffix.is_empty() {
        path.push(suffix);
    }
    Ok(path)
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
    use std::fs;

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

    struct ConfigTestFiles {
        root: PathBuf,
    }

    impl ConfigTestFiles {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ds4-smart-proxy-config-test-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&root).unwrap();
            Self {
                root: fs::canonicalize(root).unwrap(),
            }
        }

        fn file(&self, name: &str, contents: &[u8], mode: u32) -> PathBuf {
            let path = self.root.join(name);
            fs::write(&path, contents).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            fs::canonicalize(path).unwrap()
        }

        fn config(&self) -> ModeAwareConfig {
            let mut config = ModeAwareConfig::parse(mode_aware_config()).unwrap();
            config.ds4.binary = self.file("ds4-server", b"fake binary", 0o700);
            config.ds4.working_directory = self.root.clone();
            config.ds4.standalone.model = self.file("standalone.gguf", b"model", 0o600);
            config.ds4.standalone.model_manifest = self.file("standalone.json", b"{}", 0o600);
            config.ds4.mxfp4.model = self.file("mxfp4.gguf", b"model", 0o600);
            config.ds4.mxfp4.model_manifest = self.file("mxfp4.json", b"{}", 0o600);
            config.cluster.security.control_secret_file = self.file("control.key", &[1; 32], 0o600);
            config.cluster.security.peer_proxy_token_file = self.file("peer.key", &[2; 32], 0o600);
            config.cluster.security.admin_token_file = self.file("admin.key", &[3; 32], 0o600);
            config.cluster.state_path = self.root.join("cluster-state.json");
            config.cluster.manifest_cache_dir = self.root.join("manifests");
            config.ds4.standalone.kv_disk_dir = self.root.join("standalone-kv");
            config.ds4.mxfp4.kv_disk_dir = self.root.join("distributed-kv");
            config
        }
    }

    impl Drop for ConfigTestFiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
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

    #[test]
    fn validates_complete_mode_aware_config() {
        let files = ConfigTestFiles::new();
        files.config().validate().unwrap();
    }

    #[test]
    fn expands_only_leading_config_path_expressions() {
        let home = PathBuf::from(env::var_os("HOME").expect("HOME must be set for tests"));
        assert_eq!(
            expand_config_path(Path::new("$HOME/Library/test.bin")).unwrap(),
            home.join("Library/test.bin")
        );
        assert_eq!(
            expand_config_path(Path::new("${HOME}/Library/test.bin")).unwrap(),
            home.join("Library/test.bin")
        );
        assert_eq!(
            expand_config_path(Path::new("~/Library/test.bin")).unwrap(),
            home.join("Library/test.bin")
        );
        assert_eq!(
            expand_config_path(Path::new("literal/$HOME/test.bin")).unwrap(),
            PathBuf::from("literal/$HOME/test.bin")
        );
    }

    #[test]
    fn returns_actionable_legacy_migration_error() {
        let error = ModeAwareConfig::parse("backends = []").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("legacy configuration fields"));
        assert!(message.contains("SQLite database is not deleted"));
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.schema_version = 1;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("schema_version")
        );
    }

    #[test]
    fn rejects_mode_aware_port_collisions() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.cluster.peer_ingress_port = config.proxy.public_listen.port();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("port collision")
        );
    }

    #[test]
    fn rejects_invalid_bonjour_syntax() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.cluster.discovery.bonjour_service_type = "ds4cluster.tcp".into();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("bonjour_service_type")
        );
    }

    #[test]
    fn rejects_zero_discovery_intervals() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.cluster.discovery.event_debounce = Duration::ZERO;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("event_debounce")
        );
    }

    #[test]
    fn rejects_non_executable_ds4_binary() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.ds4.binary = files.file("not-executable", b"binary", 0o600);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("executable")
        );
    }

    #[test]
    fn rejects_unknown_model_variant_and_residency() {
        let bad_variant =
            mode_aware_config().replace("model_variant = \"q2-q4\"", "model_variant = \"q8\"");
        assert!(toml::from_str::<ModeAwareConfig>(&bad_variant).is_err());
        let bad_residency = mode_aware_config()
            .replace("residency = \"ssd-streaming\"", "residency = \"automatic\"");
        assert!(toml::from_str::<ModeAwareConfig>(&bad_residency).is_err());
    }

    #[test]
    fn rejects_ssd_options_for_resident_profile() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.ds4.standalone.residency = Residency::Resident;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("SSD streaming")
        );
    }

    #[test]
    fn rejects_shared_standalone_and_distributed_kv_path() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.ds4.mxfp4.kv_disk_dir = config.ds4.standalone.kv_disk_dir.clone();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("kv_disk_dir")
        );
    }

    #[test]
    fn rejects_layer_gap_overlap_or_missing_output() {
        let files = ConfigTestFiles::new();
        let mut gap = files.config();
        gap.ds4.mxfp4.worker_layers = "21:output".into();
        assert!(
            gap.validate()
                .unwrap_err()
                .to_string()
                .contains("gap or overlap")
        );

        let mut missing_output = files.config();
        missing_output.ds4.mxfp4.worker_layers = "20:42".into();
        assert!(
            missing_output
                .validate()
                .unwrap_err()
                .to_string()
                .contains("own output")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_insecure_or_duplicate_secret_files() {
        let files = ConfigTestFiles::new();
        let mut insecure = files.config();
        insecure.cluster.security.control_secret_file = files.file("insecure.key", &[4; 32], 0o644);
        assert!(
            insecure
                .validate()
                .unwrap_err()
                .to_string()
                .contains("0600")
        );

        let mut duplicate = files.config();
        duplicate.cluster.security.admin_token_file =
            duplicate.cluster.security.control_secret_file.clone();
        assert!(
            duplicate
                .validate()
                .unwrap_err()
                .to_string()
                .contains("different")
        );
    }

    #[test]
    fn rejects_zero_or_inconsistent_timeouts() {
        let files = ConfigTestFiles::new();
        let mut zero = files.config();
        zero.proxy.timeouts.connect = Duration::ZERO;
        assert!(zero.validate().unwrap_err().to_string().contains("connect"));

        let mut inconsistent = files.config();
        inconsistent.cluster.timeouts.rendezvous_hello = Duration::from_secs(1);
        assert!(
            inconsistent
                .validate()
                .unwrap_err()
                .to_string()
                .contains("rendezvous_hello")
        );
    }

    #[test]
    fn rejects_generated_ds4_option_overrides() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.ds4.standalone.extra_args = vec!["--port=9000".into()];
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("generated option --port")
        );
    }
}
