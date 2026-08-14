use super::*;

use crate::{
    cluster::{
        AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlError, ControlMessage,
        ControlMode, ControlResponseStatus, ControlRole, DistributedControlPhase,
        LocalStandaloneLifecycle, NodeDescriptor, RendezvousControlSnapshot, WorkerEventKind,
        spawn_state_machine,
    },
    proxy::{ModeAwareProxyOptions, ModeAwareTargetSnapshot},
    target::{LocalRole, StableMode},
};
use std::{
    net::IpAddr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct FakeStandalone {
    running: Arc<AtomicBool>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl LocalStandaloneLifecycle for FakeStandalone {
    fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let starts = self.starts.clone();
        Box::pin(async move {
            starts.fetch_add(1, Ordering::SeqCst);
            running.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
    }
}

#[derive(Default)]
struct FakeDistributedCoordinator {
    running: Arc<AtomicBool>,
    start_hangs: Arc<AtomicBool>,
    route_ready: Arc<AtomicBool>,
    route_lost: Arc<AtomicBool>,
    route_changed: Arc<tokio::sync::Notify>,
    stops: Arc<AtomicUsize>,
}

impl FakeDistributedCoordinator {
    fn set_route_ready(&self, ready: bool) {
        self.route_ready.store(ready, Ordering::SeqCst);
        if ready {
            self.route_lost.store(false, Ordering::SeqCst);
        }
        self.route_changed.notify_waiters();
    }

    fn lose_route(&self) {
        self.route_ready.store(false, Ordering::SeqCst);
        self.route_lost.store(true, Ordering::SeqCst);
        self.route_changed.notify_waiters();
    }
}

impl DistributedCoordinatorLifecycle for FakeDistributedCoordinator {
    fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let hangs = self.start_hangs.clone();
        Box::pin(async move {
            running.store(true, Ordering::SeqCst);
            if hangs.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let ready = self.route_ready.clone();
        let changed = self.route_changed.clone();
        Box::pin(async move {
            while !ready.load(Ordering::SeqCst) {
                changed.notified().await;
            }
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let lost = self.route_lost.clone();
        let changed = self.route_changed.clone();
        Box::pin(async move {
            while !lost.load(Ordering::SeqCst) {
                changed.notified().await;
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct FakePeer {
    drains: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl CoordinatorPeerLifecycle for FakePeer {
    fn begin_drain(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let drains = self.drains.clone();
        Box::pin(async move {
            drains.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn stop_worker(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

const NOW: u64 = 10_000;

fn descriptor(role: ControlRole, node_id: &str) -> NodeDescriptor {
    NodeDescriptor {
        protocol_version: 1,
        node_id: node_id.into(),
        role,
        generation: 7,
        mode: ControlMode::SoloStandalone,
        deployment_id: Some("deployment-a".into()),
    }
}

fn authenticated() -> AuthenticatedPeer {
    AuthenticatedPeer::new_for_test("worker-node", IpAddr::from([10, 99, 0, 2]), NOW)
}

fn pair(request_id: &str) -> ControlMessage {
    ControlMessage {
        request_id: request_id.into(),
        generation: 7,
        deployment_id: None,
        command: ControlCommand::Pair {
            descriptor: descriptor(ControlRole::Worker, "worker-node"),
        },
    }
}

fn coordinator() -> CoordinatorControl {
    CoordinatorControl::new(
        descriptor(ControlRole::Coordinator, "coordinator-node"),
        Duration::from_secs(15),
        Duration::from_secs(5),
    )
    .unwrap()
}

fn ready_worker_control() -> CoordinatorControl {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-promotion"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    control.note_prepare_sent(7).unwrap();
    control
        .handle(
            ControlEndpoint::WorkerEvent,
            ControlMessage {
                request_id: "ready-promotion".into(),
                generation: 7,
                deployment_id: Some("deployment-a".into()),
                command: ControlCommand::WorkerEvent {
                    event: WorkerEventKind::Ready,
                },
            },
            &authenticated(),
            true,
            NOW + 5_000,
        )
        .unwrap();
    control
}

fn validated_hello() -> Ds4Hello {
    Ds4Hello {
        model_id: 1,
        quant_bits: 2,
        layer_start: 20,
        layer_end: 60,
        has_output: true,
        has_hidden: true,
        context_size: 262_144,
        layer_count: 61,
        listen_port: 9911,
        model_name: "deepseek-v4-flash-mxfp4".into(),
    }
}

fn proxy() -> Arc<ModeAwareProxyState> {
    Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse("http://127.0.0.1:8000").unwrap(),
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
    )
}

async fn awaiting_hello_cluster() -> (ClusterHandle, tokio::task::JoinHandle<()>) {
    let (cluster, task) = spawn_state_machine(ClusterSnapshot::booting(LocalRole::Coordinator), 16);
    let mut generation = 0;
    for kind in [
        ClusterEventKind::BeginSoloStandalone,
        ClusterEventKind::LocalStandaloneReady,
        ClusterEventKind::BeginPairing,
        ClusterEventKind::PairingReady,
        ClusterEventKind::BeginPromotion,
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
    assert_eq!(cluster.snapshot().state, ClusterState::AwaitingWorkerHello);
    (cluster, task)
}

async fn promotion_runtime(
    coordinator: Arc<FakeDistributedCoordinator>,
) -> (
    CoordinatorDistributedRuntime,
    Arc<ModeAwareProxyState>,
    Arc<FakeStandalone>,
    Arc<FakePeer>,
    tokio::task::JoinHandle<()>,
) {
    let (cluster, task) = awaiting_hello_cluster().await;
    let proxy = proxy();
    proxy.set_target(ProxyTarget::LocalStandalone, true);
    proxy.admission().start_serving();
    let standalone = Arc::new(FakeStandalone::default());
    standalone.running.store(true, Ordering::SeqCst);
    let peer = Arc::new(FakePeer::default());
    let runtime = CoordinatorDistributedRuntime::new(
        cluster,
        proxy.clone(),
        standalone.clone(),
        coordinator,
        peer.clone(),
        CoordinatorRuntimeTimeouts {
            drain: Duration::from_millis(200),
            startup: Duration::from_millis(30),
            complete_route: Duration::from_millis(200),
            route_loss_grace: Duration::from_millis(10),
        },
        PromotionRetryPolicy {
            backoff: Duration::from_millis(50),
            maximum_consecutive_failures: 3,
        },
    )
    .unwrap();
    (runtime, proxy, standalone, peer, task)
}

#[test]
fn propose_candidate_adopts_a_higher_worker_generation_and_keeps_local_otherwise() {
    // G-02 / design §4: the coordinator is the session authority and proposes a candidate that
    // is no lower than either side. A higher worker generation is adopted; a lower one is
    // ignored. The generation never goes backwards.
    let mut control = coordinator();
    assert_eq!(control.generation(), 7);

    let candidate = control.propose_candidate(102).unwrap();
    assert_eq!(candidate, 102);
    assert_eq!(
        control.generation(),
        102,
        "higher worker generation is adopted"
    );

    let candidate = control.propose_candidate(50).unwrap();
    assert_eq!(
        candidate, 102,
        "lower peer generation never lowers the session"
    );
    assert_eq!(control.generation(), 102);
}

#[test]
fn authenticated_scoped_pair_becomes_present_only_after_stability() {
    let mut control = coordinator();
    let response = control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    assert_eq!(response.status, ControlResponseStatus::Applied);
    assert!(!control.peer_present(NOW + 4_999));
    assert!(control.peer_present(NOW + 5_000));
    assert!(!control.peer_present(NOW + 15_000));
}

#[test]
fn duplicate_is_idempotent_and_renews_lease() {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    let duplicate = control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW + 4_000,
        )
        .unwrap();
    assert_eq!(duplicate.status, ControlResponseStatus::Duplicate);
    assert_eq!(duplicate.lease_expires_at_millis, Some(NOW + 19_000));
    assert!(control.peer_present(NOW + 18_999));
}

#[test]
fn authenticated_node_descriptor_poll_renews_an_active_lease() {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    let response = control
        .node_descriptor(&authenticated(), true, NOW + 5_000)
        .unwrap();
    assert_eq!(response.lease_expires_at_millis, Some(NOW + 20_000));
    assert!(control.peer_present(NOW + 19_999));
}

#[test]
fn stale_generation_and_changed_duplicate_are_conflicts() {
    let mut control = coordinator();
    let mut stale = pair("pair-old");
    stale.generation = 6;
    assert_eq!(
        control.handle(ControlEndpoint::Pair, stale, &authenticated(), true, NOW,),
        Err(ControlError::GenerationMismatch {
            expected: 7,
            received: 6,
        })
    );
    assert_eq!(
        ControlError::GenerationMismatch {
            expected: 7,
            received: 6,
        }
        .http_status(),
        409
    );

    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    let mut changed = pair("pair-1");
    if let ControlCommand::Pair { descriptor } = &mut changed.command {
        descriptor.mode = ControlMode::Transitioning;
    }
    assert_eq!(
        control.handle(
            ControlEndpoint::Pair,
            changed,
            &authenticated(),
            true,
            NOW + 1,
        ),
        Err(ControlError::IdempotencyConflict)
    );
}

#[test]
fn route_loss_or_lease_interruption_removes_peer_presence() {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    assert!(control.peer_present(NOW + 5_000));
    control.invalidate_route();
    assert!(!control.peer_present(NOW + 5_001));

    let mut unscoped = coordinator();
    assert_eq!(
        unscoped.handle(
            ControlEndpoint::Pair,
            pair("pair-2"),
            &authenticated(),
            false,
            NOW,
        ),
        Err(ControlError::RouteNotScoped)
    );
    assert!(!unscoped.peer_present(NOW + 5_000));
}

#[test]
fn deployment_mismatch_returns_precondition_failed() {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-1"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    let message = ControlMessage {
        request_id: "event-1".into(),
        generation: 7,
        deployment_id: Some("deployment-b".into()),
        command: ControlCommand::WorkerEvent {
            event: WorkerEventKind::Ready,
        },
    };
    let error = control
        .handle(
            ControlEndpoint::WorkerEvent,
            message,
            &authenticated(),
            true,
            NOW + 1,
        )
        .unwrap_err();
    assert_eq!(error, ControlError::DeploymentMismatch);
    assert_eq!(error.http_status(), 412);
}

#[test]
fn distributed_ack_sequence_rejects_reorder_duplicate_change_and_old_generation() {
    let mut control = coordinator();
    control
        .handle(
            ControlEndpoint::Pair,
            pair("pair-distributed"),
            &authenticated(),
            true,
            NOW,
        )
        .unwrap();
    let prepare = control.prepare_worker_message("prepare-1").unwrap();
    assert_eq!(prepare.generation, 7);
    assert_eq!(prepare.deployment_id.as_deref(), Some("deployment-a"));
    control.note_prepare_sent(7).unwrap();
    assert_eq!(
        control.rendezvous_snapshot(ClusterState::AwaitingWorkerHello, NOW + 5_000),
        RendezvousControlSnapshot {
            state: ClusterState::AwaitingWorkerHello,
            generation: 7,
            deployment_id: Some("deployment-a".into()),
            lease_valid: true,
        }
    );

    let ready = ControlMessage {
        request_id: "ready-1".into(),
        generation: 7,
        deployment_id: Some("deployment-a".into()),
        command: ControlCommand::WorkerEvent {
            event: WorkerEventKind::Ready,
        },
    };
    control
        .handle(
            ControlEndpoint::WorkerEvent,
            ready.clone(),
            &authenticated(),
            true,
            NOW + 1,
        )
        .unwrap();
    assert_eq!(control.phase(), DistributedControlPhase::WorkerReady);
    assert_eq!(
        control
            .handle(
                ControlEndpoint::WorkerEvent,
                ready,
                &authenticated(),
                true,
                NOW + 2,
            )
            .unwrap()
            .status,
        ControlResponseStatus::Duplicate
    );

    let drained = ControlMessage {
        request_id: "drained-1".into(),
        generation: 7,
        deployment_id: Some("deployment-a".into()),
        command: ControlCommand::Drained,
    };
    assert_eq!(
        control.handle(
            ControlEndpoint::Drained,
            drained.clone(),
            &authenticated(),
            true,
            NOW + 3,
        ),
        Err(ControlError::InvalidPhase {
            phase: DistributedControlPhase::WorkerReady
        })
    );
    assert_eq!(
        control.begin_drain_message("drain-1").unwrap().generation,
        7
    );
    control.note_begin_drain_sent(7).unwrap();
    control
        .handle(
            ControlEndpoint::Drained,
            drained.clone(),
            &authenticated(),
            true,
            NOW + 4,
        )
        .unwrap();
    assert_eq!(control.phase(), DistributedControlPhase::Drained);
    let demote = control.demote_message("demote-1").unwrap();
    assert_eq!(demote.generation, 7);
    control.note_demote_complete(demote.generation).unwrap();
    assert_eq!(control.phase(), DistributedControlPhase::Paired);

    control.advance_generation(8);
    assert_eq!(
        control.handle(
            ControlEndpoint::Drained,
            drained,
            &authenticated(),
            true,
            NOW + 5,
        ),
        Err(ControlError::GenerationMismatch {
            expected: 8,
            received: 7
        })
    );
}

#[tokio::test]
async fn promotion_waits_for_in_flight_stream_and_complete_route_before_serving() {
    let coordinator = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
    let permit = proxy.admission().try_acquire(true).unwrap();
    let control = ready_worker_control();
    let promotion = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .promote_after_hello(validated_hello(), &control, NOW + 5_000, Arc::new(|| true))
                .await
        }
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(standalone.stops.load(Ordering::SeqCst), 0);
    assert!(!coordinator.running.load(Ordering::SeqCst));
    drop(permit);
    tokio::time::timeout(Duration::from_millis(100), async {
        while !coordinator.running.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        runtime.cluster.snapshot().state,
        ClusterState::DistributedStarting
    );
    assert_eq!(
        proxy.target_snapshot(),
        ModeAwareTargetSnapshot {
            target: ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            ready: false,
        }
    );
    assert!(!promotion.is_finished());

    coordinator.set_route_ready(true);
    let ready = promotion.await.unwrap().unwrap();
    assert_eq!(ready.stable_mode, StableMode::DistributedMxfp4);
    assert_eq!(ready.state, ClusterState::DistributedReady);
    assert_eq!(peer.drains.load(Ordering::SeqCst), 1);
    assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
    assert!(proxy.target_snapshot().ready);
    task.abort();
}

#[tokio::test]
async fn hello_without_ready_worker_control_never_starts_promotion() {
    let child = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, _, _, task) = promotion_runtime(child.clone()).await;
    let control = coordinator();

    assert!(matches!(
        runtime
            .promote_after_hello(validated_hello(), &control, NOW, Arc::new(|| true))
            .await,
        Err(CoordinatorLifecycleError::PrerequisiteMissing)
    ));
    assert_eq!(
        runtime.cluster.snapshot().state,
        ClusterState::AwaitingWorkerHello
    );
    assert!(!child.running.load(Ordering::SeqCst));
    assert!(proxy.target_snapshot().ready);
    task.abort();
}

#[tokio::test]
async fn coordinator_startup_timeout_reaps_children_and_enters_serving_backoff() {
    let coordinator = Arc::new(FakeDistributedCoordinator::default());
    coordinator.start_hangs.store(true, Ordering::SeqCst);
    let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
    let control = ready_worker_control();

    assert!(matches!(
        runtime
            .promote_after_hello(validated_hello(), &control, NOW + 5_000, Arc::new(|| true),)
            .await,
        Err(CoordinatorLifecycleError::StartupTimeout)
    ));
    assert!(!coordinator.running.load(Ordering::SeqCst));
    assert!(standalone.running.load(Ordering::SeqCst));
    assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cluster.snapshot().state, ClusterState::Backoff);
    assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
    assert!(proxy.target_snapshot().ready);
    task.abort();
}

#[tokio::test]
async fn lease_loss_after_child_start_rejects_route_and_recovers_solo() {
    let coordinator = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
    let control = ready_worker_control();
    let valid = Arc::new(AtomicBool::new(true));
    let status = valid.clone();
    let promotion = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .promote_after_hello(
                    validated_hello(),
                    &control,
                    NOW + 5_000,
                    Arc::new(move || status.load(Ordering::SeqCst)),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_millis(100), async {
        while !coordinator.running.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    valid.store(false, Ordering::SeqCst);
    coordinator.set_route_ready(true);

    assert!(matches!(
        promotion.await.unwrap(),
        Err(CoordinatorLifecycleError::LeaseLost)
    ));
    assert_eq!(
        runtime.cluster.snapshot().state,
        ClusterState::SoloStandaloneReady
    );
    assert!(!coordinator.running.load(Ordering::SeqCst));
    assert!(standalone.running.load(Ordering::SeqCst));
    assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
    assert!(proxy.target_snapshot().ready);
    task.abort();
}

#[tokio::test]
async fn incomplete_route_never_serves_and_route_loss_demotes_after_drain() {
    let coordinator = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, standalone, peer, task) = promotion_runtime(coordinator.clone()).await;
    let control = ready_worker_control();
    let promotion = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .promote_after_hello(validated_hello(), &control, NOW + 5_000, Arc::new(|| true))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert!(!promotion.is_finished());
    assert!(!proxy.target_snapshot().ready);
    coordinator.set_route_ready(true);
    promotion.await.unwrap().unwrap();

    let permit = proxy.admission().try_acquire(true).unwrap();
    let demotion = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.wait_route_loss_and_demote().await }
    });
    coordinator.lose_route();
    tokio::time::sleep(Duration::from_millis(2)).await;
    coordinator.set_route_ready(true);
    tokio::time::sleep(Duration::from_millis(12)).await;
    assert!(!demotion.is_finished());

    coordinator.lose_route();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!demotion.is_finished());
    assert_eq!(standalone.starts.load(Ordering::SeqCst), 0);
    drop(permit);

    let paired = demotion.await.unwrap().unwrap();
    assert_eq!(paired.stable_mode, StableMode::PairedStandalone);
    assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
    assert_eq!(peer.drains.load(Ordering::SeqCst), 2);
    assert_eq!(peer.stops.load(Ordering::SeqCst), 1);
    assert!(!coordinator.running.load(Ordering::SeqCst));
    assert!(standalone.running.load(Ordering::SeqCst));
    assert!(proxy.target_snapshot().ready);
    task.abort();
}

#[tokio::test]
async fn manual_demotion_wins_route_loss_monitor_race() {
    let coordinator = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, _, peer, task) = promotion_runtime(coordinator.clone()).await;
    coordinator.set_route_ready(true);
    runtime
        .promote_after_hello(
            validated_hello(),
            &ready_worker_control(),
            NOW + 5_000,
            Arc::new(|| true),
        )
        .await
        .unwrap();

    let permit = proxy.admission().try_acquire(true).unwrap();
    coordinator.lose_route();
    let route_monitor = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.wait_route_loss_and_demote().await }
    });
    tokio::time::sleep(Duration::from_millis(2)).await;
    let manual = tokio::spawn({
        let runtime = runtime.clone();
        async move { runtime.demote().await }
    });
    tokio::time::sleep(Duration::from_millis(12)).await;

    let monitored = route_monitor.await.unwrap().unwrap();
    assert_eq!(monitored.state, ClusterState::Demoting);
    assert_eq!(peer.drains.load(Ordering::SeqCst), 2);

    drop(permit);
    let paired = manual.await.unwrap().unwrap();
    assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
    assert_eq!(peer.drains.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn third_same_promotion_failure_stops_auto_retry_but_keeps_serving() {
    let child = Arc::new(FakeDistributedCoordinator::default());
    let (runtime, proxy, _, _, task) = promotion_runtime(child).await;

    for attempt in 1_u64..=3 {
        let now = (attempt - 1) * 50;
        let failed = runtime
            .record_promotion_failure(ClusterFailure::CoordinatorStartupTimeout, now)
            .await
            .unwrap();
        if attempt < 3 {
            assert_eq!(failed.state, ClusterState::Backoff);
            assert_eq!(runtime.reconcile_backoff(now + 49).await.unwrap(), failed);
            let paired = runtime.reconcile_backoff(now + 50).await.unwrap();
            assert_eq!(paired.state, ClusterState::PairedStandaloneReady);
            runtime
                .cluster
                .apply(ClusterEvent {
                    expected_generation: paired.generation,
                    kind: ClusterEventKind::BeginPromotion,
                })
                .await
                .unwrap();
        } else {
            assert_eq!(failed.state, ClusterState::ManualInterventionRequired);
        }
    }

    let status = runtime.promotion_failure_status().await;
    assert_eq!(status.consecutive, 3);
    assert!(status.manual);
    assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
    assert!(proxy.target_snapshot().ready);
    assert_eq!(
        runtime.reconcile_backoff(u64::MAX).await.unwrap().state,
        ClusterState::ManualInterventionRequired
    );
    assert_eq!(
        runtime.operator_reconcile().await.unwrap().state,
        ClusterState::PairedStandaloneReady
    );
    assert_eq!(runtime.promotion_failure_status().await.consecutive, 0);
    task.abort();
}
