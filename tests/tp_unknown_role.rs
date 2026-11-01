use sha2::{Digest, Sha256};
use siderostat::{
    cluster::{Ds4CapabilityManifest, RuntimeDeployment},
    config::{DistributedTopology, ModeAwareConfig, Quantization, Transport},
    target::LocalRole,
};
use std::path::PathBuf;

struct Fixture {
    dir: PathBuf,
    config: ModeAwareConfig,
    capability: Ds4CapabilityManifest,
    deployment: RuntimeDeployment,
}

impl Fixture {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("siderostat-tp-unknown-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let model = dir.join("fixture.gguf");
        let bytes = b"isolated model fixture";
        std::fs::write(&model, bytes).unwrap();
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let binary_digest = "a".repeat(64);
        let source = "b".repeat(40);
        let roles = [("worker", "ds4"), ("coordinator", "ds4-server")].map(|(r, k)| {
            serde_json::json!({"role":r,"executable_kind":k,"path":format!("/nonexistent/{k}"),"binary_sha256":binary_digest,"compatible_binary_sha256":[binary_digest],"source_commit":source,"arch":"aarch64","backend":"metal","help_sha256":binary_digest})
        });
        let capability: Ds4CapabilityManifest = serde_json::from_value(serde_json::json!({"schema_version":1,"source_commit":source,"main_ancestry_proof":{"tracked_commit":source,"ancestor_of_main":true,"evidence":"fixture"},"build_id":"fixture","role_artifacts":roles,"transport":"rdma","model_family":"fixture-family","capabilities":["tensor-parallel"],"restrictions":[],"reference":"fixture","verification":{"verified":true,"checked_at_millis":0,"notes":[]}})).unwrap();
        let cap_path = dir.join("capability.json");
        std::fs::write(&cap_path, serde_json::to_vec(&capability).unwrap()).unwrap();
        let mut config = ModeAwareConfig::parse(include_str!(
            "../contrib/macos/Resources/default-config.toml"
        ))
        .unwrap();
        config.ds4.distributed.topology = DistributedTopology::TensorParallel;
        config.ds4.distributed.transport = Transport::Rdma;
        config.ds4.distributed.quantization = Quantization::Q2;
        config.ds4.distributed.context_size = 8192;
        config.ds4.distributed.model = model;
        config.ds4.distributed.capability_manifest = Some(cap_path);
        let deployment =
            RuntimeDeployment::from_tp_capability(&config, &capability, digest, bytes.len() as u64)
                .unwrap();
        Self {
            dir,
            config,
            capability,
            deployment,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn unknown_role_preserves_common_validation_without_requiring_local_artifact() {
    let f = Fixture::new();
    f.deployment
        .verify_tp(&f.config, LocalRole::Unknown)
        .await
        .unwrap();
    let error = f
        .deployment
        .tp_command(&f.config, LocalRole::Unknown, "10.99.10.1".parse().unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("TP role unknown"));
}

#[tokio::test]
async fn unknown_role_rejects_changed_model_and_unverified_capability() {
    let mut f = Fixture::new();
    std::fs::write(&f.config.ds4.distributed.model, b"changed").unwrap();
    let error = f
        .deployment
        .verify_tp(&f.config, LocalRole::Unknown)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("TP model digest differs"));
    std::fs::write(&f.config.ds4.distributed.model, b"isolated model fixture").unwrap();
    f.capability.verification.verified = false;
    std::fs::write(
        f.config
            .ds4
            .distributed
            .capability_manifest
            .as_ref()
            .unwrap(),
        serde_json::to_vec(&f.capability).unwrap(),
    )
    .unwrap();
    let error = f
        .deployment
        .verify_tp(&f.config, LocalRole::Unknown)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("TP capability is not verified stable")
    );
}

#[tokio::test]
async fn known_role_still_requires_local_executable_validation() {
    let f = Fixture::new();
    for role in [LocalRole::Worker, LocalRole::Coordinator] {
        assert!(
            f.deployment.verify_tp(&f.config, role).await.is_err(),
            "known role accepted missing executable: {role:?}"
        );
    }
}

#[tokio::test]
async fn unknown_role_defers_local_role_file_but_checks_both_role_approvals() {
    let mut f = Fixture::new();
    f.config.ds4.distributed.role_artifact = Some(f.dir.join("missing-role.json"));
    f.deployment
        .resolve_local_artifact(&f.config, LocalRole::Unknown)
        .await
        .unwrap();
    assert!(
        f.deployment
            .resolve_local_artifact(&f.config, LocalRole::Worker)
            .await
            .is_err()
    );
    f.capability.role_artifacts[0].help_sha256 = Some("c".repeat(64));
    std::fs::write(
        f.config
            .ds4
            .distributed
            .capability_manifest
            .as_ref()
            .unwrap(),
        serde_json::to_vec(&f.capability).unwrap(),
    )
    .unwrap();
    let error = f
        .deployment
        .verify_tp(&f.config, LocalRole::Unknown)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("TP role is not approved by capability")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn known_roles_validate_local_binary_and_help_after_role_is_known() {
    use std::os::unix::fs::PermissionsExt;

    let mut f = Fixture::new();
    let help = b"--tensor-parallel --transport --rdma-device --rdma-gid-index\n";
    let binary = b"#!/bin/sh\nprintf '%s\\n' '--tensor-parallel --transport --rdma-device --rdma-gid-index'\n";
    let hex = |bytes: &[u8]| {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    for artifact in &mut f.capability.role_artifacts {
        let path = f.dir.join(format!("{:?}", artifact.role));
        std::fs::write(&path, binary).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        artifact.path = std::fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        artifact.binary_sha256 = hex(binary);
        artifact.compatible_binary_sha256 = vec![hex(binary)];
        artifact.help_sha256 = Some(hex(help));
    }
    std::fs::write(
        f.config
            .ds4
            .distributed
            .capability_manifest
            .as_ref()
            .unwrap(),
        serde_json::to_vec(&f.capability).unwrap(),
    )
    .unwrap();
    f.deployment = RuntimeDeployment::from_tp_capability(
        &f.config,
        &f.capability,
        hex(b"isolated model fixture"),
        b"isolated model fixture".len() as u64,
    )
    .unwrap();
    for role in [LocalRole::Worker, LocalRole::Coordinator] {
        f.deployment.verify_tp(&f.config, role).await.unwrap();
    }
    std::fs::write(&f.capability.role_artifacts[0].path, b"modified").unwrap();
    let error = f
        .deployment
        .verify_tp(&f.config, LocalRole::Worker)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("TP role binary digest mismatch"));
    f.deployment
        .verify_tp(&f.config, LocalRole::Coordinator)
        .await
        .unwrap();
}
