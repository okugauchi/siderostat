//! H06 Manager window host contract.

use siderostat_monitor::client::MetricsClient;
use siderostat_monitor::config::MonitorConfig;
use siderostat_monitor::manager_window::{
    ManagerCommand, ManagerEvent, ManagerViewModel, ManagerWindowHost,
};

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

#[cfg(any(not(target_os = "macos"), not(feature = "test-support")))]
#[test]
fn manager_window_host_is_macos_only() {
    // The production binary is a macOS menu-bar app. Keep the integration
    // target buildable on other hosts without pretending to run AppKit there.
}
