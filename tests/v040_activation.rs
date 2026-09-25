//! v0.4.0 M08 — cluster-wide activation transaction。M08。
//!
//! 公開 API（manager::activation の状態マシン + production::activation の
//! runtime 依頼境界）経由で受入 case を検証する。実 child 起動・OS 接触は
//! 行わない（dry-run / fake 境界で駆動。実配線は H01 承認後の H 系）。M08。
//!
//! 受入 case:
//! - 入力: 片側 artifact 無し → 停止 0（旧 runtime を保持）
//! - 入力: 起動失敗 → rollback（previous へ）
//! - 入力: 同時 Force → 安全 yield（新 TP を開始しない）
//! - 入力: ack 紛失 → Complete でない
use siderostat::cluster::OperationPolicy;
use siderostat::cluster::production::activation::{
    ActivationDriver, ActivationVerdict, execute_activation,
};
use siderostat::manager::activation::{
    ActivationError, ActivationPhase, ActivationRequest, NodeArtifactProvider, NodeId, NodePhase,
    PrepareOutcome, begin_drain, commit_ack, mark_ready, prepare_activation, rollback_to_previous,
};
use std::collections::HashMap;

struct FakeProvider {
    digests: HashMap<NodeId, String>,
}
impl NodeArtifactProvider for FakeProvider {
    fn verified_artifact(&self, node: &NodeId) -> Option<String> {
        self.digests.get(node).cloned()
    }
}

fn provider(local: bool, peer: bool) -> FakeProvider {
    let mut d = HashMap::new();
    if local {
        d.insert("local".to_string(), "d-local".to_string());
    }
    if peer {
        d.insert("peer".to_string(), "d-peer".to_string());
    }
    FakeProvider { digests: d }
}

fn req(op: &str) -> ActivationRequest {
    ActivationRequest {
        operation_id: op.to_string(),
        expected_generation: 1,
        profile_id: "profile-test".to_string(),
        policy_epoch: 1,
        nodes: vec!["local".to_string(), "peer".to_string()],
    }
}

struct OkDriver;
impl ActivationDriver for OkDriver {
    fn drain_and_stop_old(&mut self) -> Result<(), ActivationError> {
        Ok(())
    }
    fn start_new_and_wait_ready(&mut self) -> Result<(), ActivationError> {
        Ok(())
    }
    fn commit(&mut self) -> Result<Vec<String>, ActivationError> {
        Ok(vec!["local".into(), "peer".into()])
    }
}

struct FailStartDriver;
impl ActivationDriver for FailStartDriver {
    fn drain_and_stop_old(&mut self) -> Result<(), ActivationError> {
        Ok(())
    }
    fn start_new_and_wait_ready(&mut self) -> Result<(), ActivationError> {
        Err(ActivationError::InvalidTransition(
            "start failed".to_string(),
        ))
    }
    fn commit(&mut self) -> Result<Vec<String>, ActivationError> {
        Ok(Vec::new())
    }
}

/// 受入: 片側 artifact 無し → 停止 0（旧 runtime を保持）。M08。
#[test]
fn m08_one_side_missing_stops_zero() {
    // 両 node を指定し、peer の artifact が無い。M08。
    let outcome = prepare_activation(req("op-a"), &provider(true, false)).expect("prepare");
    assert!(matches!(outcome, PrepareOutcome::StoppedZero(_)));
    // execute_activation 経由でも StoppedZero。旧 runtime を保持し child は
    // 停止しない（drain に進まない）。M08。
    let verdict = execute_activation(
        req("op-a"),
        &provider(true, false),
        &mut OkDriver,
        OperationPolicy::Automatic,
        Some(1),
    )
    .expect("stopped zero");
    assert!(matches!(verdict, ActivationVerdict::StoppedZero(_)));
}

/// 受入: 起動失敗 → rollback（previous へ）。M08。
#[test]
fn m08_startup_failure_rolls_back() {
    // execute_activation: start_new_and_wait_ready 失敗 → RolledBack。M08。
    let verdict = execute_activation(
        req("op-b"),
        &provider(true, true),
        &mut FailStartDriver,
        OperationPolicy::Automatic,
        Some(1),
    )
    .expect("rollback");
    assert!(matches!(verdict, ActivationVerdict::RolledBack(_)));
    // 状態マシン単体でも rollback は previous へ（digest 消去・policy epoch
    // 保持）。M08。
    let outcome = prepare_activation(req("op-b"), &provider(true, true)).expect("prepare");
    let PrepareOutcome::Prepared(journal) = outcome else {
        panic!("prepared");
    };
    let rolled = rollback_to_previous(journal);
    assert_eq!(rolled.phase, ActivationPhase::RolledBack);
    assert!(rolled.nodes.iter().all(|n| n.artifact_digest.is_none()));
    assert_eq!(rolled.policy_epoch, 1);
}

/// 受入: 同時 Force → 安全 yield（新 TP を開始しない）。M08。
#[test]
fn m08_force_policy_yields() {
    let verdict = execute_activation(
        req("op-c"),
        &provider(true, true),
        &mut OkDriver,
        OperationPolicy::ForcedStandalone,
        Some(1),
    )
    .expect("yield");
    assert!(matches!(verdict, ActivationVerdict::Yielded(_)));
}

/// 受入: ack 紛失 → Complete でない。両 node ack 揃うまで Committing。M08。
#[test]
fn m08_lost_ack_never_completes() {
    let outcome = prepare_activation(req("op-d"), &provider(true, true)).expect("prepare");
    let PrepareOutcome::Prepared(journal) = outcome else {
        panic!("prepared");
    };
    let journal = begin_drain(journal).expect("drain");
    let journal = mark_ready(journal).expect("ready");
    // 片側 ack のみ → Committing（Complete でない）。M08。
    let j1 = commit_ack(journal, &"local".to_string()).expect("local ack");
    assert_eq!(j1.phase, ActivationPhase::Committing);
    assert!(
        j1.nodes
            .iter()
            .any(|n| n.node == "peer" && n.phase != NodePhase::Committed)
    );
    // 他 node ack が揃うと Complete。M08。
    let j2 = commit_ack(j1, &"peer".to_string()).expect("peer ack");
    assert_eq!(j2.phase, ActivationPhase::Complete);
}

/// 両 node 揃い Automatic → Complete（execute_activation）。M08。
#[test]
fn m08_both_present_automatic_completes() {
    let verdict = execute_activation(
        req("op-e"),
        &provider(true, true),
        &mut OkDriver,
        OperationPolicy::Automatic,
        Some(1),
    )
    .expect("complete");
    assert!(matches!(verdict, ActivationVerdict::Complete(_)));
}

/// expected_generation 必須。runtime lease は runtime owner が取得する。M08。
#[test]
fn m08_generation_required() {
    let mut r = req("op-f");
    r.expected_generation = 0;
    let err = prepare_activation(r, &provider(true, true)).expect_err("gen required");
    assert_eq!(err, ActivationError::MissingGeneration);
}
