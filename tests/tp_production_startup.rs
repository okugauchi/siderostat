use siderostat::{
    cluster::{
        OperationId, OperationKind, OperationLease, build_tp_coordinator_command,
        build_tp_worker_command,
    },
    config::{DistributedTopology, ModeAwareConfig, Transport},
};

fn config() -> ModeAwareConfig {
    let mut c = ModeAwareConfig::parse(include_str!(
        "../contrib/macos/Resources/default-config.toml"
    ))
    .unwrap();
    c.ds4.distributed.topology = DistributedTopology::TensorParallel;
    c.ds4.distributed.transport = Transport::Rdma;
    c.ds4.distributed.coordinator_layers = None;
    c.ds4.distributed.worker_layers = None;
    c.ds4.distributed.extra_args.clear();
    c.ds4.distributed.context_size = 8192;
    c.ds4.distributed.rdma_device = Some("rdma_en5".into());
    c.ds4.distributed.rdma_gid = Some("2".into());
    c
}

#[test]
fn tp_commands_preserve_context_and_rdma_selection() {
    let c = config();
    for cmd in [
        build_tp_worker_command(&c.ds4, "10.99.10.1".parse().unwrap(), 9911).unwrap(),
        build_tp_coordinator_command(&c.ds4, "10.99.10.1".parse().unwrap(), 9911).unwrap(),
    ] {
        let args: Vec<_> = cmd.argv.iter().map(|s| s.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|w| w == ["--ctx", "8192"]));
        assert!(args.windows(2).any(|w| w == ["--rdma-device", "rdma_en5"]));
        assert!(args.windows(2).any(|w| w == ["--rdma-gid-index", "2"]));
        assert!(!args.contains(&"--layers"));
    }
}

#[test]
fn tp_extra_args_cannot_override_context_or_device() {
    for flag in [
        "--ctx",
        "--rdma-device",
        "--rdma-gid-index",
        "--tensor-parallel",
        "--ssd-streaming",
    ] {
        let mut c = config();
        c.ds4.distributed.extra_args = vec![flag.into(), "1".into()];
        assert!(
            build_tp_worker_command(&c.ds4, "10.99.10.1".parse().unwrap(), 9911).is_err(),
            "accepted {flag}"
        );
    }
}

#[test]
fn child_operations_are_exclusive_across_kinds() {
    let lease = OperationLease::new();
    let id = OperationId(uuid::Uuid::new_v4());
    lease.try_acquire(OperationKind::Promotion, id).unwrap();
    for kind in [
        OperationKind::Restart,
        OperationKind::Recovery,
        OperationKind::Demotion,
        OperationKind::Policy,
    ] {
        assert!(
            lease
                .try_acquire(kind, OperationId(uuid::Uuid::new_v4()))
                .is_err()
        );
    }
    lease.release(OperationKind::Promotion, id);
    assert!(
        lease
            .try_acquire(OperationKind::Recovery, OperationId(uuid::Uuid::new_v4()))
            .is_ok()
    );
}

#[test]
fn topology_selects_schema_three_without_lp_layers() {
    use siderostat::cluster::RuntimeDeployment;
    let digest = "a".repeat(64);
    let source = "b".repeat(40);
    let mut roles = Vec::new();
    for (role, kind) in [("worker", "ds4"), ("coordinator", "ds4-server")] {
        roles.push(serde_json::json!({"role":role,"executable_kind":kind,
            "path":format!("/tmp/{kind}"),"binary_sha256":digest,
            "compatible_binary_sha256":[digest],"source_commit":source,
            "arch":"aarch64","backend":"metal","help_sha256":digest}));
    }
    let bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version":3,"profile_id":"tp-test","source_commit":source,
        "role_artifacts":roles,"model":{"catalog_id":"fixture","sha256":digest,
            "size":32,"family":"test-family","quantization":"q2"},
        "transport":"rdma","context_size":8192,"argv_contract_id":"v040-tp"
    }))
    .unwrap();
    let deployment = RuntimeDeployment::parse(&bytes, DistributedTopology::TensorParallel).unwrap();
    assert_eq!(deployment.model_family(), "test-family");
    assert!(deployment.tp().is_some());
    let worker = deployment
        .tp_command(
            &config(),
            siderostat::target::LocalRole::Worker,
            "10.99.10.1".parse().unwrap(),
        )
        .unwrap();
    let coordinator = deployment
        .tp_command(
            &config(),
            siderostat::target::LocalRole::Coordinator,
            "10.99.10.1".parse().unwrap(),
        )
        .unwrap();
    assert_eq!(worker.executable, std::path::PathBuf::from("/tmp/ds4"));
    assert_eq!(
        coordinator.executable,
        std::path::PathBuf::from("/tmp/ds4-server")
    );
    assert!(RuntimeDeployment::parse(&bytes, DistributedTopology::LayerParallel).is_err());
}

#[test]
fn tp_log_readiness_requires_complete_rdma_handshake() {
    use siderostat::cluster::TpLogReadiness;
    let mut status = TpLogReadiness::default();
    status.observe("ds4-tp: worker connected, transport=rdma gate-timeout=750ms");
    assert!(!status.connected());
    status.observe("tensor parallelism bound: rank 0, 50/50 expert ownership, tcp transport");
    assert!(!status.connected());
    status.observe("tensor parallelism bound: rank 0, 50/50 expert ownership, rdma transport");
    assert!(status.connected());
    status.observe("TP gate exchange failed");
    assert!(!status.connected());
    status.observe("tensor parallelism bound: rank 0, 50/50 expert ownership, rdma transport");
    assert!(
        !status.connected(),
        "a failed child cannot become ready from late logs"
    );
}

#[test]
fn unverified_capability_cannot_generate_a_tp_deployment() {
    use siderostat::cluster::*;
    let digest = "a".repeat(64);
    let source = "b".repeat(40);
    let roles=[("worker","ds4"),("coordinator","ds4-server")].map(|(r,k)|serde_json::json!({"role":r,"executable_kind":k,"path":format!("/tmp/{k}"),"binary_sha256":digest,"compatible_binary_sha256":[digest],"source_commit":source,"arch":"aarch64","backend":"metal","help_sha256":digest}));
    let mut cap:Ds4CapabilityManifest=serde_json::from_value(serde_json::json!({"schema_version":1,"source_commit":source,"main_ancestry_proof":{"tracked_commit":source,"ancestor_of_main":true,"evidence":"fixture"},"build_id":"fixture","role_artifacts":roles,"transport":"rdma","model_family":"fixture-family","capabilities":["tensor-parallel"],"restrictions":[],"reference":"fixture","verification":{"verified":false,"checked_at_millis":0,"notes":[]}})).unwrap();
    let mut c = config();
    c.ds4.distributed.quantization = siderostat::config::Quantization::Q2;
    assert!(RuntimeDeployment::from_tp_capability(&c, &cap, digest.clone(), 32).is_err());
    cap.verification.verified = true;
    let result = RuntimeDeployment::from_tp_capability(&c, &cap, digest, 32).unwrap();
    assert_eq!(result.model_family(), "fixture-family");
    assert_eq!(result.tp().unwrap().context_size, 8192);
}

#[tokio::test]
async fn force_intent_is_not_rejected_while_promotion_owns_the_lease() {
    use siderostat::cluster::*;
    struct Executor(Arc<tokio::sync::Notify>);
    use std::sync::Arc;
    impl AdminExecutor for Executor {
        fn execute(&self, action: AdminAction) -> AdminFuture {
            let notify = self.0.clone();
            Box::pin(async move {
                if matches!(action, AdminAction::Promote) {
                    notify.notified().await;
                }
                Ok(serde_json::json!({"cluster_complete":true}))
            })
        }
    }
    let notify = Arc::new(tokio::sync::Notify::new());
    let controller =
        AdminController::new(vec![42; 32], Arc::new(Executor(notify.clone()))).unwrap();
    controller.start(AdminAction::Promote).unwrap();
    let result = controller.start_policy(
        uuid::Uuid::new_v4(),
        1,
        OperationPolicy::ForcedStandalone,
        0,
        "local".into(),
    );
    notify.notify_one();
    assert!(
        result.is_ok(),
        "Force intent must reach the runtime while promotion is active"
    );
}
