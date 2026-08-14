#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    net::SocketAddr,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

pub struct FakeDs4Process {
    child: Child,
    pub address: SocketAddr,
    pub startup_elapsed: Duration,
}

impl FakeDs4Process {
    pub async fn spawn(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let started = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_fake-ds4"))
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawn fake-ds4")?;
        let stdout = child.stdout.take().context("capture fake-ds4 stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .context("wait for fake-ds4 listener")??;
        let address = line
            .trim()
            .strip_prefix("fake-ds4 listening on ")
            .context("parse fake-ds4 listener line")?
            .parse()
            .context("parse fake-ds4 socket address")?;
        Ok(Self {
            child,
            address,
            startup_elapsed: started.elapsed(),
        })
    }

    pub async fn terminate(mut self) -> Result<std::process::ExitStatus> {
        let pid = self.child.id().context("fake-ds4 has no pid")?;
        send_sigterm(pid)?;
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("wait for fake-ds4 SIGTERM exit")?
            .context("wait for fake-ds4")
    }

    pub async fn wait(mut self) -> Result<std::process::ExitStatus> {
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("wait for fake-ds4 exit")?
            .context("wait for fake-ds4")
    }
}

pub fn temporary_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("siderostat-{name}-{}", uuid::Uuid::new_v4()))
}

pub async fn wait_until_file_exists(path: &Path) -> Result<()> {
    for _ in 0..50 {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    bail!("timed out waiting for {}", path.display())
}

#[cfg(unix)]
fn send_sigterm(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    // SAFETY: `pid` comes from the owned child and SIGTERM has no pointer arguments.
    if unsafe { kill(pid as i32, SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("send SIGTERM to fake-ds4")
    }
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) -> Result<()> {
    bail!("SIGTERM test support requires Unix")
}

// ---- R0-04 production-equivalent two-node harness ---------------------------------
//
// Drives `ProductionClusterRuntime` (control HTTP + state machine + lifecycle) for a
// coordinator and a worker on separate runtimes, separate loopback addresses, and separate
// persistent state paths. Fake child lifecycles record start/stop plus PID-equivalent
// identity/profile/generation so R0-05/R0-06 can inject faults and assert on them.

use futures::future::BoxFuture;
use siderostat::{
    cluster::{
        ChildIdentity, DistributedCoordinatorLifecycle, DistributedManifest,
        DistributedWorkerLifecycle, LocalStandaloneLifecycle, ModeRuntime,
        ProductionClusterRuntime,
    },
    config::ModeAwareConfig,
    proxy::{ModeAwareProxyOptions, ModeAwareProxyState},
    target::LocalRole,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::task::JoinHandle;

/// In-memory record of a fake DS4 child: running state plus PID-equivalent identity,
/// profile, and generation. `start` sets the identity; `stop` clears it.
#[derive(Clone, Default)]
pub struct RecordedChild {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    pid: Arc<AtomicU32>,
    generation: Arc<AtomicU64>,
    profile: Arc<std::sync::Mutex<String>>,
}

impl RecordedChild {
    pub fn start(&self, generation: u64, pid: u32, profile: impl Into<String>) {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        self.pid.store(pid, Ordering::SeqCst);
        self.generation.store(generation, Ordering::SeqCst);
        *self.profile.lock().unwrap() = profile.into();
    }

    pub fn stop(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    pub fn stops(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }

    pub fn identity(&self) -> Option<ChildIdentity> {
        if !self.is_running() {
            return None;
        }
        Some(ChildIdentity {
            pid: self.pid.load(Ordering::SeqCst),
            executable: PathBuf::new(),
            argv_sha256: [0u8; 32],
            profile_id: self.profile.lock().unwrap().clone(),
            generation: self.generation.load(Ordering::SeqCst),
            spawned_at_millis: 0,
            process_start_micros: 0,
        })
    }
}

#[derive(Clone)]
pub struct FakeStandalone {
    child: RecordedChild,
    profile: &'static str,
    pid: u32,
}

impl FakeStandalone {
    pub fn new(profile: &'static str, pid: u32) -> Self {
        Self {
            child: RecordedChild::default(),
            profile,
            pid,
        }
    }

    pub fn child(&self) -> &RecordedChild {
        &self.child
    }
}

impl LocalStandaloneLifecycle for FakeStandalone {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        let profile = self.profile;
        let pid = self.pid;
        Box::pin(async move {
            child.start(generation, pid, profile);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        Box::pin(async move {
            child.stop();
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let child = self.child.clone();
        Box::pin(async move { Ok(child.is_running()) })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        let child = self.child.clone();
        Box::pin(async move { child.identity() })
    }
}

#[derive(Clone)]
pub struct FakeWorkerChild {
    child: RecordedChild,
    profile: &'static str,
    pid: u32,
}

impl FakeWorkerChild {
    pub fn new(profile: &'static str, pid: u32) -> Self {
        Self {
            child: RecordedChild::default(),
            profile,
            pid,
        }
    }

    pub fn child(&self) -> &RecordedChild {
        &self.child
    }
}

impl DistributedWorkerLifecycle for FakeWorkerChild {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        let profile = self.profile;
        let pid = self.pid;
        Box::pin(async move {
            child.start(generation, pid, profile);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        Box::pin(async move {
            child.stop();
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let child = self.child.clone();
        Box::pin(async move { Ok(child.is_running()) })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        let child = self.child.clone();
        Box::pin(async move { child.identity() })
    }
}

#[derive(Clone)]
pub struct FakeCoordinatorChild {
    child: RecordedChild,
    profile: &'static str,
    pid: u32,
}

impl FakeCoordinatorChild {
    pub fn new(profile: &'static str, pid: u32) -> Self {
        Self {
            child: RecordedChild::default(),
            profile,
            pid,
        }
    }

    pub fn child(&self) -> &RecordedChild {
        &self.child
    }
}

impl DistributedCoordinatorLifecycle for FakeCoordinatorChild {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        let profile = self.profile;
        let pid = self.pid;
        Box::pin(async move {
            child.start(generation, pid, profile);
            Ok(())
        })
    }

    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        Box::pin(async move {
            anyhow::ensure!(child.is_running(), "coordinator child not running");
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let child = self.child.clone();
        Box::pin(async move {
            child.stop();
            Ok(())
        })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(std::future::pending())
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let child = self.child.clone();
        Box::pin(async move { Ok(child.is_running()) })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        let child = self.child.clone();
        Box::pin(async move { child.identity() })
    }
}

/// Synthetic DS4D HELLO frame bytes used to satisfy the coordinator rendezvous listener during
/// promotion. Matches the WorkerHelloExpectation in `test_config` (layer_start 20, output
/// layer, context 262144, model deepseek-v4-flash) per docs/spec.md section 17.2.
pub fn fake_worker_hello_bytes() -> Vec<u8> {
    include_str!("../fixtures/ds4/hello40-schema-v1.hex")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| line.split_ascii_whitespace())
        .map(|value| u8::from_str_radix(value, 16).unwrap())
        .collect()
}

/// Spawn a task that repeatedly connects to the coordinator DS4 rendezvous port and writes a
/// fake worker HELLO, so `promote()` can complete its `accept_one`. Returns when the hello is
/// accepted (or panics if the listener never becomes reachable).
pub fn inject_fake_worker_hello(ds4_distributed_port: u16) -> JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let bytes = fake_worker_hello_bytes();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            match tokio::net::TcpStream::connect(("127.0.0.1", ds4_distributed_port)).await {
                Ok(mut stream) => {
                    stream.write_all(&bytes).await.unwrap();
                    stream.shutdown().await.unwrap();
                    return;
                }
                Err(_) => {
                    if tokio::time::Instant::now() >= deadline {
                        panic!(
                            "coordinator DS4 rendezvous listener on port {ds4_distributed_port} \
                             never became reachable"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    })
}

fn proxy_state() -> anyhow::Result<Arc<ModeAwareProxyState>> {
    Ok(Arc::new(ModeAwareProxyState::new(
        url::Url::parse("http://127.0.0.1:8000")?,
        url::Url::parse("http://127.0.0.1:18082")?,
        ModeAwareProxyOptions {
            max_in_flight: 8,
            request_body_limit_bytes: 4096,
            response_header_timeout: Duration::from_secs(1),
            first_body_byte_timeout: Duration::from_secs(1),
            stream_idle_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(100),
        },
    )?))
}

fn manifest() -> DistributedManifest {
    DistributedManifest {
        schema_version: 2,
        profile: "distributed-mxfp4".into(),
        ds4_binary_sha256: "11".repeat(32),
        compatible_ds4_binary_sha256: vec!["11".repeat(32), "44".repeat(32)],
        ds4_source_commit: "b0309611041655f4e45671cfd9c9886aff161406".into(),
        model_sha256: "22".repeat(32),
        model_size: 100,
        checkpoint: "flash-0731".into(),
        model_family: "deepseek-v4-flash".into(),
        quantization: "mxfp4-experts".into(),
        context_size: 262_144,
        coordinator_layers: "0:19".into(),
        worker_layers: "20:output".into(),
        ds4_wire_schema: "ds4d-v1-hello40".into(),
        argv_profile_sha256: "33".repeat(32),
    }
}

fn test_config(
    node_id: &str,
    coordinator_address: &str,
    worker_address: &str,
    control_port: u16,
    ds4_distributed_port: u16,
    peer_ingress_port: u16,
    state_path: PathBuf,
    manifest_cache: PathBuf,
) -> ModeAwareConfig {
    let state_path = state_path.display().to_string();
    let manifest_cache = manifest_cache.display().to_string();
    let mut config = ModeAwareConfig::parse(&format!(
        r#"
schema_version = 2

[proxy]
public_listen = "127.0.0.1:18080"
admin_listen = "127.0.0.1:18081"
request_body_limit_bytes = 33554432
max_in_flight = 8

[proxy.timeouts]
connect = "5s"
response_headers = "60s"
first_body_byte = "300s"
stream_idle = "300s"

[cluster]
enabled = true
node_id = "{node_id}"
interface = "bridge0"
coordinator_address = "{coordinator_address}"
worker_address = "{worker_address}"
control_port = {control_port}
ds4_distributed_port = {ds4_distributed_port}
peer_ingress_port = {peer_ingress_port}
state_path = "{state_path}"
manifest_cache_dir = "{manifest_cache}"

[cluster.discovery]
mode = "static"
bonjour_service_type = "_ds4cluster._tcp"
bonjour_domain = "local."
event_debounce = "100ms"
reconcile_interval = "200ms"

[cluster.security]
control_secret_file = "{manifest_cache}/control.key"
peer_proxy_token_file = "{manifest_cache}/peer-proxy.key"
admin_token_file = "{manifest_cache}/admin.key"
max_clock_skew = "30s"
nonce_ttl = "5m"

[cluster.policy]
auto_pair = true
auto_promote = true
auto_demote = true
required_peer_stability = "100ms"
route_loss_grace = "300ms"
promotion_backoff = "50ms"
max_consecutive_promotion_failures = 3

[cluster.timeouts]
peer_connect = "1s"
peer_request = "3s"
control_lease = "15s"
drain = "1s"
stop = "2s"
rendezvous_hello = "5s"
worker_startup = "5s"
coordinator_startup = "5s"
complete_route = "2s"
standalone_startup = "5s"

[ds4]
binary = "/nonexistent/ds4-server"
working_directory = "/nonexistent"
http_host = "127.0.0.1"
http_port = 8000
allow_sigkill = false

[ds4.dspark]
enabled = true
support_model = "/nonexistent/support.gguf"
confidence = 0.7
strict = false

[ds4.standalone]
profile_id = "flash-standalone"
model = "/nonexistent/standalone.gguf"
model_manifest = "{manifest_cache}/standalone.json"
checkpoint = "flash-0731"
model_variant = "q2-q4"
residency = "resident"
context_size = 262144
kv_disk_dir = "{manifest_cache}/kv-standalone"
kv_disk_space_mb = 262144
extra_args = []

[ds4.mxfp4]
model = "/nonexistent/mxfp4.gguf"
model_manifest = "{manifest_cache}/mxfp4.json"
checkpoint = "flash-0731"
context_size = 262144
coordinator_layers = "0:19"
worker_layers = "20:output"
kv_disk_dir = "{manifest_cache}/kv-distributed"
kv_disk_space_mb = 262144
extra_args = []

[logging]
format = "json"
level = "info"

[notifications]
enabled = false
sound = false
"#
    ))
    .expect("parse test cluster config");
    config.cluster.state_path = state_path.into();
    config.cluster.manifest_cache_dir = manifest_cache.into();
    config
}

/// One production-equivalent node: separate runtime, separate loopback listener, separate
/// persistent state path, and fake child lifecycles.
pub struct Node {
    pub role: LocalRole,
    pub config: ModeAwareConfig,
    pub proxy: Arc<ModeAwareProxyState>,
    pub standalone: Arc<FakeStandalone>,
    pub worker_child: Option<Arc<FakeWorkerChild>>,
    pub coordinator_child: Option<Arc<FakeCoordinatorChild>>,
    pub ds4_distributed_port: u16,
    pub mode: Arc<ModeRuntime>,
    pub production: Arc<ProductionClusterRuntime>,
    pub serve: JoinHandle<()>,
}

impl Node {
    async fn build(
        role: LocalRole,
        node_id: &str,
        coordinator_address: &str,
        worker_address: &str,
        control_port: u16,
        peer_control_port: u16,
        state_path: PathBuf,
        manifest_cache: PathBuf,
        listener: tokio::net::TcpListener,
        baseline_generation: u64,
    ) -> anyhow::Result<Self> {
        let (ds4_distributed_port, peer_ingress_port) = match role {
            LocalRole::Coordinator => (19911, 19082),
            LocalRole::Worker => (19912, 19083),
            LocalRole::Unknown => anyhow::bail!("unknown role"),
        };
        let config = test_config(
            node_id,
            coordinator_address,
            worker_address,
            control_port,
            ds4_distributed_port,
            peer_ingress_port,
            state_path,
            manifest_cache,
        );
        let proxy = proxy_state()?;
        let standalone = Arc::new(FakeStandalone::new(
            "flash-standalone",
            match role {
                LocalRole::Coordinator => 9101,
                _ => 9102,
            },
        ));
        let mode = Arc::new(
            ModeRuntime::spawn_ready_at(
                role,
                proxy.clone(),
                standalone.clone(),
                Duration::from_secs(1),
                baseline_generation,
            )
            .await?,
        );
        let (worker_child, coordinator_child) = match role {
            LocalRole::Worker => (
                Some(Arc::new(FakeWorkerChild::new("flash-worker", 9202))),
                None,
            ),
            LocalRole::Coordinator => (
                None,
                Some(Arc::new(FakeCoordinatorChild::new(
                    "flash-coordinator",
                    9201,
                ))),
            ),
            LocalRole::Unknown => anyhow::bail!("unknown role"),
        };
        let worker_trait: Option<Arc<dyn DistributedWorkerLifecycle>> = worker_child
            .clone()
            .map(|child| child as Arc<dyn DistributedWorkerLifecycle>);
        let coordinator_trait: Option<Arc<dyn DistributedCoordinatorLifecycle>> = coordinator_child
            .clone()
            .map(|child| child as Arc<dyn DistributedCoordinatorLifecycle>);
        let production = Arc::new(ProductionClusterRuntime::new_with_lifecycles(
            config.clone(),
            role,
            mode.clone(),
            proxy.clone(),
            standalone.clone(),
            manifest(),
            vec![0x42; 32],
            peer_control_port,
            worker_trait,
            coordinator_trait,
        )?);
        let app = production
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        let serve = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Ok(Self {
            role,
            config,
            proxy,
            standalone,
            worker_child,
            coordinator_child,
            ds4_distributed_port,
            mode,
            production,
            serve,
        })
    }
}

/// A running coordinator + worker pair on distinct loopback addresses and state paths.
pub struct TwoNode {
    pub coordinator: Node,
    pub worker: Node,
}

impl TwoNode {
    pub async fn boot() -> anyhow::Result<Self> {
        Self::boot_with_baseline(0, 0).await
    }

    /// Boot both nodes with explicit state-machine baseline generations. A higher baseline
    /// models a node whose persistent control session generation is ahead of its peer (for
    /// example after it alone was restarted with newer state), which drives the R0-06
    /// direction-dependent generation-mismatch cases.
    pub async fn boot_with_baseline(
        coordinator_baseline: u64,
        worker_baseline: u64,
    ) -> anyhow::Result<Self> {
        // This host binds only one IPv4 loopback (127.0.0.1) and one IPv6 loopback (::1);
        // 127.0.0.2 and ::2 are not aliased, and cross-family (IPv4 vs IPv6) control
        // connections are unsupported. The two nodes are therefore isolated on the same
        // loopback address with distinct control ports, runtimes, and persistent state paths.
        let coordinator_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let worker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let coordinator_port = coordinator_listener.local_addr()?.port();
        let worker_port = worker_listener.local_addr()?.port();

        let coordinator_state = temporary_path("reconnect-coordinator-state");
        let worker_state = temporary_path("reconnect-worker-state");
        let coordinator_cache = temporary_path("reconnect-coordinator-manifests");
        let worker_cache = temporary_path("reconnect-worker-manifests");
        std::fs::create_dir_all(&coordinator_state)?;
        std::fs::create_dir_all(&worker_state)?;
        std::fs::create_dir_all(&coordinator_cache)?;
        std::fs::create_dir_all(&worker_cache)?;

        let coordinator = Node::build(
            LocalRole::Coordinator,
            "reconnect-coordinator",
            "127.0.0.1",
            "127.0.0.1",
            coordinator_port,
            worker_port,
            coordinator_state,
            coordinator_cache,
            coordinator_listener,
            coordinator_baseline,
        )
        .await?;
        let worker = Node::build(
            LocalRole::Worker,
            "reconnect-worker",
            "127.0.0.1",
            "127.0.0.1",
            worker_port,
            coordinator_port,
            worker_state,
            worker_cache,
            worker_listener,
            worker_baseline,
        )
        .await?;
        Ok(Self {
            coordinator,
            worker,
        })
    }

    pub fn node(&self, role: LocalRole) -> &Node {
        match role {
            LocalRole::Coordinator => &self.coordinator,
            LocalRole::Worker => &self.worker,
            LocalRole::Unknown => panic!("unknown role"),
        }
    }

    /// Coordinator-initiated normal first pair over real control HTTP.
    pub async fn pair(&self) -> anyhow::Result<()> {
        self.coordinator.production.pair().await?;
        Ok(())
    }

    /// Drive promotion to DistributedReady over real control HTTP. The coordinator's
    /// `promote()` blocks on its rendezvous listener waiting for a DS4 worker HELLO, which the
    /// fake worker child never sends, so we inject a synthetic HELLO frame in a background task
    /// while `promote()` runs. Both nodes then converge on DistributedReady.
    pub async fn promote_to_distributed(&self) -> anyhow::Result<()> {
        let hello = inject_fake_worker_hello(self.coordinator.ds4_distributed_port);
        self.coordinator.production.promote().await?;
        hello.await?;
        let converged = self
            .wait_until_both(
                siderostat::target::ClusterState::DistributedReady,
                Duration::from_secs(10),
            )
            .await;
        anyhow::ensure!(
            converged,
            "nodes did not converge to DistributedReady; coordinator={:?} worker={:?}",
            self.coordinator.mode.snapshot().state,
            self.worker.mode.snapshot().state,
        );
        Ok(())
    }

    /// Poll until both nodes reach the given cluster state, or return the last snapshot.
    pub async fn wait_until_both(
        &self,
        state: siderostat::target::ClusterState,
        timeout: Duration,
    ) -> bool {
        wait_until(timeout, || async move {
            self.coordinator.mode.snapshot().state == state
                && self.worker.mode.snapshot().state == state
        })
        .await
    }

    /// Abort serve tasks and fail the test if any distributed child was left running.
    pub async fn shutdown(self) {
        self.coordinator.serve.abort();
        self.worker.serve.abort();
        let _ = self.coordinator.serve.await;
        let _ = self.worker.serve.await;
        if let Some(child) = &self.coordinator.coordinator_child
            && child.child().is_running()
        {
            panic!("coordinator distributed child left running at shutdown (orphan)");
        }
        if let Some(child) = &self.worker.worker_child
            && child.child().is_running()
        {
            panic!("worker distributed child left running at shutdown (orphan)");
        }
    }
}

/// Deadline-based poll. The caller re-reads the latest snapshot after `false` is returned.
pub async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
