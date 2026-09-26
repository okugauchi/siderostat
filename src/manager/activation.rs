//! DS4 Manager — cluster-wide activation transaction。M08。
//!
//! C04 に基づき、activation の進行を journal に記録し、状態マシンで管理
//! する。manager 自身は child を直接 signal しない。drain / 旧 child 停止 /
//! 新 profile 起動 / ready / commit ack は `ProductionClusterRuntime` 境界
//! （crate::cluster::production::activation）に依頼する。M08。
//!
//! 受入 case（全て必須）:
//! - 入力: 片側 artifact 無し → 停止 0（旧 runtime を保持）
//! - 入力: 起動失敗 → rollback（previous へ）
//! - 入力: 同時 Force → 安全 yield（新 TP を開始しない）
//! - 入力: ack 紛失 → Complete でない
//!
//! レビュー重点: 片 node だけ binary を変えたまま TP を開始しない。stage
//! API と activation API の権限・意図を分離。M08。
use std::path::PathBuf;

/// node 識別子。M08。
pub type NodeId = String;

/// activation journal の状態（C04: activation は stage→両 node
/// prepare→drain→旧 child 停止→新 ready→両 node commit ack→active 公開）。M08。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPhase {
    /// 両 node digest/epoch prepare 済み。M08。
    Prepared,
    /// runtime lease/drain 中。M08。
    Draining,
    /// 新 profile ready。M08。
    Ready,
    /// 両 node commit ack 待ち（片側 ack のみでもここ。Complete でない）。M08。
    Committing,
    /// active 公開済み（両 node ack 揃った）。M08。
    Complete,
    /// 起動失敗 → previous へ rollback。M08。
    RolledBack,
}

/// 片 node の activation 進行状態。M08。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePhase {
    NotStarted,
    Prepared,
    Draining,
    Ready,
    Committed,
}

/// 片 node の activation 状態。M08。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeActivationState {
    pub node: NodeId,
    pub phase: NodePhase,
    /// verified artifact の digest（prepare 時に記録）。M08。
    pub artifact_digest: Option<String>,
}

/// activation journal。両 node の状態と epoch を記録する（private dir、
/// atomic 保存）。M08。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivationJournal {
    /// 操作 ID。M08。
    pub operation_id: String,
    /// expected generation（activation/rollback に要求、C04）。M08。
    pub expected_generation: u64,
    /// runtime lease（activation/rollback に要求、C04）。M08。
    pub runtime_lease: String,
    /// policy epoch。M08。
    pub policy_epoch: u64,
    /// 全体フェーズ。M08。
    pub phase: ActivationPhase,
    /// 両 node の状態。M08。
    pub nodes: Vec<NodeActivationState>,
}

/// 片 node の verified artifact を提供する境界（production/activation.rs
/// が注入。fake 境界で検証）。M08。
pub trait NodeArtifactProvider {
    /// verified artifact の digest を返す。無いなら None（片側不足）。M08。
    fn verified_artifact(&self, node: &NodeId) -> Option<String>;
}

/// activation 要求（expected_generation + runtime lease 必須、C04）。M08。
#[derive(Debug, Clone)]
pub struct ActivationRequest {
    pub operation_id: String,
    pub expected_generation: u64,
    pub runtime_lease: String,
    pub policy_epoch: u64,
    /// 両 node ID（local, peer）。M08。
    pub nodes: Vec<NodeId>,
}

/// activation エラー。M08。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationError {
    /// expected_generation が未指定（0）または lease が空 → 拒否。M08。
    MissingGenerationOrLease,
    /// 両 node を指定しない → 拒否。M08。
    RequiresTwoNodes,
    /// 不正な遷移（例: Prepared 前の drain）。M08。
    InvalidTransition(String),
    /// node が journal に無い。M08。
    UnknownNode(String),
    /// journal 保存失敗。M08。
    Io(String),
}

impl std::fmt::Display for ActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivationError::MissingGenerationOrLease => {
                write!(f, "expected_generation and runtime lease are required")
            }
            ActivationError::RequiresTwoNodes => write!(f, "activation requires two nodes"),
            ActivationError::InvalidTransition(msg) => write!(f, "invalid transition: {msg}"),
            ActivationError::UnknownNode(id) => write!(f, "unknown node: {id}"),
            ActivationError::Io(msg) => write!(f, "journal io: {msg}"),
        }
    }
}

impl std::error::Error for ActivationError {}

/// prepare の結果。片側 artifact 不足なら StoppedZero（停止 0、旧 runtime
/// を保持）。M08。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// 両 node digest 確認済み。Prepared へ進める。M08。
    Prepared(ActivationJournal),
    /// 片側 artifact 無し → 停止 0。旧 runtime を保持。M08。
    StoppedZero(ActivationJournal),
}

/// activation を prepare する。両 node の verified artifact を確認し、
/// digest と epoch を journal に記録する。片側不足なら StoppedZero。M08。
pub fn prepare_activation(
    req: ActivationRequest,
    provider: &dyn NodeArtifactProvider,
) -> Result<PrepareOutcome, ActivationError> {
    if req.expected_generation == 0 || req.runtime_lease.is_empty() {
        return Err(ActivationError::MissingGenerationOrLease);
    }
    if req.nodes.len() != 2 {
        return Err(ActivationError::RequiresTwoNodes);
    }
    let mut nodes = Vec::new();
    let mut all_present = true;
    for node in &req.nodes {
        let digest = provider.verified_artifact(node);
        if digest.is_none() {
            all_present = false;
        }
        nodes.push(NodeActivationState {
            node: node.clone(),
            phase: NodePhase::Prepared,
            artifact_digest: digest,
        });
    }
    let journal = ActivationJournal {
        operation_id: req.operation_id,
        expected_generation: req.expected_generation,
        runtime_lease: req.runtime_lease,
        policy_epoch: req.policy_epoch,
        phase: ActivationPhase::Prepared,
        nodes,
    };
    if all_present {
        Ok(PrepareOutcome::Prepared(journal))
    } else {
        // 片側 artifact 無し → 停止 0。旧 runtime を保持。M08。
        Ok(PrepareOutcome::StoppedZero(journal))
    }
}

/// drain を開始する（runtime lease/drain。Prepared からのみ）。M08。
pub fn begin_drain(mut journal: ActivationJournal) -> Result<ActivationJournal, ActivationError> {
    if journal.phase != ActivationPhase::Prepared {
        return Err(ActivationError::InvalidTransition(format!(
            "drain requires prepared, got {:?}",
            journal.phase
        )));
    }
    journal.phase = ActivationPhase::Draining;
    for node in &mut journal.nodes {
        node.phase = NodePhase::Draining;
    }
    Ok(journal)
}

/// 新 profile が ready になった（両 node。Draining からのみ）。M08。
pub fn mark_ready(mut journal: ActivationJournal) -> Result<ActivationJournal, ActivationError> {
    if journal.phase != ActivationPhase::Draining {
        return Err(ActivationError::InvalidTransition(format!(
            "ready requires draining, got {:?}",
            journal.phase
        )));
    }
    journal.phase = ActivationPhase::Ready;
    for node in &mut journal.nodes {
        node.phase = NodePhase::Ready;
    }
    Ok(journal)
}

/// commit ack を記録する。両 node の ack が揃うまで Complete にしない
/// （ack 紛失 → Complete でない）。M08。
pub fn commit_ack(
    mut journal: ActivationJournal,
    node: &NodeId,
) -> Result<ActivationJournal, ActivationError> {
    if journal.phase != ActivationPhase::Ready && journal.phase != ActivationPhase::Committing {
        return Err(ActivationError::InvalidTransition(format!(
            "commit ack requires ready/committing, got {:?}",
            journal.phase
        )));
    }
    let target = journal
        .nodes
        .iter_mut()
        .find(|n| &n.node == node)
        .ok_or_else(|| ActivationError::UnknownNode(node.clone()))?;
    target.phase = NodePhase::Committed;
    journal.phase = ActivationPhase::Committing;
    // 両 node 揃ったら Complete（active 公開）。M08。
    if journal
        .nodes
        .iter()
        .all(|n| n.phase == NodePhase::Committed)
    {
        journal.phase = ActivationPhase::Complete;
    }
    Ok(journal)
}

/// 起動失敗 → previous へ rollback。artifact rollback で最新 policy を
/// 巻き戻さない（policy_epoch は保持）。M08。
pub fn rollback_to_previous(mut journal: ActivationJournal) -> ActivationJournal {
    journal.phase = ActivationPhase::RolledBack;
    for node in &mut journal.nodes {
        node.phase = NodePhase::NotStarted;
        node.artifact_digest = None;
    }
    journal
}

/// journal を private dir に atomic 保存する（dir 0700 / file 0600）。M08。
pub fn save_journal(
    journal: &ActivationJournal,
    dir: &std::path::Path,
) -> Result<(), ActivationError> {
    let bytes =
        serde_json::to_vec_pretty(journal).map_err(|e| ActivationError::Io(e.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| ActivationError::Io(e.to_string()))?;
    // private dir（0700）。M08。
    let _ = set_private_perms(dir);
    let path = dir.join(format!("activation-{}.json", journal.operation_id));
    let tmp = dir.join(format!("activation-{}.json.tmp", journal.operation_id));
    std::fs::write(&tmp, &bytes).map_err(|e| ActivationError::Io(e.to_string()))?;
    let _ = set_private_perms(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| ActivationError::Io(e.to_string()))?;
    Ok(())
}

fn set_private_perms(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// journal を load する。無ければ None。M08。
pub fn load_journal(
    dir: &std::path::Path,
    operation_id: &str,
) -> Result<Option<ActivationJournal>, ActivationError> {
    let path = dir.join(format!("activation-{operation_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| ActivationError::Io(e.to_string()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| ActivationError::Io(e.to_string()))
}

/// 既定の journal dir（private root 下）。M08。
pub fn journal_dir(root: &std::path::Path) -> PathBuf {
    root.join("ds4").join("operations")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        digests: std::collections::HashMap<NodeId, String>,
    }

    impl NodeArtifactProvider for FakeProvider {
        fn verified_artifact(&self, node: &NodeId) -> Option<String> {
            self.digests.get(node).cloned()
        }
    }

    fn req(op: &str) -> ActivationRequest {
        ActivationRequest {
            operation_id: op.to_string(),
            expected_generation: 7,
            runtime_lease: "lease-7".to_string(),
            policy_epoch: 3,
            nodes: vec!["local".to_string(), "peer".to_string()],
        }
    }

    fn both() -> FakeProvider {
        let mut d = std::collections::HashMap::new();
        d.insert("local".to_string(), "d-local".to_string());
        d.insert("peer".to_string(), "d-peer".to_string());
        FakeProvider { digests: d }
    }

    /// 受入: 片側 artifact 無し → 停止 0（旧 runtime を保持）。M08。
    #[test]
    fn one_side_missing_is_stopped_zero() {
        let mut provider = both();
        provider.digests.remove("peer");
        let out = prepare_activation(req("op1"), &provider).expect("prepare");
        assert!(matches!(out, PrepareOutcome::StoppedZero(_)));
        // StoppedZero でも両 node の digest は journal に残る（旧 runtime 保持）。M08。
        match out {
            PrepareOutcome::StoppedZero(j) => {
                assert_eq!(j.phase, ActivationPhase::Prepared);
                assert!(
                    j.nodes
                        .iter()
                        .any(|n| n.node == "peer" && n.artifact_digest.is_none())
                );
            }
            _ => panic!("must be stopped zero"),
        }
    }

    /// 両 node 揃えば Prepared。M08。
    #[test]
    fn both_present_prepares() {
        let out = prepare_activation(req("op2"), &both()).expect("prepare");
        assert!(matches!(out, PrepareOutcome::Prepared(_)));
    }

    /// generation/lease 未指定は拒否。M08。
    #[test]
    fn missing_generation_or_lease_rejected() {
        let mut r = req("op3");
        r.expected_generation = 0;
        let err = prepare_activation(r, &both()).expect_err("gen required");
        assert_eq!(err, ActivationError::MissingGenerationOrLease);
        let mut r = req("op3b");
        r.runtime_lease = String::new();
        let err = prepare_activation(r, &both()).expect_err("lease required");
        assert_eq!(err, ActivationError::MissingGenerationOrLease);
    }

    /// 受入: ack 紛失 → Complete でない。両 node ack 揃うまで Committing。M08。
    #[test]
    fn lost_ack_never_completes() {
        let out = prepare_activation(req("op4"), &both()).expect("prepare");
        let PrepareOutcome::Prepared(journal) = out else {
            panic!("prepared");
        };
        let journal = begin_drain(journal).expect("drain");
        let journal = mark_ready(journal).expect("ready");
        // 片側 ack のみ → Committing（Complete でない）。M08。
        let j1 = commit_ack(journal.clone(), &"local".to_string()).expect("local ack");
        assert_eq!(j1.phase, ActivationPhase::Committing);
        assert!(
            j1.nodes
                .iter()
                .any(|n| n.node == "local" && n.phase == NodePhase::Committed)
        );
        assert!(
            j1.nodes
                .iter()
                .any(|n| n.node == "peer" && n.phase != NodePhase::Committed)
        );
        // 他 node の ack が揃うと Complete。M08。
        let j2 = commit_ack(j1, &"peer".to_string()).expect("peer ack");
        assert_eq!(j2.phase, ActivationPhase::Complete);
        assert!(j2.nodes.iter().all(|n| n.phase == NodePhase::Committed));
    }

    /// 受入: 起動失敗 → rollback。policy epoch は保持（policy を巻き戻さない）。M08。
    #[test]
    fn startup_failure_rolls_back_policy_intact() {
        let out = prepare_activation(req("op5"), &both()).expect("prepare");
        let PrepareOutcome::Prepared(journal) = out else {
            panic!("prepared");
        };
        let journal = rollback_to_previous(journal);
        assert_eq!(journal.phase, ActivationPhase::RolledBack);
        // artifact digest は消える（previous へ）。policy epoch は保持。M08。
        assert!(journal.nodes.iter().all(|n| n.artifact_digest.is_none()));
        assert_eq!(journal.policy_epoch, 3);
    }

    /// 不正遷移は拒否（Prepared 前の drain）。M08。
    #[test]
    fn invalid_transition_rejected() {
        let journal = ActivationJournal {
            operation_id: "op6".to_string(),
            expected_generation: 1,
            runtime_lease: "l".to_string(),
            policy_epoch: 1,
            phase: ActivationPhase::Ready,
            nodes: vec![],
        };
        let err = begin_drain(journal).expect_err("ready cannot drain");
        assert!(matches!(err, ActivationError::InvalidTransition(_)));
    }

    /// journal 保存/load の往復（atomic・private）。M08。
    #[test]
    fn journal_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("siderostat-m08-jrn");
        let _ = std::fs::remove_dir_all(&dir);
        let out = prepare_activation(req("op7"), &both()).expect("prepare");
        let PrepareOutcome::Prepared(journal) = out else {
            panic!("prepared");
        };
        save_journal(&journal, &dir).expect("save");
        let loaded = load_journal(&dir, "op7").expect("load").expect("exists");
        assert_eq!(loaded, journal);
    }
}
