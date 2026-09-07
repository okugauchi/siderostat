//! T03 — TP 専用 config validation の受入 case。。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::config::{DistributedTopology, ModeAwareConfig, Transport};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// validate_paths を通すための一時ファイル群。config.rs の ConfigTestFiles 相当。。。
struct ConfigFiles {
    root: PathBuf,
}

impl ConfigFiles {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "siderostat-v040-config-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            root: fs::canonicalize(&root).unwrap(),
        }
    }

    fn file(&self, name: &str, mode: u32) -> PathBuf {
        self.file_with(name, mode, &[1_u8; 32])
    }

    fn file_with(&self, name: &str, mode: u32, contents: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        // 32 バイト以上で security file（control_secret_file 等）の最小要件を満たす。
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::canonicalize(path).unwrap()
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    /// LP config を返す。全 validate を通過できる実ファイルを配置する。。。
    fn config(&self) -> ModeAwareConfig {
        let mut config = ModeAwareConfig::parse(lp_toml()).unwrap();
        config.ds4.binary = self.file("ds4-server", 0o700);
        config.ds4.working_directory = self.dir("work");
        config.ds4.dspark.support_model = Some(self.file("dspark-support.gguf", 0o600));
        config.ds4.standalone.model = self.file("standalone.gguf", 0o600);
        config.ds4.standalone.model_manifest = self.file("standalone.json", 0o600);
        config.ds4.distributed.model = self.file("mxfp4.gguf", 0o600);
        config.ds4.distributed.model_manifest = self.file("mxfp4.json", 0o600);
        config.cluster.security.control_secret_file =
            self.file_with("control-secret", 0o600, &[1_u8; 32]);
        config.cluster.security.peer_proxy_token_file =
            self.file_with("peer-proxy", 0o600, &[2_u8; 32]);
        config.cluster.security.admin_token_file = self.file_with("admin", 0o600, &[3_u8; 32]);
        config.cluster.state_path = self.dir("cluster-state");
        config.cluster.manifest_cache_dir = self.dir("manifests");
        config.ds4.standalone.kv_disk_dir = self.dir("standalone-kv");
        config.ds4.distributed.kv_disk_dir = self.dir("distributed-kv");
        config
    }

    /// TP config（rdma / range 無し / DSpark 無効）を返す。。。
    fn tp_config(&self) -> ModeAwareConfig {
        let mut config = self.config();
        config.ds4.dspark = siderostat::config::Ds4DsparkConfig {
            enabled: false,
            support_model: None,
            confidence: None,
            strict: false,
        };
        config.ds4.distributed.topology = DistributedTopology::TensorParallel;
        config.ds4.distributed.transport = Transport::Rdma;
        config.ds4.distributed.coordinator_layers = None;
        config.ds4.distributed.worker_layers = None;
        config.ds4.distributed.extra_args = vec![];
        config
    }
}

impl Drop for ConfigFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn lp_toml() -> &'static str {
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
support_model = "$HOME/LLM/ds4/gguf/support.gguf"
confidence = 0.7
strict = false

[ds4.standalone]
profile_id = "flash-0731-q2-q4-resident-dspark"
model = "$HOME/LLM/ds4/gguf/standalone.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/standalone.json"
checkpoint = "flash-0731"
quantization = "q2-q4"
residency = "resident"
context_size = 262144
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/standalone"
kv_disk_space_mb = 262144
extra_args = []

[ds4.distributed]
topology = "layer-parallel"
quantization = "mxfp4"
model = "$HOME/LLM/ds4/gguf/mxfp4.gguf"
model_manifest = "$HOME/Library/Application Support/siderostat/manifests/mxfp4.json"
checkpoint = "flash-0731"
context_size = 262144
coordinator_layers = "0:19"
worker_layers = "20:output"
kv_disk_dir = "$HOME/Library/Caches/ds4-kv/distributed"
kv_disk_space_mb = 262144
extra_args = ["--debug"]

[logging]
format = "json"
level = "info"
"#
}

/// 受入 case 1: TP + rdma + range 無し → 成功。。。
#[test]
fn accepts_tensor_parallel_rdma_without_layer_ranges() {
    let files = ConfigFiles::new();
    let config = files.tp_config();
    config
        .validate()
        .expect("TP+rdma+range-less must be accepted");
}

/// 受入 case 2: TP + tcp → v0.4 scope で明示拒否。。。
#[test]
fn rejects_tensor_parallel_over_tcp() {
    let files = ConfigFiles::new();
    let mut config = files.tp_config();
    config.ds4.distributed.transport = Transport::Tcp;
    let error = config.validate().unwrap_err().to_string();
    assert!(
        error.contains("rdma"),
        "TP over tcp must be rejected: {error}"
    );
}

/// 受入 case 3: LP + range 無し → 拒否。。。
#[test]
fn rejects_layer_parallel_without_layer_ranges() {
    let files = ConfigFiles::new();
    let mut config = files.config();
    config.ds4.distributed.coordinator_layers = None;
    config.ds4.distributed.worker_layers = None;
    let error = config.validate().unwrap_err().to_string();
    assert!(
        error.contains("layer ranges are required"),
        "LP without ranges must be rejected: {error}"
    );
}

/// 受入 case 4: --role / --transport override → 拒否。。。
#[test]
fn rejects_managed_argument_override_in_tensor_parallel() {
    let files = ConfigFiles::new();
    let mut config = files.tp_config();
    config.ds4.distributed.extra_args = vec!["--role=worker".into()];
    let error = config.validate().unwrap_err().to_string();
    assert!(
        error.contains("must not override"),
        "override must be rejected: {error}"
    );

    let mut config2 = files.tp_config();
    config2.ds4.distributed.extra_args = vec!["--transport".into(), "rdma".into()];
    let error2 = config2.validate().unwrap_err().to_string();
    assert!(
        error2.contains("must not override"),
        "override must be rejected: {error2}"
    );
}

/// 事後条件: 既存 schema（LP）の config が読み込め、既定 topology は LP のまま。。。
#[test]
fn legacy_schema_loads_and_default_topology_is_layer_parallel() {
    let files = ConfigFiles::new();
    let config = files.config();
    assert_eq!(config.schema_version, 2);
    assert_eq!(
        config.ds4.distributed.topology,
        DistributedTopology::LayerParallel
    );
    // 既定 transport は tcp（LP の既定）。
    assert_eq!(config.ds4.distributed.transport, Transport::Tcp);
    config
        .validate()
        .expect("legacy LP config must remain valid");
}
