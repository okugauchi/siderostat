//! cluster-wide activation transaction — ProductionClusterRuntime 依頼境界。M08。
//!
//! manager 自身は child を直接 signal しない。drain / 旧 owned child 停止 /
//! 新 profile 起動 / ready / commit ack は ProductionClusterRuntime（この境界が
//! 抽象する `ActivationDriver`）へ依頼する。実 child 起動・OS 接触は行わない
//! （dry-run / fake 境界で駆動。実配線は H01 承認後の H 系）。M08。
//!
//! 受入 case（全て必須）:
//! - 入力: 片側 artifact 無し → 停止 0（旧 runtime を保持）
//! - 入力: 起動失敗 → rollback（previous へ）
//! - 入力: 同時 Force → 安全 yield（新 TP を開始しない）
//! - 入力: ack 紛失 → Complete でない
//!
//! レビュー重点: 片 node だけ binary を変えたまま TP を開始しない。stage
//! API と activation API の権限・意図を分離。M08。
use crate::cluster::policy::OperationPolicy;
use crate::cluster::production::tp::{TpStartVerdict, check_tp_start};
use crate::manager::activation::NodeArtifactProvider;
use crate::manager::activation::{
    ActivationError, ActivationJournal, ActivationPhase, NodeId, NodePhase, PrepareOutcome,
    begin_drain, commit_ack, mark_ready, prepare_activation, rollback_to_previous,
};

/// activation 実行の結果。M08。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationVerdict {
    /// 両 node commit ack が揃い active 公開済み。M08。
    Complete(ActivationJournal),
    /// One or more explicit commit acknowledgements are still outstanding.
    AwaitingPeerAck(ActivationJournal),
    /// 片側 artifact 無し → 停止 0。旧 runtime を保持。M08。
    StoppedZero(ActivationJournal),
    /// 新 profile 起動失敗 → previous へ rollback。M08。
    RolledBack(ActivationJournal),
    /// Force policy（ForcedStandalone）→ 新 TP を開始しない（安全 yield）。M08。
    Yielded(ActivationJournal),
}

/// ProductionClusterRuntime への activation 依頼境界。実装は H 系で
/// ProductionClusterRuntime を配線（この trait が child 操作を抽象し、
/// manager が child を直接 signal しないことを保証）。M08。
pub trait ActivationDriver {
    /// runtime lease/drain を開始し、旧 owned child を停止する。M08。
    fn drain_and_stop_old(&mut self) -> Result<(), ActivationError>;
    /// 新 profile を起動して ready を待つ。失敗は Err（→ rollback）。M08。
    fn start_new_and_wait_ready(&mut self) -> Result<(), ActivationError>;
    /// Return only participant IDs whose commit acknowledgement was actually received.
    /// A local commit is never treated as an implicit peer acknowledgement.
    fn commit(&mut self) -> Result<Vec<NodeId>, ActivationError>;
}

/// cluster-wide activation transaction を実行する。M08。
///
/// 1. 両 node の verified artifact を確認（片側不足 → StoppedZero）。M08。
/// 2. policy が ForcedStandalone なら新 TP を開始しない（安全 yield）。M08。
/// 3. drain → 旧 child 停止 → 新 profile 起動 → ready → 両 node commit ack。M08。
///    起動失敗は previous へ rollback。M08。
pub fn execute_activation(
    req: crate::manager::activation::ActivationRequest,
    provider: &dyn NodeArtifactProvider,
    driver: &mut dyn ActivationDriver,
    operator_policy: OperationPolicy,
    peer_protocol_version: Option<u16>,
) -> Result<ActivationVerdict, ActivationError> {
    // 両 node の verified artifact を確認。片側不足 → 停止 0。M08。
    let outcome = prepare_activation(req, provider)?;
    let journal = match outcome {
        PrepareOutcome::Prepared(j) => j,
        PrepareOutcome::StoppedZero(j) => return Ok(ActivationVerdict::StoppedZero(j)),
    };

    // Force policy → 新 TP を開始しない（安全 yield）。M08。
    let verdict = check_tp_start(operator_policy, peer_protocol_version);
    if !verdict.allows_tp() {
        let journal = rollback_to_previous(journal);
        return Ok(ActivationVerdict::Yielded(journal));
    }

    // drain → 旧 child 停止。M08。
    let journal = begin_drain(journal)?;
    driver.drain_and_stop_old()?;

    // 新 profile 起動 → ready。起動失敗は previous へ rollback。M08。
    if driver.start_new_and_wait_ready().is_err() {
        return Ok(ActivationVerdict::RolledBack(rollback_to_previous(journal)));
    }
    let journal = mark_ready(journal)?;

    // Acknowledgements are explicit; the driver cannot turn one local commit into two acks.
    let acknowledgements = driver.commit()?;
    let mut journal = journal;
    for node_id in acknowledgements {
        journal = commit_ack(journal, &node_id)?;
    }
    if journal.phase == ActivationPhase::Complete {
        Ok(ActivationVerdict::Complete(journal))
    } else {
        Ok(ActivationVerdict::AwaitingPeerAck(journal))
    }
}

/// TpStartVerdict の名前（診断用。TpStartVerdict は pub(crate) のため
/// crate 外には公開しない。module テストのみで使用）。M08。
#[allow(dead_code)]
pub(crate) fn verdict_name(v: TpStartVerdict) -> &'static str {
    v.name()
}

/// 片 node の phase 名（診断用）。M08。
pub fn node_phase_name(p: NodePhase) -> &'static str {
    match p {
        NodePhase::NotStarted => "not-started",
        NodePhase::Prepared => "prepared",
        NodePhase::Draining => "draining",
        NodePhase::Ready => "ready",
        NodePhase::Committed => "committed",
    }
}

/// 片 node の digest を journal から取得（両 node 同一 digest の確認用）。
/// 片 node だけ binary を変えたまま TP を開始しない。M08。
pub fn node_digests(journal: &ActivationJournal) -> Vec<(NodeId, Option<String>)> {
    journal
        .nodes
        .iter()
        .map(|n| (n.node.clone(), n.artifact_digest.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::activation::ActivationRequest;

    struct FakeProvider {
        digests: std::collections::HashMap<NodeId, String>,
    }
    impl NodeArtifactProvider for FakeProvider {
        fn verified_artifact(&self, node: &NodeId) -> Option<String> {
            self.digests.get(node).cloned()
        }
    }
    fn provider(local: bool, peer: bool) -> FakeProvider {
        let mut d = std::collections::HashMap::new();
        if local {
            d.insert("local".to_string(), "d-local".to_string());
        }
        if peer {
            d.insert("peer".to_string(), "d-peer".to_string());
        }
        FakeProvider { digests: d }
    }
    fn req() -> ActivationRequest {
        ActivationRequest {
            operation_id: "op".to_string(),
            expected_generation: 1,
            profile_id: "profile-local".to_string(),
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
        fn commit(&mut self) -> Result<Vec<NodeId>, ActivationError> {
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
        fn commit(&mut self) -> Result<Vec<NodeId>, ActivationError> {
            Ok(Vec::new())
        }
    }

    /// 受入: 片側 artifact 無し → 停止 0。M08。
    #[test]
    fn one_side_missing_stops_zero() {
        let out = execute_activation(
            req(),
            &provider(false, true),
            &mut OkDriver,
            OperationPolicy::Automatic,
            Some(1),
        )
        .expect("stopped zero");
        assert!(matches!(out, ActivationVerdict::StoppedZero(_)));
    }

    /// 両 node 揃い Automatic → Complete。M08。
    #[test]
    fn both_present_automatic_completes() {
        let out = execute_activation(
            req(),
            &provider(true, true),
            &mut OkDriver,
            OperationPolicy::Automatic,
            Some(1),
        )
        .expect("complete");
        assert!(matches!(out, ActivationVerdict::Complete(_)));
    }

    #[test]
    fn one_commit_ack_remains_waiting_for_peer_ack() {
        struct LocalAckDriver;
        impl ActivationDriver for LocalAckDriver {
            fn drain_and_stop_old(&mut self) -> Result<(), ActivationError> {
                Ok(())
            }
            fn start_new_and_wait_ready(&mut self) -> Result<(), ActivationError> {
                Ok(())
            }
            fn commit(&mut self) -> Result<Vec<NodeId>, ActivationError> {
                Ok(vec!["local".into()])
            }
        }

        let out = execute_activation(
            req(),
            &provider(true, true),
            &mut LocalAckDriver,
            OperationPolicy::Automatic,
            Some(1),
        )
        .expect("partial commit remains pending");
        let ActivationVerdict::AwaitingPeerAck(journal) = out else {
            panic!("a single acknowledgement must not complete a two-node activation");
        };
        assert_eq!(journal.phase, ActivationPhase::Committing);
        assert_eq!(journal.nodes[0].phase, NodePhase::Committed);
        assert_eq!(journal.nodes[1].phase, NodePhase::Ready);
    }

    /// 受入: 起動失敗 → rollback。M08。
    #[test]
    fn startup_failure_rolls_back() {
        let out = execute_activation(
            req(),
            &provider(true, true),
            &mut FailStartDriver,
            OperationPolicy::Automatic,
            Some(1),
        )
        .expect("rollback");
        assert!(matches!(out, ActivationVerdict::RolledBack(_)));
    }

    /// 受入: 同時 Force → 安全 yield（新 TP を開始しない）。M08。
    #[test]
    fn force_policy_yields() {
        let out = execute_activation(
            req(),
            &provider(true, true),
            &mut OkDriver,
            OperationPolicy::ForcedStandalone,
            Some(1),
        )
        .expect("yield");
        assert!(matches!(out, ActivationVerdict::Yielded(_)));
    }

    /// 両 node 同一 digest を確認（片 node だけ binary を変えない）。M08。
    #[test]
    fn both_nodes_share_digests() {
        let outcome = prepare_activation(req(), &provider(true, true)).expect("prepare");
        let PrepareOutcome::Prepared(j) = outcome else {
            panic!("prepared");
        };
        let digests = node_digests(&j);
        assert_eq!(digests.len(), 2);
        assert!(digests.iter().all(|(_, d)| d.is_some()));
    }

    /// verdict 名（診断）。M08。
    #[test]
    fn verdict_name_smoke() {
        assert_eq!(
            verdict_name(TpStartVerdict::PolicyForcedStandalone),
            "tp-start-policy-forced-standalone"
        );
        assert_eq!(node_phase_name(NodePhase::Ready), "ready");
    }
}
