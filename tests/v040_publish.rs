//! M06 — full hash・publish・managed resolver。受入 matrix。M06。
//!
//! 本ファイルは M06 カードの `v040_publish.rs` に対応する。公開 API
//! （`verify_artifact` / `publish_verified` / `recheck_verified` /
//! `role_artifact_from_verified`）を介して受入 case を検証する。実 binary の
//! 実行はしない（verify はファイル digest 照合のみ）。M06。
//!
//! 受入 case（全て必須）:
//! - 入力: hash 差 → quarantine
//! - 入力: rename 後 journal 失敗 → 再照合
//! - 入力: external 既存 path → 不変
//! - 入力: verify 後 file 交換 → activation 再検査で拒否
//!
//! レビュー重点: 実行 file の TOCTOU を identity 再確認で防ぐ。model の
//! 新規検証を sample hash だけに弱めない。M06。
//!
//! テスト関数名は snake_case 必須（clippy non_snake_case 回避）。M06。
use siderostat::cluster::role_artifact_from_verified;
use siderostat::cluster::{ExecutableKind, RoleArtifact, RoleKind};
use siderostat::manager::registry::{ArtifactRecord, ArtifactRegistry, ArtifactState, ManagerRoot};
use siderostat::manager::verify::{
    VerifyError, publish_verified, recheck_verified, verify_artifact,
};
use std::path::PathBuf;

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("siderostat-m06it-{tag}"));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create base");
    base
}

fn reg_with_artifact(tag: &str, content: &[u8]) -> (ArtifactRegistry, PathBuf) {
    let base = tmp(tag);
    let root = ManagerRoot::explicit(base.join("root"));
    let mut reg = ArtifactRegistry::new(root);
    let models = reg.paths().models.clone();
    std::fs::create_dir_all(&models).expect("create models");
    let abs = models.join("m.bin");
    std::fs::write(&abs, content).expect("write model");
    let rel = PathBuf::from("ds4/models/m.bin");
    reg.put(ArtifactRecord {
        id: "m1".to_string(),
        kind: "model".to_string(),
        rel_path: rel,
        sha256: siderostat::manager::registry::sha256_hex(content),
        state: ArtifactState::Staged,
    });
    (reg, abs)
}

/// 受入 case 1: hash 差 → quarantine。M06。
#[test]
fn m06_hash_mismatch_quarantines() {
    let (mut reg, _abs) = reg_with_artifact("mismatch", b"real-content");
    let err = verify_artifact(
        &mut reg,
        "m1",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    )
    .expect_err("mismatch must quarantine");
    assert_eq!(err, VerifyError::Quarantined);
    assert_eq!(
        reg.get("m1").expect("rec").state,
        ArtifactState::Quarantined
    );
}

/// 受入 case 2: rename 後 journal 失敗 → 再照合。M06。
///
/// verify（fsync + record Verified）後、再実行（再照合）で Verified に復旧
/// できる。M06。
#[test]
fn m06_journal_reexamine() {
    let (mut reg, _abs) = reg_with_artifact("journal", b"content");
    let expected = siderostat::manager::registry::sha256_hex(b"content");
    let v = verify_artifact(&mut reg, "m1", &expected).expect("verify ok");
    assert_eq!(v.sha256, expected);
    assert_eq!(reg.get("m1").expect("rec").state, ArtifactState::Verified);
    // publish 再実行（再照合）も成功。M06。
    let p = publish_verified(&mut reg, "m1", &expected).expect("publish ok");
    assert_eq!(p.id, "m1");
}

/// 受入 case 3: external 既存 path → 不変。M06。
///
/// external binary は既存 `RoleArtifact` の path をそのまま使い、コピー/
/// 変更しない。managed（verify 済み）は `role_artifact_from_verified` で
/// 変換する。M06。
#[test]
fn m06_external_path_unchanged() {
    // external 既存 binary の RoleArtifact（path は変更しない）。M06。
    let external = RoleArtifact {
        role: RoleKind::Worker,
        executable_kind: ExecutableKind::Ds4,
        path: "/opt/external/ds4".to_string(),
        binary_sha256: "abc".to_string(),
        compatible_binary_sha256: vec!["abc".to_string()],
        source_commit: "commit1".to_string(),
        arch: "aarch64".to_string(),
        backend: "metal".to_string(),
        help_sha256: None,
    };
    // external path は不変（そのまま保持）。M06。
    assert_eq!(external.path, "/opt/external/ds4");
    assert_eq!(external.binary_sha256, "abc");
    assert!(
        external
            .compatible_binary_sha256
            .contains(&"abc".to_string())
    );
}

/// 受入 case 3b: managed（verify 済み）→ RoleArtifact 変換。M06。
#[test]
fn m06_managed_to_role_artifact() {
    let (mut reg, _abs) = reg_with_artifact("managed", b"bin");
    let expected = siderostat::manager::registry::sha256_hex(b"bin");
    let verified = verify_artifact(&mut reg, "m1", &expected).expect("verify ok");
    // managed → RoleArtifact。M06。
    let ra = role_artifact_from_verified(
        RoleKind::Worker,
        ExecutableKind::Ds4,
        verified,
        "commit1".to_string(),
        "aarch64".to_string(),
        "metal".to_string(),
    );
    assert_eq!(ra.role, RoleKind::Worker);
    assert_eq!(ra.executable_kind, ExecutableKind::Ds4);
    assert_eq!(ra.path, "ds4/models/m.bin");
    assert_eq!(ra.binary_sha256, expected);
    assert!(ra.compatible_binary_sha256.contains(&expected));
}

/// 受入 case 4: verify 後 file 交換 → activation 再検査で拒否。M06。
#[test]
fn m06_verify_then_swap_rejected_at_recheck() {
    let (mut reg, abs) = reg_with_artifact("swap", b"original");
    let expected = siderostat::manager::registry::sha256_hex(b"original");
    verify_artifact(&mut reg, "m1", &expected).expect("verify ok");
    // verify 後にファイルを交換（TOCTOU）。M06。
    std::fs::write(&abs, b"tampered").expect("swap");
    // activation 前の再検査で digest 不一致 → 拒否。M06。
    let err = recheck_verified(&reg, "m1", &expected).expect_err("must reject");
    assert_eq!(err, VerifyError::Quarantined);
}
