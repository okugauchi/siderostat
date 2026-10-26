#![cfg(feature = "test-support")]
mod support;
use futures::future::BoxFuture;
use siderostat::{
    cluster::*,
    config::{DistributedTopology, Transport},
    target::{ClusterState, LocalRole},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
struct Child {
    role: &'static str,
    events: Arc<Mutex<Vec<String>>>,
    running: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    warmup_ok: Arc<AtomicBool>,
    stop_ok: Arc<AtomicBool>,
    warmup_entered: Arc<tokio::sync::Notify>,
    warmup_blocked: Arc<AtomicBool>,
}
impl TpLocalLifecycle for Child {
    fn preflight(&self, _: Option<std::net::IpAddr>) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn start(&self, _: u64, _: std::net::IpAddr) -> BoxFuture<'static, anyhow::Result<()>> {
        self.running.store(true, Ordering::SeqCst);
        self.events.lock().unwrap().push(self.role.into());
        Box::pin(async { Ok(()) })
    }
    fn status(&self, _: u64) -> BoxFuture<'static, anyhow::Result<TpLocalStatus>> {
        let running = self.running.load(Ordering::SeqCst);
        let connected = self.connected.load(Ordering::SeqCst) && running;
        Box::pin(async move {
            Ok(TpLocalStatus {
                running,
                connected,
                http_ready: running,
            })
        })
    }
    fn warmup(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        self.events.lock().unwrap().push("warmup".into());
        let ok = self.warmup_ok.load(Ordering::SeqCst);
        let entered = self.warmup_entered.clone();
        let blocked = self.warmup_blocked.clone();
        Box::pin(async move {
            entered.notify_one();
            if blocked.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            anyhow::ensure!(ok, "warm-up failed");
            Ok(())
        })
    }
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let ok = self.stop_ok.load(Ordering::SeqCst);
        if ok {
            self.running.store(false, Ordering::SeqCst);
        }
        Box::pin(async move {
            anyhow::ensure!(ok, "owned child stop failed");
            Ok(())
        })
    }
}

fn manifest() -> RuntimeDeployment {
    let digest = "a".repeat(64);
    let source = "b".repeat(40);
    let roles = [("worker","ds4"),("coordinator","ds4-server")].map(|(r,k)|serde_json::json!({"role":r,"executable_kind":k,"path":format!("/tmp/{k}"),"binary_sha256":digest,"compatible_binary_sha256":[digest],"source_commit":source,"arch":"aarch64","backend":"metal","help_sha256":digest}));
    RuntimeDeployment::parse(&serde_json::to_vec(&serde_json::json!({"schema_version":3,"profile_id":"fixture-tp","source_commit":source,"role_artifacts":roles,"model":{"catalog_id":"fixture","sha256":digest,"size":32,"family":"fixture","quantization":"q2"},"transport":"rdma","context_size":8192,"argv_contract_id":"v040-tp"})).unwrap(),DistributedTopology::TensorParallel).unwrap()
}

struct Pair {
    runtimes: Vec<ProductionClusterRuntime>,
    modes: Vec<Arc<ModeRuntime>>,
    proxies: Vec<Arc<siderostat::proxy::ModeAwareProxyState>>,
    children: Vec<Child>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    root: std::path::PathBuf,
}
impl Drop for Pair {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Pair {
    async fn boot() -> Self {
        let root = std::env::temp_dir().join(format!("tp-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ports = [
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        ];
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut pair = Self {
            runtimes: vec![],
            modes: vec![],
            proxies: vec![],
            children: vec![],
            tasks: vec![],
            root,
        };
        for (i, listener) in [a, b].into_iter().enumerate() {
            let role = if i == 0 {
                LocalRole::Coordinator
            } else {
                LocalRole::Worker
            };
            let mut c = support::test_config(
                &format!("node-{i}"),
                "127.0.0.1",
                "127.0.0.1",
                ports[i],
                9911,
                18082,
                pair.root.join(format!("state-{i}")),
                pair.root.clone(),
            );
            c.ds4.distributed.topology = DistributedTopology::TensorParallel;
            c.ds4.distributed.transport = Transport::Rdma;
            c.ds4.distributed.coordinator_layers = None;
            c.ds4.distributed.worker_layers = None;
            c.ds4.distributed.context_size = 8192;
            c.ds4.distributed.rdma_address = Some("127.0.0.1".into());
            c.cluster.timeouts.complete_route = Duration::from_millis(150);
            c.cluster.policy.required_peer_stability = Duration::from_millis(1);
            c.cluster.policy.promotion_backoff = Duration::from_millis(1);
            let proxy = support::proxy_state().unwrap();
            let local = Arc::new(support::FakeStandalone::new("local", 9000 + i as u32));
            let mode = Arc::new(
                ModeRuntime::spawn_ready(
                    role,
                    proxy.clone(),
                    local.clone(),
                    Duration::from_secs(1),
                )
                .await
                .unwrap(),
            );
            let child = Child {
                role: if i == 0 { "coordinator" } else { "worker" },
                events: events.clone(),
                running: Arc::new(AtomicBool::new(false)),
                connected: Arc::new(AtomicBool::new(true)),
                warmup_ok: Arc::new(AtomicBool::new(true)),
                stop_ok: Arc::new(AtomicBool::new(true)),
                warmup_entered: Arc::new(tokio::sync::Notify::new()),
                warmup_blocked: Arc::new(AtomicBool::new(false)),
            };
            let runtime = ProductionClusterRuntime::new_with_tp_lifecycle(
                c,
                role,
                mode.clone(),
                proxy.clone(),
                local,
                manifest(),
                vec![0x42; 32],
                ports[1 - i],
                Arc::new(child.clone()),
            )
            .unwrap();
            runtime.set_network_evidence(NetworkSnapshot {
                epoch: 1,
                state: ThunderboltIpState::AuthenticatedPeer,
                role,
                local_address: Some("127.0.0.1".parse().unwrap()),
                expected_peer_address: Some("127.0.0.1".parse().unwrap()),
                peer_present: true,
            });
            let router = runtime
                .router()
                .into_make_service_with_connect_info::<std::net::SocketAddr>();
            pair.tasks.push(tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            }));
            pair.runtimes.push(runtime);
            pair.modes.push(mode);
            pair.proxies.push(proxy);
            pair.children.push(child);
        }
        pair
    }
}

#[tokio::test]
async fn real_control_path_starts_worker_first_and_requires_warmup() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    assert_eq!(
        pair.proxies[1].target_snapshot().target,
        siderostat::target::ProxyTarget::Coordinator
    );
    assert_eq!(
        pair.proxies[1].target_snapshot().target,
        pair.modes[1].snapshot().target
    );
    pair.runtimes[0].promote().await.unwrap();
    for mode in &pair.modes {
        assert_eq!(mode.snapshot().state, ClusterState::TensorParallelReady);
    }
    assert_eq!(
        *pair.children[0].events.lock().unwrap(),
        ["worker", "coordinator", "warmup"]
    );
    pair.runtimes[0].demote().await.unwrap();
    assert!(
        pair.children
            .iter()
            .all(|c| !c.running.load(Ordering::SeqCst))
    );
}

#[tokio::test]
async fn warmup_failure_keeps_routes_closed_and_recovers_standalone() {
    let pair = Pair::boot().await;
    pair.children[0].warmup_ok.store(false, Ordering::SeqCst);
    pair.runtimes[0].pair().await.unwrap();
    assert!(pair.runtimes[0].promote().await.is_err());
    assert_ne!(
        pair.modes[0].snapshot().state,
        ClusterState::TensorParallelReady
    );
    assert!(
        pair.children
            .iter()
            .all(|c| !c.running.load(Ordering::SeqCst))
    );
    assert_eq!(
        pair.proxies[0].admission().snapshot().state,
        siderostat::admission::AdmissionState::Serving
    );
}

#[tokio::test]
async fn missing_handshake_never_warms_up_or_publishes() {
    let pair = Pair::boot().await;
    pair.children[1].connected.store(false, Ordering::SeqCst);
    pair.runtimes[0].pair().await.unwrap();
    assert!(pair.runtimes[0].promote().await.is_err());
    assert!(
        !pair.children[0]
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == "warmup")
    );
}

#[tokio::test]
async fn force_policy_stops_both_tp_children() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.runtimes[0]
        .apply_policy_transaction(OperationPolicy::ForcedStandalone, 0, uuid::Uuid::new_v4())
        .await
        .unwrap();
    assert!(
        pair.children
            .iter()
            .all(|c| !c.running.load(Ordering::SeqCst))
    );
    for mode in &pair.modes {
        assert_eq!(mode.snapshot().state, ClusterState::SoloStandaloneReady);
    }
}

#[tokio::test]
async fn ten_reconnects_do_not_exhaust_failure_budget() {
    let pair = Pair::boot().await;
    let mut previous = 0;
    for _ in 0..10 {
        pair.runtimes[0].pair().await.unwrap();
        pair.runtimes[0].promote().await.unwrap();
        let session = pair.modes[0].snapshot().tp.unwrap().session.0;
        assert!(session > previous);
        previous = session;
        pair.runtimes[0].demote().await.unwrap();
    }
}

#[tokio::test]
async fn stopped_peer_cannot_remain_tp_ready() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.children[1].running.store(false, Ordering::SeqCst);
    pair.runtimes[0].reconcile().await.unwrap();
    assert_eq!(
        pair.modes[0].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
}

#[tokio::test]
async fn retry_budget_survives_fallback_and_runtime_recreation() {
    let pair = Pair::boot().await;
    let store = Arc::new(StateStore::acquire(pair.root.join("journal.json")).unwrap());
    pair.runtimes[0].attach_policy_store(store.clone());
    pair.children[0].warmup_ok.store(false, Ordering::SeqCst);
    for _ in 0..3 {
        pair.runtimes[0].pair().await.unwrap();
        assert!(pair.runtimes[0].promote().await.is_err());
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let saved = store.load().unwrap().unwrap();
    assert_eq!(saved.tp.unwrap().failed_attempts, 3);
    let next = Pair::boot().await;
    next.runtimes[0].attach_policy_store(store);
    next.runtimes[0].pair().await.unwrap();
    assert!(next.runtimes[0].promote().await.is_err());
    assert!(
        next.children[0].events.lock().unwrap().is_empty(),
        "restart must not reset budget"
    );
}

async fn signed_tp(
    pair: &Pair,
    command: &str,
    session: u64,
    operation: uuid::Uuid,
) -> reqwest::StatusCode {
    let body=serde_json::to_vec(&serde_json::json!({"version":1,"deployment":manifest().deployment_id().unwrap(),"epoch":0,"session":session,"operation":operation,"coordinator_address":"127.0.0.1","command":command})).unwrap();
    let auth = ControlAuthenticator::new_at_source(
        ControlSecret::new(vec![0x42; 32]).unwrap(),
        "127.0.0.1".parse().unwrap(),
    );
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let signed = auth
        .sign(
            "node-0",
            "POST",
            "/v2/tp",
            millis,
            uuid::Uuid::new_v4().simple().to_string(),
            &body,
        )
        .unwrap();
    reqwest::Client::new()
        .post(format!("http://{}/v2/tp", pair.runtimes[1].listen_addr()))
        .header(HEADER_NODE, signed.node_id())
        .header(HEADER_TIMESTAMP, signed.timestamp_millis())
        .header(HEADER_NONCE, signed.nonce())
        .header(HEADER_SIGNATURE, signed.signature())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn duplicate_stop_is_idempotent_but_cannot_stop_a_new_session() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    let op = uuid::Uuid::new_v4();
    assert!(signed_tp(&pair, "prepare", 1, op).await.is_success());
    assert!(signed_tp(&pair, "stop", 1, op).await.is_success());
    assert!(signed_tp(&pair, "stop", 1, op).await.is_success());
    pair.runtimes[0].pair().await.unwrap();
    let next = uuid::Uuid::new_v4();
    assert!(signed_tp(&pair, "prepare", 2, next).await.is_success());
    assert!(!signed_tp(&pair, "stop", 1, op).await.is_success());
    assert!(pair.children[1].running.load(Ordering::SeqCst));
    assert!(signed_tp(&pair, "stop", 2, next).await.is_success());
}

#[tokio::test]
async fn planned_restart_keeps_local_admission_closed_after_child_stop() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.runtimes[0].begin_planned_restart();
    pair.runtimes[0]
        .stop_for_planned_restart(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!pair.children[0].running.load(Ordering::SeqCst));
    assert_ne!(
        pair.proxies[0].admission().snapshot().state,
        siderostat::admission::AdmissionState::Serving
    );
}

#[tokio::test]
async fn worker_prepare_loss_recovers() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    let op = uuid::Uuid::new_v4();
    assert!(signed_tp(&pair, "prepare", 1, op).await.is_success());
    pair.children[1].running.store(false, Ordering::SeqCst);
    pair.runtimes[1].reconcile().await.unwrap();
    assert_eq!(
        pair.modes[1].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
}

#[tokio::test]
async fn ready_failures_exhaust_budget() {
    let pair = Pair::boot().await;
    for _ in 0..3 {
        pair.runtimes[0].pair().await.unwrap();
        pair.runtimes[0].promote().await.unwrap();
        pair.children[1].running.store(false, Ordering::SeqCst);
        pair.runtimes[0].reconcile().await.unwrap();
    }
    pair.runtimes[0].pair().await.unwrap();
    assert!(
        pair.runtimes[0].promote().await.is_err(),
        "fourth automatic attempt must be blocked"
    );
}

#[tokio::test]
async fn restart_cleanup_does_not_restart_standalone_or_serve() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.runtimes[0].begin_planned_restart();
    pair.runtimes[0]
        .stop_for_planned_restart(Duration::from_secs(1))
        .await
        .unwrap();
    pair.runtimes[0].stop_distributed().await.unwrap();
    assert_ne!(
        pair.proxies[0].admission().snapshot().state,
        siderostat::admission::AdmissionState::Serving,
        "run_servers cleanup must keep admission blocked"
    );
}

#[tokio::test]
async fn prepared_worker_recovers_when_coordinator_control_disappears() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    assert!(
        signed_tp(&pair, "prepare", 1, uuid::Uuid::new_v4())
            .await
            .is_success()
    );
    pair.tasks[0].abort();
    pair.runtimes[1].reconcile().await.unwrap();
    assert_eq!(
        pair.modes[1].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert!(!pair.children[1].running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn operator_reconcile_verifies_standalone_instead_of_recreating_tp_ready() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.children[0].running.store(false, Ordering::SeqCst);
    let snapshot = pair.modes[0].snapshot();
    pair.modes[0]
        .cluster_handle()
        .apply(ClusterEvent::new(
            snapshot.generation,
            ClusterEventKind::RequireManualIntervention,
        ))
        .await
        .unwrap();
    pair.runtimes[0].operator_reconcile().await.unwrap();
    assert_eq!(
        pair.modes[0].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert!(!pair.children[0].running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn forced_policy_interrupts_an_unfinished_warmup() {
    let pair = Pair::boot().await;
    pair.children[0]
        .warmup_blocked
        .store(true, Ordering::SeqCst);
    pair.runtimes[0].pair().await.unwrap();
    let runtime = pair.runtimes[0].clone();
    let promotion = tokio::spawn(async move { runtime.promote().await });
    pair.children[0].warmup_entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(3),
        pair.runtimes[0].apply_policy_transaction(
            OperationPolicy::ForcedStandalone,
            0,
            uuid::Uuid::new_v4(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(promotion.await.unwrap().is_err());
    assert!(
        pair.children
            .iter()
            .all(|c| !c.running.load(Ordering::SeqCst))
    );
    assert_eq!(
        pair.modes[0].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
}

#[tokio::test]
async fn tp_diagnostics_reports_the_real_lifecycle_slot() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    let d = pair.runtimes[0].diagnostics().await;
    let child = d.children.distributed_coordinator.unwrap();
    assert!(child.running);
    assert_eq!(child.ready, Some(true));
    assert_eq!(child.pid, None);
    assert!(d.control_session.lease.valid);
}

#[tokio::test]
async fn prepared_worker_times_out_if_live_coordinator_abandons_startup() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    assert!(
        signed_tp(&pair, "prepare", 1, uuid::Uuid::new_v4())
            .await
            .is_success()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            pair.runtimes[1].reconcile().await.unwrap();
            if pair.modes[1].snapshot().state == ClusterState::SoloStandaloneReady {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!pair.children[1].running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn failed_owned_child_stop_requires_manual_recovery_and_keeps_admission_closed() {
    let pair = Pair::boot().await;
    pair.runtimes[0].pair().await.unwrap();
    pair.runtimes[0].promote().await.unwrap();
    pair.children[0].stop_ok.store(false, Ordering::SeqCst);
    assert!(pair.runtimes[0].demote().await.is_err());
    assert!(pair.children[0].running.load(Ordering::SeqCst));
    assert_eq!(
        pair.modes[0].snapshot().state,
        ClusterState::ManualInterventionRequired
    );
    assert_ne!(
        pair.proxies[0].admission().snapshot().state,
        siderostat::admission::AdmissionState::Serving
    );
    pair.children[0].stop_ok.store(true, Ordering::SeqCst);
    pair.runtimes[0].operator_reconcile().await.unwrap();
    assert!(!pair.children[0].running.load(Ordering::SeqCst));
    assert_eq!(
        pair.modes[0].snapshot().state,
        ClusterState::SoloStandaloneReady
    );
}
