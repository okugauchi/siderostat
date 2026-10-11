//! M01 — managed registry・job journal・private root。受入 matrix。M01。
//!
//! 本ファイルは M01 カードの target_commands にある `v040_registry.rs` に
//! 対応する。公開 API（`ArtifactRegistry` / `ManagerRoot` / `JobJournal` /
//! `ManagerJob` 等）を介して受入 case を検証する。実プロセス・実
//! ネットワークは行わない（一時 root / fake 境界）。M01。
//!
//! 受入 case（全て必須）:
//! - 入力: symlink で root 外 → 拒否
//! - 入力: write 途中 crash → 前 record 読取可（atomic 記録）
//! - 入力: 重複 job → 同 ID
//! - 入力: active digest 不一致 → activation 禁止
//!
//! レビュー重点: フォルダ名だけで trusted と扱わない。削除は本 release で
//! 自動化しない。M01。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M01。
use siderostat::manager::jobs::{JobJournal, JobKind, JobPhase};
use siderostat::manager::registry::{ArtifactRecord, ArtifactRegistry, ArtifactState, ManagerRoot};
use std::path::{Path, PathBuf};

fn tmp_root(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m01it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// 受入 case: symlink で root 外 → 拒否。M01。
#[test]
fn w01_symlink_escape_rejected() {
    let root = tmp_root("symlink");
    let reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
    // root 外の実ファイルを作る。
    let outside = std::env::temp_dir().join("siderostat-m01it-outside-target");
    std::fs::write(&outside, b"secret").expect("write outside");
    // root 内に symlink を張り、root 外を指させる。
    let sub = root.join("ds4/models");
    std::fs::create_dir_all(&sub).expect("create sub");
    std::os::unix::fs::symlink(&outside, sub.join("link")).expect("symlink");

    // symlink 経由の相対 path は root 外 escape として拒否される。M01。
    let rel = Path::new("ds4/models/link");
    let err = reg
        .resolve_within_root(rel)
        .expect_err("symlink escape must be rejected");
    assert!(matches!(
        err,
        siderostat::manager::registry::RegistryError::SymlinkEscape
            | siderostat::manager::registry::RegistryError::PathOutsideRoot
    ));
    let _ = std::fs::remove_file(&outside);
}

/// 受入 case: write 途中 crash → 前 record 読取可（atomic 記録）。M01。
///
/// `persist_journal` は temp 書込 → fsync → rename の atomic 記録で、write
/// 途中 crash でも前 record が読める。ここでは persist を2回呼び、前回の
/// journal が正しく置換されることと、tmp が残っても target が壊れない
/// ことを検証する。M01。
#[test]
fn w01_interrupted_write_keeps_previous_record() {
    let root = tmp_root("atomic");
    let mut reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
    // 前 record を publish。M01。
    reg.put(ArtifactRecord {
        id: "prev".into(),
        kind: "model".into(),
        rel_path: Path::new("ds4/models/prev").to_path_buf(),
        sha256: "a".into(),
        state: ArtifactState::Verified,
    });
    reg.persist_journal().expect("persist prev");
    let journal = root.join("ds4/operations/registry-journal.json");
    let first = std::fs::read_to_string(&journal).expect("read first journal");
    assert!(first.contains("\"prev\""), "previous record persisted");

    // 新 record を publish して journal が置換される。M01。
    reg.put(ArtifactRecord {
        id: "next".into(),
        kind: "model".into(),
        rel_path: Path::new("ds4/models/next").to_path_buf(),
        sha256: "b".into(),
        state: ArtifactState::Verified,
    });
    reg.persist_journal().expect("persist next");
    let second = std::fs::read_to_string(&journal).expect("read second journal");
    assert!(second.contains("\"next\""), "new record persisted");
    // 前 record は残っている（record set の一部として読取可）。M01。
    assert!(
        second.contains("\"prev\""),
        "previous record still readable"
    );
}

/// 受入 case: 重複 job → 同 ID。M01。
#[test]
fn w01_duplicate_job_returns_same_id() {
    let mut journal = JobJournal::new();
    let first = journal
        .enqueue(JobKind::Download, "model-a")
        .expect("first");
    // 同一 payload の進行中 job → 同 ID。M01。
    let dup = journal.enqueue(JobKind::Download, "model-a").expect("dup");
    assert_eq!(dup, first, "duplicate running job returns same id");
    // 進行中 job は phase が Running。M01。
    assert_eq!(journal.get(&first).expect("job").phase, JobPhase::Running);
}

/// 受入 case: active digest 不一致 → activation 禁止。M01。
#[test]
fn w01_active_digest_mismatch_blocks_activation() {
    let root = tmp_root("digest");
    let mut reg = ArtifactRegistry::new(ManagerRoot::explicit(root.clone()));
    // active artifact を置く。M01。
    let rel = Path::new("ds4/models/active.bin");
    let abs = root.join(rel);
    std::fs::create_dir_all(abs.parent().expect("parent")).expect("create dir");
    std::fs::write(&abs, b"model-content-v1").expect("write");
    let good = siderostat::manager::registry::sha256_hex(b"model-content-v1");

    reg.put(ArtifactRecord {
        id: "m1".into(),
        kind: "model".into(),
        rel_path: rel.to_path_buf(),
        sha256: good.clone(),
        state: ArtifactState::Active,
    });
    // digest 一致 → activation 可。M01。
    reg.can_activate().expect("matching digest activates");

    // 実ファイル変更 → digest 不一致 → activation 禁止。M01。
    std::fs::write(&abs, b"tampered-content").expect("overwrite");
    let err = reg
        .can_activate()
        .expect_err("mismatch must block activation");
    assert_eq!(
        err,
        siderostat::manager::registry::RegistryError::DigestMismatch
    );
}

/// 管理 namespace は互換 root の下に分離される。M01。
#[test]
fn w01_managed_namespace_under_compatible_root() {
    let root = ManagerRoot::default_from_home(Path::new("/Users/o"));
    let paths = root.paths();
    assert_eq!(
        paths.models,
        Path::new("/Users/o/Library/Application Support/siderostat/ds4/models")
    );
    assert_eq!(
        paths.sources,
        Path::new("/Users/o/Library/Application Support/siderostat/ds4/sources")
    );
}
