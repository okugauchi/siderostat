#![cfg(feature = "test-support")]

//! M1: 2-node dry-run integration test.
//!
//! Drives the full clustering path (pair -> promote -> demote) over real control HTTP using the
//! actual dry-run lifecycles (`ProductionClusterRuntime::new_dry_run`): the worker sends a
//! simulated DS4 HELLO to the coordinator's rendezvous listener and the coordinator derives
//! route readiness from the control plane. No real ds4-server process is spawned, stopped, or
//! restarted, and no persistent state is read or written.

mod support;

use siderostat::{
    cluster::{
        Ds4Command, Ds4Profile, ModeRuntime, ProductionClusterRuntime, StandaloneSupervisor,
    },
    config::{ModeAwareConfig, Quantization, Residency, SpeculativeSupport},
    metrics::Metrics,
    target::{ClusterState, LocalRole},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use support::{free_loopback_port, manifest, proxy_state, temporary_path, test_config, wait_until};

/// A dry-run node: its own control HTTP serve task and a `ProductionClusterRuntime` built with
/// the dry-run lifecycles. `config.cluster.control_port` is the *peer's* control port (matching
/// `new_dry_run`, which reads the peer endpoint from the config), so the two nodes on one host
/// must be given each other's distinct control ports.
struct DryRunNode {
    _config: ModeAwareConfig,
    mode: Arc<ModeRuntime>,
    production: Arc<ProductionClusterRuntime>,
    serve: tokio::task::JoinHandle<()>,
}

fn dry_run_standalone() -> StandaloneSupervisor {
    let command = Ds4Command {
        executable: "/bin/true".into(),
        working_directory: "/tmp".into(),
        argv: Vec::new(),
        profile: Ds4Profile {
            profile_id: "dry-run".into(),
            quantization: Quantization::Mxfp4,
            residency: Residency::Resident,
            speculative_support: SpeculativeSupport::None,
        },
    };
    StandaloneSupervisor::new_dry_run(
        command,
        url::Url::parse("http://127.0.0.1:0/v1/models").unwrap(),
        Duration::from_secs(1),
        Duration::from_millis(10),
        Duration::from_secs(1),
        false,
        Arc::new(Metrics::default()),
    )
}

async fn build_node(
    role: LocalRole,
    node_id: &str,
    peer_control_port: u16,
    ds4_distributed_port: u16,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<DryRunNode> {
    let state_path = temporary_path("dryrun-state");
    let cache = temporary_path("dryrun-manifests");
    std::fs::create_dir_all(&state_path)?;
    std::fs::create_dir_all(&cache)?;
    // control_port in the config is the peer's port (new_dry_run uses it as the control client's
    // peer endpoint); peer_ingress_port is unused by the dry-run runtime.
    let config = test_config(
        node_id,
        "127.0.0.1",
        "127.0.0.1",
        peer_control_port,
        ds4_distributed_port,
        free_loopback_port().await?,
        state_path,
        cache,
    );
    let proxy = proxy_state()?;
    let standalone = Arc::new(dry_run_standalone());
    let mode = Arc::new(
        ModeRuntime::spawn_ready_at(
            role,
            proxy.clone(),
            standalone.clone(),
            Duration::from_secs(1),
            0,
        )
        .await?,
    );
    let production = Arc::new(ProductionClusterRuntime::new_dry_run(
        config.clone(),
        role,
        mode.clone(),
        proxy.clone(),
        standalone,
        Arc::new(Metrics::default()),
        manifest(),
        vec![0x42; 32],
        peer_control_port,
        None,
    )?);
    let listener_std = listener.into_std()?;
    let serve_listener = tokio::net::TcpListener::from_std(listener_std.try_clone()?)?;
    let app = production
        .router()
        .into_make_service_with_connect_info::<SocketAddr>();
    let serve = tokio::spawn(async move {
        axum::serve(serve_listener, app).await.unwrap();
    });
    Ok(DryRunNode {
        _config: config,
        mode,
        production,
        serve,
    })
}

async fn wait_node(node: &DryRunNode, state: ClusterState, timeout: Duration) -> bool {
    wait_until(
        timeout,
        || async move { node.mode.snapshot().state == state },
    )
    .await
}

#[tokio::test]
async fn two_node_dry_run_pairs_promotes_and_demotes_without_real_processes() {
    let coordinator_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coordinator_port = coordinator_listener.local_addr().unwrap().port();
    let worker_port = worker_listener.local_addr().unwrap().port();
    let ds4_distributed_port = free_loopback_port().await.unwrap();

    // Each node's config.control_port points at the peer's control port, so the two dry-run
    // runtimes on one host talk to each other over real control HTTP.
    let coordinator = build_node(
        LocalRole::Coordinator,
        "dryrun-coordinator",
        worker_port,
        ds4_distributed_port,
        coordinator_listener,
    )
    .await
    .unwrap();
    let worker = build_node(
        LocalRole::Worker,
        "dryrun-worker",
        coordinator_port,
        ds4_distributed_port,
        worker_listener,
    )
    .await
    .unwrap();

    // Both boot as solo. No real child is ever spawned, so the standalone child reports no pid
    // and is not running.
    assert!(
        wait_node(
            &coordinator,
            ClusterState::SoloStandaloneReady,
            Duration::from_secs(5)
        )
        .await
    );
    assert!(
        wait_node(
            &worker,
            ClusterState::SoloStandaloneReady,
            Duration::from_secs(5)
        )
        .await
    );
    // The dry-run standalone is marked running (simulated lifecycle) but never owns a real
    // child: no pid is reported, so no ds4-server process was spawned.
    let coordinator_boot = coordinator.production.diagnostics().await;
    let standalone = coordinator_boot.children.standalone.as_ref().unwrap();
    assert!(standalone.running);
    assert!(standalone.pid.is_none());
    assert_eq!(standalone.ready, Some(true));

    // Pair over real control HTTP.
    coordinator.production.pair().await.unwrap();
    assert!(
        wait_node(
            &coordinator,
            ClusterState::PairedStandaloneReady,
            Duration::from_secs(5)
        )
        .await
    );
    assert!(
        wait_node(
            &worker,
            ClusterState::PairedStandaloneReady,
            Duration::from_secs(5)
        )
        .await
    );

    // Promote. The dry-run worker sends a simulated HELLO to the coordinator's rendezvous
    // listener and the coordinator derives route readiness from the control plane, so this
    // converges on DistributedReady without a real DS4 process.
    coordinator.production.promote().await.unwrap();
    assert!(
        wait_node(
            &coordinator,
            ClusterState::DistributedReady,
            Duration::from_secs(8)
        )
        .await
    );
    assert!(
        wait_node(
            &worker,
            ClusterState::DistributedReady,
            Duration::from_secs(8)
        )
        .await
    );

    // Simulated distributed children are reported as running (the lifecycle flags flipped) but
    // never carry a real pid: no ds4-server process was spawned on either side.
    let coordinator_ready = coordinator.production.diagnostics().await;
    let coordinator_child = coordinator_ready
        .children
        .distributed_coordinator
        .as_ref()
        .unwrap();
    assert!(coordinator_child.running);
    assert!(coordinator_child.pid.is_none());
    let worker_ready = worker.production.diagnostics().await;
    let worker_child = worker_ready.children.distributed_worker.as_ref().unwrap();
    assert!(worker_child.running);
    assert!(worker_child.pid.is_none());

    // Demote back to paired standalone.
    coordinator.production.demote().await.unwrap();
    assert!(
        wait_node(
            &coordinator,
            ClusterState::PairedStandaloneReady,
            Duration::from_secs(8)
        )
        .await
    );
    assert!(
        wait_node(
            &worker,
            ClusterState::PairedStandaloneReady,
            Duration::from_secs(8)
        )
        .await
    );
    let coordinator_demoted = coordinator.production.diagnostics().await;
    let demoted_child = coordinator_demoted
        .children
        .distributed_coordinator
        .as_ref()
        .unwrap();
    assert!(!demoted_child.running);

    // Cleanup: stop serve tasks. Nothing was ever spawned, so no orphan child is possible.
    coordinator.serve.abort();
    worker.serve.abort();
}

#[tokio::test]
async fn dry_run_node_boots_without_a_real_child() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = build_node(
        LocalRole::Coordinator,
        "dryrun-boot",
        0,
        free_loopback_port().await.unwrap(),
        listener,
    )
    .await
    .unwrap();
    assert!(
        wait_node(
            &node,
            ClusterState::SoloStandaloneReady,
            Duration::from_secs(5)
        )
        .await
    );
    let diagnostics = node.production.diagnostics().await;
    let standalone = diagnostics.children.standalone.as_ref().unwrap();
    assert!(standalone.running);
    assert!(standalone.pid.is_none());
    assert!(diagnostics.children.distributed_worker.is_none());
    let coordinator_child = diagnostics
        .children
        .distributed_coordinator
        .as_ref()
        .unwrap();
    assert!(!coordinator_child.running);
    assert!(coordinator_child.pid.is_none());
    node.serve.abort();
}
