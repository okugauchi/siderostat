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

const DEFAULT_CONFIG_FILE: &str = "siderostat.toml";

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
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub sound: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: cfg!(target_os = "macos"),
            sound: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StartupCleanupConfig {
    /// Automatically stop detected stale processes after the startup notification countdown.
    /// Set to false when an operator wants startup cleanup to be declined by default.
    pub auto_restart: bool,
}

impl Default for StartupCleanupConfig {
    fn default() -> Self {
        Self { auto_restart: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeAwareConfig {
    pub schema_version: u32,
    pub proxy: ProxyConfig,
    pub cluster: ClusterConfig,
    pub ds4: Ds4Config,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub startup_cleanup: StartupCleanupConfig,
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
    #[serde(default)]
    pub dspark: Ds4DsparkConfig,
    pub standalone: Ds4StandaloneConfig,
    pub mxfp4: Ds4Mxfp4Config,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ds4DsparkConfig {
    pub enabled: bool,
    pub support_model: Option<PathBuf>,
    pub confidence: Option<f64>,
    pub strict: bool,
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

impl ModelVariant {
    pub fn name(self) -> &'static str {
        match self {
            ModelVariant::Q2 => "q2",
            ModelVariant::Q2Q4 => "q2-q4",
            ModelVariant::Mxfp4 => "mxfp4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Residency {
    Resident,
    SsdStreaming,
}

impl Residency {
    pub fn name(self) -> &'static str {
        match self {
            Residency::Resident => "resident",
            Residency::SsdStreaming => "ssd-streaming",
        }
    }
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
        if let Some(path) = &mut self.ds4.dspark.support_model {
            *path = expand_config_path(path)?;
        }
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
        validate_dspark(self)?;
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

/// 各durationを非ゼロ検証する。名前はフィールドパスから生成するため、duration追加時に
/// 検証名とフィールド参照が二重管理になることはない。
macro_rules! require_positive_duration {
    ($config:expr, $($group:ident.$field:ident.$leaf:ident),+ $(,)?) => {
        $(
            anyhow::ensure!(
                !$config.$group.$field.$leaf.is_zero(),
                concat!(
                    stringify!($group), ".", stringify!($field), ".", stringify!($leaf),
                    " must be greater than zero"
                )
            );
        )+
    };
}

fn validate_durations(config: &ModeAwareConfig) -> anyhow::Result<()> {
    require_positive_duration!(
        config,
        proxy.timeouts.connect,
        proxy.timeouts.response_headers,
        proxy.timeouts.first_body_byte,
        proxy.timeouts.stream_idle,
        cluster.discovery.event_debounce,
        cluster.discovery.reconcile_interval,
        cluster.security.max_clock_skew,
        cluster.security.nonce_ttl,
        cluster.policy.required_peer_stability,
        cluster.policy.route_loss_grace,
        cluster.policy.promotion_backoff,
        cluster.timeouts.peer_connect,
        cluster.timeouts.peer_request,
        cluster.timeouts.control_lease,
        cluster.timeouts.drain,
        cluster.timeouts.stop,
        cluster.timeouts.rendezvous_hello,
        cluster.timeouts.worker_startup,
        cluster.timeouts.coordinator_startup,
        cluster.timeouts.complete_route,
        cluster.timeouts.standalone_startup,
    );
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
    if let Some(path) = &config.ds4.dspark.support_model {
        validate_regular_file("ds4.dspark.support_model", path, false)?;
    }
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

fn validate_dspark(config: &ModeAwareConfig) -> anyhow::Result<()> {
    let dspark = &config.ds4.dspark;
    if dspark.enabled {
        anyhow::ensure!(
            dspark.support_model.is_some(),
            "ds4.dspark.support_model is required when DSpark is enabled"
        );
        anyhow::ensure!(
            config.ds4.standalone.residency == Residency::Resident,
            "DSpark requires standalone residency = 'resident'; current DS4 does not support --ssd-streaming with --mtp"
        );
    } else {
        anyhow::ensure!(
            dspark.support_model.is_none() && dspark.confidence.is_none() && !dspark.strict,
            "ds4.dspark support_model/confidence/strict require enabled = true"
        );
    }
    if let Some(confidence) = dspark.confidence {
        anyhow::ensure!(
            confidence.is_finite() && (0.0..=1.0).contains(&confidence),
            "ds4.dspark.confidence must be between 0 and 1"
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

pub(crate) fn validate_extra_args(name: &str, arguments: &[String]) -> anyhow::Result<()> {
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
        "--mtp",
        "--dspark",
        "--dspark-confidence",
        "--dspark-strict",
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

fn resolve_config_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("SIDEROSTAT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let current = PathBuf::from(DEFAULT_CONFIG_FILE);
    if current.exists() {
        return Ok(current);
    }
    let platform = platform_default_path();
    anyhow::ensure!(
        platform.exists(),
        "configuration not found; tried {}, SIDEROSTAT_CONFIG, {}, and {}",
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
            .join("Library/Application Support/siderostat/config.toml")
    } else {
        PathBuf::from("/etc/siderostat/config.toml")
    }
}

fn ensure_loopback_or_explicit(ip: IpAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        ip.is_loopback(),
        "admin_listen must use a loopback address until admin authentication is configured"
    );
    Ok(())
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_duration(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid duration: {value}")))
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
    } else {
        (value.strip_suffix('d')?, 86_400_000)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .map(Duration::from_millis)
}

/// 検証テスト用: 指定したドット区切りパスのdurationをゼロにする。
#[cfg(test)]
fn zero_duration_at(config: &mut ModeAwareConfig, path: &str) {
    let duration = match path {
        "proxy.timeouts.connect" => &mut config.proxy.timeouts.connect,
        "proxy.timeouts.response_headers" => &mut config.proxy.timeouts.response_headers,
        "proxy.timeouts.first_body_byte" => &mut config.proxy.timeouts.first_body_byte,
        "proxy.timeouts.stream_idle" => &mut config.proxy.timeouts.stream_idle,
        "cluster.discovery.event_debounce" => &mut config.cluster.discovery.event_debounce,
        "cluster.discovery.reconcile_interval" => &mut config.cluster.discovery.reconcile_interval,
        "cluster.security.max_clock_skew" => &mut config.cluster.security.max_clock_skew,
        "cluster.security.nonce_ttl" => &mut config.cluster.security.nonce_ttl,
        "cluster.policy.required_peer_stability" => {
            &mut config.cluster.policy.required_peer_stability
        }
        "cluster.policy.route_loss_grace" => &mut config.cluster.policy.route_loss_grace,
        "cluster.policy.promotion_backoff" => &mut config.cluster.policy.promotion_backoff,
        "cluster.timeouts.peer_connect" => &mut config.cluster.timeouts.peer_connect,
        "cluster.timeouts.peer_request" => &mut config.cluster.timeouts.peer_request,
        "cluster.timeouts.control_lease" => &mut config.cluster.timeouts.control_lease,
        "cluster.timeouts.drain" => &mut config.cluster.timeouts.drain,
        "cluster.timeouts.stop" => &mut config.cluster.timeouts.stop,
        "cluster.timeouts.rendezvous_hello" => &mut config.cluster.timeouts.rendezvous_hello,
        "cluster.timeouts.worker_startup" => &mut config.cluster.timeouts.worker_startup,
        "cluster.timeouts.coordinator_startup" => &mut config.cluster.timeouts.coordinator_startup,
        "cluster.timeouts.complete_route" => &mut config.cluster.timeouts.complete_route,
        "cluster.timeouts.standalone_startup" => &mut config.cluster.timeouts.standalone_startup,
        _ => unreachable!("unknown duration path {path}"),
    };
    *duration = Duration::ZERO;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
state_path = "$HOME/Library/Application Support/siderostat/cluster-state.json"
manifest_cache_dir = "$HOME/Library/Application Support/siderostat/manifests"

[cluster.discovery]
mode = "bonjour-with-static-fallback"
bonjour_service_type = "_ds4cluster._tcp"
bonjour_domain = "local."
event_debounce = "500ms"
reconcile_interval = "30s"

[cluster.security]
control_secret_file = "$HOME/Library/Application Support/siderostat/secrets/cluster-control"
peer_proxy_token_file = "$HOME/Library/Application Support/siderostat/secrets/peer-proxy"
admin_token_file = "$HOME/Library/Application Support/siderostat/secrets/admin"
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
allow_sigkill = true

[ds4.dspark]
enabled = true
support_model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-DSpark-support-0731.gguf"
confidence = 0.7
strict = false

[ds4.standalone]
profile_id = "flash-0731-q2-q4-resident-dspark"
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-Layers37-42Q4KExperts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/standalone-flash-0731-q2-q4-ssd.json"
checkpoint = "flash-0731"
model_variant = "q2-q4"
residency = "resident"
context_size = 262144
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/standalone/flash-0731-q2-q4-resident-dspark"
kv_disk_space_mb = 262144
extra_args = []

[ds4.mxfp4]
model = "$HOME/LLM/ds4/gguf/DeepSeek-V4-Flash-MXFP4Experts-0731.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/mxfp4-0731.json"
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

[notifications]
enabled = true
sound = true

[startup_cleanup]
auto_restart = true
"#
    }

    struct ConfigTestFiles {
        root: PathBuf,
    }

    impl ConfigTestFiles {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("siderostat-config-test-{}", uuid::Uuid::new_v4()));
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
            config.ds4.dspark.support_model =
                Some(self.file("dspark-support.gguf", b"support", 0o600));
            config.ds4.standalone.model = self.file("standalone.gguf", b"model", 0o600);
            config.ds4.standalone.model_manifest = self.file("standalone.json", b"{}", 0o600);
            config.ds4.mxfp4.model = self.file("mxfp4.gguf", b"model", 0o600);
            config.ds4.mxfp4.model_manifest = self.file("mxfp4.json", b"{}", 0o600);
            config.cluster.security.control_secret_file =
                self.file("cluster-control", &[1; 32], 0o600);
            config.cluster.security.peer_proxy_token_file =
                self.file("peer-proxy", &[2; 32], 0o600);
            config.cluster.security.admin_token_file = self.file("admin", &[3; 32], 0o600);
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
    fn parses_duration_units() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604_800)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3_600)));
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
        assert_eq!(config.ds4.standalone.residency, Residency::Resident);
        assert!(config.ds4.dspark.enabled);
        assert_eq!(config.ds4.dspark.confidence, Some(0.7));
        assert_eq!(
            config.cluster.timeouts.rendezvous_hello,
            Duration::from_secs(900)
        );
        assert!(config.notifications.enabled);
        assert!(config.notifications.sound);
        assert!(config.startup_cleanup.auto_restart);
    }

    #[test]
    fn notifications_section_defaults_are_backward_compatible() {
        let input =
            mode_aware_config().replace("[notifications]\nenabled = true\nsound = true\n", "");
        let config: ModeAwareConfig = toml::from_str(&input).unwrap();
        assert_eq!(config.notifications.enabled, cfg!(target_os = "macos"));
        assert!(config.notifications.sound);
    }

    #[test]
    fn startup_cleanup_defaults_are_backward_compatible() {
        let input = mode_aware_config().replace("[startup_cleanup]\nauto_restart = true\n", "");
        let config: ModeAwareConfig = toml::from_str(&input).unwrap();
        assert!(config.startup_cleanup.auto_restart);
    }

    #[test]
    fn parses_repository_schema_v2_example() {
        let config = ModeAwareConfig::parse(include_str!("../siderostat.example.toml"))
            .expect("repository example must remain parseable");
        assert_eq!(config.schema_version, 2);
        assert_eq!(config.ds4.standalone.model_variant, ModelVariant::Q2Q4);
        assert_eq!(config.ds4.mxfp4.coordinator_layers, "0:19");
        assert!(config.ds4.dspark.enabled);
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
        let bad_residency =
            mode_aware_config().replace("residency = \"resident\"", "residency = \"automatic\"");
        assert!(toml::from_str::<ModeAwareConfig>(&bad_residency).is_err());
    }

    #[test]
    fn rejects_ssd_options_for_resident_profile() {
        let files = ConfigTestFiles::new();
        let mut config = files.config();
        config.ds4.standalone.residency = Residency::Resident;
        config.ds4.standalone.ssd_cache_experts = Some("32GB".into());
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
    fn rejects_every_duration_at_zero_with_dotted_path() {
        let files = ConfigTestFiles::new();
        let paths = [
            "proxy.timeouts.connect",
            "proxy.timeouts.response_headers",
            "proxy.timeouts.first_body_byte",
            "proxy.timeouts.stream_idle",
            "cluster.discovery.event_debounce",
            "cluster.discovery.reconcile_interval",
            "cluster.security.max_clock_skew",
            "cluster.security.nonce_ttl",
            "cluster.policy.required_peer_stability",
            "cluster.policy.route_loss_grace",
            "cluster.policy.promotion_backoff",
            "cluster.timeouts.peer_connect",
            "cluster.timeouts.peer_request",
            "cluster.timeouts.control_lease",
            "cluster.timeouts.drain",
            "cluster.timeouts.stop",
            "cluster.timeouts.rendezvous_hello",
            "cluster.timeouts.worker_startup",
            "cluster.timeouts.coordinator_startup",
            "cluster.timeouts.complete_route",
            "cluster.timeouts.standalone_startup",
        ];
        for path in paths {
            let mut config = files.config();
            zero_duration_at(&mut config, path);
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains(&format!("{path} must be greater than zero")),
                "expected {path} to be validated, got: {error}"
            );
        }
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

    #[test]
    fn validates_dspark_requirements_and_confidence() {
        let files = ConfigTestFiles::new();

        let mut missing = files.config();
        missing.ds4.dspark.support_model = None;
        assert!(
            missing
                .validate()
                .unwrap_err()
                .to_string()
                .contains("support_model is required")
        );

        let mut streaming = files.config();
        streaming.ds4.standalone.residency = Residency::SsdStreaming;
        assert!(
            streaming
                .validate()
                .unwrap_err()
                .to_string()
                .contains("DSpark requires standalone residency")
        );

        let mut confidence = files.config();
        confidence.ds4.dspark.confidence = Some(1.01);
        assert!(
            confidence
                .validate()
                .unwrap_err()
                .to_string()
                .contains("between 0 and 1")
        );

        let mut disabled = files.config();
        disabled.ds4.dspark.enabled = false;
        assert!(
            disabled
                .validate()
                .unwrap_err()
                .to_string()
                .contains("require enabled = true")
        );
    }

    #[test]
    fn rejects_dspark_override_in_extra_args() {
        let files = ConfigTestFiles::new();
        for argument in [
            "--mtp",
            "--dspark",
            "--dspark-confidence=0",
            "--dspark-strict",
        ] {
            let mut config = files.config();
            config.ds4.standalone.extra_args = vec![argument.into()];
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("must not override generated option")
            );
        }
    }

    #[test]
    fn enum_names_are_stable_metric_labels() {
        assert_eq!(ModelVariant::Q2.name(), "q2");
        assert_eq!(ModelVariant::Q2Q4.name(), "q2-q4");
        assert_eq!(ModelVariant::Mxfp4.name(), "mxfp4");

        assert_eq!(Residency::Resident.name(), "resident");
        assert_eq!(Residency::SsdStreaming.name(), "ssd-streaming");
    }
}
