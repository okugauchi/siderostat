//! T01 — capability manifest と main 由来判定の受入 case。。。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    CAPABILITY_MANIFEST_SCHEMA_VERSION, CapabilityStatus, Ds4CapabilityManifest, ExecutableKind,
    MainAncestryProof, RoleArtifact, RoleKind, Verification,
};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const COMMIT: &str = "9ab705347c1775e7599ede7eb81a6255ec7dccb5";

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
            evidence: "ls-remote origin main".into(),
        },
        build_id: "build-1".into(),
        role_artifacts: vec![worker_artifact()],
        transport: "rdma".into(),
        model_family: "deepseek".into(),
        capabilities: vec!["tensor-parallel".into()],
        restrictions: vec!["unsupported-os".into()],
        reference: "https://github.com/okugauchi/ds4".into(),
        verification: Verification {
            verified: true,
            checked_at_millis: 0,
            notes: vec![],
        },
    }
}

/// 受入 case 1: unsupported OS → reason 保持。restrictions に理由を保持し、該当 capability
/// を Unsupported にする。他の正常 capability を巻き添えにしない。。
#[test]
fn unsupported_os_reports_unsupported_with_reason_preserved() {
    let m = manifest();
    assert_eq!(
        m.status_for("unsupported-os").unwrap(),
        CapabilityStatus::Unsupported
    );
    // reason は restrictions に保持されている。。
    assert!(m.restrictions.contains(&"unsupported-os".to_string()));
    // 他 capability は巻き添えにしない（Stable）。。
    assert_eq!(
        m.status_for("tensor-parallel").unwrap(),
        CapabilityStatus::Stable
    );
}

/// 受入 case 2: 同じ source で role binary digest 違い → 役割ごとの許可集合で判定。。
/// worker と coordinator の digest が異なっても、各 role の許可集合に含まれていれば OK。。
#[test]
fn different_digest_per_role_resolved_by_role_specific_set() {
    let worker = worker_artifact();
    let coord = RoleArtifact {
        role: RoleKind::Coordinator,
        executable_kind: ExecutableKind::Ds4Server,
        path: "/usr/local/bin/ds4-server".into(),
        binary_sha256: OTHER_DIGEST.into(),
        compatible_binary_sha256: vec![OTHER_DIGEST.into()],
        source_commit: COMMIT.into(),
        arch: "aarch64".into(),
        backend: "metal".into(),
        help_sha256: OTHER_DIGEST.into(),
    };
    let mut m = manifest();
    m.role_artifacts = vec![worker, coord];
    m.validate()
        .expect("role-specific sets allow differing digests");
    // main 由来 + verified → Stable。。
    assert_eq!(m.assess().unwrap().status, CapabilityStatus::Stable);
}

/// 受入 case 3a: 任意 digest → 拒否。。
#[test]
fn arbitrary_digest_is_rejected() {
    let mut m = manifest();
    m.role_artifacts[0].binary_sha256 = "not-a-digest".into();
    assert!(m.validate().is_err());
}

/// 受入 case 3b: 不正 schema → 拒否。。
#[test]
fn unknown_schema_is_rejected() {
    let mut m = manifest();
    m.schema_version = 99;
    assert!(m.validate().is_err());
}

/// 事後条件: main 由来だけでは stable にしない。未検証 TP は Candidate。。
#[test]
fn unverified_main_is_candidate_not_stable() {
    let mut m = manifest();
    m.verification.verified = false;
    assert_eq!(m.assess().unwrap().status, CapabilityStatus::Candidate);
    assert!(m.assess().unwrap().reason.is_some());
}

/// 事後条件: 未 main は Reference。。
#[test]
fn non_main_is_reference() {
    let mut m = manifest();
    m.main_ancestry_proof.ancestor_of_main = false;
    assert_eq!(m.assess().unwrap().status, CapabilityStatus::Reference);
}
