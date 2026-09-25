//! G04 / C04 / model/activation view: model 選択・download・activate・
//! rollback GUI の受入 case を公開 API（ModelView / ManagerApi）経由で
//! 検証する。実 network は使わず、fake ManagerApi 境界で activate /
//! rollback を記録する。レビュー重点: 各 stage ごとに承認 dialog を
//! 重複させず、実 runtime 変更の承認は一つの activation 操作へ集約。
use siderostat_core::manager::{
    HardwareReadiness, ProfileCompatibility,
    api::{
        ManagerArtifactDto, ManagerArtifactReferenceDto, ManagerInventoryResponse, ManagerJobDto,
        ManagerNodeReadinessDto, ManagerSourceReceiptDto, ManagerStagedProfileDto,
    },
};
use siderostat_monitor::manager_window::{
    ManagerApi, ManagerCommand, ManagerPreparationAction, ManagerViewModel, ModelEntry, ModelView,
};

#[derive(Default)]
struct FakeManagerApi {
    submit_calls: Vec<(String, String)>,
    job_ids: Vec<String>,
}

impl FakeManagerApi {
    fn with_jobs(ids: &[&str]) -> Self {
        Self {
            job_ids: ids.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
}

impl ManagerApi for FakeManagerApi {
    async fn submit(&mut self, kind: &str, payload_key: &str) -> Result<String, String> {
        self.submit_calls
            .push((kind.to_string(), payload_key.to_string()));
        let id = self
            .job_ids
            .get(self.submit_calls.len() - 1)
            .cloned()
            .unwrap_or_else(|| format!("job-{}", self.submit_calls.len()));
        Ok(id)
    }

    async fn cancel(&mut self, _job_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(future)
}

fn model(name: &str, checksum: Option<&str>, encoder: &str, support: &str) -> ModelEntry {
    ModelEntry {
        name: name.to_string(),
        origin: None,
        size: Some(1024),
        checksum: checksum.map(str::to_string),
        checksum_verified: false,
        registry_verified: false,
        pending_reason: None,
        license: "MIT".to_string(),
        encoder: encoder.to_string(),
        support: support.to_string(),
    }
}

fn verified_model(name: &str, encoder: &str, support: &str) -> ModelEntry {
    ModelEntry {
        checksum: Some(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        ),
        checksum_verified: true,
        registry_verified: true,
        ..model(name, None, encoder, support)
    }
}

fn manager_inventory(node_id: &str, build_id: &str, model_id: &str) -> ManagerInventoryResponse {
    let commit = "a".repeat(40);
    ManagerInventoryResponse {
        node_id: node_id.to_string(),
        node_role: Some("coordinator".to_string()),
        source_commits: vec![ManagerSourceReceiptDto {
            receipt_id: format!("source-{commit}"),
            full_commit: commit.clone(),
            main_proof: "refs/heads/main".to_string(),
            fetched_at: 1,
        }],
        artifacts: vec![
            ManagerArtifactDto {
                id: build_id.to_string(),
                kind: "build".to_string(),
                digest: "b".repeat(64),
                size: 10,
                verified: true,
                source_commit: Some(commit),
                role: Some("ds4-server".to_string()),
                catalog_id: None,
            },
            ManagerArtifactDto {
                id: model_id.to_string(),
                kind: "model".to_string(),
                digest: "c".repeat(64),
                size: 20,
                verified: true,
                source_commit: None,
                role: None,
                catalog_id: Some("catalog-ds4".to_string()),
            },
        ],
        profiles: Vec::new(),
        node_readiness: ManagerNodeReadinessDto {
            ready: false,
            reason: Some("no staged profile".to_string()),
        },
        active_digest: None,
        previous_digest: Some("e".repeat(64)),
        activation_phase: None,
    }
}

/// 受入 case 1: checksum 無し → activate disabled。G04。/
#[test]
fn missing_checksum_disables_activation() {
    let mut api = FakeManagerApi::with_jobs(&["act-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![model("m1", None, "openai-whisper", "supported")]);
    assert!(!view.can_activate("m1"), "no checksum -> activate disabled");
    let err = block_on(view.start_activation("m1", &mut api)).expect_err("activate");
    assert!(err.contains("activate できません"));
    assert!(api.submit_calls.is_empty(), "no POST when checksum missing");
}

/// 受入 case 2: Vision 不整合 → 理由。G04。/
#[test]
fn vision_mismatch_surfaces_reason() {
    let mut view = ModelView::new();
    view.set_models(vec![
        verified_model("m-ok", "openai-whisper", "supported"),
        verified_model("m-enc", "other-encoder", "supported"),
        verified_model("m-unsup", "openai-whisper", "unsupported"),
    ]);
    assert!(view.can_activate("m-ok"));
    assert!(view.vision_reason("m-ok").is_none());
    let reason = view
        .vision_reason("m-enc")
        .expect("encoder mismatch reason");
    assert!(reason.contains("encoder 不整合"), "{reason}");
    assert!(!view.can_activate("m-enc"));
    let reason = view.vision_reason("m-unsup").expect("unsupported reason");
    assert!(reason.contains("Vision 対応外"), "{reason}");
    assert!(!view.can_activate("m-unsup"));
}

/// 受入 case 3: build/download 中 → 旧 active 表示。G04。/
#[test]
fn old_active_shown_while_build_or_download_runs() {
    let mut view = ModelView::new();
    let running_build = ManagerJobDto {
        id: "build-1".to_string(),
        kind: "build".to_string(),
        progress: 40,
        phase: "running".to_string(),
        error: String::new(),
        created_at: 0,
        updated_at: 0,
        cancel: true,
    };
    view.apply_status(&[], Some("old-active"));
    assert_eq!(view.current_active(), Some("old-active"));
    view.apply_status(&[running_build], Some("new-active"));
    assert_eq!(view.current_active(), Some("old-active"));
}

/// 受入 case 4: rollback → previous 保持。G04。/
#[test]
fn rollback_keeps_previous() {
    let mut api = FakeManagerApi::with_jobs(&["rollback-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![verified_model("m1", "openai-whisper", "supported")]);
    view.apply_status(&[], Some("active-a"));
    let _ = block_on(view.start_activation("m1", &mut api)).expect("activate");
    assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
    let _ = block_on(view.rollback("active-a", &mut api)).expect("rollback");
    assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
    assert_eq!(api.submit_calls.len(), 2);
    assert_eq!(api.submit_calls[0].0, "activate");
    assert_eq!(api.submit_calls[1].0, "rollback");
}

/// レビュー重点: activation は一つの操作へ集約（承認 dialog を各 stage
/// で重複させない）。G04。/
#[test]
fn activation_is_single_operation() {
    let mut api = FakeManagerApi::with_jobs(&["act-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![verified_model("m1", "openai-whisper", "supported")]);
    let id = block_on(view.start_activation("m1", &mut api)).expect("activate");
    assert_eq!(id, "act-1");
    // download / stage / activate を分割せず、一つの activate 操作に集約。G04。/
    assert_eq!(api.submit_calls.len(), 1);
    assert_eq!(api.submit_calls[0].0, "activate");
}

/// size/license/checksum/encoder/support を model 一覧で表示できる。G04。/
#[test]
fn model_list_carries_size_license_checksum_encoder_support() {
    let m = model("m1", Some("sha-abc"), "openai-whisper", "supported");
    assert_eq!(m.size, Some(1024));
    assert_eq!(m.license, "MIT");
    assert_eq!(m.checksum.as_deref(), Some("sha-abc"));
    assert_eq!(m.encoder, "openai-whisper");
    assert_eq!(m.support, "supported");
}

#[test]
fn preparation_actions_reference_only_the_expected_nodes_local_inventory() {
    let local_build = format!("build-{}", "1".repeat(64));
    let local_model = format!("model-{}", "2".repeat(64));
    let peer_build = format!("build-{}", "3".repeat(64));
    let peer_model = format!("model-{}", "4".repeat(64));
    let mut vm = ManagerViewModel::new();
    vm.set_expected_node_id("local-node");
    let mut local_inventory = manager_inventory("local-node", &local_build, &local_model);
    let latest_commit = "f".repeat(40);
    local_inventory
        .source_commits
        .push(ManagerSourceReceiptDto {
            receipt_id: format!("source-{latest_commit}"),
            full_commit: latest_commit.clone(),
            main_proof: latest_commit,
            fetched_at: 2,
        });
    assert!(vm.apply_inventory(local_inventory));

    let build = vm.preparation_action(ManagerPreparationAction::BuildCoordinator);
    assert!(
        build.enabled,
        "local source receipt enables build: {build:?}"
    );
    assert!(matches!(
        build.command,
        Some(ManagerCommand::Build { ref source_receipt_id, ref role })
            if source_receipt_id == &format!("source-{}", "f".repeat(40)) && role == "ds4-server"
    ));
    let wrong_role_build = vm.preparation_action(ManagerPreparationAction::BuildWorker);
    assert!(!wrong_role_build.enabled);
    assert!(
        wrong_role_build
            .reason
            .unwrap()
            .contains("coordinator role")
    );
    let stage = vm.preparation_action(ManagerPreparationAction::StageProfile);
    assert!(
        stage.enabled,
        "local verified artifacts enable stage: {stage:?}"
    );
    assert!(matches!(
        stage.command,
        Some(ManagerCommand::Stage { ref build_artifact_id, ref model_artifact_id })
            if build_artifact_id == &local_build && model_artifact_id == &local_model
    ));

    assert!(!vm.apply_inventory(manager_inventory("peer-node", &peer_build, &peer_model)));
    let stage = vm.preparation_action(ManagerPreparationAction::StageProfile);
    assert!(
        !stage.enabled,
        "peer inventory must be rejected for local manager"
    );
    assert!(stage.reason.unwrap().contains("node identity"));
    assert!(!vm.inventory_summary().contains(&peer_build));
}

#[test]
fn worker_inventory_enables_only_worker_build_and_stage_pair() {
    let build_id = format!("build-{}", "7".repeat(64));
    let model_id = format!("model-{}", "8".repeat(64));
    let mut inventory = manager_inventory("worker-node", &build_id, &model_id);
    inventory.node_role = Some("worker".to_string());
    inventory.artifacts[0].role = Some("ds4".to_string());
    let mut vm = ManagerViewModel::new();
    vm.set_expected_node_id("worker-node");
    assert!(vm.apply_inventory(inventory));

    assert!(
        !vm.preparation_action(ManagerPreparationAction::BuildCoordinator)
            .enabled
    );
    let worker_build = vm.preparation_action(ManagerPreparationAction::BuildWorker);
    assert!(worker_build.enabled);
    assert!(matches!(
        worker_build.command,
        Some(ManagerCommand::Build { ref role, .. }) if role == "ds4"
    ));
    let stage = vm.preparation_action(ManagerPreparationAction::StageProfile);
    assert!(stage.enabled);
    assert!(matches!(
        stage.command,
        Some(ManagerCommand::Stage { ref build_artifact_id, ref model_artifact_id })
            if build_artifact_id == &build_id && model_artifact_id == &model_id
    ));
}

#[test]
fn unverified_profile_pending_stage_and_hardware_state_explain_disabled_actions() {
    let build_id = format!("build-{}", "5".repeat(64));
    let model_id = format!("model-{}", "6".repeat(64));
    let mut pending = manager_inventory("local-node", &build_id, &model_id);
    pending
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.kind == "model")
        .unwrap()
        .verified = false;
    let mut vm = ManagerViewModel::new();
    vm.apply_inventory(pending);

    let verify = vm.preparation_action(ManagerPreparationAction::VerifyModel);
    assert!(verify.enabled);
    assert!(matches!(
        verify.command,
        Some(ManagerCommand::Verify { ref artifact_id }) if artifact_id == &model_id
    ));
    let stage = vm.preparation_action(ManagerPreparationAction::StageProfile);
    assert!(!stage.enabled);
    assert!(stage.reason.unwrap().contains("未検証"));
    let download = vm.preparation_action(ManagerPreparationAction::DownloadModel);
    assert!(!download.enabled);
    assert!(download.reason.unwrap().contains("catalog"));
    let activate = vm.preparation_action(ManagerPreparationAction::Activate);
    assert!(!activate.enabled);
    assert!(activate.reason.unwrap().contains("transaction"));

    vm.apply_status(
        &[ManagerJobDto {
            id: "stage-running".to_string(),
            kind: "stage".to_string(),
            progress: 0,
            phase: "running".to_string(),
            error: String::new(),
            created_at: 0,
            updated_at: 0,
            cancel: true,
        }],
        None,
    );
    let stage = vm.preparation_action(ManagerPreparationAction::StageProfile);
    assert!(!stage.enabled);
    assert!(stage.reason.unwrap().contains("進行中"));

    let mut staged = manager_inventory("local-node", &build_id, &model_id);
    staged.profiles.push(ManagerStagedProfileDto {
        profile_id: "profile-pending-hardware".to_string(),
        node_role: "coordinator".to_string(),
        role_artifacts: vec![ManagerArtifactReferenceDto {
            id: build_id,
            digest: Some("b".repeat(64)),
            verified: true,
        }],
        model_artifact: ManagerArtifactReferenceDto {
            id: model_id,
            digest: Some("c".repeat(64)),
            verified: true,
        },
        config_fingerprint: "d".repeat(64),
        compatibility: ProfileCompatibility::Compatible,
        hardware_readiness: HardwareReadiness::Pending,
        activation_ready: false,
    });
    let mut hardware_pending_vm = ManagerViewModel::new();
    hardware_pending_vm.apply_inventory(staged);
    assert_eq!(
        hardware_pending_vm
            .preparation_action(ManagerPreparationAction::StageProfile)
            .reason
            .as_deref(),
        Some("既存profileのhardware readinessがpending")
    );
}
