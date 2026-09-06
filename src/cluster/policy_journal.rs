//! v0.4.0 操作方針 journal（P01 / C03）。
//!
//! PolicyJournal は policy の intent と applied 結果を、副作用の前に fsync + atomic rename
//! で永続化する。intent は effect 前に保存し、結果は完了後に保存する。停止途中の
//! restart で Automatic が復活する穴を閉じるために、pending_operation を journal に残し、
//! 起動時に安全ラッチ（ForcedStandalone 抑止）として復元する。
//!
//! 永続化の実体は StateStore（既存の fsync + atomic rename を再利用）に委譲する。
//! 第二の永続化実装を作らない。policy の cluster-wide 適用・排他は P02〜P04 が所有する。

use super::policy::OperationPolicy;
use super::state_store::{
    PersistentClusterState, PersistentOperationPhase, PersistentPendingOperation, StateStore,
    StateStoreError,
};

/// 進行中 operation の kind 文字列（C03 の pending_operation.kind）。安定した識別子。
pub const OPERATION_KIND_FORCE_STANDALONE: &str = "force-standalone";
pub const OPERATION_KIND_AUTOMATIC: &str = "automatic";

/// policy journal の読み出しビュー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyJournalView {
    pub operator_policy: OperationPolicy,
    pub applied_policy: OperationPolicy,
    pub policy_epoch: u64,
    pub pending_operation: Option<PersistentPendingOperation>,
}

/// policy intent を副作用の前に永続化する journal。StateStore を包む。
pub struct PolicyJournal<'a> {
    store: &'a StateStore,
}

impl<'a> PolicyJournal<'a> {
    pub fn new(store: &'a StateStore) -> Self {
        Self { store }
    }

    /// 現在の journal を読み出す。state が無ければ既定（Automatic / epoch 0）。
    pub fn load(&self) -> Result<PolicyJournalView, StateStoreError> {
        let state = self.store.load()?;
        Ok(PolicyJournalView {
            operator_policy: state
                .as_ref()
                .map(|s| s.operator_policy)
                .unwrap_or_default(),
            applied_policy: state.as_ref().map(|s| s.applied_policy).unwrap_or_default(),
            policy_epoch: state.as_ref().map(|s| s.policy_epoch).unwrap_or(0),
            pending_operation: state.and_then(|s| s.pending_operation),
        })
    }

    /// Force intent を effect の前に永続化する。operator_policy を ForcedStandalone にし、
    /// pending_operation を IntentSaved として journal に残す。この保存は fsync + atomic
    /// rename で行われる（StateStore::save）。保存に失敗した場合は副作用を一切行っては
    /// ならない（呼び出し側が child 操作をしないこと）。
    pub fn persist_force_intent(
        &self,
        epoch: u64,
        operation_id: uuid::Uuid,
    ) -> Result<PersistentClusterState, StateStoreError> {
        let mut state = self.store.load()?.unwrap_or_default_for_save();
        // 世代は進めず、policy epoch だけを設定する。desired_mode は intent に流用しない。
        state.operator_policy = OperationPolicy::ForcedStandalone;
        state.policy_epoch = epoch;
        state.pending_operation = Some(PersistentPendingOperation {
            id: operation_id,
            kind: OPERATION_KIND_FORCE_STANDALONE.to_string(),
            phase: PersistentOperationPhase::IntentSaved,
            desired: OperationPolicy::ForcedStandalone,
            peer_ack: false,
            last_failure: None,
        });
        self.store.save(&state)?;
        Ok(state)
    }

    /// Force 適用結果を完了後に永続化する。applied_policy を更新し、pending_operation を
    /// Applied として記録する（cluster-wide 完了後のみ。片側 commit 不明では呼ばない）。
    pub fn record_force_applied(
        &self,
        epoch: u64,
    ) -> Result<PersistentClusterState, StateStoreError> {
        let mut state = self.store.load()?.unwrap_or_default_for_save();
        state.applied_policy = OperationPolicy::ForcedStandalone;
        state.policy_epoch = epoch;
        if let Some(mut pending) = state.pending_operation.take() {
            pending.phase = PersistentOperationPhase::Applied;
            pending.peer_ack = true;
            state.pending_operation = Some(pending);
        }
        self.store.save(&state)?;
        Ok(state)
    }

    /// Automatic 復帰 intent を永続化する。pending は Automatic で記録し、TP 再接続の
    /// 抑止を解除するのは適用完了後。P05 で cluster-wide 適用に接続する。
    pub fn persist_automatic_intent(
        &self,
        epoch: u64,
    ) -> Result<PersistentClusterState, StateStoreError> {
        let mut state = self.store.load()?.unwrap_or_default_for_save();
        state.operator_policy = OperationPolicy::Automatic;
        state.policy_epoch = epoch;
        state.pending_operation = Some(PersistentPendingOperation {
            id: uuid::Uuid::new_v4(),
            kind: OPERATION_KIND_AUTOMATIC.to_string(),
            phase: PersistentOperationPhase::IntentSaved,
            desired: OperationPolicy::Automatic,
            peer_ack: false,
            last_failure: None,
        });
        self.store.save(&state)?;
        Ok(state)
    }

    /// 起動時に journal から安全ラッチを復元する。ForcedStandalone intent が残っていれば
    /// operator_policy は ForcedStandalone のままで、TP 開始は抑止される（P04 の policy
    /// gate が参照する）。副作用は行わない。
    pub fn restored_safety_latch(
        &self,
    ) -> Result<Option<PersistentPendingOperation>, StateStoreError> {
        Ok(self.load()?.pending_operation)
    }
}

/// PersistentClusterState の既定（state ファイルが無い場合の journal 保存用）。
trait DefaultForSave {
    fn unwrap_or_default_for_save(self) -> PersistentClusterState;
}

impl DefaultForSave for Option<PersistentClusterState> {
    fn unwrap_or_default_for_save(self) -> PersistentClusterState {
        match self {
            Some(state) => state,
            None => PersistentClusterState {
                schema_version: super::state_store::PERSISTENT_STATE_SCHEMA_VERSION,
                generation: 0,
                control_session_generation: 0,
                desired_mode: super::state_store::PersistentMode::SoloStandalone,
                last_stable_mode: super::state_store::PersistentMode::SoloStandalone,
                cluster_state: "booting".into(),
                proxy_target: super::state_store::PersistentProxyTarget::Unavailable,
                active_profile: None,
                child: None,
                last_failure: None,
                operator_policy: OperationPolicy::Automatic,
                applied_policy: OperationPolicy::Automatic,
                policy_epoch: 0,
                pending_operation: None,
            },
        }
    }
}
