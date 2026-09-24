//! H06 Manager window host contract.

use siderostat_monitor::client::MetricsClient;
use siderostat_monitor::config::MonitorConfig;
use siderostat_monitor::manager_window::{
    ManagerCommand, ManagerEvent, ManagerViewModel, ManagerWindowHost, ModelEntry, ModelView,
    ProfileReadiness,
};

use siderostat_core::manager::api::{ManagerJobDto, ManagerStatusResponse};

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn manager_host_reuses_one_window_and_keeps_commands_local() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new());

    host.show_or_focus().expect("show window");
    let first_identity = host.window_identity();
    host.show_or_focus().expect("focus window");
    assert_eq!(host.window_identity(), first_identity);
    assert!(host.is_visible());

    host.send_command(ManagerCommand::FetchSource)
        .expect("send fetch");
    host.send_command(ManagerCommand::Build {
        target: "ds4-server".to_string(),
    })
    .expect("send build");
    host.send_command(ManagerCommand::Download {
        profile: "mxfp4-0731".to_string(),
    })
    .expect("send download");
    host.send_command(ManagerCommand::Verify {
        profile: "mxfp4-0731".to_string(),
    })
    .expect("send verify");
    host.send_command(ManagerCommand::Stage {
        profile: "mxfp4-0731".to_string(),
    })
    .expect("send stage");
    host.send_command(ManagerCommand::Activate {
        profile: "mxfp4-0731".to_string(),
        expected_generation: 3,
        runtime_lease: "lease-3".to_string(),
    })
    .expect("send activate");
    host.send_command(ManagerCommand::Rollback {
        previous: "old-digest".to_string(),
        expected_generation: 3,
        runtime_lease: "lease-3".to_string(),
    })
    .expect("send rollback");

    assert_eq!(host.drain_commands().len(), 7);
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
    host.hide();
}

#[cfg(all(target_os = "macos", feature = "test-support"))]
#[test]
fn manager_fixture_matrix_keeps_running_cancelled_terminal_and_old_active() {
    let client = MetricsClient::new(&MonitorConfig::default()).expect("client");
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new());
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
fn profile_fixture_matrix_marks_pending_and_rejects_incompatible_profiles() {
    let mut view = ModelView::new();
    view.set_models(vec![
        fixture_model("mxfp4-0731", None, "openai-whisper", "supported"),
        fixture_model("vision-bad", Some("sha"), "other-encoder", "supported"),
        fixture_model(
            "glm-unsupported",
            Some("sha"),
            "openai-whisper",
            "unsupported",
        ),
        fixture_model("prefix-bad", Some("sha"), "openai-whisper", "supported"),
        fixture_model("q2-ready", Some("sha"), "openai-whisper", "supported"),
    ]);
    view.set_prefix_file_compatible("prefix-bad", false);

    assert_eq!(
        view.profile_readiness("mxfp4-0731"),
        ProfileReadiness::Pending("checksum未検証".to_string())
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
    let mut host = ManagerWindowHost::for_test(client, ManagerViewModel::new());
    assert!(host.profiles_summary().contains("profile未準備"));
    host.set_model_view(view);
    let rendered = host.profiles_summary();
    for reason in [
        "checksum未検証",
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
        size: 1,
        checksum: checksum.map(str::to_string),
        license: "MIT".to_string(),
        encoder: encoder.to_string(),
        support: support.to_string(),
    }
}

#[cfg(any(not(target_os = "macos"), not(feature = "test-support")))]
#[test]
fn manager_window_host_is_macos_only() {
    // The production binary is a macOS menu-bar app. Keep the integration
    // target buildable on other hosts without pretending to run AppKit there.
}
