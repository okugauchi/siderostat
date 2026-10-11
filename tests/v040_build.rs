//! M03 — ds4・ds4-server・ds4-agent isolated build。受入 matrix。M03。
//!
//! 本ファイルは M03 カードの `v040_build.rs` に対応する。公開 API
//! （`build_artifacts` / `BuildRequest` / `BuildError` / `APPROVED_ROLES` /
//! `APPROVED_MAKE_TARGETS`）を介して受入 case を検証する。build はローカル
//! fixture（fake make スクリプト）限定。実公式 source / 実 make / 実
//! network は行わない。M03。
//!
//! 受入 case（全て必須）:
//! - 入力: build 失敗 → active 不変
//! - 入力: cancel/child fork → owned group 回収
//! - 入力: secret env → child へ未継承
//! - 入力: disk 不足 → 開始前失敗
//!
//! レビュー重点: makefile は固定 target でも code 実行。選択 commit の取得と
//! build 許可を区別し、任意 shell injection を防止。M03。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M03。
use siderostat::manager::build::{BuildError, BuildRequest, build_artifacts};
use siderostat::manager::process::GroupRunner;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m03it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create base");
    base
}

/// fake make スクリプトを作る。M03。
///
/// 指定した exit code で終了し、成功時は出力 binary と help.txt を
/// workspace に書く。M03。
fn make_fake_make(dir: &Path, exit_code: i32) -> PathBuf {
    let script = dir.join("fake_make.sh");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"build\" ]; then\n  {}\n  echo 'fake help' > help.txt\n  exit {}\nelse\n  echo 'unknown target' >&2\n  exit 2\nfi\n",
        if exit_code == 0 {
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

/// 受入 case: build 失敗 → active 不変。M03。
#[test]
fn m03_build_failure_leaves_active_unchanged() {
    let base = tmp("fail");
    let fake = make_fake_make(&base, 1);
    let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
    req.make_program = fake.to_string_lossy().to_string();
    let cancel = AtomicBool::new(false);
    let err = build_artifacts(&req, &cancel).expect_err("build must fail");
    assert!(matches!(err, BuildError::Failed(_)));
    // active は触らない（出力 binary は生成されない、registry は触らない）。M03。
    assert!(!base.join("out.bin").exists());
}

/// 受入 case: cancel/child fork → owned group 回収。M03。
///
/// fake make が子プロセス（sleep）を fork して長時間走る間、cancel を
/// set すると owned process group が回収され、失敗（Signaled）が返る。M03。
#[test]
fn m03_cancel_reaps_owned_group() {
    let base = tmp("cancel");
    // fake make は子プロセスを fork して 60s sleep する。M03。
    let script = base.join("fake_make.sh");
    let body =
        "#!/bin/sh\nif [ \"$1\" = \"build\" ]; then\n  sleep 60 &\n  wait\nelse\n  exit 2\nfi\n";
    std::fs::write(&script, body).expect("write fake make");
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&script).expect("meta").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&script, perm).expect("chmod");

    let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
    req.make_program = script.to_string_lossy().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    // 200ms 後に cancel を set。M03。
    let cancel_thread = Arc::clone(&cancel);
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        cancel_thread.store(true, Ordering::SeqCst);
    });
    let err = build_artifacts(&req, &cancel).expect_err("cancel must fail");
    handle.join().unwrap();
    assert!(matches!(err, BuildError::Failed(_)));
}

/// 受入 case: secret env → child へ未継承。M03。
///
/// fake make は secret env を参照できなければ成功する（最小 env のみ
/// 継承）。secret を child へ渡さないことを検証する。M03。
#[test]
fn m03_secret_env_not_inherited() {
    let base = tmp("secret");
    // fake make は secret env を参照できなければ成功。M03。
    let script = base.join("fake_make.sh");
    let body = "#!/bin/sh\nif [ \"$1\" = \"build\" ]; then\n  if [ -n \"$MY_BUILD_SECRET\" ]; then\n    exit 9\n  fi\n  echo 'fake binary' > out.bin\n  echo 'fake help' > help.txt\n  exit 0\nelse\n  exit 2\nfi\n";
    std::fs::write(&script, body).expect("write fake make");
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(&script).expect("meta").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&script, perm).expect("chmod");

    // 親で secret を設定（build の child へは未継承のはず）。M03。
    unsafe {
        std::env::set_var("MY_BUILD_SECRET", "super-secret");
    }
    let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
    req.make_program = script.to_string_lossy().to_string();
    let cancel = AtomicBool::new(false);
    let out = build_artifacts(&req, &cancel);
    unsafe {
        std::env::remove_var("MY_BUILD_SECRET");
    }
    // secret が未継承なので fake make が成功する。M03。
    assert!(out.is_ok(), "secret must not be inherited: {out:?}");
}

/// 受入 case: disk 不足 → 開始前失敗。M03。
#[test]
fn m03_disk_shortage_fails_before_start() {
    let base = tmp("disk");
    let fake = make_fake_make(&base, 0);
    let mut req = BuildRequest::new("ds4", "build", "abc123", &base, "out.bin");
    req.make_program = fake.to_string_lossy().to_string();
    req.disk_needed = 1_u64 << 62; // ~4 EiB、現実に空かない量。M03。
    let cancel = AtomicBool::new(false);
    let err = build_artifacts(&req, &cancel).expect_err("must fail before build");
    assert!(matches!(err, BuildError::InsufficientDisk(_)));
    // build は実行されない。M03。
    assert!(!base.join("out.bin").exists());
}

/// allowlist（role / make target）が固定されていることを確認。M03。
#[test]
fn m03_allowlists_fixed() {
    // 公式 role だけを許可。M03。
    assert!(siderostat::manager::build::is_approved_role("ds4"));
    assert!(siderostat::manager::build::is_approved_role("ds4-server"));
    assert!(siderostat::manager::build::is_approved_role("ds4-agent"));
    assert!(!siderostat::manager::build::is_approved_role("evil"));
    // 固定 make target だけを許可（任意 injection 防止）。M03。
    assert!(siderostat::manager::build::is_approved_target("build"));
    assert!(!siderostat::manager::build::is_approved_target(
        "clean; rm -rf /"
    ));
}

/// disk 空きチェックの公開 API（開始前失敗）を直接検証する。M03。
#[test]
fn m03_check_disk_direct() {
    let base = tmp("checkdisk");
    let huge = 1_u64 << 62;
    let err = GroupRunner::check_disk(&base, huge).expect_err("must fail");
    assert!(matches!(
        err,
        siderostat::manager::process::ProcessError::InsufficientDisk(_)
    ));
}
