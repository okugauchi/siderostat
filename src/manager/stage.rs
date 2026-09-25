//! DS4 Manager — stage profile・軽量compatibility smoke。M07。
//!
//! C04 に基づき、activation の前段階として `StagedProfile` を生成する。
//! source/role/transport/model/context/KV/encoder/prefix-file を検査し、
//! validated stage と hardware smoke 待ちを区別する。起動未実行を ready 済み
//! としない。H 以前は小 fixture のみ（実重い model を自動フェーズで load
//! しない）。M07。
//!
//! 受入 case（全て必須）:
//! - 入力: model family 差 → stage 拒否
//! - 入力: 空き RAM 未確認 → hardware pending
//! - 入力: prefix-file digest 差 → 拒否
//! - 入力: external artifact → 同じ契約で検証（compatibility.rs）
//!
//! レビュー重点: main 更新を無条件で active にしない。重い model を自動
//! フェーズで load しない。M07。
use crate::manager::catalog::ModelCatalogEntry;
use std::path::PathBuf;

/// Validated runtime settings used by the managed Stage adapter. The fingerprint
/// is the only persisted representation of the configuration; paths and argv
/// remain transient process input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRuntimeConfig {
    /// When configuration/runtime ownership identifies the local role, Stage
    /// rejects a build artifact intended for the other cluster node.
    pub expected_node_role: Option<String>,
    pub expected_family: String,
    pub context_size: u64,
    pub expected_prefix_digest: Option<String>,
    pub config_fingerprint: String,
    /// Stage does not infer free-memory readiness from configuration. A later
    /// hardware check may set this only after observing the local node.
    pub ram_confirmed: bool,
}

impl StageRuntimeConfig {
    /// Snapshot the DS4 runtime settings that constrain a staged candidate.
    /// The serialized debug form is hashed immediately and never persisted.
    pub fn from_validated_config(config: &crate::config::ModeAwareConfig) -> Self {
        let context_size = if config.cluster.enabled {
            config.ds4.distributed.context_size as u64
        } else {
            config.ds4.standalone.context_size as u64
        };
        let fingerprint_material = format!(
            "manager-stage-config-v1\nnode={}\ncluster_enabled={}\nds4={:?}",
            config.cluster.node_id, config.cluster.enabled, config.ds4
        );
        Self {
            expected_node_role: (!config.cluster.enabled).then(|| "coordinator".into()),
            expected_family: "ds4".into(),
            context_size,
            expected_prefix_digest: None,
            config_fingerprint: super::registry::sha256_hex(fingerprint_material.as_bytes()),
            ram_confirmed: false,
        }
    }
}

/// stage 状態。M07。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagedProfileStatus {
    /// 全検査通過（ただし起動は未実行。ready 済みではない）。M07。
    Validated,
    /// 空き RAM 未確認。hardware smoke 待ち。M07。
    HardwarePending,
}

/// staged profile。M07。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StagedProfile {
    /// profile ID。M07。
    pub profile_id: String,
    /// role 別 artifact（managed verified または external）。M07。
    pub role_artifacts: Vec<PathBuf>,
    /// model catalog entry（family/prefix-file digest 検証に使用）。M07。
    pub model: ModelCatalogEntry,
    /// context size。M07。
    pub context_size: u64,
    /// stage 状態（Validated / HardwarePending）。M07。
    pub status: StagedProfileStatus,
}

/// activation plan。runtime lease はruntime owner が実行時に取得する。M07。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivationPlan {
    /// 対象 staged profile。M07。
    pub profile_id: String,
    /// expected generation（activation/rollback に要求）。M07。
    pub expected_generation: u64,
    /// 空き RAM 確認済み（Validated のみ activation plan を生成）。M07。
    pub ready: bool,
}

/// stage エラー。M07。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageError {
    /// model family が期待と不一致 → stage 拒否。M07。
    FamilyMismatch(String),
    /// prefix-file digest が期待と不一致 → 拒否。M07。
    PrefixDigestMismatch(String),
    /// model catalog に prefix-file が無いのに指定された。M07。
    MissingPrefixFile,
    /// prefix-file が必要だが、runtime config に期待 digest が無い。M07。
    MissingExpectedPrefixDigest,
    /// 空き RAM 未確認（hardware pending）。M07。
    RamUnconfirmed,
    /// role artifact が無い。M07。
    NoRoleArtifacts,
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageError::FamilyMismatch(msg) => write!(f, "model family mismatch: {msg}"),
            StageError::PrefixDigestMismatch(msg) => {
                write!(f, "prefix-file digest mismatch: {msg}")
            }
            StageError::MissingPrefixFile => write!(f, "missing prefix-file"),
            StageError::MissingExpectedPrefixDigest => {
                write!(f, "expected prefix-file digest is not configured")
            }
            StageError::RamUnconfirmed => write!(f, "free RAM unconfirmed (hardware pending)"),
            StageError::NoRoleArtifacts => write!(f, "no role artifacts"),
        }
    }
}

impl std::error::Error for StageError {}

/// stage 要求。M07。
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// profile ID。M07。
    pub profile_id: String,
    /// role 別 artifact（managed verified または external）。M07。
    pub role_artifacts: Vec<PathBuf>,
    /// model catalog entry。M07。
    pub model: ModelCatalogEntry,
    /// 期待 model family。M07。
    pub expected_family: String,
    /// context size。M07。
    pub context_size: u64,
    /// 期待 prefix-file digest（model に指定がある場合）。M07。
    pub expected_prefix_digest: Option<String>,
    /// 空き RAM 確認済みか。未確認なら HardwarePending。M07。
    pub ram_confirmed: bool,
}

/// stage を検証して `StagedProfile` を生成する。M07。
///
/// - model family が期待と一致しない → 拒否。M07。
/// - prefix-file digest が期待と一致しない（または model に無いのに
///   指定された）→ 拒否。M07。
/// - 空き RAM 未確認 → HardwarePending（起動未実行を ready としない）。
///   Validated は起動未実行であり ready 済みではない。M07。
pub fn stage_profile(req: StageRequest) -> Result<StagedProfile, StageError> {
    if req.role_artifacts.is_empty() {
        return Err(StageError::NoRoleArtifacts);
    }
    // model family 差 → stage 拒否。M07。
    if !req.model.family.eq_ignore_ascii_case(&req.expected_family) {
        return Err(StageError::FamilyMismatch(format!(
            "expected {} got {}",
            req.expected_family, req.model.family
        )));
    }
    // prefix-file digest 検証。M07。
    match (&req.model.prefix_file, &req.expected_prefix_digest) {
        (Some(actual), Some(expected)) => {
            if actual != expected {
                return Err(StageError::PrefixDigestMismatch(format!(
                    "expected {expected} got {actual}"
                )));
            }
        }
        (None, Some(_)) => return Err(StageError::MissingPrefixFile),
        (Some(_), None) => return Err(StageError::MissingExpectedPrefixDigest),
        _ => {}
    }
    // 空き RAM 未確認 → HardwarePending。M07。
    let status = if req.ram_confirmed {
        StagedProfileStatus::Validated
    } else {
        StagedProfileStatus::HardwarePending
    };
    Ok(StagedProfile {
        profile_id: req.profile_id,
        role_artifacts: req.role_artifacts,
        model: req.model,
        context_size: req.context_size,
        status,
    })
}

/// activation plan を生成する。Validated のみ ready=true。M07。
///
/// 空き RAM 未確認（HardwarePending）は ready=false（起動未実行を ready
/// 済みとしない）。M07。
pub fn build_activation_plan(staged: &StagedProfile, expected_generation: u64) -> ActivationPlan {
    ActivationPlan {
        profile_id: staged.profile_id.clone(),
        expected_generation,
        ready: staged.status == StagedProfileStatus::Validated,
    }
}

/// 軽量 compatibility smoke の結果。M07。
///
/// smoke target は隔離 fake/小 fixture で固定。実重い model を自動
/// フェーズで load しない。M07。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmokeEvidence {
    /// profile ID。M07。
    pub profile_id: String,
    /// smoke 対象（fixture path）。M07。
    pub fixture: String,
    /// smoke 成功。M07。
    pub ok: bool,
}
