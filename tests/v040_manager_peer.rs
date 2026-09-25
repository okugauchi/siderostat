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
        manager::{
            ManagerActivationRequest, ManagerPeerPhase, ManagerPeerRequest, ManagerRuntimeOperation,
        },
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
    manager_release_fixture(config, "worker-profile-01", "worker", "ds4")
}

fn manager_release_fixture(
    config: &siderostat::config::ModeAwareConfig,
    profile_id: &str,
    node_role: &str,
    build_role: &str,
) -> WorkerReleaseFixture {
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

    let executable = format!("fixture {build_role} executable").into_bytes();
    let executable_path = root.join("build-input.bin");
    fs::write(&executable_path, &executable).expect("write build input");
    let executable_digest = sha256(&executable);
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
                        role: build_role.into(),
                        target: build_role.into(),
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

    let profile_id = profile_id.to_owned();
    let config_fingerprint = StageRuntimeConfig::from_validated_config(config).config_fingerprint;
    release
        .record_profile(StagedProfileRecord {
            profile_id: profile_id.clone(),
            node_role: node_role.into(),
            role_artifact_ids: vec![build_id],
            model_artifact_id: model_id,
            model_catalog_id: "fixture-model-v1".into(),
            config_fingerprint,
            compatibility: ProfileCompatibility::Compatible,
            hardware_readiness: HardwareReadiness::Ready,
        })
        .expect("record staged profile");

    let previous = ReleaseIdentity::ExternalBaseline {
        config_fingerprint: StageRuntimeConfig::from_validated_config(config).config_fingerprint,
        executable_sha256: sha256(
            &fs::read(&config.ds4.binary).expect("read fixture external executable"),
        ),
        model_sha256: sha256(
            &fs::read(&config.ds4.standalone.model).expect("read fixture external model"),
        ),
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

fn add_managed_previous_profile(
    fixture: &WorkerReleaseFixture,
    config: &siderostat::config::ModeAwareConfig,
    profile_id: &str,
    node_role: &str,
    build_role: &str,
) {
    let mut store = fixture.store.lock().expect("fixture store lock");
    let source_commit = "b".repeat(40);
    let source_receipt_id = store
        .record_source(SourceRecord {
            remote: "https://github.com/antirez/ds4.git".into(),
            full_commit: source_commit.clone(),
            main_proof: source_commit.clone(),
            fetched_at: 2,
        })
        .expect("record previous source receipt");
    let current_profile = store
        .snapshot()
        .profiles
        .get(&fixture.profile_id)
        .expect("current staged profile")
        .clone();
    let executable = format!("fixture previous {build_role} executable").into_bytes();
    let executable_path = fixture.root.join(format!("{profile_id}-build-input.bin"));
    fs::write(&executable_path, &executable).expect("write previous build input");
    let executable_digest = sha256(&executable);
    let build_id = store
        .publish_artifact(
            &executable_path,
            ArtifactDraft {
                kind: ArtifactKind::Build,
                expected_sha256: executable_digest.clone(),
                expected_size: executable.len() as u64,
                provenance: ArtifactProvenance::Build {
                    source_receipt_id,
                    record: BuildRecord {
                        source: source_commit,
                        flags: "fixture-previous".into(),
                        toolchain: "fixture".into(),
                        arch: std::env::consts::ARCH.into(),
                        role: build_role.into(),
                        target: build_role.into(),
                        digest: executable_digest,
                        help_digest: "c".repeat(64),
                    },
                },
            },
        )
        .expect("publish previous build")
        .id;
    store
        .record_profile(StagedProfileRecord {
            profile_id: profile_id.into(),
            node_role: node_role.into(),
            role_artifact_ids: vec![build_id],
            model_artifact_id: current_profile.model_artifact_id,
            model_catalog_id: current_profile.model_catalog_id,
            config_fingerprint: StageRuntimeConfig::from_validated_config(config)
                .config_fingerprint,
            compatibility: ProfileCompatibility::Compatible,
            hardware_readiness: HardwareReadiness::Ready,
        })
        .expect("record previous staged profile");
    store
        .set_release_pointers(ReleaseIdentity::ManagedProfile(profile_id.into()), None)
        .expect("make previous managed release active");
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
        source_commit: "a".repeat(40),
        model_digest: sha256(b"fixture model"),
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
    assert_eq!(status.protocol_version, 2);
    assert_eq!(status.node_id, "reconnect-worker");
    let remote_profile = status
        .profiles
        .iter()
        .find(|profile| profile.profile_id == fixture.profile_id)
        .expect("status returns the worker-local staged profile summary");
    assert_eq!(remote_profile.candidate_digest, fixture.candidate_digest);
    assert_eq!(remote_profile.model_digest, sha256(b"fixture model"));
    assert_eq!(remote_profile.source_commit, "a".repeat(40));

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
            "http://127.0.0.1:{}/v2/manager/status",
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

#[tokio::test]
async fn coordinator_activation_waits_for_both_durable_commits_and_peer_finalize() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    assert_eq!(
        nodes.coordinator.mode.snapshot().generation,
        nodes.worker.mode.snapshot().generation
    );

    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-01",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture =
        manager_release_fixture(&nodes.worker.config, "worker-profile-01", "worker", "ds4");
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let coordinator_starts = nodes.coordinator.standalone.child().starts();
    let worker_starts = nodes.worker.standalone.child().starts();
    let generation = nodes.coordinator.mode.snapshot().generation;
    let outcome = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-activate-01".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    let outcome = outcome.unwrap_or_else(|error| {
        let coordinator_journal = coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .get("manager-cluster-activate-01")
            .cloned();
        let worker_journal = worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .get("manager-cluster-activate-01")
            .cloned();
        panic!("{error:?}; coordinator={coordinator_journal:?}; worker={worker_journal:?}");
    });
    assert_eq!(outcome["phase"], "complete");
    assert_eq!(
        nodes.coordinator.standalone.child().starts(),
        coordinator_starts + 1
    );
    assert_eq!(nodes.worker.standalone.child().starts(), worker_starts + 1);
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(siderostat::manager::ReleaseIdentity::ManagedProfile(
            coordinator_fixture.profile_id.clone()
        ))
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(siderostat::manager::ReleaseIdentity::ManagedProfile(
            worker_fixture.profile_id.clone()
        ))
    );
    let starts_after_commit = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let replay = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-activate-01".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("completed operation replay returns its durable result");
    assert_eq!(replay["phase"], "complete");
    assert_eq!(
        starts_after_commit,
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        "a duplicate operation must not start another child"
    );
    let coordinator_journal = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-activate-01"]
        .clone();
    assert_eq!(
        coordinator_journal.phase,
        siderostat::manager::PersistedActivationPhase::Complete
    );
    assert_eq!(coordinator_journal.participants.len(), 2);
    assert_ne!(
        coordinator_journal.participants["reconnect-coordinator"].candidate_digest,
        coordinator_journal.participants["reconnect-worker"].candidate_digest,
        "node-local role artifacts have distinct candidate digests"
    );
    assert!(
        coordinator_journal
            .participants
            .values()
            .all(|participant| {
                participant.prepare_ack.is_some()
                    && participant.drain_ack.is_some()
                    && participant.ready_ack.is_some()
                    && participant.commit_ack.is_some()
            })
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-activate-01"]
            .phase,
        siderostat::manager::PersistedActivationPhase::Complete
    );

    let worker_commit_ack = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-activate-01"]
        .participants["reconnect-worker"]
        .commit_ack
        .clone()
        .expect("durable worker commit ack");
    nodes.worker.standalone.child().stop();
    nodes.worker.proxy.admission().block();
    let duplicate_finalize = peer_client(&nodes, LocalRole::Worker, "reconnect-coordinator")
        .manager_peer(&ManagerPeerRequest {
            operation_id: "manager-cluster-activate-01".into(),
            profile_id: worker_fixture.profile_id.clone(),
            candidate_digest: worker_fixture.candidate_digest.clone(),
            source_commit: "a".repeat(40),
            model_digest: sha256(b"fixture model"),
            previous_digest: Some(worker_fixture.previous_digest.clone()),
            expected_generation: generation,
            policy_epoch: 0,
            phase: ManagerPeerPhase::Commit,
            ack_id: Some(worker_commit_ack),
        })
        .await;
    assert!(
        duplicate_finalize.is_err(),
        "a completed commit replay must not reopen admission after the child stops"
    );
    assert_eq!(
        nodes.worker.proxy.admission().snapshot().state,
        siderostat::admission::AdmissionState::Blocked
    );

    nodes.shutdown().await;
}

#[tokio::test]
async fn missing_peer_profile_fails_before_either_child_is_stopped() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-missing-peer",
        "coordinator",
        "ds4-server",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let stops = (
        nodes.coordinator.standalone.child().stops(),
        nodes.worker.standalone.child().stops(),
    );
    let running = (
        nodes.coordinator.standalone.child().is_running(),
        nodes.worker.standalone.child().is_running(),
    );
    let generation = nodes.coordinator.mode.snapshot().generation;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-missing-peer-profile".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::UnpairedPeer)
    );
    assert_eq!(
        starts,
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        )
    );
    assert_eq!(
        stops,
        (
            nodes.coordinator.standalone.child().stops(),
            nodes.worker.standalone.child().stops(),
        )
    );
    assert!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .is_empty()
    );
    assert_eq!(
        running,
        (
            nodes.coordinator.standalone.child().is_running(),
            nodes.worker.standalone.child().is_running(),
        ),
        "preflight rejection must preserve both observed child states"
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn disconnected_peer_fails_preflight_without_stopping_either_child() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let mut nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-disconnected-peer",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-disconnected-peer",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let stops = (
        nodes.coordinator.standalone.child().stops(),
        nodes.worker.standalone.child().stops(),
    );
    nodes.worker.stop_serve().await;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-disconnected-peer".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: nodes.coordinator.mode.snapshot().generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::UnpairedPeer)
    );
    assert_eq!(
        starts,
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        )
    );
    assert_eq!(
        stops,
        (
            nodes.coordinator.standalone.child().stops(),
            nodes.worker.standalone.child().stops(),
        )
    );
    assert!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .is_empty()
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn forced_standalone_peer_rejects_activation_without_stopping_local_child() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-forced-peer",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-forced-peer",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    nodes.worker.production.restore_policy(
        siderostat::cluster::OperationPolicy::ForcedStandalone,
        siderostat::cluster::OperationPolicy::ForcedStandalone,
        0,
        false,
    );
    let starts = nodes.coordinator.standalone.child().starts();
    let stops = nodes.coordinator.standalone.child().stops();
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-forced-peer".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: nodes.coordinator.mode.snapshot().generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::UnpairedPeer)
    );
    assert_eq!(nodes.coordinator.standalone.child().starts(), starts);
    assert_eq!(nodes.coordinator.standalone.child().stops(), stops);
    assert!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .is_empty()
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn local_drain_timeout_restores_peer_prepare_without_stopping_child() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-local-drain-timeout",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-local-drain-timeout",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let in_flight = nodes
        .coordinator
        .proxy
        .admission()
        .try_acquire(true)
        .expect("hold an in-flight local request");
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let running_before = (
        nodes.coordinator.standalone.child().is_running(),
        nodes.worker.standalone.child().is_running(),
    );
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-local-drain-timeout".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: nodes.coordinator.mode.snapshot().generation,
            },
        ))
        .await;
    drop(in_flight);
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::NotReady)
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-local-drain-timeout"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-local-drain-timeout"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        starts
    );
    assert_eq!(
        (
            nodes.coordinator.standalone.child().is_running(),
            nodes.worker.standalone.child().is_running(),
        ),
        running_before
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn remote_drain_timeout_without_running_previous_requires_manual_intervention() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-remote-drain-timeout",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-remote-drain-timeout",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let in_flight = nodes
        .worker
        .proxy
        .admission()
        .try_acquire(true)
        .expect("hold an in-flight worker request");
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-remote-drain-timeout".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: nodes.coordinator.mode.snapshot().generation,
            },
        ))
        .await;
    drop(in_flight);
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::ManualIntervention)
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-remote-drain-timeout"]
            .phase,
        siderostat::manager::PersistedActivationPhase::ManualIntervention
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-remote-drain-timeout"]
            .phase,
        siderostat::manager::PersistedActivationPhase::ManualIntervention
    );
    assert_eq!(
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        (starts.0 + 1, starts.1),
        "coordinator restores its unchanged command; the unproven worker stays stopped"
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(!nodes.worker.standalone.child().is_running());
    assert_eq!(
        nodes.coordinator.proxy.admission().snapshot().state,
        siderostat::admission::AdmissionState::Blocked
    );
    assert_eq!(
        nodes.worker.proxy.admission().snapshot().state,
        siderostat::admission::AdmissionState::Blocked
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn candidate_start_failure_restores_previous_on_both_nodes() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-rollback",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-rollback",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let coordinator_previous = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    let worker_previous = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    let coordinator_starts = nodes.coordinator.standalone.child().starts();
    let worker_starts = nodes.worker.standalone.child().starts();
    nodes.worker.standalone.set_start_fails_once();
    let generation = nodes.coordinator.mode.snapshot().generation;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-rollback-01".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::NotReady)
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        coordinator_previous
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        worker_previous
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-rollback-01"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-rollback-01"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        nodes.coordinator.standalone.child().starts(),
        coordinator_starts + 2,
        "coordinator started the candidate then proved its previous command ready"
    );
    assert_eq!(
        nodes.worker.standalone.child().starts(),
        worker_starts + 1,
        "worker candidate failed once, then previous command became ready"
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    nodes.shutdown().await;
}

#[tokio::test]
async fn lost_peer_commit_ack_rolls_back_without_publishing_complete() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-lost-commit-ack",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-lost-commit-ack",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let coordinator_previous = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    let worker_previous = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    nodes
        .worker
        .production
        .lose_next_manager_peer_ack_for_test(ManagerPeerPhase::Commit);
    let generation = nodes.coordinator.mode.snapshot().generation;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-lost-commit-ack".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::NotReady)
    );
    let coordinator_journal = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-lost-commit-ack"]
        .clone();
    assert_eq!(
        coordinator_journal.phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert!(
        coordinator_journal
            .participants
            .values()
            .all(|participant| {
                participant.phase == siderostat::manager::PersistedActivationPhase::RolledBack
                    && participant.rollback_ack.is_some()
            })
    );
    assert_eq!(
        coordinator_journal.participants["reconnect-worker"].commit_ack, None,
        "Coordinator must not fabricate the lost peer commit acknowledgement"
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-lost-commit-ack"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        coordinator_previous
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        worker_previous
    );
    assert_eq!(
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        (starts.0 + 2, starts.1 + 2)
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    nodes.shutdown().await;
}

#[tokio::test]
async fn force_policy_change_during_start_prevents_complete_and_restores_previous() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-force-during-start",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-force-during-start",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let coordinator_previous = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    let worker_previous = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone();
    nodes
        .coordinator
        .standalone
        .set_start_delay(Duration::from_millis(250));
    let coordinator = nodes.coordinator.production.clone();
    let profile_id = coordinator_fixture.profile_id.clone();
    let generation = nodes.coordinator.mode.snapshot().generation;
    let operation = tokio::spawn(async move {
        coordinator
            .manager_operation_for_test(ManagerRuntimeOperation::Activate(
                ManagerActivationRequest {
                    operation_id: "manager-cluster-force-during-start".into(),
                    profile_id,
                    expected_generation: generation,
                },
            ))
            .await
    });
    nodes.coordinator.standalone.wait_for_start_attempt().await;
    nodes.coordinator.production.restore_policy(
        siderostat::cluster::OperationPolicy::ForcedStandalone,
        siderostat::cluster::OperationPolicy::ForcedStandalone,
        1,
        false,
    );
    let result = operation.await.expect("activation actor task");
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::NotReady)
    );
    assert_eq!(
        nodes.coordinator.production.operator_policy(),
        siderostat::cluster::OperationPolicy::ForcedStandalone,
        "the latest Force policy remains latched after transaction rollback"
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        coordinator_previous
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        worker_previous
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-force-during-start"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-force-during-start"]
            .phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    nodes.shutdown().await;
}

#[tokio::test]
async fn lost_peer_ready_ack_rolls_back_both_nodes_without_commit() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-lost-ready-ack",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-lost-ready-ack",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let starts = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    nodes
        .worker
        .production
        .lose_next_manager_peer_ack_for_test(ManagerPeerPhase::Start);
    let generation = nodes.coordinator.mode.snapshot().generation;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-lost-ready-ack".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::NotReady)
    );
    let coordinator_journal = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-lost-ready-ack"]
        .clone();
    let worker_journal = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-lost-ready-ack"]
        .clone();
    assert_eq!(
        coordinator_journal.phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        coordinator_journal.participants["reconnect-worker"].ready_ack, None,
        "the lost worker readiness ack is not fabricated"
    );
    assert_eq!(
        worker_journal.phase,
        siderostat::manager::PersistedActivationPhase::RolledBack
    );
    assert_eq!(
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        (starts.0 + 2, starts.1 + 2),
        "both candidates are replaced by a verified previous start"
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    nodes.shutdown().await;
}

#[tokio::test]
async fn previous_release_start_failure_keeps_both_nodes_manual_and_closed() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-manual-rollback",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-manual-rollback",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    nodes.worker.standalone.set_start_fails(true);
    let generation = nodes.coordinator.mode.snapshot().generation;
    let result = nodes
        .coordinator
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-manual-rollback".into(),
                profile_id: coordinator_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await;
    assert_eq!(
        result,
        Err(siderostat::cluster::production::manager::ManagerRuntimeError::ManualIntervention)
    );
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-manual-rollback"]
            .phase,
        siderostat::manager::PersistedActivationPhase::ManualIntervention
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-manual-rollback"]
            .phase,
        siderostat::manager::PersistedActivationPhase::ManualIntervention
    );
    assert_eq!(
        nodes.coordinator.mode.snapshot().state,
        ClusterState::ManualInterventionRequired
    );
    assert_eq!(
        nodes.worker.mode.snapshot().state,
        ClusterState::ManualInterventionRequired
    );
    assert!(!nodes.worker.standalone.child().is_running());
    assert_eq!(
        nodes.coordinator.proxy.admission().snapshot().state,
        siderostat::admission::AdmissionState::Blocked
    );
    assert_eq!(
        nodes.worker.proxy.admission().snapshot().state,
        siderostat::admission::AdmissionState::Blocked
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn explicit_rollback_selects_each_nodes_managed_previous_profile() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-new",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture =
        manager_release_fixture(&nodes.worker.config, "worker-profile-new", "worker", "ds4");
    add_managed_previous_profile(
        &coordinator_fixture,
        &nodes.coordinator.config,
        "coordinator-profile-old",
        "coordinator",
        "ds4-server",
    );
    add_managed_previous_profile(
        &worker_fixture,
        &nodes.worker.config,
        "worker-profile-old",
        "worker",
        "ds4",
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let generation = nodes.coordinator.mode.snapshot().generation;

    nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-before-rollback".into(),
                profile_id: worker_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("activate new release on both nodes");

    let result = nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Rollback(
            siderostat::cluster::production::manager::ManagerRollbackRequest {
                operation_id: "manager-cluster-explicit-rollback".into(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("restore each node's own previous profile");
    assert_eq!(result["phase"], "complete");
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(ReleaseIdentity::ManagedProfile(
            "coordinator-profile-old".into()
        ))
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(ReleaseIdentity::ManagedProfile("worker-profile-old".into()))
    );
    let rollback = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .activation_journals["manager-cluster-explicit-rollback"]
        .clone();
    assert_eq!(
        rollback.phase,
        siderostat::manager::PersistedActivationPhase::Complete
    );
    assert_eq!(
        rollback.participants["reconnect-worker"].candidate_profile_id, "worker-profile-old",
        "peer target is taken from the worker's own previous pointer"
    );
    let starts_after_rollback = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let replay = nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Rollback(
            siderostat::cluster::production::manager::ManagerRollbackRequest {
                operation_id: "manager-cluster-explicit-rollback".into(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("completed rollback retry returns its durable result");
    assert_eq!(replay["phase"], "complete");
    assert_eq!(
        starts_after_rollback,
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        "a duplicate rollback must not start another child"
    );
    nodes.shutdown().await;
}

#[tokio::test]
async fn explicit_rollback_restores_each_nodes_verified_external_baseline() {
    let _serial = NODE_TEST_LOCK.lock().await;
    let nodes = support::TwoNode::boot().await.expect("boot nodes");
    nodes.pair().await.expect("pair nodes");
    let coordinator_fixture = manager_release_fixture(
        &nodes.coordinator.config,
        "coordinator-profile-from-baseline",
        "coordinator",
        "ds4-server",
    );
    let worker_fixture = manager_release_fixture(
        &nodes.worker.config,
        "worker-profile-from-baseline",
        "worker",
        "ds4",
    );
    let coordinator_baseline = coordinator_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone()
        .expect("coordinator external baseline");
    let worker_baseline = worker_fixture
        .store
        .lock()
        .unwrap()
        .snapshot()
        .release_pointers
        .active
        .clone()
        .expect("worker external baseline");
    assert_ne!(
        coordinator_baseline, worker_baseline,
        "the paired nodes exercise independent external baseline identities"
    );
    nodes
        .coordinator
        .production
        .attach_manager_store(coordinator_fixture.store.clone());
    nodes
        .worker
        .production
        .attach_manager_store(worker_fixture.store.clone());
    let generation = nodes.worker.mode.snapshot().generation;

    nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Activate(
            ManagerActivationRequest {
                operation_id: "manager-cluster-baseline-activate".into(),
                profile_id: worker_fixture.profile_id.clone(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("activate managed release on both nodes");

    let ReleaseIdentity::ExternalBaseline { model_sha256, .. } = &worker_baseline else {
        panic!("worker fixture starts from an external baseline");
    };
    let worker_active_digest = sha256(
        serde_json::to_string(
            &worker_fixture
                .store
                .lock()
                .unwrap()
                .snapshot()
                .release_pointers
                .active,
        )
        .unwrap()
        .as_bytes(),
    );
    let status = peer_client(&nodes, LocalRole::Worker, "reconnect-coordinator")
        .manager_peer(&ManagerPeerRequest {
            operation_id: "manager-cluster-baseline-status".into(),
            profile_id: "external-baseline".into(),
            candidate_digest: sha256(serde_json::to_string(&worker_baseline).unwrap().as_bytes()),
            source_commit: "0".repeat(40),
            model_digest: model_sha256.clone(),
            previous_digest: Some(worker_active_digest),
            expected_generation: generation,
            policy_epoch: 0,
            phase: ManagerPeerPhase::Status,
            ack_id: None,
        })
        .await
        .expect("worker status exposes its external baseline rollback target");
    assert_eq!(
        status.previous_profile_id.as_deref(),
        Some("external-baseline")
    );
    assert!(status.profiles.iter().any(|profile| {
        profile.profile_id == "external-baseline"
            && profile.candidate_digest
                == sha256(serde_json::to_string(&worker_baseline).unwrap().as_bytes())
    }));

    let rollback = nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Rollback(
            siderostat::cluster::production::manager::ManagerRollbackRequest {
                operation_id: "manager-cluster-baseline-rollback".into(),
                expected_generation: generation,
            },
        ))
        .await;
    assert!(
        rollback.is_ok(),
        "restore each node's verified external baseline: {rollback:?}; coordinator journal={:?}; worker journal={:?}",
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .get("manager-cluster-baseline-rollback"),
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals
            .get("manager-cluster-baseline-rollback")
    );
    let starts_after_rollback = (
        nodes.coordinator.standalone.child().starts(),
        nodes.worker.standalone.child().starts(),
    );
    let replay = nodes
        .worker
        .production
        .manager_operation_for_test(ManagerRuntimeOperation::Rollback(
            siderostat::cluster::production::manager::ManagerRollbackRequest {
                operation_id: "manager-cluster-baseline-rollback".into(),
                expected_generation: generation,
            },
        ))
        .await
        .expect("completed external baseline rollback retry returns its durable result");
    assert_eq!(replay["phase"], "complete");
    assert_eq!(
        starts_after_rollback,
        (
            nodes.coordinator.standalone.child().starts(),
            nodes.worker.standalone.child().starts(),
        ),
        "a duplicate external rollback must not start another child"
    );

    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(coordinator_baseline)
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .release_pointers
            .active,
        Some(worker_baseline)
    );
    assert!(nodes.coordinator.standalone.child().is_running());
    assert!(nodes.worker.standalone.child().is_running());
    assert_eq!(
        coordinator_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-baseline-rollback"]
            .phase,
        siderostat::manager::PersistedActivationPhase::Complete
    );
    assert_eq!(
        worker_fixture
            .store
            .lock()
            .unwrap()
            .snapshot()
            .activation_journals["manager-cluster-baseline-rollback"]
            .phase,
        siderostat::manager::PersistedActivationPhase::Complete
    );
    nodes.shutdown().await;
}
