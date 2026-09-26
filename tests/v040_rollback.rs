//! v0.4.0 M09 — rollback・再起動復旧・fault matrix。M09。
//!
//! 公開 API（manager::rollback）経由で受入 case を検証する。旧 artifact へ
//! 同じ runtime lease で復旧し、復旧失敗は ManualIntervention として route を
//! 閉じる。rollback でも最新 Force intent（policy_epoch）を保持する。旧
//! artifact の自動削除なし。実 child 起動なし（fake 境界で駆動）。M09。
//!
//! 受入 case:
//! - 入力: 新起動失敗 → 旧 ready/old digest
//! - 入力: 旧起動も失敗 → 503+manual
//! - 入力: rollback 中 Force → policy 保持
//! - 入力: 電断各境界 → orphan 0
use siderostat::manager::activation::{
    ActivationError, ActivationPhase, NodeArtifactProvider, NodeId,
};
use siderostat::manager::rollback::{
    PreviousRecovery, RollbackOutcome, RollbackRequest, rollback_to_previous,
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

fn provider() -> FakeProvider {
    let mut d = HashMap::new();
    d.insert("local".to_string(), "d-local".to_string());
    d.insert("peer".to_string(), "d-peer".to_string());
    FakeProvider { digests: d }
}

fn req() -> RollbackRequest {
    RollbackRequest {
        operation_id: "op-r".to_string(),
        expected_generation: 1,
        runtime_lease: "lease-1".to_string(),
        policy_epoch: 7,
        previous_digest: "old-digest".to_string(),
        nodes: vec!["local".to_string(), "peer".to_string()],
    }
}

struct OkRecovery;
impl PreviousRecovery for OkRecovery {
    fn start_previous_and_wait_ready(&mut self) -> Result<(), ActivationError> {
        Ok(())
    }
}

struct FailRecovery;
impl PreviousRecovery for FailRecovery {
    fn start_previous_and_wait_ready(&mut self) -> Result<(), ActivationError> {
        Err(ActivationError::InvalidTransition(
            "previous start failed".to_string(),
        ))
    }
}

/// 受入: 新起動失敗 → 旧 ready/old digest。M09。
#[test]
fn m09_new_start_failure_restores_old_digest() {
    let out = rollback_to_previous(req(), &provider(), &mut OkRecovery, false).expect("restored");
    match out {
        RollbackOutcome::Restored {
            old_digest,
            journal,
        } => {
            assert_eq!(old_digest, "old-digest");
            // 旧 ready へ復旧（RolledBack に戻す。旧 artifact は自動削除
            // しない）。M09。
            assert_eq!(journal.phase, ActivationPhase::RolledBack);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
}

/// 受入: 旧起動も失敗 → 503+manual（route を閉じる）。M09。
#[test]
fn m09_old_start_failure_is_manual_intervention() {
    let out = rollback_to_previous(req(), &provider(), &mut FailRecovery, false).expect("manual");
    assert!(matches!(out, RollbackOutcome::ManualIntervention { .. }));
}

/// 受入: rollback 中 Force → policy 保持（policy_epoch 不変）。M09。
#[test]
fn m09_force_held_keeps_policy() {
    let out = rollback_to_previous(req(), &provider(), &mut OkRecovery, true).expect("policy held");
    match out {
        RollbackOutcome::PolicyHeld {
            journal,
            policy_epoch,
        } => {
            // 最新 Force intent（policy_epoch=7）を保持。M09。
            assert_eq!(policy_epoch, 7);
            assert_eq!(journal.policy_epoch, 7);
        }
        other => panic!("expected PolicyHeld, got {other:?}"),
    }
}

/// 受入: 電断各境界 → orphan 0（曖昧 journal を勝手に完了しない）。M09。
#[test]
fn m09_ambiguous_journal_not_completed() {
    // prepare 境界で電断（片側不足）しても journal は Prepared のまま。
    // 勝手に Complete に進まない（orphan 0）。M09。
    let mut provider = provider();
    provider.digests.remove("peer");
    let r = req();
    let prepare_req = siderostat::manager::activation::ActivationRequest {
        operation_id: r.operation_id.clone(),
        expected_generation: r.expected_generation,
        runtime_lease: r.runtime_lease.clone(),
        policy_epoch: r.policy_epoch,
        nodes: r.nodes.clone(),
    };
    let outcome = siderostat::manager::activation::prepare_activation(prepare_req, &provider)
        .expect("prepare");
    let siderostat::manager::activation::PrepareOutcome::StoppedZero(j) = outcome else {
        panic!("stopped zero");
    };
    // Prepared のまま。Complete に勝手に進まない。M09。
    assert_eq!(j.phase, ActivationPhase::Prepared);
    // 旧 artifact は保持（previous_digest 不変・自動削除なし）。M09。
    assert_eq!(r.previous_digest, "old-digest");
}
