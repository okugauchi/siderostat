//! DS4 Manager — 軽量 compatibility smoke。M07。
//!
//! C04 に基づき、staged profile の compatibility を軽量 smoke（隔離 fake /
//! 小 fixture）で検証する。実重い model を自動フェーズで load しない。
//! managed と external の両方の artifact を同じ契約で検証する（受入 case:
//! external artifact → 同じ契約で検証）。M07。
//!
//! レビュー重点: main 更新を無条件で active にしない。重い model を自動
//! フェーズで load しない。M07。
use crate::manager::catalog::ModelCatalogEntry;
use crate::manager::stage::{StageError, StageRequest, StagedProfileStatus, stage_profile};
use std::path::PathBuf;

/// compatibility smoke エラー。M07。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityError {
    /// stage 検証失敗。M07。
    Stage(StageError),
    /// smoke fixture が無い。M07。
    MissingFixture,
}

impl std::fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompatibilityError::Stage(e) => write!(f, "stage: {e}"),
            CompatibilityError::MissingFixture => write!(f, "missing smoke fixture"),
        }
    }
}

impl std::error::Error for CompatibilityError {}

impl From<StageError> for CompatibilityError {
    fn from(e: StageError) -> Self {
        CompatibilityError::Stage(e)
    }
}

/// 軽量 compatibility smoke の要求。M07。
#[derive(Debug, Clone)]
pub struct SmokeRequest {
    /// profile ID。M07。
    pub profile_id: String,
    /// role 別 artifact（managed verified または external、共通 PathBuf）。M07。
    pub role_artifacts: Vec<PathBuf>,
    /// model catalog entry（managed/external 共通）。M07。
    pub model: ModelCatalogEntry,
    /// 期待 model family。M07。
    pub expected_family: String,
    /// context size。M07。
    pub context_size: u64,
    /// 期待 prefix-file digest。M07。
    pub expected_prefix_digest: Option<String>,
    /// 空き RAM 確認済みか。M07。
    pub ram_confirmed: bool,
    /// smoke fixture（小 fixture。実重い model を load しない）。M07。
    pub fixture: PathBuf,
}

/// 軽量 compatibility smoke を実行する。M07。
///
/// 1. staged profile を検証（model family / prefix-file / RAM。managed と
///    external で同じ契約）。M07。
/// 2. smoke fixture の存在を確認（実重い model を load しない）。M07。
/// 3. Validated のみ smoke 成功を返す。HardwarePending は smoke を実行せず
///    pending を返す（起動未実行を ready 済みとしない）。M07。
pub fn compatibility_smoke(req: SmokeRequest) -> Result<SmokeOutcome, CompatibilityError> {
    // 共通契約で stage 検証（external artifact も同じ）。M07。
    let staged = stage_profile(StageRequest {
        profile_id: req.profile_id.clone(),
        role_artifacts: req.role_artifacts.clone(),
        model: req.model.clone(),
        expected_family: req.expected_family,
        context_size: req.context_size,
        expected_prefix_digest: req.expected_prefix_digest,
        ram_confirmed: req.ram_confirmed,
    })?;
    // smoke fixture の存在確認（実重い model を load しない）。M07。
    if !req.fixture.exists() {
        return Err(CompatibilityError::MissingFixture);
    }
    // Validated のみ smoke 成功。HardwarePending は smoke を実行しない。M07。
    match staged.status {
        StagedProfileStatus::Validated => Ok(SmokeOutcome {
            profile_id: staged.profile_id,
            fixture: req.fixture,
            ok: true,
        }),
        StagedProfileStatus::HardwarePending => Ok(SmokeOutcome {
            profile_id: staged.profile_id,
            fixture: req.fixture,
            ok: false,
        }),
    }
}

/// smoke 結果。M07。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmokeOutcome {
    /// profile ID。M07。
    pub profile_id: String,
    /// smoke fixture path。M07。
    pub fixture: PathBuf,
    /// smoke 成功（Validated のみ true）。M07。
    pub ok: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::catalog::{CapabilityStatus, ModelCatalogEntry};

    fn model(family: &str, prefix: Option<String>) -> ModelCatalogEntry {
        ModelCatalogEntry {
            catalog_id: "m1".to_string(),
            url: "https://models.example.com/m.bin".to_string(),
            redirect_allowlist: vec![],
            size: 1024,
            sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            license: "mit".to_string(),
            family: family.to_string(),
            quantization: "q4".to_string(),
            encoder: None,
            support: None,
            prefix_file: prefix,
            reference: Some("ref".to_string()),
            main_integrated: true,
            ram_reference: Some("16GiB".to_string()),
            compatibility: vec![],
            status: CapabilityStatus::Candidate,
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("siderostat-m07sm-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create base");
        base
    }

    /// model family 差 → stage 拒否。M07。
    #[test]
    fn family_mismatch_rejected() {
        let base = tmp("family");
        let fixture = base.join("smoke.bin");
        std::fs::write(&fixture, b"smoke").expect("fixture");
        let req = SmokeRequest {
            profile_id: "p1".to_string(),
            role_artifacts: vec![base.join("r1")],
            model: model("dspark", None),
            expected_family: "ds4".to_string(),
            context_size: 4096,
            expected_prefix_digest: None,
            ram_confirmed: true,
            fixture,
        };
        let err = compatibility_smoke(req).expect_err("family mismatch must reject");
        assert!(matches!(
            err,
            CompatibilityError::Stage(StageError::FamilyMismatch(_))
        ));
    }

    /// 空き RAM 未確認 → hardware pending（smoke を実行しない）。M07。
    #[test]
    fn unconfirmed_ram_is_hardware_pending() {
        let base = tmp("ram");
        let fixture = base.join("smoke.bin");
        std::fs::write(&fixture, b"smoke").expect("fixture");
        let req = SmokeRequest {
            profile_id: "p1".to_string(),
            role_artifacts: vec![base.join("r1")],
            model: model("ds4", None),
            expected_family: "ds4".to_string(),
            context_size: 4096,
            expected_prefix_digest: None,
            ram_confirmed: false,
            fixture,
        };
        let out = compatibility_smoke(req).expect("pending");
        assert!(!out.ok);
    }

    /// prefix-file digest 差 → 拒否。M07。
    #[test]
    fn prefix_digest_mismatch_rejected() {
        let base = tmp("prefix");
        let fixture = base.join("smoke.bin");
        std::fs::write(&fixture, b"smoke").expect("fixture");
        let req = SmokeRequest {
            profile_id: "p1".to_string(),
            role_artifacts: vec![base.join("r1")],
            model: model("ds4", Some("actual-prefix".to_string())),
            expected_family: "ds4".to_string(),
            context_size: 4096,
            expected_prefix_digest: Some("expected-prefix".to_string()),
            ram_confirmed: true,
            fixture,
        };
        let err = compatibility_smoke(req).expect_err("prefix mismatch must reject");
        assert!(matches!(
            err,
            CompatibilityError::Stage(StageError::PrefixDigestMismatch(_))
        ));
    }

    /// Validated → smoke 成功。M07。
    #[test]
    fn validated_smoke_ok() {
        let base = tmp("ok");
        let fixture = base.join("smoke.bin");
        std::fs::write(&fixture, b"smoke").expect("fixture");
        let req = SmokeRequest {
            profile_id: "p1".to_string(),
            role_artifacts: vec![base.join("r1")],
            model: model("ds4", None),
            expected_family: "ds4".to_string(),
            context_size: 4096,
            expected_prefix_digest: None,
            ram_confirmed: true,
            fixture,
        };
        let out = compatibility_smoke(req).expect("smoke ok");
        assert!(out.ok);
    }
}
