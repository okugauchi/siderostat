//! DS4 Manager — owned process group 実行。M03。
//!
//! C04 に基づき、build を最小環境（secret 無し・隔離 workspace）の owned
//! process group で spawn し、cancel 時は group を回収して partial output を
//! 非公開にする。disk 不足は開始前失敗。M03。
//!
//! 受入 case（全て必須）:
//! - 入力: cancel/child fork → owned group 回収
//! - 入力: secret env → child へ未継承
//! - 入力: disk 不足 → 開始前失敗
//!   M03。
use std::collections::HashMap;
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// 実行結果。M03。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// 終了 status（exit code or signal）。M03。
    pub status: RunStatus,
    /// stdout（成功時）。M03。
    pub stdout: String,
    /// stderr。M03。
    pub stderr: String,
}

/// 終了 status。M03。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    /// 正常終了。M03。
    Exited(i32),
    /// シグナルで終了（cancel 等）。M03。
    Signaled,
}

/// process 実行エラー。M03。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// spawn 失敗。M03。
    Spawn(String),
    /// 開始前の disk 不足。M03。
    InsufficientDisk(String),
    /// 実行失敗（非零 exit）。M03。
    Failed(RunOutput),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessError::Spawn(msg) => write!(f, "spawn failed: {msg}"),
            ProcessError::InsufficientDisk(msg) => write!(f, "insufficient disk: {msg}"),
            ProcessError::Failed(out) => write!(f, "command failed: {:?}", out.status),
        }
    }
}

impl std::error::Error for ProcessError {}

/// 実行するコマンドの指定。M03。
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// 実行バイナリ（引数配列で渡す、shell 文字列不可）。M03。
    pub program: String,
    /// 引数配列。M03。
    pub args: Vec<String>,
    /// 作業ディレクトリ（隔離 workspace）。M03。
    pub cwd: PathBuf,
    /// 継承する最小 env（PATH 等。secret は含めない）。M03。
    pub env: HashMap<String, String>,
}

impl CommandSpec {
    /// 最小 env で構築（secret は含めない）。M03。
    pub fn minimal(program: impl Into<String>, args: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        let mut env = HashMap::new();
        // 最小 PATH（make 等の実行に必要）。secret は含めない。M03。
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        Self {
            program: program.into(),
            args,
            cwd: cwd.into(),
            env,
        }
    }

    /// 継承 env へ追加する（secret を渡さないこと）。M03。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

/// owned process group で実行し、cancel 時に group を回収する。M03。
pub struct GroupRunner {
    _marker: std::marker::PhantomData<()>,
}

impl Default for GroupRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupRunner {
    /// runner を構築する。M03。
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }

    /// 開始前に disk 空き容量を確認し、必要量を下回れば開始前失敗。M03。
    ///
    /// `path` の所在 filesystem の空き bytes を取得し、`needed_bytes` 未満なら
    /// `InsufficientDisk` を返す。M03。
    pub fn check_disk(path: &std::path::Path, needed_bytes: u64) -> Result<(), ProcessError> {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| ProcessError::InsufficientDisk("path has interior NUL".into()))?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            return Err(ProcessError::InsufficientDisk(format!(
                "statvfs({}): {}",
                path.display(),
                err
            )));
        }
        let available = stat.f_bavail as u64 * stat.f_frsize as u64;
        if available < needed_bytes {
            return Err(ProcessError::InsufficientDisk(format!(
                "need {} bytes, have {} bytes on {}",
                needed_bytes,
                available,
                path.display()
            )));
        }
        Ok(())
    }

    /// 引数配列でコマンドを owned process group として実行する。M03。
    ///
    /// 出力は別スレッドで収集し、メインループは `try_wait()` + cancel 監視
    /// を行う。cancel が set されたら owned process group（SIGTERM →
    /// SIGKILL）を回収する。secret env は継承しない（最小 env のみ）。
    /// partial output は公開せず、完了時のみ返す。M03。
    pub fn run_group(
        &self,
        spec: &CommandSpec,
        cancel: &AtomicBool,
    ) -> Result<RunOutput, ProcessError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        cmd.current_dir(&spec.cwd);
        // 最小 env（secret 未継承）。M03。
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // owned process group を作る（cancel で group を回収するため）。M03。
        cmd.process_group(0);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child: Child = cmd
            .spawn()
            .map_err(|e| ProcessError::Spawn(e.to_string()))?;
        let pid = child.id();

        let so = child.stdout.take();
        let se = child.stderr.take();
        let stdout_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Some(mut r) = so {
                let mut chunk = [0_u8; 8192];
                loop {
                    match r.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
            }
            buf
        });
        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Some(mut r) = se {
                let mut chunk = [0_u8; 8192];
                loop {
                    match r.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
            }
            buf
        });

        // メインループ: try_wait + cancel 監視。M03。
        let mut signaled = false;
        loop {
            if cancel.load(Ordering::SeqCst) {
                // owned process group を回収。M03。
                let _ = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
                signaled = true;
                // 短い待機後に SIGKILL。M03。
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
                loop {
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(e) => {
                    let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    return Err(ProcessError::Spawn(e.to_string()));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let stdout = stdout_handle.join().unwrap_or_default();
        let stderr = stderr_handle.join().unwrap_or_default();
        let status = child.wait();
        let run_status = match status {
            Ok(s) if s.success() => RunStatus::Exited(0),
            Ok(s) => {
                if s.signal().is_some() {
                    RunStatus::Signaled
                } else {
                    RunStatus::Exited(s.code().unwrap_or(1))
                }
            }
            Err(_) => RunStatus::Signaled,
        };

        // cancel で終了した場合、partial output は公開しない（非公開）。M03。
        if signaled && matches!(run_status, RunStatus::Signaled) {
            return Err(ProcessError::Failed(RunOutput {
                status: RunStatus::Signaled,
                stdout: String::new(),
                stderr: String::new(),
            }));
        }

        let out = RunOutput {
            status: run_status,
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        };
        if matches!(out.status, RunStatus::Exited(0)) {
            Ok(out)
        } else {
            Err(ProcessError::Failed(out))
        }
    }

    /// owned process group を回収する（cancel）。M03。
    pub fn cancel_group(pid: u32) {
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m03proc-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    /// secret env が child へ未継承であることを検証する。M03。
    ///
    /// 親で `MY_SECRET` を設定し、最小 env の child がそれを参照できない
    /// ことを確認する。M03。
    #[test]
    fn secret_env_not_inherited() {
        let base = tmp("secret");
        // 親で secret を設定。M03。
        unsafe {
            std::env::set_var("MY_SECRET_M03", "super-secret-value");
        }
        let spec = CommandSpec::minimal(
            "/bin/sh",
            vec!["-c".to_string(), "test -z \"$MY_SECRET_M03\"".to_string()],
            &base,
        );
        let cancel = AtomicBool::new(false);
        let out = GroupRunner::new().run_group(&spec, &cancel);
        unsafe {
            std::env::remove_var("MY_SECRET_M03");
        }
        // 最小 env（env_clear）なので child から secret が見えず、test -z が成功
        // する。M03。
        assert!(out.is_ok(), "secret must not be inherited: {out:?}");
    }

    /// cancel → owned group 回収（SIGTERM → SIGKILL）を検証する。M03。
    #[test]
    fn cancel_reaps_owned_group() {
        let base = tmp("cancel");
        // 子プロセス + その fork（sleep を起動）を owned group で起動し、
        // cancel で group が回収されることを確認。M03。
        let script = r#"
            sleep 60 &
            wait
        "#;
        let spec =
            CommandSpec::minimal("/bin/sh", vec!["-c".to_string(), script.to_string()], &base);
        let cancel = Arc::new(AtomicBool::new(false));
        // 別スレッドで 200ms 後に cancel を set。M03。
        let cancel_thread = Arc::clone(&cancel);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            cancel_thread.store(true, Ordering::SeqCst);
        });
        let result = GroupRunner::new().run_group(&spec, &cancel);
        handle.join().unwrap();
        // cancel された → 失敗（Signaled）で、partial output は非公開。M03。
        assert!(result.is_err(), "cancel must fail: {result:?}");
    }

    /// disk 不足 → 開始前失敗。M03。
    #[test]
    fn insufficient_disk_fails_before_start() {
        // 巨大な必要量（現実に空かない量）を要求 → InsufficientDisk。M03。
        let base = tmp("disk");
        let huge = 1_u64 << 62; // ~4 EiB
        let err = GroupRunner::check_disk(&base, huge).expect_err("must fail");
        assert!(matches!(err, ProcessError::InsufficientDisk(_)));
    }
}
