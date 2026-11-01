//! Topology-aware deployment loading. LP keeps its original schema and identity.
use super::{
    CapabilityStatus, DistributedManifest, Ds4CapabilityManifest, RoleKind, TpDeploymentManifest,
    fingerprint_file,
};
use crate::{
    config::{DistributedTopology, ModeAwareConfig},
    target::LocalRole,
};
use anyhow::{Context, ensure};

#[derive(Debug, Clone)]
pub enum RuntimeDeployment {
    LayerParallel(DistributedManifest),
    TensorParallel(TpDeploymentManifest),
}

impl From<DistributedManifest> for RuntimeDeployment {
    fn from(value: DistributedManifest) -> Self {
        Self::LayerParallel(value)
    }
}

impl RuntimeDeployment {
    pub fn tp_command(
        &self,
        config: &ModeAwareConfig,
        role: LocalRole,
        coordinator: std::net::IpAddr,
    ) -> anyhow::Result<super::Ds4Command> {
        let m = self
            .tp()
            .context("TP command requires schema3 deployment")?;
        let (kind, mut command) = match role {
            LocalRole::Worker => (
                RoleKind::Worker,
                super::build_tp_worker_command(
                    &config.ds4,
                    coordinator,
                    config.cluster.ds4_distributed_port,
                )?,
            ),
            LocalRole::Coordinator => (
                RoleKind::Coordinator,
                super::build_tp_coordinator_command(
                    &config.ds4,
                    coordinator,
                    config.cluster.ds4_distributed_port,
                )?,
            ),
            LocalRole::Unknown => anyhow::bail!("TP role unknown"),
        };
        command.executable = m
            .role_artifacts
            .iter()
            .find(|a| a.role == kind)
            .context("TP role missing")?
            .path
            .clone()
            .into();
        Ok(command)
    }
    pub async fn resolve_local_artifact(
        &mut self,
        config: &ModeAwareConfig,
        role: LocalRole,
    ) -> anyhow::Result<()> {
        let Self::TensorParallel(m) = self else {
            return Ok(());
        };
        let kind = match role {
            LocalRole::Worker => RoleKind::Worker,
            LocalRole::Coordinator => RoleKind::Coordinator,
            LocalRole::Unknown => return Ok(()),
        };
        if let Some(path) = &config.ds4.distributed.role_artifact {
            let local: super::RoleArtifact = serde_json::from_slice(&tokio::fs::read(path).await?)?;
            let slot = m
                .role_artifacts
                .iter_mut()
                .find(|a| a.role == kind)
                .context("TP local role missing")?;
            ensure!(
                local.role == kind
                    && local.binary_sha256 == slot.binary_sha256
                    && local.source_commit == slot.source_commit
                    && local.executable_kind == slot.executable_kind
                    && local.help_sha256 == slot.help_sha256,
                "local TP role artifact differs from deployment"
            );
            slot.path = local.path;
        }
        Ok(())
    }
    pub fn from_tp_capability(
        config: &ModeAwareConfig,
        cap: &Ds4CapabilityManifest,
        model_sha256: String,
        model_size: u64,
    ) -> anyhow::Result<Self> {
        ensure!(
            cap.status_for("tensor-parallel")? == CapabilityStatus::Stable
                && cap.capabilities.iter().any(|c| c == "tensor-parallel"),
            "TP capability must be verified stable"
        );
        ensure!(
            cap.source_commit == cap.main_ancestry_proof.tracked_commit && cap.transport == "rdma",
            "TP source/transport capability mismatch"
        );
        let result = Self::TensorParallel(TpDeploymentManifest {
            schema_version: super::TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            profile_id: format!(
                "{}-{}-tp",
                config.ds4.distributed.checkpoint,
                config.ds4.distributed.quantization.name()
            ),
            source_commit: cap.source_commit.clone(),
            role_artifacts: cap.role_artifacts.clone(),
            model: super::ModelIdentity {
                catalog_id: config.ds4.distributed.checkpoint.clone(),
                sha256: model_sha256,
                size: model_size,
                family: cap.model_family.clone(),
                quantization: config.ds4.distributed.quantization.name().into(),
                encoder_digest: None,
                support_digest: None,
                prefix_file_digest: None,
            },
            transport: "rdma".into(),
            context_size: u64::from(config.ds4.distributed.context_size),
            argv_contract_id: "siderostat-tp-v1".into(),
        });
        result.validate()?;
        Ok(result)
    }
    pub fn parse(bytes: &[u8], topology: DistributedTopology) -> anyhow::Result<Self> {
        let result = match topology {
            DistributedTopology::LayerParallel => {
                Self::LayerParallel(serde_json::from_slice(bytes)?)
            }
            DistributedTopology::TensorParallel => {
                Self::TensorParallel(serde_json::from_slice(bytes)?)
            }
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::LayerParallel(m) => m.validate()?,
            Self::TensorParallel(m) => {
                m.validate()?;
                ensure!(m.transport == "rdma", "TP requires explicit RDMA transport");
                ensure!(
                    m.role_artifacts.len() == 2,
                    "TP requires both role artifacts"
                );
                ensure!(
                    m.role_artifacts
                        .iter()
                        .all(|r| r.help_sha256.is_some() && r.backend == "metal"),
                    "TP requires verified Metal role help"
                );
            }
        }
        Ok(())
    }

    pub fn deployment_id(&self) -> anyhow::Result<String> {
        Ok(match self {
            Self::LayerParallel(m) => m.deployment_id()?,
            Self::TensorParallel(m) => m.deployment_id()?,
        })
    }
    pub fn model_family(&self) -> &str {
        match self {
            Self::LayerParallel(m) => &m.model_family,
            Self::TensorParallel(m) => &m.model.family,
        }
    }
    pub fn tp(&self) -> Option<&TpDeploymentManifest> {
        match self {
            Self::TensorParallel(m) => Some(m),
            _ => None,
        }
    }
    pub fn lp(&self) -> anyhow::Result<&DistributedManifest> {
        match self {
            Self::LayerParallel(m) => Ok(m),
            _ => anyhow::bail!("LP operation cannot use a TP deployment"),
        }
    }

    /// Runs before startup cleanup or any managed child is changed.
    /// An unknown role validates common identity and both role approvals, but defers
    /// local executable validation until a subsequent boot with a known role. TP
    /// startup must remain blocked until that role-specific validation succeeds.
    pub async fn verify_tp(&self, config: &ModeAwareConfig, role: LocalRole) -> anyhow::Result<()> {
        let Some(m) = self.tp() else { return Ok(()) };
        ensure!(
            u64::from(config.ds4.distributed.context_size) == m.context_size,
            "TP context differs from deployment"
        );
        ensure!(
            config.ds4.distributed.quantization.name() == m.model.quantization,
            "TP quantization differs from deployment"
        );
        let path = config
            .ds4
            .distributed
            .capability_manifest
            .as_ref()
            .context("TP capability_manifest is required")?;
        let cap: Ds4CapabilityManifest = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        ensure!(
            cap.status_for("tensor-parallel")? == CapabilityStatus::Stable
                && cap.capabilities.iter().any(|c| c == "tensor-parallel"),
            "TP capability is not verified stable"
        );
        ensure!(
            cap.source_commit == m.source_commit
                && cap.main_ancestry_proof.tracked_commit == m.source_commit
                && cap.transport == "rdma"
                && cap.model_family == m.model.family,
            "TP capability/deployment mismatch"
        );
        let model = fingerprint_file(&config.ds4.distributed.model).await?;
        ensure!(
            model.size == m.model.size && model.sha256 == m.model.sha256,
            "TP model digest differs from deployment"
        );
        let local_role = match role {
            LocalRole::Worker => Some(RoleKind::Worker),
            LocalRole::Coordinator => Some(RoleKind::Coordinator),
            LocalRole::Unknown => None,
        };
        for artifact in &m.role_artifacts {
            let approved = cap
                .role_artifacts
                .iter()
                .find(|a| a.role == artifact.role)
                .context("TP capability lacks role")?;
            ensure!(
                approved.source_commit == m.source_commit
                    && approved.executable_kind == artifact.executable_kind
                    && approved
                        .compatible_binary_sha256
                        .contains(&artifact.binary_sha256)
                    && approved.help_sha256 == artifact.help_sha256,
                "TP role is not approved by capability"
            );
            // Only the local role path is used; peer paths need not exist on this host.
            if Some(artifact.role) != local_role {
                continue;
            }
            let path = std::path::Path::new(&artifact.path);
            ensure!(
                path.is_absolute()
                    && tokio::fs::canonicalize(path).await? == path
                    && !tokio::fs::symlink_metadata(path)
                        .await?
                        .file_type()
                        .is_symlink(),
                "TP role path must be canonical and non-symlink"
            );
            let actual = fingerprint_file(path).await?;
            ensure!(
                actual.sha256 == artifact.binary_sha256,
                "TP role binary digest mismatch"
            );
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::process::Command::new(path)
                    .args(["--help", "distributed"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            ensure!(output.status.success(), "TP role help failed");
            use sha2::{Digest, Sha256};
            let mut help = output.stdout;
            help.extend(output.stderr);
            ensure!(
                Some(super::manifest::lower_hex(&Sha256::digest(&help))) == artifact.help_sha256,
                "TP role help digest mismatch"
            );
            let help = String::from_utf8_lossy(&help);
            ensure!(
                [
                    "--tensor-parallel",
                    "--transport",
                    "--rdma-device",
                    "--rdma-gid-index"
                ]
                .iter()
                .all(|flag| help.contains(flag)),
                "TP role lacks required help contract"
            );
        }
        Ok(())
    }
}
