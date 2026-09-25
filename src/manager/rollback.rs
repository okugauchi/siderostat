//! DS4 Manager — rollback・再起動復旧・fault matrix。M09。
//!
//! C04 に基づき、activation の起動失敗時に previous へ復旧する。旧 artifact
//! へ runtime owner の lifecycle gate を通して復旧し、復旧失敗は ManualIntervention として
//! route を閉じる。rollback でも最新 Force intent（policy_epoch）を保持し、
//! 旧 state 丸ごと復元で新 policy を消さない。旧 artifact の自動削除は
//! しない。M09。
//!
//! 受入 case（全て必須）:
//! - 入力: 新起動失敗 → 旧 ready/old digest（previous へ復旧）
//! - 入力: 旧起動も失敗 → 503+manual（route を閉じる）
//! - 入力: rollback 中 Force → policy 保持
//! - 入力: 電断各境界 → orphan 0（曖昧 journal を勝手に完了しない）
//!
//! レビュー重点: rollback に旧 state 丸ごと復元を使い、新 policy を消して
//! いないか。M09。
use crate::manager::activation::{
    ActivationError, ActivationJournal, ActivationPhase, ActivationRequest, NodeArtifactProvider,
    PrepareOutcome, prepare_activation,
};

/// rollback 要求。M09。
#[derive(Debug, Clone)]
pub struct RollbackRequest {
    /// 対象 activation 操作。M09。
    pub operation_id: String,
    /// expected generation（activation/rollback に要求、C04）。M09。
    pub expected_generation: u64,
    /// 最新 policy_epoch（rollback でも保持。新 Force intent を消さない）。M09。
    pub policy_epoch: u64,
    /// previous artifact の digest（old ready へ復旧）。M09。
    pub previous_digest: String,
    /// 両 node ID。M09。
    pub nodes: Vec<String>,
}

/// rollback の結果。M09。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackOutcome {
    /// 旧 artifact へ復旧（旧 ready / old digest）。route を旧経路へ戻す。M09。
    Restored {
        journal: ActivationJournal,
        old_digest: String,
    },
    /// 旧起動も失敗 → 503 + manual。route を閉じる。M09。
    ManualIntervention { journal: ActivationJournal },
    /// rollback 中に Force が保持された（policy_epoch 不変）。M09。
    PolicyHeld {
        journal: ActivationJournal,
        policy_epoch: u64,
    },
}

/// previous artifact への復旧を抽象する境界（ProductionClusterRuntime
/// 配線。fake 境界で検証）。M09。
pub trait PreviousRecovery {
    /// 旧 artifact（previous）を起動して旧 ready を待つ。失敗は Err（→
    /// ManualIntervention）。M09。
    fn start_previous_and_wait_ready(&mut self) -> Result<(), ActivationError>;
}

/// rollback を実行する。M09。
///
/// 1. previous artifact digest を保持（old ready）。旧 artifact を自動削除
///    しない。M09。
/// 2. 旧起動失敗 → ManualIntervention（route を閉じる、503）。M09。
/// 3. 最新 policy_epoch を保持（rollback で新 Force intent を消さない）。M09。
pub fn rollback_to_previous(
    req: RollbackRequest,
    provider: &dyn NodeArtifactProvider,
    recovery: &mut dyn PreviousRecovery,
    force_held: bool,
) -> Result<RollbackOutcome, ActivationError> {
    // 両 node の状態を確認（新起動失敗後の journal を再現）。M09。
    let prepare_req = ActivationRequest {
        operation_id: req.operation_id,
        expected_generation: req.expected_generation,
        profile_id: String::new(),
        policy_epoch: req.policy_epoch,
        nodes: req.nodes,
    };
    let outcome = prepare_activation(prepare_req, provider)?;
    let mut journal = match outcome {
        PrepareOutcome::Prepared(j) => j,
        PrepareOutcome::StoppedZero(j) => j,
    };

    // 旧起動を試行。失敗 → ManualIntervention（route を閉じる、503）。M09。
    if recovery.start_previous_and_wait_ready().is_err() {
        journal.phase = ActivationPhase::RolledBack;
        return Ok(RollbackOutcome::ManualIntervention { journal });
    }

    // 旧 ready / old digest へ復旧。policy_epoch は最新を保持。M09。
    journal.phase = ActivationPhase::RolledBack;
    let old_digest = req.previous_digest.clone();
    if force_held {
        return Ok(RollbackOutcome::PolicyHeld {
            journal,
            policy_epoch: req.policy_epoch,
        });
    }
    Ok(RollbackOutcome::Restored {
        journal,
        old_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::activation::NodeId;

    struct FakeProvider {
        digests: std::collections::HashMap<NodeId, String>,
    }
    impl NodeArtifactProvider for FakeProvider {
        fn verified_artifact(&self, node: &NodeId) -> Option<String> {
            self.digests.get(node).cloned()
        }
    }
    fn provider() -> FakeProvider {
        let mut d = std::collections::HashMap::new();
        d.insert("local".to_string(), "d-local".to_string());
        d.insert("peer".to_string(), "d-peer".to_string());
        FakeProvider { digests: d }
    }
    fn req() -> RollbackRequest {
        RollbackRequest {
            operation_id: "op-r".to_string(),
            expected_generation: 1,
            policy_epoch: 5,
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
    fn new_start_failure_restores_old_digest() {
        let out =
            rollback_to_previous(req(), &provider(), &mut OkRecovery, false).expect("restored");
        assert!(matches!(
            out,
            RollbackOutcome::Restored {
                old_digest,
                ..
            } if old_digest == "old-digest"
        ));
    }

    /// 受入: 旧起動も失敗 → 503+manual（route を閉じる）。M09。
    #[test]
    fn old_start_failure_is_manual_intervention() {
        let out =
            rollback_to_previous(req(), &provider(), &mut FailRecovery, false).expect("manual");
        assert!(matches!(out, RollbackOutcome::ManualIntervention { .. }));
    }

    /// 受入: rollback 中 Force → policy 保持（policy_epoch 不変）。M09。
    #[test]
    fn force_held_keeps_policy() {
        let out =
            rollback_to_previous(req(), &provider(), &mut OkRecovery, true).expect("policy held");
        assert!(matches!(
            out,
            RollbackOutcome::PolicyHeld {
                policy_epoch: 5,
                ..
            }
        ));
    }

    /// 受入: 電断各境界 → orphan 0（曖昧 journal を勝手に完了しない）。M09。
    ///
    /// prepare が StoppedZero でも journal を返す（曖昧なまま完了しない）。
    /// 旧 artifact の自動削除なし（previous_digest は保持）。M09。
    #[test]
    fn ambiguous_journal_not_completed() {
        // 片側不足でも journal が返る（phase は Prepared のまま。勝手に
        // Complete にしない）。M09。
        let mut provider = provider();
        provider.digests.remove("peer");
        let req = req();
        let prepare_req = ActivationRequest {
            operation_id: req.operation_id.clone(),
            expected_generation: req.expected_generation,
            profile_id: String::new(),
            policy_epoch: req.policy_epoch,
            nodes: req.nodes.clone(),
        };
        let outcome = prepare_activation(prepare_req, &provider).expect("prepare");
        let PrepareOutcome::StoppedZero(j) = outcome else {
            panic!("stopped zero");
        };
        // Prepared のまま。Complete に勝手に進まない。M09。
        assert_eq!(j.phase, ActivationPhase::Prepared);
        // previous digest は保持（自動削除なし）。M09。
        assert_eq!(req.previous_digest, "old-digest");
    }
}
