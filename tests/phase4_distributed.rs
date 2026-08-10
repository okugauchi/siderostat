#![cfg(feature = "test-support")]

use anyhow::Result;
use futures::future::BoxFuture;
use siderostat::{
    admission::AdmissionGate,
    cluster::{
        AuthenticatedPeer, ClusterEvent, ClusterEventKind, ClusterSnapshot, ControlAuthenticator,
        ControlCommand, ControlEndpoint, ControlMessage, ControlMode, ControlRole, ControlSecret,
        CoordinatorControl, CoordinatorDistributedRuntime, CoordinatorLifecycleError,
        CoordinatorPeerLifecycle, CoordinatorRuntimeTimeouts, DistributedCoordinatorLifecycle,
        DistributedWorkerLifecycle, Ds4Hello, LocalStandaloneLifecycle, NodeDescriptor,
        PromotionRetryPolicy, RendezvousControlSnapshot, RendezvousListener,
        WorkerDistributedRuntime, WorkerEventKind, WorkerHelloExpectation, spawn_state_machine,
    },
    proxy::{ModeAwareProxyOptions, ModeAwareProxyState},
    target::{ClusterState, LocalRole, ProxyTarget, StableMode},
};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncWriteExt;

#[derive(Default)]
struct FakeStandalone {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl LocalStandaloneLifecycle for FakeStandalone {
    fn start(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let starts = self.starts.clone();
        Box::pin(async move {
            starts.fetch_add(1, Ordering::SeqCst);
            running.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
    }
}

#[derive(Default)]
struct FakeWorkerChild {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl DistributedWorkerLifecycle for FakeWorkerChild {
    fn start(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let starts = self.starts.clone();
        Box::pin(async move {
            starts.fetch_add(1, Ordering::SeqCst);
            running.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
    }
}

#[derive(Default)]
struct FakeCoordinatorChild {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    hang_start: Arc<AtomicBool>,
}

impl DistributedCoordinatorLifecycle for FakeCoordinatorChild {
    fn start(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let starts = self.starts.clone();
        let hang = self.hang_start.clone();
        Box::pin(async move {
            starts.fetch_add(1, Ordering::SeqCst);
            running.store(true, Ordering::SeqCst);
            if hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn wait_ready(&self) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        Box::pin(async move {
            anyhow::ensure!(running.load(Ordering::SeqCst), "coordinator not running");
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, Result<()>> {
        Box::pin(std::future::pending())
    }
}

struct FakePeer {
    worker: WorkerDistributedRuntime,
    admission: AdmissionGate,
    drains: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl CoordinatorPeerLifecycle for FakePeer {
    fn begin_drain(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let drains = self.drains.clone();
        Box::pin(async move {
            drains.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop_worker(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let worker = self.worker.clone();
        let admission = self.admission.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            worker.cancel().await?;
            admission.start_serving();
            stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

fn proxy() -> Arc<ModeAwareProxyState> {
    Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse("http://127.0.0.1:8000").unwrap(),
            url::Url::parse("http://127.0.0.1:18082").unwrap(),
            ModeAwareProxyOptions {
                max_in_flight: 8,
                request_body_limit_bytes: 4096,
                response_header_timeout: Duration::from_secs(1),
                first_body_byte_timeout: Duration::from_secs(1),
                stream_idle_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_millis(100),
            },
        )
        .unwrap(),
    )
}

fn descriptor(role: ControlRole, generation: u64) -> NodeDescriptor {
    NodeDescriptor {
        protocol_version: 1,
        node_id: match role {
            ControlRole::Coordinator => "coordinator",
            ControlRole::Worker => "worker",
        }
        .into(),
        role,
        generation,
        mode: ControlMode::PairedStandalone,
        deployment_id: Some("deployment-phase4".into()),
    }
}

fn authenticated_worker() -> AuthenticatedPeer {
    let authenticator = ControlAuthenticator::new(
        ControlSecret::new(vec![0x44; 32]).unwrap(),
        "worker",
        IpAddr::from([10, 99, 0, 2]),
    );
    let headers = authenticator
        .sign(
            "worker",
            "POST",
            "/v1/pair",
            1_000,
            "phase4-worker-nonce",
            b"pair",
        )
        .unwrap();
    authenticator
        .verify(
            "POST",
            "/v1/pair",
            b"pair",
            IpAddr::from([10, 99, 0, 2]),
            &headers,
            1_000,
        )
        .unwrap()
}

fn ready_control(generation: u64, suffix: usize) -> CoordinatorControl {
    let mut control = CoordinatorControl::new(
        descriptor(ControlRole::Coordinator, generation),
        Duration::from_secs(15),
        Duration::ZERO,
    )
    .unwrap();
    control
        .handle(
            ControlEndpoint::Pair,
            ControlMessage {
                request_id: format!("pair-{suffix}"),
                generation,
                deployment_id: None,
                command: ControlCommand::Pair {
                    descriptor: descriptor(ControlRole::Worker, generation),
                },
            },
            &authenticated_worker(),
            true,
            1_000,
        )
        .unwrap();
    control.note_prepare_sent(generation).unwrap();
    control
        .handle(
            ControlEndpoint::WorkerEvent,
            ControlMessage {
                request_id: format!("ready-{suffix}"),
                generation,
                deployment_id: Some("deployment-phase4".into()),
                command: ControlCommand::WorkerEvent {
                    event: WorkerEventKind::Ready,
                },
            },
            &authenticated_worker(),
            true,
            1_001,
        )
        .unwrap();
    control
}

fn fixture() -> Vec<u8> {
    include_str!("fixtures/ds4/hello40-schema-v1.hex")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| line.split_ascii_whitespace())
        .map(|value| u8::from_str_radix(value, 16).unwrap())
        .collect()
}

async fn accept_fake_child_hello(generation: u64) -> Ds4Hello {
    let control = RendezvousControlSnapshot {
        state: ClusterState::AwaitingWorkerHello,
        generation,
        deployment_id: Some("deployment-phase4".into()),
        lease_valid: true,
    };
    let listener = RendezvousListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        WorkerHelloExpectation {
            coordinator_address: IpAddr::from([127, 0, 0, 1]),
            worker_address: IpAddr::from([127, 0, 0, 1]),
            control: control.clone(),
            layer_start: 20,
            layer_end: 42,
            has_output: true,
            context_size: 262_144,
            model_name: "deepseek-v4-flash".into(),
        },
    )
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    let sender = tokio::spawn(async move {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(&fixture()).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let hello = listener
        .accept_one(Duration::from_secs(1), || control)
        .await
        .unwrap();
    sender.await.unwrap();
    hello
}

#[tokio::test]
async fn fake_two_node_distributed_cycles_and_failure_recovery() {
    let (cluster, state_task) =
        spawn_state_machine(ClusterSnapshot::booting(LocalRole::Coordinator), 32);
    let mut generation = 0;
    for kind in [
        ClusterEventKind::BeginSoloStandalone,
        ClusterEventKind::LocalStandaloneReady,
        ClusterEventKind::BeginPairing,
        ClusterEventKind::PairingReady,
    ] {
        generation = cluster
            .apply(ClusterEvent {
                expected_generation: generation,
                kind,
            })
            .await
            .unwrap()
            .generation;
    }

    let coordinator_proxy = proxy();
    coordinator_proxy.set_target(ProxyTarget::LocalStandalone, true);
    coordinator_proxy.admission().start_serving();
    let coordinator_standalone = Arc::new(FakeStandalone::default());
    coordinator_standalone.running.store(true, Ordering::SeqCst);
    let coordinator_child = Arc::new(FakeCoordinatorChild::default());

    let worker_admission = AdmissionGate::new(8);
    worker_admission.start_serving();
    let worker_standalone = Arc::new(FakeStandalone::default());
    let worker_child = Arc::new(FakeWorkerChild::default());
    let worker_runtime = WorkerDistributedRuntime::new(
        worker_admission.clone(),
        worker_standalone,
        worker_child.clone(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_millis(5),
    )
    .unwrap();
    let peer = Arc::new(FakePeer {
        worker: worker_runtime.clone(),
        admission: worker_admission,
        drains: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
    });
    let runtime = CoordinatorDistributedRuntime::new(
        cluster.clone(),
        coordinator_proxy.clone(),
        coordinator_standalone.clone(),
        coordinator_child.clone(),
        peer.clone(),
        CoordinatorRuntimeTimeouts {
            drain: Duration::from_secs(1),
            startup: Duration::from_millis(30),
            complete_route: Duration::from_secs(1),
            route_loss_grace: Duration::from_millis(10),
        },
        PromotionRetryPolicy {
            backoff: Duration::from_millis(50),
            maximum_consecutive_failures: 3,
        },
    )
    .unwrap();

    for cycle in 0..10 {
        let awaiting = cluster
            .apply(ClusterEvent {
                expected_generation: cluster.snapshot().generation,
                kind: ClusterEventKind::BeginPromotion,
            })
            .await
            .unwrap();
        worker_runtime
            .prepare(awaiting.generation, Arc::new(|| true))
            .await
            .unwrap();
        let hello = accept_fake_child_hello(awaiting.generation).await;
        let control_generation = 100 + cycle as u64;
        let control = ready_control(control_generation, cycle);
        let distributed = runtime
            .promote_after_hello(hello, &control, 1_001, Arc::new(|| true))
            .await
            .unwrap();
        assert_eq!(distributed.stable_mode, StableMode::DistributedMxfp4);
        assert!(coordinator_child.running.load(Ordering::SeqCst));
        assert!(worker_child.running.load(Ordering::SeqCst));

        let paired = runtime.demote().await.unwrap();
        assert_eq!(paired.stable_mode, StableMode::PairedStandalone);
        assert!(!coordinator_child.running.load(Ordering::SeqCst));
        assert!(!worker_child.running.load(Ordering::SeqCst));
    }

    let awaiting = cluster
        .apply(ClusterEvent {
            expected_generation: cluster.snapshot().generation,
            kind: ClusterEventKind::BeginPromotion,
        })
        .await
        .unwrap();
    worker_runtime
        .prepare(awaiting.generation, Arc::new(|| true))
        .await
        .unwrap();
    let hello = accept_fake_child_hello(awaiting.generation).await;
    let control = ready_control(999, 999);
    coordinator_child.hang_start.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime
            .promote_after_hello(hello, &control, 1_001, Arc::new(|| true))
            .await,
        Err(CoordinatorLifecycleError::StartupTimeout)
    ));
    assert_eq!(cluster.snapshot().state, ClusterState::Backoff);
    assert!(!coordinator_child.running.load(Ordering::SeqCst));
    assert!(!worker_child.running.load(Ordering::SeqCst));
    assert!(coordinator_standalone.running.load(Ordering::SeqCst));
    assert_eq!(coordinator_child.starts.load(Ordering::SeqCst), 11);
    assert_eq!(worker_child.starts.load(Ordering::SeqCst), 11);
    assert_eq!(peer.drains.load(Ordering::SeqCst), 21);
    assert_eq!(peer.stops.load(Ordering::SeqCst), 11);
    assert!(coordinator_proxy.target_snapshot().ready);
    state_task.abort();
}
