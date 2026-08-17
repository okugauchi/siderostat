#![cfg(all(feature = "test-support", target_os = "macos"))]

use siderostat::{
    cluster::{
        ControlAuthenticator, ControlCommand, ControlEndpoint, ControlMessage, ControlMode,
        ControlRole, ControlSecret, Ds4Command, Ds4Profile, EventOwner, LocalStandaloneLifecycle,
        ModeRuntime, NodeDescriptor, StandaloneSupervisor, WorkerControl,
    },
    config::{ModelVariant, Residency},
    metrics::Metrics,
    proxy::{ModeAwareProxyOptions, ModeAwareProxyState},
    target::{LocalRole, StableMode},
};
use std::{ffi::OsString, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

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

fn authenticated() -> siderostat::cluster::AuthenticatedPeer {
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
            "phase3-nonce-0001",
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
async fn real_fake_child_starts_stops_falls_back_and_recovers_after_crash() {
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let command = Ds4Command {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-ds4")),
        working_directory: std::env::temp_dir(),
        argv: vec![
            OsString::from("--listen"),
            OsString::from(address.to_string()),
            OsString::from("--exit-after-ms"),
            OsString::from("500"),
            OsString::from("--emit-dspark-activation"),
        ],
        profile: Ds4Profile {
            profile_id: "phase3-fake".into(),
            model_variant: ModelVariant::Q2,
            residency: Residency::Resident,
            dspark_required: true,
        },
    };
    let models_url = url::Url::parse(&format!("http://{address}/v1/models")).unwrap();
    let supervisor = Arc::new(StandaloneSupervisor::new(
        command,
        models_url.clone(),
        Duration::from_secs(2),
        Duration::from_millis(20),
        Duration::from_secs(1),
        false,
        Arc::new(Metrics::default()),
    ));
    let proxy = Arc::new(
        ModeAwareProxyState::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            url::Url::parse("http://127.0.0.1:9").unwrap(),
            ModeAwareProxyOptions {
                max_in_flight: 4,
                request_body_limit_bytes: 4096,
                response_header_timeout: Duration::from_secs(1),
                first_body_byte_timeout: Duration::from_secs(1),
                stream_idle_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_millis(100),
            },
        )
        .unwrap(),
    );
    let runtime = ModeRuntime::spawn_ready(
        LocalRole::Worker,
        proxy,
        supervisor.clone(),
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    let first_pid = supervisor.child_identity().await.unwrap().pid;
    assert!(
        reqwest::get(models_url.clone())
            .await
            .unwrap()
            .status()
            .is_success()
    );

    let mut control = WorkerControl::new(
        descriptor(ControlRole::Worker, "worker"),
        Duration::from_millis(100),
        Duration::ZERO,
    )
    .unwrap();
    control
        .handle(
            ControlEndpoint::Pair,
            ControlMessage {
                request_id: "pair-phase3".into(),
                generation: 2,
                deployment_id: None,
                command: ControlCommand::Pair {
                    descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
                },
            },
            &authenticated(),
            true,
            1_000,
        )
        .unwrap();
    assert_eq!(
        runtime
            .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, 1_000)
            .await
            .unwrap()
            .stable_mode,
        StableMode::PairedStandalone
    );
    assert!(supervisor.child_identity().await.is_none());

    assert_eq!(
        runtime
            .reconcile_peer(EventOwner::PeriodicReconcile, &mut control, 1_100)
            .await
            .unwrap()
            .stable_mode,
        StableMode::SoloStandalone
    );
    let second_pid = supervisor.child_identity().await.unwrap().pid;
    assert_ne!(first_pid, second_pid);
    tokio::time::sleep(Duration::from_millis(550)).await;
    runtime.reconcile_local().await.unwrap();
    let third_pid = supervisor.child_identity().await.unwrap().pid;
    assert_ne!(second_pid, third_pid);
    assert!(
        reqwest::get(models_url)
            .await
            .unwrap()
            .status()
            .is_success()
    );
}

#[tokio::test]
async fn dspark_profile_without_activation_event_fails_readiness_and_reaps_child() {
    let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let command = Ds4Command {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_fake-ds4")),
        working_directory: std::env::temp_dir(),
        argv: vec![
            OsString::from("--listen"),
            OsString::from(address.to_string()),
        ],
        profile: Ds4Profile {
            profile_id: "phase3-dspark-missing-activation".into(),
            model_variant: ModelVariant::Q2Q4,
            residency: Residency::Resident,
            dspark_required: true,
        },
    };
    let supervisor = StandaloneSupervisor::new(
        command,
        url::Url::parse(&format!("http://{address}/v1/models")).unwrap(),
        Duration::from_secs(1),
        Duration::from_millis(20),
        Duration::from_secs(1),
        false,
        Arc::new(Metrics::default()),
    );
    let error = supervisor.start(1).await.unwrap_err();
    assert!(format!("{error:#}").contains("DSpark activation"));
    assert!(supervisor.child_identity().await.is_none());
}
