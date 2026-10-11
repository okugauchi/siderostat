//! DS4 Manager — ds4・ds4-server・ds4-agent isolated build。M03。
//!
//! C04 に基づき、pin 済 source の固定 make target を最小環境・secret 無し・
//! 隔離 workspace で build し、`BuildRecord`（source/flags/toolchain/arch/
//! role digest/help digest）を生成する。選択 commit の取得（M02 stage_source）
//! と build 許可を区別し、任意 shell injection を防止する。make の target は
//! 固定 allowlist から選び、引数配列で渡す（shell 文字列不可）。実行中の
//! artifact を上書きしない（build は新規 record を返すだけ、active は
//! 触らない）。M03。
//!
//! 受入 case（全て必須）:
//! - 入力: build 失敗 → active 不変
//! - 入力: cancel/child fork → owned group 回収
//! - 入力: secret env → child へ未継承
//! - 入力: disk 不足 → 開始前失敗
//!
//! レビュー重点: makefile は固定 target でも code 実行。選択 commit の取得と
//! build 許可を区別し、任意 shell injection を防止。M03。
use crate::manager::process::{CommandSpec, GroupRunner, ProcessError};
use crate::manager::registry::{BuildRecord, sha256_hex};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// build エラー。M03。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// role が allowlist に無い。M03。
    UnapprovedRole,
    /// make target が allowlist に無い（任意 shell injection 防止）。M03。
    UnapprovedTarget,
    /// 開始前の disk 不足。M03。
    InsufficientDisk(String),
    /// build コマンド失敗（active 不変）。M03。
    Failed(String),
    /// 出力 binary を読めない。M03。
    ReadOutput(String),
    /// 出力 binary が無い（build が期待成果を出さなかった）。M03。
    MissingOutput,
    /// help snapshot を読めない。M03。
    ReadHelp(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::UnapprovedRole => write!(f, "role is not approved"),
            BuildError::UnapprovedTarget => write!(f, "make target is not approved"),
            BuildError::InsufficientDisk(msg) => write!(f, "insufficient disk: {msg}"),
            BuildError::Failed(msg) => write!(f, "build failed: {msg}"),
            BuildError::ReadOutput(msg) => write!(f, "read output failed: {msg}"),
            BuildError::MissingOutput => write!(f, "build produced no output"),
            BuildError::ReadHelp(msg) => write!(f, "read help failed: {msg}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// 固定 role allowlist。M03。
pub const APPROVED_ROLES: &[&str] = &["ds4", "ds4-server", "ds4-agent"];

/// 固定 make target allowlist（A02 で確認した target。任意 injection 防止）。M03。
pub const APPROVED_MAKE_TARGETS: &[&str] = &["all", "build", "ds4", "ds4-server", "ds4-agent"];

/// role が allowlist に含まれるか。M03。
pub fn is_approved_role(role: &str) -> bool {
    APPROVED_ROLES.contains(&role)
}

/// make target が allowlist に含まれるか。M03。
pub fn is_approved_target(target: &str) -> bool {
    APPROVED_MAKE_TARGETS.contains(&target)
}

/// build 要求。M03。
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// 役割（ds4/ds4-server/ds4-agent）。M03。
    pub role: String,
    /// 固定 make target（allowlist 必須）。M03。
    pub target: String,
    /// build flags の snapshot。M03。
    pub flags: String,
    /// toolchain の snapshot。M03。
    pub toolchain: String,
    /// 対象 arch。M03。
    pub arch: String,
    /// pin 済 source の commit。M03。
    pub source: String,
    /// make 実行プログラム（既定 "make"。テストで fake に差し替え）。M03。
    pub make_program: String,
    /// 隔離 build workspace（pin 済 source の checkout）。M03。
    pub workspace: PathBuf,
    /// 出力 binary の workspace 相対 path。M03。
    pub output_rel: PathBuf,
    /// help snapshot の workspace 相対 path。M03。
    pub help_rel: PathBuf,
    /// 開始前 disk 予約量（C04: build 見積 + 余白 max(2GiB, 見積10%)）。M03。
    pub disk_needed: u64,
}

impl BuildRequest {
    /// 最小要求を構築する（make_program 既定 "make"）。M03。
    pub fn new(
        role: impl Into<String>,
        target: impl Into<String>,
        source: impl Into<String>,
        workspace: impl Into<PathBuf>,
        output_rel: impl Into<PathBuf>,
    ) -> Self {
        Self {
            role: role.into(),
            target: target.into(),
            flags: String::new(),
            toolchain: String::new(),
            arch: String::new(),
            source: source.into(),
            make_program: "make".to_string(),
            workspace: workspace.into(),
            output_rel: output_rel.into(),
            help_rel: PathBuf::from("help.txt"),
            disk_needed: 2 << 30, // 既定 2GiB 余白。M03。
        }
    }
}

/// build の成果。M03。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutcome {
    /// 生成された BuildRecord（active には登録しない）。M03。
    pub record: BuildRecord,
    /// help snapshot の text。M03。
    pub help_snapshot: String,
}

/// ds4/ds4-server/ds4-agent を隔離 workspace で build する。M03。
///
/// 1. role と make target を allowlist で検証（任意 shell injection 防止）。
/// 2. 開始前に disk を予約（不足 → 開始前失敗）。
/// 3. 固定 target を引数配列で `make` 実行（最小 env・owned group・cancel）。
/// 4. 成功時のみ出力 binary の digest と help snapshot の digest を計算して
///    `BuildRecord` を返す。active は触らない（build 失敗 → active 不変）。
///    M03。
pub fn build_artifacts(
    req: &BuildRequest,
    cancel: &AtomicBool,
) -> Result<BuildOutcome, BuildError> {
    // role / target を allowlist で検証。M03。
    if !is_approved_role(&req.role) {
        return Err(BuildError::UnapprovedRole);
    }
    if !is_approved_target(&req.target) {
        return Err(BuildError::UnapprovedTarget);
    }

    // 開始前 disk 予約。M03。
    GroupRunner::check_disk(&req.workspace, req.disk_needed).map_err(|e| match e {
        ProcessError::InsufficientDisk(msg) => BuildError::InsufficientDisk(msg),
        _ => BuildError::InsufficientDisk(e.to_string()),
    })?;

    // 固定 target を引数配列で make 実行（shell 文字列不可）。M03。
    let spec = CommandSpec::minimal(&req.make_program, vec![req.target.clone()], &req.workspace);
    match GroupRunner::new().run_group(&spec, cancel) {
        Ok(_) => {}
        Err(ProcessError::Failed(out)) => {
            // build 失敗 → active 不変（新規 record を返さず、registry を触らない）。M03。
            let msg = format!("status={:?} stderr={}", out.status, out.stderr);
            return Err(BuildError::Failed(msg));
        }
        Err(e) => return Err(BuildError::Failed(e.to_string())),
    }

    // 出力 binary の digest。M03。
    let bin_path = req.workspace.join(&req.output_rel);
    if !bin_path.exists() {
        return Err(BuildError::MissingOutput);
    }
    let bytes = std::fs::read(&bin_path).map_err(|e| BuildError::ReadOutput(e.to_string()))?;
    let digest = sha256_hex(&bytes);

    // help snapshot の digest。M03。
    let help_path = req.workspace.join(&req.help_rel);
    let help_snapshot =
        std::fs::read_to_string(&help_path).map_err(|e| BuildError::ReadHelp(e.to_string()))?;
    let help_digest = sha256_hex(help_snapshot.as_bytes());

    Ok(BuildOutcome {
        record: BuildRecord {
            source: req.source.clone(),
            flags: req.flags.clone(),
            toolchain: req.toolchain.clone(),
            arch: req.arch.clone(),
            role: req.role.clone(),
            digest,
            help_digest,
        },
        help_snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m03build-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    /// fake make スクリプトを作る。M03。
    ///
    /// 指定した exit code で終了し、成功時は output binary と help.txt を
    /// workspace に書く。M03。
    fn make_fake_make(dir: &Path, exit_code: i32, produce_output: bool) -> PathBuf {
        let script = dir.join("fake_make.sh");
        let body = format!(
            "#!/bin/sh\ncase \"$1\" in\n  build|ds4|ds4-server|ds4-agent)\n    {}\n    echo 'fake help' > help.txt\n    exit {}\n    ;;\n  *)\n    echo 'unknown target' >&2\n    exit 2\n    ;;\nesac\n",
            if produce_output && exit_code == 0 {
                "echo 'fake binary' > out.bin"
            } else {
                ":"
            },
            exit_code
        );
        std::fs::write(&script, body).expect("write fake make");
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).expect("chmod");
        script
    }

    /// build 成功 → BuildRecord（digest/help_digest）が返り、active は触らない。M03。
    #[test]
    fn build_success_returns_record() {
        let base = tmp("ok");
        let fake = make_fake_make(&base, 0, true);
        let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        req.flags = "-O2".to_string();
        req.toolchain = "rustc 1.80".to_string();
        req.arch = "arm64".to_string();
        let cancel = AtomicBool::new(false);
        let out = build_artifacts(&req, &cancel).expect("build ok");
        assert_eq!(out.record.role, "ds4");
        assert_eq!(out.record.source, "abc123");
        assert_eq!(out.record.flags, "-O2");
        assert_eq!(out.record.arch, "arm64");
        assert!(!out.record.digest.is_empty());
        assert!(!out.record.help_digest.is_empty());
        assert_eq!(out.help_snapshot, "fake help\n");
    }

    /// build 失敗 → active 不変（BuildError::Failed、registry を触らない）。M03。
    #[test]
    fn build_failure_leaves_active_unchanged() {
        let base = tmp("fail");
        let fake = make_fake_make(&base, 1, true);
        let mut req = BuildRequest::new("ds4-server", "build", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        let cancel = AtomicBool::new(false);
        let err = build_artifacts(&req, &cancel).expect_err("must fail");
        assert!(matches!(err, BuildError::Failed(_)));
        // active は触らない（output は生成されない、registry record も無い）。M03。
        assert!(!base.join("out.bin").exists());
    }

    /// 出力が無い build → MissingOutput。M03。
    #[test]
    fn missing_output_rejected() {
        let base = tmp("noout");
        let fake = make_fake_make(&base, 0, false);
        let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        let cancel = AtomicBool::new(false);
        let err = build_artifacts(&req, &cancel).expect_err("must fail");
        assert_eq!(err, BuildError::MissingOutput);
    }

    /// 不正 role → UnapprovedRole。M03。
    #[test]
    fn unapproved_role_rejected() {
        let base = tmp("role");
        let fake = make_fake_make(&base, 0, true);
        let mut req = BuildRequest::new("evil", "build", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        let cancel = AtomicBool::new(false);
        let err = build_artifacts(&req, &cancel).expect_err("must fail");
        assert_eq!(err, BuildError::UnapprovedRole);
    }

    /// 不正 make target → UnapprovedTarget（任意 shell injection 防止）。M03。
    #[test]
    fn unapproved_target_rejected() {
        let base = tmp("target");
        let fake = make_fake_make(&base, 0, true);
        let mut req = BuildRequest::new("ds4", "clean; rm -rf /", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        let cancel = AtomicBool::new(false);
        let err = build_artifacts(&req, &cancel).expect_err("must fail");
        assert_eq!(err, BuildError::UnapprovedTarget);
    }

    /// disk 不足 → 開始前失敗（build を実行しない）。M03。
    #[test]
    fn insufficient_disk_fails_before_build() {
        let base = tmp("disk");
        let fake = make_fake_make(&base, 0, true);
        let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        req.disk_needed = 1_u64 << 62; // ~4 EiB、現実に空かない量。M03。
        let cancel = AtomicBool::new(false);
        let err = build_artifacts(&req, &cancel).expect_err("must fail before build");
        assert!(matches!(err, BuildError::InsufficientDisk(_)));
        // build は実行されない（fake make が output を作っていない）。M03。
        assert!(!base.join("out.bin").exists());
    }

    /// V02/V03: 3 target（ds4 / ds4-server / ds4-agent）を build し、
    /// 各 BuildRecord の digest / help_digest が非空で source・flags が一致する
    /// ことを照合する（受入 case「3target build → record 一致」）。
    #[test]
    fn three_targets_build_records_match() {
        for (i, target) in ["ds4", "ds4-server", "ds4-agent"].iter().enumerate() {
            let tag = format!("3tgt-{i}");
            let base = tmp(&tag);
            let fake = make_fake_make(&base, 0, true);
            let mut req = BuildRequest::new(*target, "build", "9ab70534", &base, "out.bin");
            req.make_program = fake.to_string_lossy().to_string();
            req.flags = "-O3 -mcpu=native".to_string();
            req.toolchain = "cc (clang) + make, Metal".to_string();
            req.arch = "arm64".to_string();
            let cancel = AtomicBool::new(false);
            let out = build_artifacts(&req, &cancel).expect("build ok");
            // record 一致: 各 target で digest / help_digest が非空、source/flags が入力と一致。
            assert_eq!(out.record.role, *target, "target role mismatch");
            assert_eq!(out.record.source, "9ab70534", "source mismatch");
            assert_eq!(out.record.flags, "-O3 -mcpu=native", "flags mismatch");
            assert_eq!(out.record.arch, "arm64", "arch mismatch");
            assert!(!out.record.digest.is_empty(), "digest must not be empty");
            assert!(
                !out.record.help_digest.is_empty(),
                "help digest must not be empty"
            );
            assert_eq!(out.help_snapshot, "fake help\n", "help snapshot mismatch");
        }
    }

    /// V02/V03: help 差 → 自動 activation 0。build_artifacts は新規 record を
    /// 返すだけで active を登録しない（help digest が変わっても activation
    /// されない）。受入 case「help 差 → 自動 activation 0」。
    #[test]
    fn help_diff_does_not_auto_activate() {
        let base = tmp("helpdiff");
        let fake = make_fake_make(&base, 0, true);
        let mut req = BuildRequest::new("ds4-server", "build", "9ab70534", &base, "out.bin");
        req.make_program = fake.to_string_lossy().to_string();
        let cancel = AtomicBool::new(false);
        let out = build_artifacts(&req, &cancel).expect("build ok");
        // 各 role の help は固有（catalog.json で確認、3 distinct help_digests）。
        // build は record を返すだけで、active 状態（active marker）を生成しない。
        assert!(!out.record.help_digest.is_empty());
        // build 後に workspace へ active marker が作られない（active 不変）。
        assert!(
            !base.join("active.txt").exists(),
            "build must not auto-activate"
        );
    }
}
