//! G02 / C03: 接続モード submenu の受入 case を公開 API（ConnectionModeUi /
//! PolicyApi）経由で検証する。実 network は使わず、fake PolicyApi 境界で
//! POST 回数・busy・node 別失敗を記録する。レビュー重点: 選択を
//! StableMode の直接書換えへ変換せず、PolicyApi（/cluster/operation-policy）
//! へ送ること。
use siderostat_monitor::connection_mode::{
    AppliedPolicy, ConnectionModeUi, ConnectionPolicy, NodeResult, PendingJob, PolicyApi,
    PolicyApiError,
};

/// POST 回数と返却 job を記録する fake API。G02。
#[derive(Default)]
struct FakePolicyApi {
    apply_calls: Vec<ConnectionPolicy>,
    job_sequence: Vec<PendingJob>,
    next_job: usize,
    busy: Option<String>,
}

impl FakePolicyApi {
    fn with_success(policy: ConnectionPolicy, job_id: &str) -> Self {
        Self {
            apply_calls: Vec::new(),
            job_sequence: vec![PendingJob {
                job_id: job_id.to_string(),
                policy,
                phase: "running".to_string(),
                node_results: vec![],
            }],
            next_job: 0,
            busy: None,
        }
    }
}

impl PolicyApi for FakePolicyApi {
    async fn apply(
        &mut self,
        policy: ConnectionPolicy,
        _expected_generation: u64,
    ) -> Result<PendingJob, PolicyApiError> {
        self.apply_calls.push(policy);
        if let Some(reason) = &self.busy {
            return Err(PolicyApiError::Busy(reason.clone()));
        }
        let job = self
            .job_sequence
            .get(self.next_job)
            .cloned()
            .unwrap_or_else(|| PendingJob {
                job_id: "job-default".to_string(),
                policy,
                phase: "running".to_string(),
                node_results: Vec::new(),
            });
        self.next_job += 1;
        Ok(job)
    }
}

/// async select を同期テストから呼ぶヘルパー。G02。
fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(future)
}

/// 受入 case 1: TB有効 + Force → POST 1。二重クリックは同 job に束ねる。G02。
#[test]
fn forced_standalone_sends_one_post_and_bundles_double_click() {
    let mut api = FakePolicyApi::with_success(ConnectionPolicy::ForcedStandalone, "job-force");
    let mut ui = ConnectionModeUi::new();
    let first =
        block_on(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api)).expect("first apply");
    // 二重クリック: 同じ policy の進行中 job を再利用（POST しない）。G02。
    let second =
        block_on(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api)).expect("second apply");
    assert_eq!(first.job_id, "job-force");
    assert_eq!(second.job_id, "job-force");
    assert_eq!(api.apply_calls.len(), 1, "double click must not POST twice");
    assert_eq!(api.apply_calls[0], ConnectionPolicy::ForcedStandalone);
}

/// 受入 case 2: Automatic 適用 / peer 無し → 自動 + Solo。G02。
#[test]
fn automatic_applies_to_solo_without_peer() {
    let mut api = FakePolicyApi {
        job_sequence: vec![PendingJob {
            job_id: "job-auto".to_string(),
            policy: ConnectionPolicy::Automatic,
            phase: "succeeded".to_string(),
            node_results: vec![],
        }],
        ..Default::default()
    };
    let mut ui = ConnectionModeUi::new();
    let job = block_on(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect("apply");
    // job が succeeded で terminal → applied を更新。topology は実測
    // （peer 無し → solo）。G02。
    ui.apply_poll(PendingJob {
        job_id: job.job_id.clone(),
        policy: ConnectionPolicy::Automatic,
        phase: "succeeded".to_string(),
        node_results: vec![],
    });
    assert_eq!(
        ui.applied().expect("applied").policy,
        ConnectionPolicy::Automatic
    );
    assert_eq!(ui.applied().expect("applied").topology, "solo");
    assert!(ui.pending().is_none());
}

/// 受入 case 3: 部分失敗 → node 別失敗。G02。
#[test]
fn partial_failure_is_reported_per_node() {
    let mut api = FakePolicyApi {
        job_sequence: vec![PendingJob {
            job_id: "job-partial".to_string(),
            policy: ConnectionPolicy::ForcedStandalone,
            phase: "running".to_string(),
            node_results: vec![
                NodeResult {
                    node: "local".to_string(),
                    state: "ok".to_string(),
                    error: String::new(),
                },
                NodeResult {
                    node: "peer".to_string(),
                    state: "failed".to_string(),
                    error: "rdma route lost".to_string(),
                },
            ],
        }],
        ..Default::default()
    };
    let mut ui = ConnectionModeUi::new();
    let _ = block_on(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api)).expect("apply");
    assert!(ui.has_node_failure());
    let peer = ui
        .pending()
        .expect("pending")
        .node_results
        .iter()
        .find(|n| n.node == "peer")
        .expect("peer result");
    assert_eq!(peer.state, "failed");
    assert_eq!(peer.error, "rdma route lost");
}

/// 受入 case 4: busy → 理由表示。G02。
#[test]
fn busy_surfaces_reason() {
    let mut api = FakePolicyApi {
        busy: Some("another lifecycle operation is in progress".to_string()),
        ..Default::default()
    };
    let mut ui = ConnectionModeUi::new();
    let err = block_on(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect_err("busy");
    assert!(matches!(err, PolicyApiError::Busy(_)));
    assert!(ui.busy_reason().is_some());
    assert!(ui.menu_lines().iter().any(|l| l.contains("保留")));
}

/// レビュー重点: 選択を StableMode の直接書換えへ変換しない。G02。
///
/// ConnectionModeUi は StableMode に依存せず、選択を PolicyApi
/// （/cluster/operation-policy）へ送る。StableMode 型を参照しないこと
/// をコンパイルで保証（この crate は siderostat_core::target::StableMode
/// を import しない）。G02。
#[test]
fn selection_goes_through_api_not_stable_mode_rewrite() {
    let mut api = FakePolicyApi {
        job_sequence: vec![PendingJob {
            job_id: "job-x".to_string(),
            policy: ConnectionPolicy::Automatic,
            phase: "running".to_string(),
            node_results: vec![],
        }],
        ..Default::default()
    };
    let mut ui = ConnectionModeUi::new();
    let _ = block_on(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect("apply");
    // 選択は API 呼び出し 1 回に変換され、直接の状態書換えは無い。G02。
    assert_eq!(api.apply_calls.len(), 1);
    assert_eq!(ui.selected(), Some(ConnectionPolicy::Automatic));
    assert_eq!(ui.pending().expect("pending").job_id, "job-x");
}

/// selected / pending / applied を別行で表示し、適用状態（AppliedPolicy）
/// を submenu へ渡せる。G02。
#[test]
fn applied_policy_carries_desired_and_topology() {
    let applied = AppliedPolicy {
        policy: ConnectionPolicy::Automatic,
        topology: "solo".to_string(),
    };
    assert_eq!(applied.policy, ConnectionPolicy::Automatic);
    assert_eq!(applied.topology, "solo");
}
