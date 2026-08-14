use anyhow::Result;
use futures::future::BoxFuture;
use siderostat::{
    cluster::{
        ControlAuthenticator, ControlCommand, ControlEndpoint, ControlMessage, ControlMode,
        ControlRole, ControlSecret, EventOwner, LocalStandaloneLifecycle, ModeRuntime,
        NodeDescriptor, WorkerControl,
    },
    proxy::{ModeAwareProxyOptions, ModeAwareProxyState},
    target::{ClusterState, LocalRole, ProxyTarget},
};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct CountedLifecycle {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl LocalStandaloneLifecycle for CountedLifecycle {
    fn start(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let starts = self.starts.clone();
        Box::pin(async move {
            running.store(true, Ordering::SeqCst);
            starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            running.store(false, Ordering::SeqCst);
            stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
    }
}

fn descriptor(role: ControlRole, node_id: &str, generation: u64) -> NodeDescriptor {
    NodeDescriptor {
        protocol_version: 1,
        node_id: node_id.into(),
        role,
        generation,
        mode: ControlMode::SoloStandalone,
        deployment_id: None,
    }
}

fn authenticated_coordinator() -> siderostat::cluster::AuthenticatedPeer {
    let authenticator = ControlAuthenticator::new(
        ControlSecret::new(vec![0x6b; 32]).unwrap(),
        "coordinator",
        IpAddr::from([10, 99, 0, 1]),
    );
    let headers = authenticator
        .sign(
            "coordinator",
            "POST",
            "/v1/pair",
            1_000,
            "phase5-security-nonce",
            b"pair",
        )
        .unwrap();
    authenticator
        .verify(
            "POST",
            "/v1/pair",
            b"pair",
            IpAddr::from([10, 99, 0, 1]),
            &headers,
            1_000,
        )
        .unwrap()
}

#[tokio::test]
async fn ten_route_detach_attach_cycles_converge_without_orphan_state() {
    let proxy = Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse("http://127.0.0.1:18090").unwrap(),
            url::Url::parse("http://10.99.0.1:18082").unwrap(),
            ModeAwareProxyOptions {
                max_in_flight: 4,
                request_body_limit_bytes: 4096,
                response_header_timeout: Duration::from_secs(1),
                first_body_byte_timeout: Duration::from_secs(1),
                stream_idle_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_secs(1),
            },
        )
        .unwrap(),
    );
    let lifecycle = Arc::new(CountedLifecycle::default());
    let runtime = ModeRuntime::spawn_ready(
        LocalRole::Worker,
        proxy.clone(),
        lifecycle.clone(),
        Duration::from_millis(100),
    )
    .await
    .unwrap();
    let mut control = WorkerControl::new(
        descriptor(ControlRole::Worker, "worker", runtime.snapshot().generation),
        Duration::from_millis(100),
        Duration::from_millis(5),
    )
    .unwrap();
    let authenticated = authenticated_coordinator();

    for cycle in 0_u64..10 {
        let generation = runtime.snapshot().generation;
        let now = 2_000 + cycle * 1_000;
        control
            .handle(
                ControlEndpoint::Pair,
                ControlMessage {
                    request_id: format!("pair-{cycle}"),
                    generation,
                    deployment_id: None,
                    command: ControlCommand::Pair {
                        descriptor: descriptor(ControlRole::Coordinator, "coordinator", generation),
                    },
                },
                &authenticated,
                true,
                now,
            )
            .unwrap();
        runtime
            .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, now)
            .await
            .unwrap();
        let paired = runtime
            .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, now + 5)
            .await
            .unwrap();
        assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
        assert_eq!(paired.target, ProxyTarget::Coordinator);

        control.invalidate_route();
        let solo = runtime
            .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, now + 6)
            .await
            .unwrap();
        assert_eq!(solo.state, ClusterState::SoloStandaloneReady);
        assert_eq!(solo.target, ProxyTarget::LocalStandalone);
    }

    assert_eq!(lifecycle.stops.load(Ordering::SeqCst), 10);
    assert_eq!(lifecycle.starts.load(Ordering::SeqCst), 11);
    assert!(lifecycle.running.load(Ordering::SeqCst));
    assert_eq!(runtime.snapshot().generation, 42);
}
