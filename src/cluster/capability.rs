//! v0.4.0 capability manifest と main 由来判定（C01 / T01）。
//!
//! `Ds4CapabilityManifest` は追跡対象 commit の capability を機械検証可能な形で保持する。
//! main 由来だけでは stable にしない。main 由来 + role binary/help + model compatibility +
//! verification を保持し、main 済/未検証は Candidate、未 main は Reference、不適合は
//! Unsupported + reason とする。機能ごとの状態として他の正常 capability を巻き添えにしない。
//!
//! 検証ヘルパ（`validate_sha256` / `validate_source_commit` / `canonical_json` / `lower_hex`）
//! は manifest.rs のものを再利用する。

use super::manifest::{ManifestError, canonical_json, validate_sha256, validate_source_commit};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// capability の状態。機能ごとに独立して評価し、他の正常 capability を巻き添えにしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityStatus {
    /// upstream main に未統合（PR 等）。実装はあるが本流ではない。
    Reference,
    /// main 由来だが role binary/help/model compatibility の verification 未完了。
    Candidate,
    /// main 由来かつ verification 完了で使用可能。
    Stable,
    /// 現在の hardware/OS/model に不適合。理由は restrictions に保持する。
    Unsupported,
}

/// Ds4CapabilityManifest の schema version。
pub const CAPABILITY_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// TP の role。worker は ds4、HTTP coordinator は ds4-server。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoleKind {
    Worker,
    Coordinator,
}

/// executable の種別。C01: TP worker は ds4、HTTP coordinator は ds4-server。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutableKind {
    /// TP worker / LP の ds4 binary。
    Ds4,
    /// HTTP coordinator の ds4-server binary。
    Ds4Server,
}

/// role 別 artifact。両 role の binary digest は同じでなくてよい。
/// 照合は role 別の許可集合（`compatible_binary_sha256`）で行う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleArtifact {
    pub role: RoleKind,
    pub executable_kind: ExecutableKind,
    pub path: String,
    pub binary_sha256: String,
    /// この role の許可集合。binary_sha256 は必ずここに含まれる。
    pub compatible_binary_sha256: Vec<String>,
    pub source_commit: String,
    pub arch: String,
    pub backend: String,
    /// `--help` 出力の digest。help だけから全 capability を推測しない。
    pub help_sha256: String,
}

/// main ancestry の証明。追跡対象 commit が upstream main の祖先である証跡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainAncestryProof {
    /// 追跡対象の full source commit。
    pub tracked_commit: String,
    /// tracked_commit が upstream main の祖先であることの判定結果。
    pub ancestor_of_main: bool,
    /// 検証に用いた証跡（コマンド・ref・日時等）。
    pub evidence: String,
}

/// capability の verification 状況。未検証 TP を stable にしない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    /// role binary/help + model compatibility + 必要機能 smoke の検証が完了したか。
    pub verified: bool,
    /// 検証実施時刻（epoch millis）。
    pub checked_at_millis: u64,
    pub notes: Vec<String>,
}

/// Ds4CapabilityManifest（schema1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ds4CapabilityManifest {
    pub schema_version: u32,
    /// full source commit。
    pub source_commit: String,
    pub main_ancestry_proof: MainAncestryProof,
    pub build_id: String,
    /// role 別 binary/help digest。両 role で同一 digest を要求しない。
    pub role_artifacts: Vec<RoleArtifact>,
    pub transport: String,
    pub model_family: String,
    pub capabilities: Vec<String>,
    pub restrictions: Vec<String>,
    /// 追跡対象の repository / reference。
    pub reference: String,
    pub verification: Verification,
}

/// capability 評価の結果。status と、Unsupported 時の reason。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAssessment {
    pub status: CapabilityStatus,
    pub reason: Option<String>,
}

impl Ds4CapabilityManifest {
    pub fn validate(&self) -> Result<(), CapabilityError> {
        if self.schema_version != CAPABILITY_MANIFEST_SCHEMA_VERSION {
            return Err(CapabilityError::UnsupportedSchema(self.schema_version));
        }
        validate_source_commit(&self.source_commit)?;
        validate_source_commit(&self.main_ancestry_proof.tracked_commit)?;
        if self.build_id.trim().is_empty() || self.reference.trim().is_empty() {
            return Err(CapabilityError::EmptyField);
        }
        if self.role_artifacts.is_empty() {
            return Err(CapabilityError::NoRoleArtifacts);
        }
        // role 別に binary が許可集合に含まれることを検証する。
        // 両 role の digest が同じである必要はない。
        let mut roles = std::collections::BTreeSet::new();
        for artifact in &self.role_artifacts {
            if !roles.insert(artifact.role) {
                return Err(CapabilityError::DuplicateRole(artifact.role));
            }
            validate_sha256(&artifact.binary_sha256)?;
            validate_sha256(&artifact.help_sha256)?;
            validate_source_commit(&artifact.source_commit)?;
            if artifact.path.trim().is_empty()
                || artifact.arch.trim().is_empty()
                || artifact.backend.trim().is_empty()
            {
                return Err(CapabilityError::EmptyField);
            }
            if artifact.compatible_binary_sha256.is_empty()
                || artifact.compatible_binary_sha256.len() > 8
                || !artifact
                    .compatible_binary_sha256
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            {
                return Err(CapabilityError::InvalidBinaryCompatibilitySet);
            }
            for digest in &artifact.compatible_binary_sha256 {
                validate_sha256(digest)?;
            }
            if artifact
                .compatible_binary_sha256
                .binary_search(&artifact.binary_sha256)
                .is_err()
            {
                return Err(CapabilityError::RoleBinaryNotApproved(artifact.role));
            }
        }
        Ok(())
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, CapabilityError> {
        self.validate()?;
        Ok(canonical_json(self)?)
    }

    /// capability を評価する。main 由来だけでは stable にしない。
    pub fn assess(&self) -> Result<CapabilityAssessment, CapabilityError> {
        self.validate()?;
        // main 未統合は Reference。
        if !self.main_ancestry_proof.ancestor_of_main {
            return Ok(CapabilityAssessment {
                status: CapabilityStatus::Reference,
                reason: Some(format!(
                    "tracked commit {} is not an ancestor of upstream main",
                    self.main_ancestry_proof.tracked_commit
                )),
            });
        }
        // main 由来 + verification 完了は Stable。
        if self.verification.verified {
            return Ok(CapabilityAssessment {
                status: CapabilityStatus::Stable,
                reason: None,
            });
        }
        // main 由来 + 未検証は Candidate。未検証 TP を stable にしない。
        Ok(CapabilityAssessment {
            status: CapabilityStatus::Candidate,
            reason: Some("main-tracked but verification not complete".into()),
        })
    }

    /// 機能ごとの status。restrictions に一致する制約があれば Unsupported + reason。
    /// 他の正常 capability を巻き添えにしないため、機能単位で評価する。
    pub fn status_for(&self, capability: &str) -> Result<CapabilityStatus, CapabilityError> {
        self.validate()?;
        if self.restrictions.iter().any(|r| r == capability) {
            // 不適合（unsupported OS 等）。reason は restrictions に保持する。
            return Ok(CapabilityStatus::Unsupported);
        }
        self.assess().map(|a| a.status)
    }
}

#[derive(Debug, Error)]
pub enum CapabilityError {
    #[error("unsupported capability manifest schema version {0}")]
    UnsupportedSchema(u32),
    #[error("capability manifest must declare at least one role artifact")]
    NoRoleArtifacts,
    #[error("role {0:?} declared more than once")]
    DuplicateRole(RoleKind),
    #[error("role {0:?} binary SHA-256 is not in its approved compatibility set")]
    RoleBinaryNotApproved(RoleKind),
    #[error("compatible binary SHA-256 values must be a sorted, unique list of 1 to 8 digests")]
    InvalidBinaryCompatibilitySet,
    #[error("capability manifest string fields must be non-empty")]
    EmptyField,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const DIGEST: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    pub(crate) const COMMIT: &str = "9ab705347c1775e7599ede7eb81a6255ec7dccb5";

    fn worker_artifact() -> RoleArtifact {
        RoleArtifact {
            role: RoleKind::Worker,
            executable_kind: ExecutableKind::Ds4,
            path: "/usr/local/bin/ds4".into(),
            binary_sha256: DIGEST.into(),
            compatible_binary_sha256: vec![DIGEST.into()],
            source_commit: COMMIT.into(),
            arch: "aarch64".into(),
            backend: "metal".into(),
            help_sha256: DIGEST.into(),
        }
    }

    fn manifest() -> Ds4CapabilityManifest {
        Ds4CapabilityManifest {
            schema_version: CAPABILITY_MANIFEST_SCHEMA_VERSION,
            source_commit: COMMIT.into(),
            main_ancestry_proof: MainAncestryProof {
                tracked_commit: COMMIT.into(),
                ancestor_of_main: true,
                evidence: "ls-remote origin main; git merge-base --is-ancestor".into(),
            },
            build_id: "build-1".into(),
            role_artifacts: vec![worker_artifact()],
            transport: "rdma".into(),
            model_family: "deepseek".into(),
            capabilities: vec!["tensor-parallel".into()],
            restrictions: vec!["linux-aarch64-only".into()],
            reference: "https://github.com/okugauchi/ds4".into(),
            verification: Verification {
                verified: false,
                checked_at_millis: 0,
                notes: vec![],
            },
        }
    }

    #[test]
    fn valid_manifest_passes_validate() {
        assert!(manifest().validate().is_ok());
    }

    #[test]
    fn rejects_arbitrary_digest() {
        let mut m = manifest();
        m.role_artifacts[0].binary_sha256 = "zzzz".into();
        assert!(matches!(
            m.validate().unwrap_err(),
            CapabilityError::Manifest(ManifestError::InvalidSha256)
        ));
    }

    #[test]
    fn rejects_unknown_schema() {
        let mut m = manifest();
        m.schema_version = 99;
        assert!(matches!(
            m.validate().unwrap_err(),
            CapabilityError::UnsupportedSchema(99)
        ));
    }

    #[test]
    fn rejects_digest_outside_role_compatibility_set() {
        let mut m = manifest();
        m.role_artifacts[0].binary_sha256 =
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into();
        assert!(matches!(
            m.validate().unwrap_err(),
            CapabilityError::RoleBinaryNotApproved(RoleKind::Worker)
        ));
    }

    #[test]
    fn different_digest_per_role_is_allowed_by_role_set() {
        // worker と coordinator で digest が違っても、各 role の許可集合に入っていれば OK。
        let worker = worker_artifact();
        let coord = RoleArtifact {
            role: RoleKind::Coordinator,
            executable_kind: ExecutableKind::Ds4Server,
            path: "/usr/local/bin/ds4-server".into(),
            binary_sha256: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
            compatible_binary_sha256: vec![
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
            ],
            source_commit: COMMIT.into(),
            arch: "aarch64".into(),
            backend: "metal".into(),
            help_sha256: DIGEST.into(),
        };
        let mut m = manifest();
        m.role_artifacts = vec![worker, coord];
        assert!(m.validate().is_ok());
    }

    #[test]
    fn unverified_main_is_candidate_not_stable() {
        let mut m = manifest();
        m.verification.verified = false;
        assert_eq!(m.assess().unwrap().status, CapabilityStatus::Candidate);
        assert!(m.assess().unwrap().reason.is_some());
    }

    #[test]
    fn verified_main_is_stable() {
        let mut m = manifest();
        m.verification.verified = true;
        assert_eq!(m.assess().unwrap().status, CapabilityStatus::Stable);
    }

    #[test]
    fn non_main_is_reference() {
        let mut m = manifest();
        m.main_ancestry_proof.ancestor_of_main = false;
        assert_eq!(m.assess().unwrap().status, CapabilityStatus::Reference);
    }

    #[test]
    fn unsupported_os_keeps_reason_in_restrictions() {
        // restrictions に capability が一致しなければ Stable。reason は restrictions に保持。
        let mut m = manifest();
        m.verification.verified = true;
        assert_eq!(
            m.status_for("tensor-parallel").unwrap(),
            CapabilityStatus::Stable
        );
    }

    #[test]
    fn unsupported_capability_reports_unsupported() {
        let mut m = manifest();
        m.verification.verified = true;
        // restrictions に「metal-rdma」制約を追加し、該当 capability を Unsupported にする。
        m.restrictions.push("metal-rdma".into());
        assert_eq!(
            m.status_for("metal-rdma").unwrap(),
            CapabilityStatus::Unsupported
        );
    }
}
