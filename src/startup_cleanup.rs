use crate::{
    cluster::{ProcessController, discover_startup_processes},
    notify::{Notification, NotifyPlatform, build_notifier, gui_session_available},
};
use anyhow::Context;
use std::{path::Path, time::Duration};

const STARTUP_CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const STARTUP_CLEANUP_COUNTDOWN: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct StartupCleanupOptions {
    pub decline: bool,
    pub auto_restart: bool,
    pub notifications_enabled: bool,
    pub notification_sound: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupCleanupOutcome {
    NoCandidates,
    Approved { count: usize },
    Declined { count: usize },
}

/// Detect and terminate stale local siderostat or DS4 processes after the startup notification
/// countdown. The default action is approval; an explicit CLI/config opt-out declines cleanup.
pub async fn cleanup_startup_processes(
    current_pid: u32,
    configured_ds4_binary: &Path,
    controller: &ProcessController,
    stop_timeout: Duration,
    options: StartupCleanupOptions,
) -> anyhow::Result<StartupCleanupOutcome> {
    let candidates = discover_startup_processes(current_pid, configured_ds4_binary)
        .context("failed to inspect local startup processes")?;
    if candidates.is_empty() {
        return Ok(StartupCleanupOutcome::NoCandidates);
    }

    if !confirm_cleanup(options).await? {
        tracing::warn!(count = candidates.len(), "startup process cleanup declined");
        return Ok(StartupCleanupOutcome::Declined {
            count: candidates.len(),
        });
    }

    for candidate in &candidates {
        controller
            .force_stop_approved(
                &candidate.identity(),
                stop_timeout,
                STARTUP_CLEANUP_POLL_INTERVAL,
            )
            .await
            .with_context(|| {
                format!(
                    "startup cleanup failed for {} pid {}",
                    candidate.kind.name(),
                    candidate.observed.pid
                )
            })?;
    }
    tracing::info!(
        count = candidates.len(),
        "stale startup processes stopped after restart notice"
    );
    Ok(StartupCleanupOutcome::Approved {
        count: candidates.len(),
    })
}

async fn confirm_cleanup(options: StartupCleanupOptions) -> anyhow::Result<bool> {
    if options.decline || !options.auto_restart {
        notify_startup_cleanup("既存プロセスを検出しました。起動を中止しました。", options).await;
        return Ok(false);
    }

    notify_startup_cleanup("再起動が必要です（5秒後）", options).await;
    tokio::time::sleep(STARTUP_CLEANUP_COUNTDOWN).await;
    Ok(true)
}

async fn notify_startup_cleanup(message: &str, options: StartupCleanupOptions) {
    let notifier = build_notifier(
        options.notifications_enabled,
        options.notification_sound,
        NotifyPlatform::detect(),
    );
    if !gui_session_available().await {
        tracing::warn!("startup cleanup notification unavailable outside an Aqua GUI session");
        return;
    }
    if let Err(error) = notifier
        .notify(Notification::new("siderostat", message))
        .await
    {
        tracing::warn!(error = %error, "startup cleanup notification failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_cleanup_options_default_to_auto_restart() {
        let options = StartupCleanupOptions {
            decline: false,
            auto_restart: true,
            notifications_enabled: true,
            notification_sound: true,
        };
        assert!(!options.decline);
        assert!(options.auto_restart);
    }
}
