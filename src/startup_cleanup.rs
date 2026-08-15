use crate::cluster::{ProcessController, StartupProcessCandidate, discover_startup_processes};
use anyhow::Context;
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    time::Duration,
};

const STARTUP_CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupCleanupOutcome {
    NoCandidates,
    Approved { count: usize },
    Declined { count: usize },
}

/// Detect and, only after an explicit operator confirmation, terminate stale local siderostat or
/// DS4 processes before the new instance acquires its state lock and listeners.
pub async fn cleanup_startup_processes(
    current_pid: u32,
    configured_ds4_binary: &Path,
    controller: &ProcessController,
    stop_timeout: Duration,
) -> anyhow::Result<StartupCleanupOutcome> {
    let candidates = discover_startup_processes(current_pid, configured_ds4_binary)
        .context("failed to inspect local startup processes")?;
    if candidates.is_empty() {
        return Ok(StartupCleanupOutcome::NoCandidates);
    }

    if !confirm_cleanup(&candidates).await? {
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
                    "approved startup cleanup failed for {} pid {}",
                    candidate.kind.name(),
                    candidate.observed.pid
                )
            })?;
    }
    tracing::info!(
        count = candidates.len(),
        "approved stale startup processes stopped"
    );
    Ok(StartupCleanupOutcome::Approved {
        count: candidates.len(),
    })
}

async fn confirm_cleanup(candidates: &[StartupProcessCandidate]) -> anyhow::Result<bool> {
    let message = cleanup_message(candidates);
    #[cfg(target_os = "macos")]
    {
        if let Some(approved) = confirm_with_osascript(&message).await? {
            return Ok(approved);
        }
    }
    confirm_with_tty(message).await
}

fn cleanup_message(candidates: &[StartupProcessCandidate]) -> String {
    let mut message =
        String::from("起動前に、siderostat が管理対象とみなす既存プロセスを検出しました。\n\n");
    message.push_str("「強制終了」を選ぶと、表示されたプロセスを身元確認のうえ停止します。\n");
    message.push_str("拒否した場合、siderostat は安全のため起動しません。\n\n");
    for candidate in candidates {
        let command = truncate(&candidate.command_line(), 360);
        message.push_str(&format!(
            "[{}] pid={} start={}\n{}\n\n",
            candidate.kind.name(),
            candidate.observed.pid,
            candidate.observed.start_time_micros,
            command,
        ));
    }
    message
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub("…".len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &value[..end])
}

#[cfg(target_os = "macos")]
async fn confirm_with_osascript(message: &str) -> anyhow::Result<Option<bool>> {
    let script = format!(
        "display dialog \"{}\" with title \"siderostat 起動確認\" buttons {{\"拒否\", \"強制終了\"}} default button \"拒否\"",
        escape_apple_script(message)
    );
    let output = tokio::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(error = %error, "startup confirmation dialog unavailable");
            return Ok(None);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("User canceled") {
            return Ok(Some(false));
        }
        tracing::warn!(error = %stderr.trim(), "startup confirmation dialog failed");
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).contains("強制終了"),
    ))
}

#[cfg(target_os = "macos")]
fn escape_apple_script(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

async fn confirm_with_tty(message: String) -> anyhow::Result<bool> {
    if !io::stdin().is_terminal() {
        tracing::warn!(
            "startup process cleanup requires explicit confirmation, but no interactive terminal is available"
        );
        return Ok(false);
    }
    tokio::task::spawn_blocking(move || {
        print!("{message}\n強制終了して続行しますか？ [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok::<bool, io::Error>(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    })
    .await
    .context("startup confirmation task failed")?
    .context("failed to read startup confirmation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{ObservedProcess, StartupProcessKind};
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn message_contains_process_kind_pid_and_command_without_logging_it() {
        let candidate = StartupProcessCandidate {
            kind: StartupProcessKind::Ds4,
            observed: ObservedProcess {
                pid: 42,
                executable: PathBuf::from("/Users/o/LLM/ds4/ds4-server"),
                argv: vec![OsString::from("--model"), OsString::from("/tmp/model.gguf")],
                start_time_micros: 123,
            },
        };
        let message = cleanup_message(&[candidate]);
        assert!(message.contains("ds4-server"));
        assert!(message.contains("pid=42"));
        assert!(message.contains("--model /tmp/model.gguf"));
    }

    #[test]
    fn long_command_is_truncated_at_a_utf8_boundary() {
        let value = "あ".repeat(300);
        let truncated = truncate(&value, 32);
        assert!(truncated.len() <= 32);
        assert!(truncated.ends_with('…'));
    }
}
