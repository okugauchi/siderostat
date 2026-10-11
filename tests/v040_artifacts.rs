//! T02 — 外部 artifact の role 別解決と互換 manifest の受入 case。。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    DEPLOYMENT_MANIFEST_SCHEMA_VERSION, DistributedManifest, ExecutableKind, ModelIdentity,
    ResolveError, ResolvedDs4Profile, RoleArtifact, RoleKind,
    TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION, TpDeploymentManifest, convert_layer_parallel,
};
use siderostat::config::{Residency, SpeculativeSupport};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const COMMIT: &str = "9ab705347c1775e7599ede7eb81a6255ec7dccb5";

fn worker(path: &str) -> RoleArtifact {
    RoleArtifact {
        role: RoleKind::Worker,
        executable_kind: ExecutableKind::Ds4,
        path: path.into(),
        binary_sha256: DIGEST.into(),
        compatible_binary_sha256: vec![DIGEST.into()],
        source_commit: COMMIT.into(),
        arch: "aarch64".into(),
        backend: "metal".into(),
        help_sha256: Some(DIGEST.into()),
    }
}

fn coordinator(path: &str) -> RoleArtifact {
    RoleArtifact {
        role: RoleKind::Coordinator,
        executable_kind: ExecutableKind::Ds4Server,
        path: path.into(),
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
        schema_version: TP_DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        profile_id: "tp-rdma".into(),
        source_commit: COMMIT.into(),
        role_artifacts: vec![
            worker("/host-a/usr/bin/ds4"),
            coordinator("/host-a/usr/bin/ds4-server"),
        ],
        model: model(),
        transport: "rdma".into(),
        context_size: 8192,
        argv_contract_id: DIGEST.into(),
    }
}

/// 受入 case 1: worker に ds4-server → 拒否。。
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

/// 受入 case 2a: source 差 → 拒否。同一 deployment 内で role 間の source_commit 不一致。
#[test]
fn source_difference_is_rejected() {
    let mut m = manifest();
    m.role_artifacts[1].source_commit = "0000000000000000000000000000000000000000".into();
    assert!(m.validate().is_err());
}

/// 受入 case 2b: model 差 → 別 deployment identity。。
#[test]
fn model_difference_changes_identity() {
    let m1 = manifest();
    let mut m2 = manifest();
    m2.model.sha256 = OTHER_DIGEST.into();
    assert_ne!(m1.deployment_id().unwrap(), m2.deployment_id().unwrap());
}

/// 受入 case 3: 両 host の path 差 → 共有 deployment identity は一致。。
/// host 固有 path は共有 digest に混ぜない。。
#[test]
fn host_path_difference_keeps_shared_identity() {
    let m1 = manifest();
    let mut m2 = manifest();
    m2.role_artifacts[0].path = "/host-b/opt/ds4".into();
    m2.role_artifacts[1].path = "/host-b/opt/ds4-server".into();
    assert_eq!(m1.deployment_id().unwrap(), m2.deployment_id().unwrap());
}

/// 受入 case 4: 旧 layer fixture → 従来 deployment 契約維持。。
/// schema2 DistributedManifest を内部正規形（schema3）へ変換し、
/// 従来の deployment 契約（source/model から決まり path を含まない）を維持する。。
#[test]
fn legacy_layer_manifest_converts_preserving_deployment_contract() {
    let legacy = DistributedManifest {
        schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        profile: "distributed-mxfp4".into(),
        ds4_binary_sha256: DIGEST.into(),
        compatible_ds4_binary_sha256: vec![DIGEST.into()],
        ds4_source_commit: COMMIT.into(),
        model_sha256: DIGEST.into(),
        model_size: 4096,
        checkpoint: "deepseek-v4".into(),
        model_family: "deepseek".into(),
        quantization: "q4".into(),
        topology: "layer-parallel".into(),
        speculative_support: "none".into(),
        context_size: 8192,
        coordinator_layers: "0-15".into(),
        worker_layers: "16-31".into(),
        ds4_wire_schema: "v10".into(),
        argv_profile_sha256: DIGEST.into(),
    };
    let tp = convert_layer_parallel(&legacy, "tcp".into()).unwrap();
    tp.validate().unwrap();
    // 変換後も profile / source / model の契約を維持する。。
    assert_eq!(tp.profile_id, "distributed-mxfp4");
    assert_eq!(tp.source_commit, COMMIT);
    assert_eq!(tp.model.sha256, DIGEST);
    // 共有 deployment identity は host 固有 path を含まない。。
    assert_eq!(tp.deployment_id().unwrap().len(), 64);
}

/// 事後条件: role 別解決で共有 deployment identity を生成し、host path を混ぜない。。
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
    let mut r2 = r.clone();
    r2.role_artifacts[0].path = "/other/path/ds4".into();
    assert_eq!(id, r2.deployment_id().unwrap());
}
