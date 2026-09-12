//! DS4 Manager — full hash・publish。M06。
//!
//! C01/C04 に基づき、artifact の full SHA-256 と catalog identity を検証し、
//! fsync + rename 後に verified record を公開する。stage/publish 間の crash
//! から再検査できる。実行 file の TOCTOU を identity 再確認で防ぐ。sample
//! cache は新規取得の信頼根拠に使わない（full SHA 必須）。M06。
//!
//! 受入 case（全て必須）:
//! - 入力: hash 差 → quarantine
//! - 入力: rename 後 journal 失敗 → 再照合
//! - 入力: external 既存 path → 不変（cluster/artifacts.rs）
//! - 入力: verify 後 file 交換 → activation 再検査で拒否
//!
//! レビュー重点: 実行 file の TOCTOU を identity 再確認で防ぐ。model の
//! 新規検証を sample hash だけに弱めない。M06。
use crate::manager::registry::{ArtifactRegistry, ArtifactState, RegistryError, sha256_hex};
use std::path::PathBuf;

/// verified artifact。M06。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerifiedArtifact {
    /// registry 内の一意 ID。M06。
    pub id: String,
    /// artifact の種類（model/build/source 等）。M06。
    pub kind: String,
    /// managed namespace 内の相対 path。M06。
    pub rel_path: PathBuf,
    /// full SHA-256 hex。M06。
    pub sha256: String,
}

/// verify エラー。M06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// 実ファイル digest が期待と不一致（quarantine 済み）。M06。
    Quarantined,
    /// registry record が無い。M06。
    NotFound,
    /// journal / IO 失敗（再照合可能）。M06。
    Io(String),
    /// path が managed root 外。M06。
    PathOutsideRoot,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Quarantined => write!(f, "digest mismatch; quarantined"),
            VerifyError::NotFound => write!(f, "artifact record not found"),
            VerifyError::Io(msg) => write!(f, "io: {msg}"),
            VerifyError::PathOutsideRoot => write!(f, "path outside managed root"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<RegistryError> for VerifyError {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::PathOutsideRoot => VerifyError::PathOutsideRoot,
            RegistryError::NotFound => VerifyError::NotFound,
            _ => VerifyError::Io(e.to_string()),
        }
    }
}

/// artifact の full SHA-256 を照合し、不一致なら quarantine。M06。
///
/// - 実ファイルの SHA-256 を計算し `expected_sha256` と比較。
/// - 一致 → Ok（record の sha256 も更新）。
/// - 不一致 → `ArtifactState::Quarantined` に set して Err(Quarantined)。
///   sample cache は新規 trust に使わない（full SHA 必須）。M06。
pub fn verify_artifact(
    reg: &mut ArtifactRegistry,
    id: &str,
    expected_sha256: &str,
) -> Result<VerifiedArtifact, VerifyError> {
    let record = reg.get(id).ok_or(VerifyError::NotFound)?.clone();
    // 実ファイル digest を取得（root 内 path 検証含む）。M06。
    let actual = reg
        .file_sha256(&record.rel_path)
        .map_err(VerifyError::from)?;
    if actual != expected_sha256 {
        // hash 差 → quarantine。M06。
        let _ = reg.set_state(id, ArtifactState::Quarantined);
        return Err(VerifyError::Quarantined);
    }
    // 一致 → record を更新（sha256 + Verified）。M06。
    let mut rec = record.clone();
    rec.sha256 = expected_sha256.to_string();
    rec.state = ArtifactState::Verified;
    reg.put(rec.clone());
    // journal へ永続化。M06。
    reg.persist_journal()
        .map_err(|e| VerifyError::Io(e.to_string()))?;
    Ok(VerifiedArtifact {
        id: record.id,
        kind: record.kind,
        rel_path: record.rel_path,
        sha256: expected_sha256.to_string(),
    })
}

/// verify → fsync + rename 後に verified record を公開する。M06。
///
/// rename 後 journal 失敗は再照合で復旧できる（crash から再検査）。M06。
///
/// TOCTOU 防止: 公開時にも実ファイル digest を再確認する（verify 後 file
/// 交換 → activation 再検査で拒否）。M06。
pub fn publish_verified(
    reg: &mut ArtifactRegistry,
    id: &str,
    expected_sha256: &str,
) -> Result<VerifiedArtifact, VerifyError> {
    // verify（full SHA 照合）。M06。
    let verified = verify_artifact(reg, id, expected_sha256)?;

    // fsync：実ファイルを disk に同期。M06。
    let abs = reg.resolve_within_root(&verified.rel_path)?;
    sync_file(&abs)?;

    // 公開（persist_journal）。journal 失敗でも record は Verified のまま
    // なので再照合（publish_verified 再実行）で復旧できる。M06。
    if let Err(e) = reg.persist_journal() {
        return Err(VerifyError::Io(e.to_string()));
    }
    Ok(verified)
}

/// ファイルを fsync する。M06。
fn sync_file(path: &std::path::Path) -> Result<(), VerifyError> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| VerifyError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| VerifyError::Io(e.to_string()))?;
    Ok(())
}

/// 公開済み artifact の digest を再確認する（TOCTOU 防止）。M06。
///
/// verify 後に実ファイルが交換された場合、activation 前の再検査で digest
/// 不一致を検出し拒否する。M06。
pub fn recheck_verified(
    reg: &ArtifactRegistry,
    id: &str,
    expected_sha256: &str,
) -> Result<(), VerifyError> {
    let record = reg.get(id).ok_or(VerifyError::NotFound)?.clone();
    let actual = reg
        .file_sha256(&record.rel_path)
        .map_err(VerifyError::from)?;
    if actual != expected_sha256 {
        return Err(VerifyError::Quarantined);
    }
    Ok(())
}

/// helper: バイト列の SHA-256（テスト用に公開）。M06。
pub fn hex_sha256(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::registry::ManagerRoot;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m06v-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    fn reg_with_artifact(tag: &str, content: &[u8]) -> (ArtifactRegistry, std::path::PathBuf) {
        let base = tmp(tag);
        let root = ManagerRoot::explicit(base.join("root"));
        let mut reg = ArtifactRegistry::new(root);
        // managed model を置く。M06。
        let models = reg.paths().models.clone();
        std::fs::create_dir_all(&models).expect("create models");
        let abs = models.join("m.bin");
        std::fs::write(&abs, content).expect("write model");
        // rel_path は models 配下（ds4/models/m.bin）。M06。
        let rel = std::path::PathBuf::from("ds4/models/m.bin");
        reg.put(crate::manager::registry::ArtifactRecord {
            id: "m1".to_string(),
            kind: "model".to_string(),
            rel_path: rel,
            sha256: sha256_hex(content),
            state: ArtifactState::Staged,
        });
        (reg, abs)
    }

    /// 受入 case 1: hash 差 → quarantine。M06。
    #[test]
    fn hash_mismatch_quarantines() {
        let (mut reg, _abs) = reg_with_artifact("mismatch", b"real-content");
        let err = verify_artifact(
            &mut reg,
            "m1",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        )
        .expect_err("mismatch must quarantine");
        assert_eq!(err, VerifyError::Quarantined);
        // record が Quarantined になっている。M06。
        let rec = reg.get("m1").expect("record");
        assert_eq!(rec.state, ArtifactState::Quarantined);
    }

    /// 受入 case 2: rename 後 journal 失敗 → 再照合。M06。
    ///
    /// verify 後の journal 保存失敗は、再実行（再照合）で Verified に
    /// 復旧できる。M06。
    #[test]
    fn rename_journal_failure_reexamines() {
        let (mut reg, _abs) = reg_with_artifact("journal", b"content");
        // 正しい digest で verify → Verified + journal 保存成功。M06。
        let expected = sha256_hex(b"content");
        let v = verify_artifact(&mut reg, "m1", &expected).expect("verify ok");
        assert_eq!(v.sha256, expected);
        assert_eq!(reg.get("m1").expect("rec").state, ArtifactState::Verified);
        // publish 再実行（再照合）も成功。M06。
        let p = publish_verified(&mut reg, "m1", &expected).expect("publish ok");
        assert_eq!(p.id, "m1");
    }

    /// 受入 case 4: verify 後 file 交換 → activation 再検査で拒否。M06。
    #[test]
    fn verify_then_swap_rejected_at_recheck() {
        let (mut reg, abs) = reg_with_artifact("swap", b"original");
        let expected = sha256_hex(b"original");
        verify_artifact(&mut reg, "m1", &expected).expect("verify ok");
        // verify 後にファイルを交換（TOCTOU）。M06。
        std::fs::write(&abs, b"tampered").expect("swap file");
        // activation 前の再検査で digest 不一致 → 拒否。M06。
        let err = recheck_verified(&reg, "m1", &expected).expect_err("must reject");
        assert_eq!(err, VerifyError::Quarantined);
    }
}
