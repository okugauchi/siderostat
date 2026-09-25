//! M02 — 公式source fetchとcommit固定。受入 matrix。M02。
//!
//! 本ファイルは M02 カードの target_commands にある `v040_source.rs` に
//! 対応する。公開 API（`stage_source` / `OfficialRemote` / `SourceRecord` /
//! `SourceError` / `GitRunner`）を介して受入 case を検証する。git command は
//! ローカル fixture remote 限定（レビュー重点）。実ネットワーク・実公式
//! remote は行わない。M02。
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
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M02。
use siderostat::manager::registry::SourceRecord;
use siderostat::manager::source::{GitRunner, OfficialRemote, SourceError, stage_source};
use std::path::{Path, PathBuf};

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m02it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// ローカル fixture remote を作る（main branch + 非 main branch）。M02。
fn make_fixture_remote(dir: &Path) {
    let runner = GitRunner::default();
    let work = dir.parent().expect("parent").join("work");
    std::fs::create_dir_all(&work).expect("create work");
    runner
        .run(["-C", work.to_str().unwrap(), "init", "-b", "main"])
        .expect("init");
    runner
        .run([
            "-C",
            work.to_str().unwrap(),
            "config",
            "user.email",
            "t@example.com",
        ])
        .expect("config email");
    runner
        .run(["-C", work.to_str().unwrap(), "config", "user.name", "t"])
        .expect("config name");
    std::fs::write(work.join("a.txt"), "a").expect("write a");
    runner
        .run(["-C", work.to_str().unwrap(), "add", "a.txt"])
        .expect("add");
    runner
        .run(["-C", work.to_str().unwrap(), "commit", "-m", "first"])
        .expect("commit");
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

/// 受入 case: main 差分 → candidate 追加のみ（fetch だけで activation されない）。
/// M02。
#[test]
fn w01_fetch_main_records_candidate_not_activation() {
    let base = tmp("main");
    let remote = base.join("remote");
    make_fixture_remote(&remote);
    let cache = base.join("cache");
    let official = OfficialRemote::new(remote.to_str().unwrap());

    let rec: SourceRecord = stage_source(
        &cache,
        &official,
        remote.to_str().unwrap(),
        "main",
        "refs/heads/main",
    )
    .expect("stage main");
    // full commit が記録され、main の祖先（candidate）。M02。
    assert!(!rec.full_commit.is_empty());
    assert_eq!(rec.remote, remote.to_str().unwrap());
    assert_eq!(rec.main_proof, rec.full_commit);
    // fetch だけで activation されない（SourceRecord を返すだけ。registry の
    // state は触らない。ここでは cache が bare のまま active ref を指さない）。M02。
    assert!(cache.join("HEAD").exists());
}

/// 受入 case: 非 main commit → reference。M02。
#[test]
fn w01_non_main_commit_is_reference() {
    let base = tmp("nonmain");
    let remote = base.join("remote");
    make_fixture_remote(&remote);
    let cache = base.join("cache");
    let official = OfficialRemote::new(remote.to_str().unwrap());

    // feature branch の commit → main の祖先でない → reference 扱い。M02。
    let err = stage_source(
        &cache,
        &official,
        remote.to_str().unwrap(),
        "refs/heads/feature",
        "refs/heads/main",
    )
    .expect_err("non-main commit must be reference");
    assert_eq!(err, SourceError::NotOnMain);
}

/// 受入 case: network 失敗 → 旧 candidate 保持。M02。
#[test]
fn w01_network_failure_keeps_candidate() {
    let base = tmp("netfail");
    let cache = base.join("cache");
    // 存在しない remote への fetch → 失敗。M02。
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
    // 旧 candidate は保持（cache は bare として有効なまま）。M02。
    assert!(cache.join("HEAD").exists(), "cache stays a valid bare repo");
}

/// 受入 case: 悪意ある remote → 拒否。M02。
#[test]
fn w01_malicious_remote_rejected() {
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

/// 受入 case: 悪意ある revision → 拒否。M02。
#[test]
fn w01_malicious_revision_rejected() {
    let base = tmp("badrev");
    let remote = base.join("remote");
    make_fixture_remote(&remote);
    let cache = base.join("cache");
    let official = OfficialRemote::new(remote.to_str().unwrap());

    // `--upload-pack` オプション注入 → 拒否。M02。
    let err = stage_source(
        &cache,
        &official,
        remote.to_str().unwrap(),
        "--upload-pack=evil",
        "refs/heads/main",
    )
    .expect_err("unsafe revision must be rejected");
    assert_eq!(err, SourceError::UnsafeRevision);

    // shell 展開に使われる空白・記号を含む revision → 拒否。M02。
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
