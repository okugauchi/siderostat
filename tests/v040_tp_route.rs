//! T09 — TP proxy target / 一 hop 転送 / 503 / stream cancel / 有限 error code。
//!
//! 事後条件「外部 request は TP worker へ直接到達せず、切替中は 503」を検証する。
//! - worker TP-ready → public request は peer ingress（Coordinator）へ一 hop 転送される。
//! - TP 途中（transition）は Unavailable → 503（NoBackendAvailable）。
//! - stream cancel で admission permit が返却される。
//! - unknown failure は有限の error code を返す（ハング/クラッシュしない）。。
//!
//! 実プロセスは spawn せず、既存 proxy / reducer / target 解決を本番経路で駆動する。

#![cfg(feature = "test-support")]

use axum::{Router, body::Body, extract::Request, routing::any};
use siderostat::{
    proxy::{
        ModeAwareProxyOptions, ModeAwareProxyState, PeerProxyToken, mode_aware_proxy_handler,
        peer_ingress_handler,
    },
    target::{ClusterState, LocalRole, ProxyTarget, StableMode, resolve_target},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

struct Server {
    address: SocketAddr,
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
            app.into_make_service_with_connect_info::<SocketAddr>(),
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

/// ヘッダは送るがストリームを送り続けて終了しない upstream ハンドラ。client cancel で
/// permit が返却されることを検証するための fixture。
async fn streaming_handler() -> axum::response::Response<Body> {
    use futures::stream;
    let chunks = stream::unfold(0_u64, |index| async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Some((
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(format!("{index}\n"))),
            index + 1,
        ))
    });
    let body = Body::from_stream(chunks);
    let mut response = axum::response::Response::new(body);
    *response.status_mut() = axum::http::StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    response
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

/// 受入 case 1: worker TP-ready → public request は coordinator へ一 hop。
/// worker proxy の target を Coordinator にし、public ingress が peer ingress
/// （coordinator 側）へ一 hop 転送されることを確認する。TP worker へ直接は到達しない。
#[tokio::test]
async fn v040_tp_worker_public_goes_to_coordinator_one_hop() {
    let worker_solo = fake_upstream("worker-direct").await;
    let coordinator_local = fake_upstream("coordinator-local").await;

    // coordinator 側 proxy: TP-ready coordinator は local standalone へ。
    let coordinator_state = proxy_state(&coordinator_local, &coordinator_local);
    coordinator_state.configure_peer_proxy(
        PeerProxyToken::new(vec![0x22; 32]).unwrap(),
        std::net::IpAddr::from([127, 0, 0, 1]),
    );
    coordinator_state.set_target(ProxyTarget::LocalStandalone, true);
    coordinator_state.admission().start_serving();
    let peer_ingress = serve(
        Router::new()
            .route("/", any(peer_ingress_handler))
            .route("/{*path}", any(peer_ingress_handler))
            .with_state(coordinator_state),
    )
    .await;

    // worker 側 proxy: TP-ready worker は Coordinator を向く（一 hop）。
    let worker_state = proxy_state(&worker_solo, &peer_ingress);
    worker_state.configure_peer_proxy(
        PeerProxyToken::new(vec![0x22; 32]).unwrap(),
        std::net::IpAddr::from([127, 0, 0, 1]),
    );
    worker_state.set_target(ProxyTarget::Coordinator, true);
    worker_state.admission().start_serving();
    let public = serve(
        Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(worker_state),
    )
    .await;

    let url = format!("http://{}/v1/chat/completions", public.address);
    let body = reqwest::Client::new()
        .post(&url)
        .body("{\"model\":\"deepseek-v4-flash\"}")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // worker の local（直接到達）ではなく coordinator を経由した応答。
    assert_eq!(body, "coordinator-local");
}

/// 受入 case 2: TP 途中（transition）→ 503 / 再送なし。NoBackendAvailable。
#[tokio::test]
async fn v040_tp_transition_returns_503_no_retry() {
    let worker_solo = fake_upstream("worker-direct").await;
    let coordinator_local = fake_upstream("coordinator-local").await;
    let state = proxy_state(&worker_solo, &coordinator_local);
    // TP 途中 = Unavailable（Transition）。admission は serving でも target 非公開。。
    state.set_target(
        ProxyTarget::Unavailable {
            reason: siderostat::target::UnavailableReason::Transition,
        },
        false,
    );
    state.admission().start_serving();
    let public = serve(
        Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(state),
    )
    .await;

    let url = format!("http://{}/v1/chat/completions", public.address);
    let response = reqwest::Client::new()
        .post(&url)
        .body("{\"model\":\"deepseek-v4-flash\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let text = response.text().await.unwrap();
    assert!(text.contains("no_backend_available"), "unexpected: {text}");
    // 再送なし: upstream には一度も到達していない（worker-solo/coordinator-local 応答ではない）。
    assert!(!text.contains("worker-direct"));
    assert!(!text.contains("coordinator-local"));
}

/// 受入 case 3: stream cancel → admission permit 返却。in_flight が 0 に戻る。。
#[tokio::test]
async fn v040_tp_stream_cancel_returns_permit() {
    use axum::http::StatusCode;
    use futures::StreamExt;

    // 応答ヘッダは送るが、ストリームは送り続けて終了しない upstream。。。
    let streaming_upstream = serve(Router::new().route("/{*path}", any(streaming_handler))).await;

    let state = proxy_state(&streaming_upstream, &streaming_upstream);
    state.set_target(ProxyTarget::LocalStandalone, true);
    state.admission().start_serving();
    let public = serve(
        Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(state.clone()),
    )
    .await;

    let url = format!("http://{}/v1/chat/completions", public.address);
    let response = reqwest::Client::new()
        .post(&url)
        .body("{\"model\":\"deepseek-v4-flash\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // permit 獲得中は in_flight 1。
    let mut body = response.bytes_stream();
    let _ = body.next().await.unwrap().unwrap();
    assert_eq!(state.admission().snapshot().in_flight, 1);
    // client が stream を cancel（drop）→ permit 返却で in_flight 0。。
    drop(body);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while state.admission().snapshot().in_flight != 0 {
        assert!(std::time::Instant::now() < deadline, "permit not returned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.admission().snapshot().in_flight, 0);
}

/// 受入 case 4: unknown failure → 有限 error code。接続不可 upstream → 有限 502。
#[tokio::test]
async fn v040_tp_unknown_failure_is_finite_error_code() {
    // 誰も listen していない address を upstream にすると connect 失敗 → 有限 502。
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    // local/coordinator upstream を dead に向ける（connect 失敗 → 有限 502）。。
    let state = Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse(&format!("http://{dead_addr}")).unwrap(),
            url::Url::parse(&format!("http://{dead_addr}")).unwrap(),
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
    );
    state.set_target(ProxyTarget::LocalStandalone, true);
    state.admission().start_serving();
    let public = serve(
        Router::new()
            .route("/", any(mode_aware_proxy_handler))
            .route("/{*path}", any(mode_aware_proxy_handler))
            .with_state(state),
    )
    .await;

    let url = format!("http://{}/v1/chat/completions", public.address);
    let response = reqwest::Client::new()
        .post(&url)
        .body("{\"model\":\"deepseek-v4-flash\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap();
    // 有限 error code（upstream_connect_failed）。
    assert!(
        text.contains("upstream_connect_failed"),
        "unexpected: {text}"
    );
}

/// target 解決: TP-ready の worker は Coordinator、coordinator は LocalStandalone。
/// TP 途中は Unavailable（503 に相当）。
#[tokio::test]
async fn v040_tp_resolve_target_maps_worker_and_transition() {
    // TP-ready worker → Coordinator（一 hop）。
    assert_eq!(
        resolve_target(
            LocalRole::Worker,
            StableMode::DistributedTensorParallel,
            ClusterState::TensorParallelReady,
            true,
        ),
        ProxyTarget::Coordinator
    );
    // TP-ready coordinator → LocalStandalone。
    assert_eq!(
        resolve_target(
            LocalRole::Coordinator,
            StableMode::DistributedTensorParallel,
            ClusterState::TensorParallelReady,
            true,
        ),
        ProxyTarget::LocalStandalone
    );
    // TP 途中（Starting / Awaiting / Demoting / Backoff）→ Unavailable（503）。
    for state in [
        ClusterState::TensorParallelStarting,
        ClusterState::AwaitingTensorParallelWorkerHello,
        ClusterState::DemotingTensorParallel,
        ClusterState::TensorParallelBackoff,
    ] {
        assert!(matches!(
            resolve_target(
                LocalRole::Worker,
                StableMode::DistributedTensorParallel,
                state,
                true,
            ),
            ProxyTarget::Unavailable { .. }
        ));
    }
}
