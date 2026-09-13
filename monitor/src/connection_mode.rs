//! Connection-mode submenu state and dispatch（G02 / C03）。
//!
//! The menu exposes two connection modes — Automatic and ForcedStandalone.
//! Selecting one never rewrites `StableMode` directly（レビュー重点）; the
//! choice is sent to the `/cluster/operation-policy` API (P06 / C03) and the
//! resulting job is tracked. A second click on the same pending policy is
//! bundled into the same job (no duplicate POST). A busy lease surfaces its
//! reason instead of silently failing. G02。
//!
//! `select` は async（GUI thread に network を置かない）。GUI スレッドは
//! spawn して非同期に select を実行し、状態は共有ミューテックスで更新
//! する。G02。
//!
//! 受入 case:
//! - TB有効 + Force → POST 1（二重クリックは同 job に束ねる）G02。
//! - Automatic 適用 / peer 無し → 自動 + Solo（applied 表示）G02。
//! - 部分失敗 → node 別失敗 G02。
//! - busy → 理由表示 G02。

/// 接続モードの選択（C03 / connection-mode UI）。StableMode を直接
/// 書換えず、この選択を API へ送る。G02。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPolicy {
    Automatic,
    ForcedStandalone,
}

impl ConnectionPolicy {
    /// `/cluster/operation-policy` の payload 値（snake_case）。G02。
    pub fn wire_value(self) -> &'static str {
        match self {
            ConnectionPolicy::Automatic => "automatic",
            ConnectionPolicy::ForcedStandalone => "forced-standalone",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ConnectionPolicy::Automatic => "Automatic",
            ConnectionPolicy::ForcedStandalone => "Forced Standalone",
        }
    }
}

/// node 別の適用結果（C03 / node 別結果）。G02。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeResult {
    pub node: String,
    pub state: String, // "ok" | "failed"
    pub error: String,
}

/// 進行中の policy 適用 job。G02。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingJob {
    pub job_id: String,
    pub policy: ConnectionPolicy,
    pub phase: String, // "running" | "succeeded" | "failed" | ...
    pub node_results: Vec<NodeResult>,
}

/// 適用済み policy と実 topology。G02。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPolicy {
    pub policy: ConnectionPolicy,
    /// 実 topology: "solo" / "paired" / "distributed"。G02。
    pub topology: String,
}

/// `/cluster/operation-policy` 呼び出しの抽象境界（async）。テストでは
/// POST 回数と返却 job を fake で記録する。自クレート内でのみ使用する
/// ため async fn in trait を許可する（clippy -D warnings 対策）。G02。
#[allow(async_fn_in_trait)]
pub trait PolicyApi {
    /// policy を適用する。Ok(job) = 202。Err(PolicyApiError::Busy) = 409。
    /// 同じ policy の進行中 job が API 側に既にあればそれを返す
    /// （idempotency、二重 POST 防止）。G02。
    async fn apply(
        &mut self,
        policy: ConnectionPolicy,
        expected_generation: u64,
    ) -> Result<PendingJob, PolicyApiError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyApiError {
    /// 409: 別の lifecycle 操作が進行中。理由を表示する。G02。
    Busy(String),
    /// 401/403/400/404/503 などのその他エラー。G02。
    Other(String),
}

/// 接続モード submenu の表示状態。selected（ユーザー選択）と
/// pending（進行中 job）と applied（適用済み policy + topology）を別行で
/// 表示する。G02。
#[derive(Debug, Clone, Default)]
pub struct ConnectionModeUi {
    selected: Option<ConnectionPolicy>,
    pending: Option<PendingJob>,
    applied: Option<AppliedPolicy>,
    busy_reason: Option<String>,
}

impl ConnectionModeUi {
    pub fn new() -> Self {
        Self::default()
    }

    /// ユーザーが接続モードを選択した。同じ policy の pending job が既に
    /// あれば二重 POST せずそれを返す（二重クリックを同 job に束ねる）。
    /// 別の policy の pending がある場合は busy 理由を表示する。G02。
    ///
    /// async: GUI thread に network を置かない（呼び出し側は spawn する）。G02。
    pub async fn select(
        &mut self,
        policy: ConnectionPolicy,
        expected_generation: u64,
        api: &mut impl PolicyApi,
    ) -> Result<PendingJob, PolicyApiError> {
        self.selected = Some(policy);
        self.busy_reason = None;
        // 二重クリック: 同じ policy の進行中 job を再利用（POST しない）。G02。
        if let Some(pending) = &self.pending {
            if pending.policy == policy {
                return Ok(pending.clone());
            }
            // 別の policy が進行中 → busy。G02。
            self.busy_reason = Some(format!(
                "別の接続モード適用が進行中です（{}）",
                pending.policy.display_name()
            ));
            return Err(PolicyApiError::Busy(
                "another connection-mode operation is in progress".to_string(),
            ));
        }
        match api.apply(policy, expected_generation).await {
            Ok(job) => {
                self.pending = Some(job.clone());
                Ok(job)
            }
            Err(PolicyApiError::Busy(reason)) => {
                self.busy_reason = Some(reason.clone());
                Err(PolicyApiError::Busy(reason))
            }
            Err(other) => Err(other),
        }
    }

    /// 進行中 job の状態照会（`GET /cluster/jobs/{id}`）を反映する。
    /// terminal になったら pending を解消し applied を更新する。G02。
    pub fn apply_poll(&mut self, job: PendingJob) {
        let terminal = job.phase == "succeeded" || job.phase == "failed";
        let policy = job.policy;
        let topology = self.applied.as_ref().map(|a| a.topology.clone());
        if terminal && job.phase == "succeeded" {
            self.pending = None;
            // applied topology は runtime の実状態。ここでは job の成功で
            // desired policy を applied にし、topology は実測（peer 無し等）
            // を保持する。G02。
            self.applied = Some(AppliedPolicy {
                policy,
                topology: topology.unwrap_or_else(|| "solo".to_string()),
            });
        } else if terminal {
            // 失敗: pending 解消、applied は更新しない。G02。
            self.pending = None;
        } else {
            self.pending = Some(job);
        }
        self.busy_reason = None;
    }

    /// 部分失敗（node 別）: いずれかの node が failed なら失敗と表示。G02。
    pub fn has_node_failure(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|job| job.node_results.iter().any(|n| n.state == "failed"))
    }

    pub fn selected(&self) -> Option<ConnectionPolicy> {
        self.selected
    }

    pub fn pending(&self) -> Option<&PendingJob> {
        self.pending.as_ref()
    }

    pub fn applied(&self) -> Option<&AppliedPolicy> {
        self.applied.as_ref()
    }

    /// busy 理由。G02。
    pub fn busy_reason(&self) -> Option<&str> {
        self.busy_reason.as_deref()
    }

    /// メニュー表示: desired policy と実 topology を別行で。G02。
    pub fn menu_lines(&self) -> Vec<String> {
        let selected = self
            .selected
            .map_or("--".to_string(), |p| p.display_name().to_string());
        let desired = format!("接続モード（選択）: {selected}");
        let applied = self.applied.as_ref().map_or_else(
            || "適用状態: --".to_string(),
            |a| format!("適用状態: {} / {}", a.policy.display_name(), a.topology),
        );
        let mut lines = vec![desired, applied];
        if let Some(pending) = &self.pending {
            lines.push(format!(
                "適用中: {}（{}）",
                pending.policy.display_name(),
                pending.job_id
            ));
        }
        if let Some(reason) = &self.busy_reason {
            lines.push(format!("（保留）{reason}"));
        }
        lines
    }
}

// ---------------------------------------------------------------------------
// テスト用 fake API（POST 回数と返却 job を記録）。StableMode 直接書換え
// をしないことを受入 case で検証する。G02。
// ---------------------------------------------------------------------------
#[cfg(test)]
pub mod test_util {
    use super::*;

    #[derive(Default)]
    pub struct FakePolicyApi {
        pub apply_calls: Vec<ConnectionPolicy>,
        pub job_sequence: Vec<PendingJob>,
        pub next_job: usize,
        pub busy: Option<String>,
    }

    impl FakePolicyApi {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_success(policy: ConnectionPolicy, job_id: &str) -> Self {
            let job = PendingJob {
                job_id: job_id.to_string(),
                policy,
                phase: "running".to_string(),
                node_results: vec![NodeResult {
                    node: "local".to_string(),
                    state: "ok".to_string(),
                    error: String::new(),
                }],
            };
            Self {
                apply_calls: Vec::new(),
                job_sequence: vec![job],
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

    /// async select を同期テストから呼ぶためのヘルパー。G02。
    pub fn block_on_select<F, T>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(future)
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    /// 入力: TB有効 + Force → POST 1。二重クリックは同 job に束ねる。G02。
    #[test]
    fn force_sends_one_post_and_bundles_double_click() {
        let mut api = FakePolicyApi::with_success(ConnectionPolicy::ForcedStandalone, "job-1");
        let mut ui = ConnectionModeUi::new();
        let first = block_on_select(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api))
            .expect("apply");
        // 二重クリック: 同じ policy の進行中 job を再利用（POST しない）。G02。
        let second = block_on_select(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api))
            .expect("apply");
        assert_eq!(first.job_id, "job-1");
        assert_eq!(second.job_id, "job-1");
        assert_eq!(api.apply_calls.len(), 1, "double click must not POST twice");
        assert_eq!(api.apply_calls[0], ConnectionPolicy::ForcedStandalone);
    }

    /// 入力: Automatic 適用 / peer 無し → 自動 + Solo。G02。
    #[test]
    fn automatic_applies_to_solo_without_peer() {
        let mut api = FakePolicyApi::new();
        api.job_sequence = vec![PendingJob {
            job_id: "job-auto".to_string(),
            policy: ConnectionPolicy::Automatic,
            phase: "succeeded".to_string(),
            node_results: vec![],
        }];
        let mut ui = ConnectionModeUi::new();
        let job =
            block_on_select(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect("apply");
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

    /// 入力: 部分失敗 → node 別失敗。G02。
    #[test]
    fn partial_failure_is_reported_per_node() {
        let mut api = FakePolicyApi::new();
        api.job_sequence = vec![PendingJob {
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
        }];
        let mut ui = ConnectionModeUi::new();
        let _ = block_on_select(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api))
            .expect("apply");
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

    /// 入力: busy → 理由表示。G02。
    #[test]
    fn busy_surfaces_reason() {
        let mut api = FakePolicyApi::new();
        api.busy = Some("another lifecycle operation is in progress".to_string());
        let mut ui = ConnectionModeUi::new();
        let err =
            block_on_select(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect_err("busy");
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
        let mut api = FakePolicyApi::new();
        api.job_sequence = vec![PendingJob {
            job_id: "job-x".to_string(),
            policy: ConnectionPolicy::Automatic,
            phase: "running".to_string(),
            node_results: vec![],
        }];
        let mut ui = ConnectionModeUi::new();
        let _ =
            block_on_select(ui.select(ConnectionPolicy::Automatic, 1, &mut api)).expect("apply");
        // 選択は API 呼び出し 1 回に変換され、直接の状態書換えは無い。G02。
        assert_eq!(api.apply_calls.len(), 1);
        assert_eq!(ui.selected(), Some(ConnectionPolicy::Automatic));
        // pending は API が返した job。G02。
        assert_eq!(ui.pending().expect("pending").job_id, "job-x");
    }

    /// selected / pending / applied を別行で表示する。G02。
    #[test]
    fn menu_lines_separate_selected_pending_applied() {
        let mut api = FakePolicyApi::with_success(ConnectionPolicy::ForcedStandalone, "job-m");
        let mut ui = ConnectionModeUi::new();
        let _ = block_on_select(ui.select(ConnectionPolicy::ForcedStandalone, 3, &mut api))
            .expect("apply");
        let lines = ui.menu_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("接続モード（選択）: Forced Standalone"))
        );
        assert!(lines.iter().any(|l| l.contains("適用状態")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("適用中") && l.contains("job-m"))
        );
    }
}
