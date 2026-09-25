#![cfg(feature = "test-support")]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use sha2::{Digest, Sha256};
use siderostat::{
    cluster::production::{
        ProductionControlClient,
        manager::{ManagerPeerPhase, ManagerPeerRequest},
    },
    manager::{
        ArtifactDraft, ArtifactKind, ArtifactProvenance, BuildRecord, HardwareReadiness,
        ManagerReleaseStore, ManagerRoot, ProfileCompatibility, ReleaseIdentity, SourceRecord,
        StageRuntimeConfig, StagedProfileRecord,
    },
    target::{ClusterState, LocalRole},
};

#[path = "support/mod.rs"]
mod support;

static NODE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct WorkerReleaseFixture {
    root: PathBuf,
    store: Arc<Mutex<ManagerReleaseStore>>,
    profile_id: String,
    candidate_digest: String,
    previous_digest: String,
}

impl Drop for WorkerReleaseFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn worker_release_fixture(config: &siderostat::config::ModeAwareConfig) -> WorkerReleaseFixture {
    let root = std::env::temp_dir().join(format!(
        "siderostat-manager-peer-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&root).expect("create worker manager fixture");
    let mut release = ManagerReleaseStore::open(
        ManagerRoot::explicit(root.clone()),
        config.cluster.node_id.clone(),
    )
    .expect("open worker manager store");

    let source_commit = "a".repeat(40);
    let receipt_id = release
        .record_source(SourceRecord {
            remote: "https://github.com/antirez/ds4.git".into(),
            full_commit: source_commit.clone(),
            main_proof: source_commit.clone(),
            fetched_at: 1,
        })
        .expect("record source receipt");

    let executable = b"fixture ds4 executable";
    let executable_path = root.join("build-input.bin");
    fs::write(&executable_path, executable).expect("write build input");
    let executable_digest = sha256(executable);
    let build_id = release
        .publish_artifact(
            &executable_path,
            ArtifactDraft {
                kind: ArtifactKind::Build,
                expected_sha256: executable_digest.clone(),
                expected_size: executable.len() as u64,
                provenance: ArtifactProvenance::Build {
                    source_receipt_id: receipt_id,
                    record: BuildRecord {
                        source: source_commit,
                        flags: "fixture".into(),
                        toolchain: "fixture".into(),
                        arch: std::env::consts::ARCH.into(),
                        role: "ds4-server".into(),
                        target: "ds4-server".into(),
                        digest: executable_digest.clone(),
                        help_digest: "b".repeat(64),
                    },
                },
            },
        )
        .expect("publish build artifact")
        .id;

    let model = b"fixture model";
    let model_path = root.join("model-input.bin");
    fs::write(&model_path, model).expect("write model input");
    let model_digest = sha256(model);
    let model_id = release
        .publish_artifact(
            &model_path,
            ArtifactDraft {
                kind: ArtifactKind::Model,
                expected_sha256: model_digest.clone(),
                expected_size: model.len() as u64,
                provenance: ArtifactProvenance::Model {
                    catalog_id: "fixture-model-v1".into(),
                },
            },
        )
        .expect("publish model artifact")
        .id;

    let profile_id = "worker-profile-01".to_owned();
    let config_fingerprint = StageRuntimeConfig::from_validated_config(config).config_fingerprint;
    release
        .record_profile(StagedProfileRecord {
            profile_id: profile_id.clone(),
            node_role: "ds4-server".into(),
            role_artifact_ids: vec![build_id],
            model_artifact_id: model_id,
            model_catalog_id: "fixture-model-v1".into(),
            config_fingerprint,
            compatibility: ProfileCompatibility::Compatible,
            hardware_readiness: HardwareReadiness::Ready,
        })
        .expect("record staged profile");

    let previous = ReleaseIdentity::ExternalBaseline {
        config_fingerprint: "c".repeat(64),
        executable_sha256: "d".repeat(64),
        model_sha256: "e".repeat(64),
    };
    release
        .set_release_pointers(previous.clone(), None)
        .expect("set external baseline");
    let candidate_digest =
        sha256(format!("manager-release-v1\n{executable_digest}\n{model_digest}").as_bytes());
    let previous_digest = sha256(serde_json::to_string(&previous).unwrap().as_bytes());

    WorkerReleaseFixture {
        root,
        store: Arc::new(Mutex::new(release)),
        profile_id,
        candidate_digest,
        previous_digest,
    }
}

fn request(
    phase: ManagerPeerPhase,
    operation_id: &str,
    profile_id: &str,
    candidate_digest: &str,
    previous_digest: &str,
    generation: u64,
    ack_id: Option<String>,
) -> ManagerPeerRequest {
    ManagerPeerRequest {
        operation_id: operation_id.into(),
        profile_id: profile_id.into(),
        candidate_digest: candidate_digest.into(),
        previous_digest: Some(previous_digest.into()),
        expected_generation: generation,
        policy_epoch: 0,
        phase,
        ack_id,
    }
}

fn peer_client(
    nodes: &support::TwoNode,
    peer_role: LocalRole,
    local_node_id: &str,
) -> ProductionControlClient {
    let (local_address, peer_address, peer_port) = match peer_role {
        LocalRole::Coordinator => (
            nodes.coordinator.config.cluster.coordinator_address,
            nodes.coordinator.config.cluster.coordinator_address,
            nodes.coordinator.config.cluster.control_port,
        ),
        LocalRole::Worker => (
            nodes.coordinator.config.cluster.coordinator_address,
            nodes.worker.config.cluster.worker_address,
            nodes.worker.config.cluster.control_port,
        ),
        LocalRole::Unknown => panic!("unknown role"),
    };
    ProductionControlClient::new(
        local_node_id.into(),
        local_address,
        peer_address,
        peer_port,
        vec![0x42; 32],
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
    .expect("build peer client")
}

#[tokio::test]
async fn authenticated_worker_prepare_is_strict_durable_and_idempotent() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    assert_eq!(
        nodes.worker.mode.snapshot().state,
        ClusterState::PairedStandaloneReady
    );
    let standalone_stops_before_peer_requests = nodes.worker.standalone.child().stops();
    let fixture = worker_release_fixture(&nodes.worker.config);
    nodes
        .worker
        .production
        .attach_manager_store(fixture.store.clone());
    let client = peer_client(&nodes, LocalRole::Worker, "reconnect-coordinator");
    let generation = nodes.worker.mode.snapshot().generation;
    let base = request(
        ManagerPeerPhase::Prepare,
        "manager-peer-op-01",
        &fixture.profile_id,
        &fixture.candidate_digest,
        &fixture.previous_digest,
        generation,
        None,
    );

    let status = client
        .manager_peer(&request(
            ManagerPeerPhase::Status,
            "manager-peer-op-01",
            &fixture.profile_id,
            &fixture.candidate_digest,
            &fixture.previous_digest,
            generation,
            None,
        ))
        .await
        .expect("peer status preflight");
    assert_eq!(status.protocol_version, 1);
    assert_eq!(status.node_id, "reconnect-worker");

    let stale = request(
        ManagerPeerPhase::Prepare,
        "manager-peer-stale-01",
        &fixture.profile_id,
        &fixture.candidate_digest,
        &fixture.previous_digest,
        generation + 1,
        None,
    );
    assert!(client.manager_peer(&stale).await.is_err());
    assert!(
        fixture
            .store
            .lock()
            .expect("worker store lock")
            .snapshot()
            .activation_journals
            .is_empty()
    );
    assert_eq!(
        nodes.worker.standalone.child().stops(),
        standalone_stops_before_peer_requests
    );

    let bad_digest = request(
        ManagerPeerPhase::Prepare,
        "manager-peer-bad-digest-01",
        &fixture.profile_id,
        &"f".repeat(64),
        &fixture.previous_digest,
        generation,
        None,
    );
    assert!(client.manager_peer(&bad_digest).await.is_err());
    assert!(
        fixture
            .store
            .lock()
            .expect("worker store lock")
            .snapshot()
            .activation_journals
            .is_empty()
    );

    let first = client.manager_peer(&base).await.expect("prepare worker");
    let duplicate = client
        .manager_peer(&base)
        .await
        .expect("idempotent duplicate prepare");
    assert_eq!(first.ack_id, duplicate.ack_id);
    assert_eq!(
        fixture
            .store
            .lock()
            .expect("worker store lock")
            .snapshot()
            .activation_journals["manager-peer-op-01"]
            .participants["reconnect-worker"]
            .prepare_ack
            .as_deref(),
        Some(first.ack_id.as_str())
    );

    let mut changed = base.clone();
    changed.candidate_digest = "9".repeat(64);
    assert!(client.manager_peer(&changed).await.is_err());
    assert_eq!(
        nodes.worker.standalone.child().stops(),
        standalone_stops_before_peer_requests
    );
    nodes.shutdown().await;
    assert!(Path::new(&fixture.root).exists());
}

#[tokio::test]
async fn peer_route_rejects_unauthenticated_wrong_node_and_wrong_role_calls() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let fixture = worker_release_fixture(&nodes.worker.config);
    nodes
        .worker
        .production
        .attach_manager_store(fixture.store.clone());
    let generation = nodes.worker.mode.snapshot().generation;
    let request = request(
        ManagerPeerPhase::Status,
        "manager-peer-auth-01",
        &fixture.profile_id,
        &fixture.candidate_digest,
        &fixture.previous_digest,
        generation,
        None,
    );

    let unauthenticated = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{}/v1/manager/status",
            nodes.worker.config.cluster.control_port
        ))
        .json(&request)
        .send()
        .await
        .expect("send unauthenticated call");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let wrong_node = peer_client(&nodes, LocalRole::Worker, "unknown-node")
        .manager_peer(&request)
        .await;
    assert!(wrong_node.is_err());

    let worker_client = ProductionControlClient::new(
        "reconnect-worker".into(),
        nodes.worker.config.cluster.worker_address,
        nodes.coordinator.config.cluster.coordinator_address,
        nodes.coordinator.config.cluster.control_port,
        vec![0x42; 32],
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
    .expect("build worker-to-coordinator client");
    assert!(worker_client.manager_peer(&request).await.is_err());
    assert!(
        fixture
            .store
            .lock()
            .expect("worker store lock")
            .snapshot()
            .activation_journals
            .is_empty()
    );
    nodes.shutdown().await;
}
