//! H06 Manager window host contract.

use siderostat_monitor::client::MetricsClient;
use siderostat_monitor::config::MonitorConfig;
use siderostat_monitor::manager_window::{
    ManagerCommand, ManagerEvent, ManagerPreparationAction, ManagerViewModel, ManagerWindowHost,
    ManifestProjectionFailure, ModelEntry, ModelView, ProfileReadiness,
};

use siderostat_core::manager::{
    HardwareReadiness, ProfileCompatibility,
    api::{
        ManagerArtifactDto, ManagerArtifactReferenceDto, ManagerInventoryResponse, ManagerJobDto,
        ManagerNodeReadinessDto, ManagerPeerInventoryDto, ManagerPeerProfileDto,
        ManagerRuntimeReadinessDto, ManagerSourceReceiptDto, ManagerStagedProfileDto,
        ManagerStatusResponse,
    },
};

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn manager_host_reuses_one_window_and_keeps_commands_local() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), ModelView::new());

    host.show_or_focus().expect("show window");
    let first_identity = host.window_identity();
    host.show_or_focus().expect("focus window");
    assert_eq!(host.window_identity(), first_identity);
    assert!(host.is_visible());

    host.send_command(ManagerCommand::FetchSource)
        .expect("send fetch");
    host.send_command(ManagerCommand::Build {
        source_receipt_id: format!("source-{}", "a".repeat(40)),
        role: "ds4-server".to_string(),
    })
    .expect("send build");
    host.send_command(ManagerCommand::Download {
        catalog_id: "catalog-ds4".to_string(),
    })
    .expect("send download");
    host.send_command(ManagerCommand::Verify {
        artifact_id: format!("model-{}", "b".repeat(64)),
    })
    .expect("send verify");
    host.send_command(ManagerCommand::Stage {
        build_artifact_id: format!("build-{}", "c".repeat(64)),
        model_artifact_id: format!("model-{}", "d".repeat(64)),
    })
    .expect("send stage");
    host.send_command(ManagerCommand::Activate {
        profile: "mxfp4-0731".to_string(),
        expected_generation: 3,
    })
    .expect("send activate");
    host.send_command(ManagerCommand::Rollback {
        expected_generation: 3,
    })
    .expect("send rollback");

    assert_eq!(host.drain_commands().len(), 7);
    host.apply_event(ManagerEvent::Inventory(fixture_inventory(
        "local-node",
        false,
        "local",
    )));
    let inventory_snapshot = host.inventory_summary().to_string();
    host.apply_event(ManagerEvent::Status(
        siderostat_core::manager::api::ManagerStatusResponse {
            jobs: vec![siderostat_core::manager::api::ManagerJobDto {
                id: "build-1".to_string(),
                kind: "build".to_string(),
                progress: 40,
                phase: "running".to_string(),
                error: String::new(),
                created_at: 0,
                updated_at: 0,
                cancel: true,
            }],
            active_digest: Some("old-digest".to_string()),
            queue_depth: 1,
        },
    ));
    host.hide();
    assert!(!host.is_visible());
    host.show_or_focus().expect("reopen window");
    assert_eq!(host.window_identity(), first_identity);
    assert_eq!(host.view_model().active_digest(), Some("old-digest"));
    assert!(host.view_model().jobs().any(|job| job.id == "build-1"));
    assert_eq!(host.inventory_summary(), inventory_snapshot);
    host.hide();
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn manager_fixture_matrix_keeps_running_cancelled_terminal_and_old_active() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), ModelView::new());
    host.apply_event(ManagerEvent::Status(ManagerStatusResponse {
        jobs: vec![
            fixture_job("build-1", "build", "running", "", false),
            fixture_job("download-1", "download", "cancelling", "", true),
            fixture_job("verify-1", "verify", "succeeded", "", false),
            fixture_job(
                "stage-1",
                "stage",
                "failed",
                "https://user:pw@example.invalid/?token=secret",
                false,
            ),
        ],
        active_digest: Some("old-active".to_string()),
        queue_depth: 2,
    }));
    assert_eq!(host.view_model().active_digest(), Some("old-active"));
    assert_eq!(
        host.view_model()
            .jobs()
            .find(|job| job.id == "build-1")
            .unwrap()
            .phase,
        "running"
    );
    assert_eq!(
        host.view_model()
            .jobs()
            .find(|job| job.id == "download-1")
            .unwrap()
            .phase,
        "cancelling"
    );
    assert_eq!(
        host.view_model()
            .jobs()
            .find(|job| job.id == "verify-1")
            .unwrap()
            .phase,
        "succeeded"
    );
    let failed = host
        .view_model()
        .jobs()
        .find(|job| job.id == "stage-1")
        .unwrap();
    assert!(!ManagerViewModel::redacted_reason(failed).contains("secret"));
    assert!(!ManagerViewModel::redacted_reason(failed).contains("pw"));
    let rendered = host.jobs_summary();
    for expected in [
        "build-1", "build", "running", "40%", "stage-1", "failed", "error:",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}: {rendered}"
        );
    }
    assert!(!rendered.contains("secret"));
    assert!(!rendered.contains("pw"));
    assert!(rendered.contains("[REDACTED]"));
    assert_eq!(
        host.view_model().latest_cancellable_job_id(),
        Some("build-1")
    );
    assert!(host.request_cancel_selected().expect("cancel action"));
    host.request_refresh().expect("refresh action");
    assert_eq!(
        host.drain_commands(),
        vec![
            ManagerCommand::Cancel {
                job_id: "build-1".to_string()
            },
            ManagerCommand::Refresh,
            ManagerCommand::RefreshInventory,
        ]
    );
    host.apply_event(ManagerEvent::Status(ManagerStatusResponse {
        jobs: vec![fixture_job("build-1", "build", "succeeded", "", false)],
        active_digest: Some("old-active".to_string()),
        queue_depth: 0,
    }));
    assert!(
        !host
            .request_cancel_selected()
            .expect("terminal cancel disabled")
    );
    assert!(host.drain_commands().is_empty());
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn activation_and_rollback_controls_require_verified_two_node_context() {
    use siderostat_core::manager::PersistedActivationPhase as Phase;

    let mut view_model = ManagerViewModel::new();
    view_model.set_expected_node_id("local-node");
    let mut inventory = transaction_ready_inventory();
    assert!(view_model.apply_inventory(inventory.clone()));
    assert_eq!(
        view_model.preparation_action(ManagerPreparationAction::Activate),
        siderostat_monitor::manager_window::ManagerActionState {
            enabled: true,
            reason: None,
            command: Some(ManagerCommand::Activate {
                profile: "profile-local".into(),
                expected_generation: 7,
            }),
        }
    );
    assert_eq!(
        view_model
            .preparation_action(ManagerPreparationAction::Rollback)
            .command,
        Some(ManagerCommand::Rollback {
            expected_generation: 7,
        })
    );

    inventory.profiles[0].model_artifact.digest = Some("6".repeat(64));
    view_model.apply_inventory(inventory.clone());
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .enabled,
        "the local profile digest must match its inventory artifact"
    );
    inventory.profiles[0].model_artifact.digest = Some("c".repeat(64));

    inventory.peer.as_mut().unwrap().profiles[0].model_digest = "6".repeat(64);
    view_model.apply_inventory(inventory.clone());
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .enabled
    );

    inventory.peer = Some(ready_peer_inventory());
    inventory.peer.as_mut().unwrap().active_digest = None;
    view_model.apply_inventory(inventory.clone());
    assert!(
        view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .reason
            .unwrap()
            .contains("peer")
    );
    inventory.peer.as_mut().unwrap().active_digest = Some("e".repeat(64));

    inventory.peer = None;
    view_model.apply_inventory(inventory.clone());
    assert!(
        view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .reason
            .unwrap()
            .contains("peer")
    );
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Rollback)
            .enabled
    );

    inventory.peer = Some(ready_peer_inventory());
    inventory.runtime.as_mut().unwrap().desired_policy = "forced-standalone".into();
    view_model.apply_inventory(inventory.clone());
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .enabled
    );

    inventory.runtime.as_mut().unwrap().desired_policy = "automatic".into();
    inventory.activation_phase = Some(Phase::Committing);
    view_model.apply_inventory(inventory.clone());
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Activate)
            .enabled
    );

    inventory.activation_phase = Some(Phase::ManualIntervention);
    view_model.apply_inventory(inventory.clone());
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Rollback)
            .enabled
    );

    inventory.activation_phase = Some(Phase::Complete);
    inventory.previous_release_ready = false;
    view_model.apply_inventory(inventory);
    assert!(
        !view_model
            .preparation_action(ManagerPreparationAction::Rollback)
            .enabled
    );
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn activation_job_success_waits_for_inventory_live_observation() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), ModelView::new());
    let mut inventory = transaction_ready_inventory();
    host.apply_event(ManagerEvent::Inventory(inventory.clone()));
    host.apply_event(ManagerEvent::Submitted {
        kind: "activate".into(),
        id: "job-1".into(),
    });
    host.apply_event(ManagerEvent::Status(ManagerStatusResponse {
        jobs: vec![fixture_job("job-1", "activate", "succeeded", "", false)],
        active_digest: None,
        queue_depth: 0,
    }));
    assert_eq!(
        host.view_model().active_digest(),
        Some("f".repeat(64).as_str())
    );
    assert!(!host.inventory_summary().contains("phase: Complete"));

    inventory.active_digest = Some("9".repeat(64));
    inventory.activation_phase = Some(siderostat_core::manager::PersistedActivationPhase::Complete);
    host.apply_event(ManagerEvent::Inventory(inventory));
    assert_eq!(
        host.view_model().active_digest(),
        Some("9".repeat(64).as_str())
    );
    assert!(host.inventory_summary().contains("phase: Complete"));
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn profile_fixture_matrix_marks_pending_and_rejects_incompatible_profiles() {
    let mut view = ModelView::new();
    view.set_models(vec![
        fixture_model("mxfp4-0731", None, "openai-whisper", "supported"),
        verified_fixture_model("vision-bad", "other-encoder", "supported"),
        verified_fixture_model("glm-unsupported", "openai-whisper", "unsupported"),
        verified_fixture_model("prefix-bad", "openai-whisper", "supported"),
        verified_fixture_model("q2-ready", "openai-whisper", "supported"),
    ]);
    view.set_prefix_file_compatible("prefix-bad", false);

    assert_eq!(
        view.profile_readiness("mxfp4-0731"),
        ProfileReadiness::Pending("checksum未宣言".to_string())
    );
    assert!(matches!(
        view.profile_readiness("vision-bad"),
        ProfileReadiness::Rejected(reason) if reason.contains("encoder")
    ));
    assert!(matches!(
        view.profile_readiness("glm-unsupported"),
        ProfileReadiness::Rejected(reason) if reason.contains("対応外")
    ));
    assert!(matches!(
        view.profile_readiness("prefix-bad"),
        ProfileReadiness::Rejected(reason) if reason.contains("prefix-file")
    ));
    assert_eq!(view.profile_readiness("q2-ready"), ProfileReadiness::Ready);
    assert!(!view.can_activate("prefix-bad"));

    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), view);
    let rendered = host.profiles_summary();
    for reason in [
        "checksum未宣言",
        "encoder 不整合",
        "対応外",
        "prefix-file不一致",
    ] {
        assert!(rendered.contains(reason), "missing {reason}: {rendered}");
    }
    assert!(rendered.contains("操作不可"));
    assert!(rendered.contains("q2-ready · Ready"));
    assert!(!host.model_view().can_activate("prefix-bad"));
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn configured_manifest_profile_is_projected_pending_at_host_creation() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut model_view = ModelView::new();
    model_view.add_declared_runtime_profile(
        "standalone",
        "configured-profile",
        None,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    model_view.add_declared_runtime_profile(
        "distributed",
        "configured-profile",
        Some(4096),
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    let host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), model_view);

    let rendered = host.profiles_summary();
    assert!(
        rendered.contains("standalone · configured-profile"),
        "{rendered}"
    );
    assert!(
        rendered.contains("distributed · configured-profile"),
        "{rendered}"
    );
    assert!(rendered.contains("Pending"), "{rendered}");
    assert!(rendered.contains("checksum未検証"), "{rendered}");
    assert!(rendered.contains("設定値 digest="), "{rendered}");
    assert!(rendered.contains("Manager registry未検証"), "{rendered}");
    assert!(rendered.contains("操作不可"), "{rendered}");
    assert!(!host.model_view().can_activate("configured-profile"));
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn duplicate_profile_names_keep_per_manifest_readiness() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let standalone = ModelEntry {
        origin: Some("standalone".to_string()),
        ..verified_fixture_model("shared-profile", "openai-whisper", "supported")
    };
    let distributed = ModelEntry {
        origin: Some("distributed".to_string()),
        ..fixture_model(
            "shared-profile",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "openai-whisper",
            "supported",
        )
    };
    let mut model_view = ModelView::new();
    model_view.set_models(vec![standalone, distributed]);
    let host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), model_view);

    let rendered = host.profiles_summary();
    assert!(
        rendered.contains("standalone · shared-profile · Ready"),
        "{rendered}"
    );
    assert!(
        rendered.contains("distributed · shared-profile · Pending"),
        "{rendered}"
    );
    assert!(!host.model_view().can_activate("shared-profile"));
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn failed_manifest_projection_keeps_other_profiles_and_shows_redacted_reason() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut model_view = ModelView::new();
    model_view.add_declared_runtime_profile(
        "standalone",
        "available-profile",
        None,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    model_view.add_manifest_failure("distributed", ManifestProjectionFailure::Read);
    let host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), model_view);

    let rendered = host.profiles_summary();
    assert!(
        rendered.contains("standalone · available-profile · Pending"),
        "{rendered}"
    );
    assert!(
        rendered.contains("distributed · profile manifest unavailable · Pending"),
        "{rendered}"
    );
    assert!(
        rendered.contains("manifest読込失敗（path非表示）"),
        "{rendered}"
    );
    assert!(!rendered.contains("/private/example/secret-manifest.json"));
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn missing_manager_active_digest_is_shown_as_unobserved() {
    let view_model = ManagerViewModel::new();
    assert_eq!(
        view_model.active_digest_display(),
        "active digest 未実測（runtime / registry 未接続）"
    );
    assert_ne!(view_model.active_digest_display(), "未設定");
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn inventory_event_projects_only_sanitized_node_local_state() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut view_model = ManagerViewModel::new();
    view_model.set_expected_node_id("local-node");
    let mut host = ManagerWindowHost::for_test(client, view_model, ModelView::new());
    let mut inventory = fixture_inventory("local-node", true, "local");
    inventory.activation_failure_class = Some("rollback-failed".into());
    let mut peer = ready_peer_inventory();
    peer.activation_failure_class = Some("peer-rollback-failed".into());
    inventory.peer = Some(peer);
    host.apply_event(ManagerEvent::Inventory(inventory));

    let rendered = host.inventory_summary();
    for expected in [
        "node: local-node",
        "source: ",
        "build-",
        "model-",
        "profile: profile-local",
        "hardware=Pending",
        "稼働中 digest: 未実測",
        "previous digest:",
        "failure class: rollback-failed",
        "failure class=peer-rollback-failed",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}: {rendered}"
        );
    }
    for forbidden in ["/Users/", "https://", "secret-token"] {
        assert!(
            !rendered.contains(forbidden),
            "leaked {forbidden}: {rendered}"
        );
    }
    assert_eq!(
        host.preparation_action(ManagerPreparationAction::StageProfile)
            .reason
            .as_deref(),
        Some("既存profileのhardware readinessがpending")
    );

    host.apply_event(ManagerEvent::Inventory(fixture_inventory(
        "peer-node",
        false,
        "peer",
    )));
    assert!(
        !host
            .inventory_summary()
            .contains(&format!("build-{}", "3".repeat(64)))
    );
    assert!(
        host.status_summary()
            .contains("node identityが一致しません")
    );
    assert!(
        !host
            .preparation_action(ManagerPreparationAction::StageProfile)
            .enabled
    );
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn enabled_preparation_action_goes_through_command_queue_with_local_ids() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut view_model = ManagerViewModel::new();
    view_model.set_expected_node_id("local-node");
    let mut host = ManagerWindowHost::for_test(client, view_model, ModelView::new());
    host.apply_event(ManagerEvent::Inventory(fixture_inventory(
        "local-node",
        false,
        "local",
    )));

    assert!(
        host.request_preparation_action(ManagerPreparationAction::BuildCoordinator)
            .expect("build action")
    );
    assert!(
        host.request_preparation_action(ManagerPreparationAction::StageProfile)
            .expect("stage action")
    );
    assert_eq!(
        host.drain_commands(),
        vec![
            ManagerCommand::Build {
                source_receipt_id: format!("source-{}", "a".repeat(40)),
                role: "ds4-server".to_string(),
            },
            ManagerCommand::Stage {
                build_artifact_id: format!("build-{}", "1".repeat(64)),
                model_artifact_id: format!("model-{}", "2".repeat(64)),
            },
        ]
    );
    assert!(
        !host
            .request_preparation_action(ManagerPreparationAction::Activate)
            .expect("activate disabled")
    );
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn inventory_api_failure_remains_failed_and_redacted_in_host_state() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new(), ModelView::new());
    host.apply_event(ManagerEvent::Failed {
        message: "inventory refresh failed: https://user:pw@example.invalid/?token=secret-token"
            .to_string(),
    });
    let rendered = format!("{} {}", host.status_summary(), host.inventory_summary());
    assert!(rendered.contains("inventory refresh failed"));
    assert!(rendered.contains("[REDACTED]"));
    for secret in ["pw", "secret-token"] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
}

fn fixture_job(id: &str, kind: &str, phase: &str, error: &str, cancel: bool) -> ManagerJobDto {
    ManagerJobDto {
        id: id.to_string(),
        kind: kind.to_string(),
        progress: 40,
        phase: phase.to_string(),
        error: error.to_string(),
        created_at: 0,
        updated_at: 0,
        cancel,
    }
}

fn fixture_model(name: &str, checksum: Option<&str>, encoder: &str, support: &str) -> ModelEntry {
    ModelEntry {
        name: name.to_string(),
        origin: None,
        size: Some(1),
        checksum: checksum.map(str::to_string),
        checksum_verified: false,
        registry_verified: false,
        pending_reason: None,
        license: "MIT".to_string(),
        encoder: encoder.to_string(),
        support: support.to_string(),
    }
}

fn verified_fixture_model(name: &str, encoder: &str, support: &str) -> ModelEntry {
    ModelEntry {
        checksum: Some(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        ),
        checksum_verified: true,
        registry_verified: true,
        ..fixture_model(name, None, encoder, support)
    }
}

fn transaction_ready_inventory() -> ManagerInventoryResponse {
    let mut inventory = fixture_inventory("local-node", true, "local");
    let profile = inventory.profiles.first_mut().expect("staged profile");
    profile.hardware_readiness = HardwareReadiness::Ready;
    profile.activation_ready = true;
    inventory.node_readiness = ManagerNodeReadinessDto {
        ready: true,
        reason: None,
    };
    inventory.active_digest = Some("f".repeat(64));
    inventory.previous_profile_id = Some("profile-previous".into());
    inventory.previous_release_ready = true;
    inventory.runtime = Some(ManagerRuntimeReadinessDto {
        cluster_enabled: true,
        generation: 7,
        state: "paired-standalone-ready".into(),
        desired_policy: "automatic".into(),
        applied_policy: "automatic".into(),
        policy_epoch: 3,
    });
    inventory.peer = Some(ready_peer_inventory());
    inventory
}

fn ready_peer_inventory() -> ManagerPeerInventoryDto {
    ManagerPeerInventoryDto {
        node_id: "remote-node".into(),
        node_role: "worker".into(),
        profiles: vec![ManagerPeerProfileDto {
            profile_id: "profile-remote".into(),
            node_role: "worker".into(),
            candidate_digest: "8".repeat(64),
            source_commit: "a".repeat(40),
            model_digest: "c".repeat(64),
            model_catalog_id: "catalog-ds4".into(),
            config_fingerprint: "d".repeat(64),
        }],
        active_digest: Some("e".repeat(64)),
        previous_profile_id: Some("previous-remote".into()),
        previous_digest: Some("7".repeat(64)),
        previous_release_ready: true,
        activation_phase: None,
        activation_failure_class: None,
    }
}

fn fixture_inventory(node_id: &str, staged: bool, prefix: &str) -> ManagerInventoryResponse {
    let commit = "a".repeat(40);
    let identity_byte = if prefix == "local" { "1" } else { "3" };
    let model_byte = if prefix == "local" { "2" } else { "4" };
    let build_id = format!("build-{}", identity_byte.repeat(64));
    let model_id = format!("model-{}", model_byte.repeat(64));
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
                id: build_id.clone(),
                kind: "build".to_string(),
                digest: "b".repeat(64),
                size: 10,
                verified: true,
                source_commit: Some(commit),
                role: Some("ds4-server".to_string()),
                catalog_id: None,
            },
            ManagerArtifactDto {
                id: model_id.clone(),
                kind: "model".to_string(),
                digest: "c".repeat(64),
                size: 20,
                verified: true,
                source_commit: None,
                role: None,
                catalog_id: Some("catalog-ds4".to_string()),
            },
        ],
        profiles: staged
            .then(|| ManagerStagedProfileDto {
                profile_id: format!("profile-{prefix}"),
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
            })
            .into_iter()
            .collect(),
        node_readiness: ManagerNodeReadinessDto {
            ready: false,
            reason: staged.then(|| "hardware readiness is pending".to_string()),
        },
        active_digest: None,
        previous_digest: Some("e".repeat(64)),
        previous_profile_id: Some("profile-previous".to_string()),
        previous_release_ready: true,
        activation_phase: None,
        activation_failure_class: None,
        runtime: None,
        peer: None,
    }
}

#[cfg(any(not(target_os = "macos"), not(feature = "test-support")))]
#[test]
fn manager_window_host_is_macos_only() {
    // The production binary is a macOS menu-bar app. Keep the integration
    // target buildable on other hosts without pretending to run AppKit there.
}
