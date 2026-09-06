//! v0.4.0 外部 artifact の role 別解決と resolved profile（C01 / T02）。
//!
//! `ResolvedDs4Profile` は外部 binary（worker ds4 / HTTP coordinator ds4-server）を
//! role 別に解決し、source/model/role/context/transport から共有 deployment identity を
//! 生成する。host 固有 path は共有 digest に混ぜない。role と executable_kind の
//! 整合（worker → ds4、coordinator → ds4-server）を検証し、不整合は拒否する。

use super::capability::{ExecutableKind, RoleArtifact, RoleKind};
use super::manifest::{ManifestError, ModelIdentity, TpDeploymentManifest};
use crate::config::{Residency, SpeculativeSupport};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// role 別に解決済みの DS4 profile。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDs4Profile {
    pub profile_id: String,
    pub topology: String,
    pub transport: String,
    pub role_artifacts: Vec<RoleArtifact>,
    pub model_identity: ModelIdentity,
    pub context: u64,
    pub residency: Residency,
    pub speculative_support: SpeculativeSupport,
    pub capability_manifest_id: String,
}

impl ResolvedDs4Profile {
    /// TpDeploymentManifest（schema3）を role 別解決する。transport/topology は明示指定。
    pub fn resolve(
        manifest: &TpDeploymentManifest,
        topology: String,
        transport: String,
        residency: Residency,
        speculative_support: SpeculativeSupport,
        capability_manifest_id: String,
    ) -> Result<Self, ResolveError> {
        // role と executable_kind の整合性を先に検証する（validate より前に明示する）。
        // worker → ds4、coordinator → ds4-server。
        for artifact in &manifest.role_artifacts {
            let expected = match artifact.role {
                RoleKind::Worker => ExecutableKind::Ds4,
                RoleKind::Coordinator => ExecutableKind::Ds4Server,
            };
            if artifact.executable_kind != expected {
                return Err(ResolveError::RoleExecutableMismatch {
                    role: artifact.role,
                    kind: artifact.executable_kind,
                    expected,
                });
            }
        }
        manifest.validate()?;
        Ok(ResolvedDs4Profile {
            profile_id: manifest.profile_id.clone(),
            topology,
            transport,
            role_artifacts: manifest.role_artifacts.clone(),
            model_identity: manifest.model.clone(),
            context: manifest.context_size,
            residency,
            speculative_support,
            capability_manifest_id,
        })
    }

    /// 共有 deployment identity。role binary/source/model/transport/context/argv 契約から
    /// 生成され、host 固有 path は含めない。
    pub fn deployment_id(&self) -> Result<String, ResolveError> {
        let mut role_binaries: Vec<&str> = self
            .role_artifacts
            .iter()
            .map(|a| a.binary_sha256.as_str())
            .collect();
        role_binaries.sort_unstable();
        let mut source_commits: Vec<&str> = self
            .role_artifacts
            .iter()
            .map(|a| a.source_commit.as_str())
            .collect();
        source_commits.sort_unstable();
        #[derive(serde::Serialize)]
        struct Identity<'a> {
            profile_id: &'a str,
            topology: &'a str,
            transport: &'a str,
            role_binaries: &'a [&'a str],
            source_commits: &'a [&'a str],
            model_sha256: &'a str,
            context: u64,
        }
        let identity = Identity {
            profile_id: &self.profile_id,
            topology: &self.topology,
            transport: &self.transport,
            role_binaries: &role_binaries,
            source_commits: &source_commits,
            model_sha256: &self.model_identity.sha256,
            context: self.context,
        };
        let bytes = super::manifest::canonical_json(&identity)?;
        Ok(super::manifest::lower_hex(&Sha256::digest(&bytes)))
    }
}

/// 旧 LP manifest（schema2）を内部正規形へ変換するアダプタ。従来の deployment 契約
/// （source/model から決まり path を含まない）を維持する。transport は明示指定する。
/// 未証明の source や transport を勝手に補完しない。
pub fn convert_layer_parallel(
    manifest: &super::manifest::DistributedManifest,
    transport: String,
) -> Result<TpDeploymentManifest, ManifestError> {
    manifest.validate()?;
    let worker = RoleArtifact {
        role: RoleKind::Worker,
        executable_kind: ExecutableKind::Ds4,
        path: String::new(), // host 固有 path は共有 digest に入れない。
        binary_sha256: manifest.ds4_binary_sha256.clone(),
        compatible_binary_sha256: manifest.compatible_ds4_binary_sha256.clone(),
        source_commit: manifest.ds4_source_commit.clone(),
        arch: String::new(),
        backend: String::new(),
        help_sha256: None,
    };
    let model = ModelIdentity {
        catalog_id: manifest.checkpoint.clone(),
        sha256: manifest.model_sha256.clone(),
        size: manifest.model_size,
        family: manifest.model_family.clone(),
        quantization: manifest.quantization.clone(),
        encoder_digest: None,
        support_digest: None,
        prefix_file_digest: None,
    };
    Ok(TpDeploymentManifest {
        schema_version: super::manifest::TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        profile_id: manifest.profile.clone(),
        source_commit: manifest.ds4_source_commit.clone(),
        role_artifacts: vec![worker],
        model,
        transport,
        context_size: manifest.context_size,
        argv_contract_id: manifest.argv_profile_sha256.clone(),
    })
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("role {role:?} requires executable {expected:?}, got {kind:?}")]
    RoleExecutableMismatch {
        role: RoleKind,
        kind: ExecutableKind,
        expected: ExecutableKind,
    },
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Residency;
    use crate::config::SpeculativeSupport;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    const COMMIT: &str = "9ab705347c1775e7599ede7eb81a6255ec7dccb5";

    fn worker() -> RoleArtifact {
        RoleArtifact {
            role: RoleKind::Worker,
            executable_kind: ExecutableKind::Ds4,
            path: "/host-a/usr/bin/ds4".into(),
            binary_sha256: DIGEST.into(),
            compatible_binary_sha256: vec![DIGEST.into()],
            source_commit: COMMIT.into(),
            arch: "aarch64".into(),
            backend: "metal".into(),
            help_sha256: Some(DIGEST.into()),
        }
    }

    fn coordinator() -> RoleArtifact {
        RoleArtifact {
            role: RoleKind::Coordinator,
            executable_kind: ExecutableKind::Ds4Server,
            path: "/host-a/usr/bin/ds4-server".into(),
            binary_sha256: OTHER_DIGEST.into(),
            compatible_binary_sha256: vec![OTHER_DIGEST.into()],
            source_commit: COMMIT.into(),
            arch: "aarch64".into(),
            backend: "metal".into(),
            help_sha256: Some(OTHER_DIGEST.into()),
        }
    }

    fn model() -> ModelIdentity {
        ModelIdentity {
            catalog_id: "deepseek-v4".into(),
            sha256: DIGEST.into(),
            size: 4096,
            family: "deepseek".into(),
            quantization: "q4".into(),
            encoder_digest: None,
            support_digest: None,
            prefix_file_digest: None,
        }
    }

    fn manifest() -> TpDeploymentManifest {
        TpDeploymentManifest {
            schema_version: super::super::manifest::TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            profile_id: "tp-rdma".into(),
            source_commit: COMMIT.into(),
            role_artifacts: vec![worker(), coordinator()],
            model: model(),
            transport: "rdma".into(),
            context_size: 8192,
            argv_contract_id: DIGEST.into(),
        }
    }

    #[test]
    fn worker_with_ds4_server_is_rejected() {
        let mut m = manifest();
        m.role_artifacts[0].executable_kind = ExecutableKind::Ds4Server;
        assert!(matches!(
            ResolvedDs4Profile::resolve(
                &m,
                "tensor-parallel".into(),
                "rdma".into(),
                Residency::Resident,
                SpeculativeSupport::None,
                "cap-1".into(),
            )
            .unwrap_err(),
            ResolveError::RoleExecutableMismatch { .. }
        ));
    }

    #[test]
    fn source_difference_is_rejected() {
        let mut m = manifest();
        m.role_artifacts[1].source_commit = "0000000000000000000000000000000000000000".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn path_difference_does_not_change_shared_identity() {
        // host 固有 path が異なっても共有 deployment identity は一致する。
        let mut m1 = manifest();
        let mut m2 = manifest();
        m1.role_artifacts[0].path = "/host-a/usr/bin/ds4".into();
        m2.role_artifacts[0].path = "/host-b/opt/ds4".into();
        m1.role_artifacts[1].path = "/host-a/usr/bin/ds4-server".into();
        m2.role_artifacts[1].path = "/host-b/opt/ds4-server".into();
        assert_eq!(m1.deployment_id().unwrap(), m2.deployment_id().unwrap());
    }

    #[test]
    fn model_difference_changes_identity() {
        let m1 = manifest();
        let mut m2 = manifest();
        m2.model.sha256 = OTHER_DIGEST.into();
        assert_ne!(m1.deployment_id().unwrap(), m2.deployment_id().unwrap());
    }

    #[test]
    fn resolve_produces_shared_identity_without_host_path() {
        let r = ResolvedDs4Profile::resolve(
            &manifest(),
            "tensor-parallel".into(),
            "rdma".into(),
            Residency::Resident,
            SpeculativeSupport::None,
            "cap-1".into(),
        )
        .unwrap();
        let id = r.deployment_id().unwrap();
        assert_eq!(id.len(), 64);
        // host 固有 path を変えても id は変わらない。
        let mut r2 = r.clone();
        r2.role_artifacts[0].path = "/other/path/ds4".into();
        assert_eq!(id, r2.deployment_id().unwrap());
    }
}
