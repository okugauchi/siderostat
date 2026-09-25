//! DS4 Manager — 公式 source fetch と commit 固定。M02。
//!
//! C04 に基づき、公式 pin 済み source を cache へ fetch し、full commit と
//! main ancestry を証明する full commit SHA を記録して `SourceRecord` を返す。git command は引数配列で
//! 実行し、shell 文字列を受理しない。submodule/filter/hook など外部実行
//! 経路を制限する。利用者の DS4 checkout・branch・global Git config を変更
//! しない。取得候補（candidate）と active commit は分離され、fetch だけで
//! activation されない。非 main commit は reference として記録する。M02。
//!
//! 受入 case（全て必須）:
//! - 入力: main 差分 → candidate 追加のみ（fetch だけで activation されない）
//! - 入力: 非 main commit → reference
//! - 入力: network 失敗 → 旧 candidate 保持
//! - 入力: 悪意ある revision/remote → 拒否
//!
//! レビュー重点: submodule/filter/hook など外部実行経路を制限。task test は
//! ローカル fixture remote 限定。M02。
//!
//! 契約: CONTRACTS.md C04 / SourceRecord・stage_source。M02。
use super::registry::SourceRecord;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{io::Read, os::unix::process::CommandExt};

/// source fetch のエラー。M02。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// A requested cancellation stopped the owned git process group.
    Canceled,
    /// remote が公式 pin 済み remote と一致しない。M02。
    UnapprovedRemote,
    /// revision が安全でない（shell 展開・`--upload-pack` 等）。M02。
    UnsafeRevision,
    /// cache path が通常の bare repository directory ではない。
    UnsafeCache,
    /// git command の実行失敗。M02。
    GitFailed(String),
    /// 対象 commit が main の祖先でない（reference 扱い）。M02。
    NotOnMain,
    /// 取得候補がまだ無い（network 失敗時の旧 candidate 保持）。M02。
    NoCandidate,
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Canceled => write!(f, "source fetch canceled"),
            SourceError::UnapprovedRemote => write!(f, "remote is not an approved official source"),
            SourceError::UnsafeRevision => write!(f, "unsafe revision"),
            SourceError::UnsafeCache => write!(f, "unsafe source cache path"),
            SourceError::GitFailed(msg) => write!(f, "git failed: {msg}"),
            SourceError::NotOnMain => write!(f, "commit is not on main"),
            SourceError::NoCandidate => write!(f, "no source candidate available"),
        }
    }
}

impl std::error::Error for SourceError {}

/// 公式 pin 済み remote。M02。
/// テストはローカル fixture remote に差し替える（review 重点）。M02。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialRemote {
    /// 公式 remote の完全一致 URL。M02。
    url: String,
}

impl OfficialRemote {
    /// 公式 remote を固定する。M02。
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    /// URL が公式 remote と完全一致するか。M02。
    pub fn matches(&self, url: &str) -> bool {
        self.url == url
    }
}

/// git command を引数配列で実行する薄いラッパー。M02。
///
/// shell 文字列を受理しない（引数配列で渡す）。submodule/filter/hook など
/// 外部実行経路を制限し、global Git config を無効化して利用者の設定を
/// 変更しない。M02。
#[derive(Debug, Clone)]
pub struct GitRunner {
    /// 実行する git の path。M02。
    git: PathBuf,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::new("git")
    }
}

impl GitRunner {
    /// git 実行器を構築する。M02。
    pub fn new(git: impl Into<PathBuf>) -> Self {
        Self { git: git.into() }
    }

    /// 引数配列で git を実行し、成功時 stdout を返す。M02。
    ///
    /// 環境を最小化する:
    /// - GIT_CONFIG_GLOBAL / GIT_CONFIG_SYSTEM を空にして利用者の設定を読まない
    /// - core.hooksPath=/dev/null で hook を無効化
    /// - submodule の外部実行は行わない（--recurse-submodules を渡さない）
    /// - GIT_TERMINAL_PROMPT=0 で対話入力を無効化
    ///   M02。
    pub fn run<I, S>(&self, args: I) -> Result<String, SourceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self
            .command(args)
            .output()
            .map_err(|e| SourceError::GitFailed(format!("spawn: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SourceError::GitFailed(stderr.trim().to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut cmd = Command::new(&self.git);
        // 外部実行経路の制限と環境最小化。M02。
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "protocol.file.allow=always"])
            .args(args);
        cmd
    }

    /// Run git in an owned process group so cancellation also stops helpers
    /// started by git (for example a network transport), then reap git.
    pub fn run_cancellable<I, S>(&self, args: I, cancel: &AtomicBool) -> Result<String, SourceError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        if cancel.load(Ordering::SeqCst) {
            return Err(SourceError::Canceled);
        }
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| SourceError::GitFailed(format!("spawn: {e}")))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
        let stderr_reader = std::thread::spawn(move || read_pipe(stderr));
        let status = loop {
            if cancel.load(Ordering::SeqCst) {
                terminate_process_group(&mut child);
                let _ = child.wait();
                // A helper may have been forked while SIGKILL was in flight.
                // Keep killing the owned group until inherited pipes close.
                while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
                    terminate_process_group(&mut child);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(SourceError::Canceled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    terminate_process_group(&mut child);
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(SourceError::GitFailed(format!("wait: {error}")));
                }
            }
        };
        // Git can exit before a helper inheriting its pipes does. Keep the
        // cancellation boundary active until both output readers have exited.
        while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
            if cancel.load(Ordering::SeqCst) {
                terminate_process_group(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(SourceError::Canceled);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let stdout = stdout_reader
            .join()
            .map_err(|_| SourceError::GitFailed("stdout reader panicked".into()))?
            .map_err(|e| SourceError::GitFailed(format!("stdout: {e}")))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| SourceError::GitFailed("stderr reader panicked".into()))?
            .map_err(|e| SourceError::GitFailed(format!("stderr: {e}")))?;
        if cancel.load(Ordering::SeqCst) {
            return Err(SourceError::Canceled);
        }
        if !status.success() {
            return Err(SourceError::GitFailed(
                String::from_utf8_lossy(&stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.read_to_end(&mut output)?;
    Ok(output)
}

fn terminate_process_group(child: &mut std::process::Child) {
    let pid = child.id();
    if pid <= i32::MAX as u32 {
        // SAFETY: `process_group(0)` made the child PID the process-group ID.
        // A negative PID sends SIGKILL to that owned group, including helpers.
        if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } == 0 {
            return;
        }
    }
    let _ = child.kill();
}

fn validate_bare_cache_layout(cache: &Path) -> Result<(), SourceError> {
    for relative in ["HEAD", "config"] {
        match std::fs::symlink_metadata(cache.join(relative)) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(SourceError::UnsafeCache);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SourceError::GitFailed(format!("cache: {error}"))),
        }
    }
    for relative in [
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
        "refs/remotes",
    ] {
        match std::fs::symlink_metadata(cache.join(relative)) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(SourceError::UnsafeCache);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SourceError::GitFailed(format!("cache: {error}"))),
        }
    }
    Ok(())
}

/// revision を安全か検証する。M02。
///
/// git の引数として安全でないパターン（`--upload-pack`、`-c`、shell 展開、
/// 空白を含む複雑な引数）を拒否する。M02。
fn validate_revision(revision: &str) -> Result<(), SourceError> {
    if revision.is_empty() {
        return Err(SourceError::UnsafeRevision);
    }
    // `--` で始まるオプション（`--upload-pack` 等）を拒否。M02。
    if revision.starts_with('-') {
        return Err(SourceError::UnsafeRevision);
    }
    // 空白・制御文字・`;`/`|`/`&` など shell 展開に使われる文字を拒否。M02。
    if revision.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, ';' | '|' | '&' | '$' | '`' | '>' | '<')
    }) {
        return Err(SourceError::UnsafeRevision);
    }
    Ok(())
}

/// source を cache へ fetch して commit を固定する。M02。
///
/// - `official` の remote と一致しない remote は拒否（悪意ある remote → 拒否）。
/// - revision を検証し、安全でないものは拒否（悪意ある revision → 拒否）。
/// - cache（bare）へ fetch し、full commit を解決する。
/// - `main_ref`（例 `refs/heads/main`）の祖先かを検証。非 main は
///   reference 扱い（NotOnMain）。M02。
pub fn stage_source(
    cache: &Path,
    official: &OfficialRemote,
    remote: &str,
    revision: &str,
    main_ref: &str,
) -> Result<SourceRecord, SourceError> {
    stage_source_cancellable(
        cache,
        official,
        remote,
        revision,
        main_ref,
        &AtomicBool::new(false),
    )
}

/// Cancellable source fetch for the manager executor. Every git command is
/// owned and stopped before this function reports cancellation.
pub fn stage_source_cancellable(
    cache: &Path,
    official: &OfficialRemote,
    remote: &str,
    revision: &str,
    main_ref: &str,
    cancel: &AtomicBool,
) -> Result<SourceRecord, SourceError> {
    // 公式 remote 固定。M02。
    if !official.matches(remote) {
        return Err(SourceError::UnapprovedRemote);
    }
    // revision 検証（悪意ある revision → 拒否）。M02。
    validate_revision(revision)?;

    // cache を bare で初期化（既にあれば fetch のみ）。M02。
    let runner = GitRunner::default();
    match std::fs::symlink_metadata(cache) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(SourceError::UnsafeCache);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(SourceError::GitFailed(format!("cache: {error}"))),
    }
    validate_bare_cache_layout(cache)?;
    if !cache.join("HEAD").exists() {
        runner.run_cancellable(
            ["init", "--bare", cache.to_str().unwrap_or_default()],
            cancel,
        )?;
    }
    validate_bare_cache_layout(cache)?;

    // main_ref を cache のローカル ref に取り込む（main ancestry 検証用）。
    // fetch だけで activation されない（SourceRecord を返すだけ）。M02。
    let main_fetch = runner.run_cancellable(
        [
            "--git-dir",
            cache.to_str().unwrap_or_default(),
            "fetch",
            remote,
            &format!("{main_ref}:{main_ref}"),
        ],
        cancel,
    );
    if main_fetch == Err(SourceError::Canceled) {
        return Err(SourceError::Canceled);
    }
    main_fetch?;

    // Pin the ancestry proof to the exact fetched main commit. A ref name alone
    // can move after this fetch and is not sufficient durable provenance.
    let main_proof = runner.run_cancellable(
        [
            "--git-dir",
            cache.to_str().unwrap_or_default(),
            "rev-parse",
            "--verify",
            &format!("{main_ref}^{{commit}}"),
        ],
        cancel,
    )?;
    if main_proof.is_empty() {
        return Err(SourceError::GitFailed("empty main proof".into()));
    }

    // fetch。network 失敗時は Err を返し、旧 candidate は保持される。M02。
    runner.run_cancellable(
        [
            "--git-dir",
            cache.to_str().unwrap_or_default(),
            "fetch",
            remote,
            revision,
        ],
        cancel,
    )?;

    // full commit を解決する。M02。
    let full_commit = runner.run_cancellable(
        [
            "--git-dir",
            cache.to_str().unwrap_or_default(),
            "rev-parse",
            "FETCH_HEAD",
        ],
        cancel,
    )?;
    if full_commit.is_empty() {
        return Err(SourceError::GitFailed("empty full commit".into()));
    }

    // main ancestry を検証する。非 main は reference 扱い（NotOnMain）。M02。
    let is_ancestor = runner.run_cancellable(
        [
            "--git-dir",
            cache.to_str().unwrap_or_default(),
            "merge-base",
            "--is-ancestor",
            &full_commit,
            &main_proof,
        ],
        cancel,
    );
    if is_ancestor == Err(SourceError::Canceled) {
        return Err(SourceError::Canceled);
    }
    if is_ancestor.is_err() {
        // merge-base --is-ancestor は非祖先のとき exit 1 を返す。M02。
        return Err(SourceError::NotOnMain);
    }

    Ok(SourceRecord {
        remote: remote.to_string(),
        full_commit,
        main_proof,
        fetched_at: now_secs(),
    })
}

/// 現在の epoch secs。M02。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_git_command_reaps_its_process_group() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let base = tmp("canceled-runner");
        std::fs::create_dir_all(&base).unwrap();
        let marker = base.join("started");
        let script = base.join("slow-git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nsleep 5 &\nprintf '%s' \"$$\" > '{}'\nwait\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let thread = std::thread::spawn(move || {
            GitRunner::new(script).run_cancellable(["ignored"], &thread_cancel)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "fixture process started");
            std::thread::yield_now();
        };
        let started = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(thread.join().unwrap(), Err(SourceError::Canceled));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel returns before sleep completes"
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "direct child was reaped");
        std::fs::remove_dir_all(base).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m02-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    /// ローカル fixture remote を作る。M02。
    ///
    /// remote に main branch（2 commit）と非 main branch（1 commit）を作る。
    /// M02。
    fn make_fixture_remote(dir: &Path) {
        // clone --bare の宛先 dir は存在してはいけない（create しない）。M02。
        // work tree は base（dir の親）内に置くが、並列実行で他テストと競合
        // しないよう base の file name を含む tag 固有の名前にする。M02。
        // dir 自体は常に "remote" なので file_name では区別できない。
        let runner = GitRunner::default();
        let base = dir.parent().expect("parent");
        let base_tag = base.file_name().and_then(|s| s.to_str()).unwrap_or("work");
        let work = base.join(format!("{base_tag}-work"));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).expect("create work");
        runner
            .run(["-C", work.to_str().unwrap(), "init", "-b", "main"])
            .expect("init work");
        runner
            .run([
                "-C",
                work.to_str().unwrap(),
                "config",
                "user.email",
                "test@example.com",
            ])
            .expect("config email");
        runner
            .run(["-C", work.to_str().unwrap(), "config", "user.name", "test"])
            .expect("config name");
        std::fs::write(work.join("a.txt"), "a").expect("write a");
        runner
            .run(["-C", work.to_str().unwrap(), "add", "a.txt"])
            .expect("add a");
        runner
            .run(["-C", work.to_str().unwrap(), "commit", "-m", "first"])
            .expect("commit first");
        std::fs::write(work.join("b.txt"), "b").expect("write b");
        runner
            .run(["-C", work.to_str().unwrap(), "add", "b.txt"])
            .expect("add b");
        runner
            .run(["-C", work.to_str().unwrap(), "commit", "-m", "second"])
            .expect("commit second");
        // 非 main branch。M02。/
        runner
            .run(["-C", work.to_str().unwrap(), "checkout", "-b", "feature"])
            .expect("checkout feature");
        std::fs::write(work.join("f.txt"), "f").expect("write f");
        runner
            .run(["-C", work.to_str().unwrap(), "add", "f.txt"])
            .expect("add f");
        runner
            .run(["-C", work.to_str().unwrap(), "commit", "-m", "feature"])
            .expect("commit feature");
        runner
            .run(["-C", work.to_str().unwrap(), "checkout", "main"])
            .expect("checkout main");
        // bare remote へ push。M02。
        runner
            .run([
                "-C",
                work.to_str().unwrap(),
                "clone",
                "--bare",
                ".",
                dir.to_str().unwrap(),
            ])
            .expect("clone bare");
    }

    /// main の commit を fetch → candidate が記録される。fetch だけで active に
    /// ならない（SourceRecord を返すだけで registry の state は触らない）。M02。
    #[test]
    fn fetch_main_records_candidate_not_activation() {
        let base = tmp("main");
        let remote = base.join("remote");
        make_fixture_remote(&remote);
        let cache = base.join("cache");
        let official = OfficialRemote::new(remote.to_str().unwrap());
        let rec = stage_source(
            &cache,
            &official,
            remote.to_str().unwrap(),
            "main",
            "refs/heads/main",
        )
        .expect("stage main");
        assert_eq!(rec.remote, remote.to_str().unwrap());
        assert!(!rec.full_commit.is_empty());
        // main の祖先 → candidate（NotOnMain ではない）。M02。
        assert_eq!(rec.main_proof, rec.full_commit);
    }

    /// 非 main commit → reference（NotOnMain）。M02。
    #[test]
    fn non_main_commit_is_reference() {
        let base = tmp("nonmain");
        let remote = base.join("remote");
        make_fixture_remote(&remote);
        let cache = base.join("cache");
        let official = OfficialRemote::new(remote.to_str().unwrap());
        // feature branch の commit を fetch → main の祖先でない → reference。M02。
        let err = stage_source(
            &cache,
            &official,
            remote.to_str().unwrap(),
            "refs/heads/feature",
            "refs/heads/main",
        )
        .expect_err("non-main must be reference");
        assert_eq!(err, SourceError::NotOnMain);
    }

    /// network 失敗 → 旧 candidate 保持。M02。
    ///
    /// 存在しない remote への fetch は失敗し、SourceError を返す（旧 candidate
    /// を壊さない）。M02。
    #[test]
    fn network_failure_keeps_no_candidate() {
        let base = tmp("netfail");
        let cache = base.join("cache");
        let official = OfficialRemote::new("http://127.0.0.1:1/nonexistent.git");
        let err = stage_source(
            &cache,
            &official,
            "http://127.0.0.1:1/nonexistent.git",
            "main",
            "refs/heads/main",
        )
        .expect_err("network failure must fail");
        assert!(matches!(err, SourceError::GitFailed(_)));
        // 旧 candidate は保持される（cache は bare として有効なまま）。M02。
        // init 済みなら HEAD が存在する（壊れていない）。M02。
        assert!(cache.join("HEAD").exists(), "cache stays a valid bare repo");
    }

    /// 悪意ある remote → 拒否。M02。
    #[test]
    fn malicious_remote_rejected() {
        let base = tmp("badremote");
        let remote = base.join("remote");
        make_fixture_remote(&remote);
        let cache = base.join("cache");
        let official = OfficialRemote::new(remote.to_str().unwrap());
        // 公式 remote と違う URL → 拒否。M02。
        let err = stage_source(
            &cache,
            &official,
            "http://evil.example.com/repo.git",
            "main",
            "refs/heads/main",
        )
        .expect_err("unapproved remote must be rejected");
        assert_eq!(err, SourceError::UnapprovedRemote);
    }

    /// 悪意ある revision → 拒否。M02。
    #[test]
    fn malicious_revision_rejected() {
        let base = tmp("badrev");
        let remote = base.join("remote");
        make_fixture_remote(&remote);
        let cache = base.join("cache");
        let official = OfficialRemote::new(remote.to_str().unwrap());
        // `--upload-pack` 等のオプション注入 → 拒否。M02。
        let err = stage_source(
            &cache,
            &official,
            remote.to_str().unwrap(),
            "--upload-pack=evil",
            "refs/heads/main",
        )
        .expect_err("unsafe revision must be rejected");
        assert_eq!(err, SourceError::UnsafeRevision);
        // 空白を含む revision → 拒否。M02。
        let err2 = stage_source(
            &cache,
            &official,
            remote.to_str().unwrap(),
            "main; rm -rf /",
            "refs/heads/main",
        )
        .expect_err("shell-like revision must be rejected");
        assert_eq!(err2, SourceError::UnsafeRevision);
    }
}
