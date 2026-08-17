use anyhow::Result;
use axum::{Router, body::Body, extract::Request, routing::any};
use futures::future::BoxFuture;
use siderostat::{
    cluster::{
        ControlAuthenticator, ControlCommand, ControlEndpoint, ControlMessage, ControlMode,
        ControlRole, ControlSecret, EventOwner, LocalStandaloneLifecycle, ModeRuntime,
        NodeDescriptor, WorkerControl,
    },
    proxy::{
        ModeAwareProxyOptions, ModeAwareProxyState, PeerProxyToken, mode_aware_proxy_handler,
        peer_ingress_handler,
    },
    target::LocalRole,
};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

struct Server {
    address: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    Server { address, task }
}

async fn fake_upstream(name: &'static str) -> Server {
    serve(Router::new().route(
        "/{*path}",
        any(move |_request: Request<Body>| async move { name }),
    ))
    .await
}

fn proxy_state(local: &Server, coordinator: &Server) -> Arc<ModeAwareProxyState> {
    Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse(&format!("http://{}", local.address)).unwrap(),
            url::Url::parse(&format!("http://{}", coordinator.address)).unwrap(),
            ModeAwareProxyOptions {
                max_in_flight: 8,
                request_body_limit_bytes: 4096,
                response_header_timeout: Duration::from_secs(2),
                first_body_byte_timeout: Duration::from_secs(2),
                stream_idle_timeout: Duration::from_secs(2),
                connect_timeout: Duration::from_millis(200),
            },
        )
        .unwrap(),
    )
}

#[derive(Default)]
struct FakeLifecycle;

impl LocalStandaloneLifecycle for FakeLifecycle {
    fn start(&self, _generation: u64) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn is_running(&self) -> BoxFuture<'static, Result<bool>> {
        Box::pin(async { Ok(true) })
    }
}

fn descriptor(role: ControlRole, node_id: &str) -> NodeDescriptor {
    NodeDescriptor {
        protocol_version: 1,
        node_id: node_id.into(),
        role,
        generation: 2,
        mode: ControlMode::SoloStandalone,
        deployment_id: None,
    }
}

fn authenticated_coordinator() -> siderostat::cluster::AuthenticatedPeer {
    let authenticator = ControlAuthenticator::new(
        ControlSecret::new(vec![0x5a; 32]).unwrap(),
        "coordinator",
        IpAddr::from([10, 99, 0, 1]),
    );
    let headers = authenticator
        .sign(
            "coordinator",
            "POST",
            "/v1/pair",
            1_000,
            "phase2-nonce-0001",
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
async fn fake_upstreams_converge_solo_paired_solo() {
    let worker_upstream = fake_upstream("worker-solo").await;
    let coordinator_upstream = fake_upstream("coordinator-local").await;

    let coordinator_state = proxy_state(&coordinator_upstream, &coordinator_upstream);
    coordinator_state.configure_peer_proxy(
        PeerProxyToken::new(vec![0x22; 32]).unwrap(),
        IpAddr::from([127, 0, 0, 1]),
    );
    coordinator_state.set_target(siderostat::target::ProxyTarget::LocalStandalone, true);
    coordinator_state.admission().start_serving();
    let peer_ingress = serve(
        Router::new()
            .route("/", any(peer_ingress_handler))
            .route("/{*path}", any(peer_ingress_handler))
            .with_state(coordinator_state),
    )
    .await;

    let worker_state = proxy_state(&worker_upstream, &peer_ingress);
    worker_state.configure_peer_proxy(
        PeerProxyToken::new(vec![0x22; 32]).unwrap(),
        IpAddr::from([127, 0, 0, 1]),
    );
    let runtime = ModeRuntime::spawn_ready(
        LocalRole::Worker,
        worker_state.clone(),
        Arc::new(FakeLifecycle),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let public = serve(
        Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(worker_state),
    )
    .await;

    let url = format!("http://{}/v1/responses", public.address);
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "worker-solo"
    );

    let mut control = WorkerControl::new(
        descriptor(ControlRole::Worker, "worker"),
        Duration::from_millis(150),
        Duration::from_millis(20),
    )
    .unwrap();
    control
        .handle(
            ControlEndpoint::Pair,
            ControlMessage {
                request_id: "pair-1".into(),
                generation: 2,
                deployment_id: None,
                command: ControlCommand::Pair {
                    descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
                },
            },
            &authenticated_coordinator(),
            true,
            1_000,
        )
        .unwrap();
    runtime
        .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, 1_020)
        .await
        .unwrap();
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "coordinator-local"
    );

    runtime
        .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, 1_150)
        .await
        .unwrap();
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "worker-solo"
    );
}
