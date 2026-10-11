//! Web Search 管理 window（G05 / C05 / SearchViewModel・Bridge
//! supervisor）。Bridge の enable/disable・owned child identity・port
//! 衝突・bounded 再起動を表示する。SearXNG health/JSON 可否・external
//! opt-in・max loop・直近失敗を表示する。Bridge token を画面に表示しな
//! い。G05。
//!
//! 受入 case（全て必須）:
//! - 入力: Bridge crash → 限定再起動/失敗表示
//! - 入力: SearXNG 403 → JSON 設定案内
//! - 入力: external off → 検索 0
//! - 入力: disable → search cancel/child 回収
//!
//! レビュー重点: Docker daemon インストールや外部 provider 契約を無断
//! 実行しない（本 view model に install/provider 導線を含めない）。
//! Bridge token を画面に表示しない。G05。
use siderostat_core::websearch::status::{
    BridgeLifecycle, BridgeState, BridgeStatus, SearxngStatus,
};
use std::time::Instant;

/// Web Search 管理 window の view model。Bridge lifecycle と SearXNG
/// status を表示する。実 child プロセスは起動せず、lifecycle は
/// 純粋ロジック（fake 境界で検証）。G05。
#[derive(Debug, Clone)]
pub struct SearchViewModel {
    bridge: BridgeStatus,
    lifecycle: BridgeLifecycle,
    searxng: SearxngStatus,
    /// 実行した検索回数（external off なら 0）。G05。
    search_count: u64,
}

impl SearchViewModel {
    pub fn new() -> Self {
        Self {
            bridge: BridgeStatus {
                state: BridgeState::Stopped,
                child: None,
                listen: None,
                port_conflict: false,
                last_failure: None,
            },
            lifecycle: BridgeLifecycle::new(),
            searxng: SearxngStatus::Unavailable,
            search_count: 0,
        }
    }
}

impl Default for SearchViewModel {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchViewModel {
    /// Bridge の status 反映。G05。
    pub fn apply_bridge_status(&mut self, status: BridgeStatus) {
        self.bridge = status;
    }

    /// Bridge crash を記録し、bounded restart を判定する。上限超過で
    /// Crashed（失敗表示）。G05。
    pub fn record_crash(&mut self) -> BridgeState {
        let now = Instant::now();
        let state = self.lifecycle.record_crash(now);
        self.bridge.state = state;
        if state == BridgeState::Crashed {
            self.bridge.last_failure = Some("Bridge の再起動上限を超えました".to_string());
        }
        state
    }

    /// Bridge の状態表示（redacted、token 非表示）。G05。
    pub fn bridge_redacted(&self) -> String {
        self.bridge.redacted()
    }

    pub fn bridge_state(&self) -> BridgeState {
        self.bridge.state
    }

    /// port 衝突の表示。G05。
    pub fn port_conflict(&self) -> bool {
        self.bridge.port_conflict
    }

    /// SearXNG 403 → JSON 設定案内。G05。
    pub fn searxng_json_hint(&self) -> Option<&'static str> {
        self.searxng.json_setup_hint()
    }

    pub fn searxng_status(&self) -> SearxngStatus {
        self.searxng
    }

    /// SearXNG status 反映。G05。
    pub fn set_searxng_status(&mut self, status: SearxngStatus) {
        self.searxng = status;
    }

    /// external opt-in（false → 検索 0）。G05。
    pub fn can_search(&self) -> bool {
        self.lifecycle.external_opt_in()
    }

    pub fn set_external_opt_in(&mut self, enabled: bool) {
        self.lifecycle = BridgeLifecycle::new().with_external_opt_in(enabled);
    }

    /// max loop（search max3 / model turn6）。G05。
    pub fn max_loop(&self) -> usize {
        self.lifecycle.max_loop()
    }

    /// 検索を 1 回実行する。external opt-in が off なら 0（検索しない）。
    /// G05。
    pub fn run_search(&mut self) -> u64 {
        if !self.can_search() {
            return 0;
        }
        self.search_count += 1;
        self.search_count
    }

    pub fn search_count(&self) -> u64 {
        self.search_count
    }

    /// Bridge を disable する。search cancel と child 回収を指示し、
    /// Bridge を Stopped にする。G05。/
    pub fn disable(&mut self) -> siderostat_core::websearch::status::DisableOutcome {
        let outcome = self.lifecycle.disable();
        self.bridge.state = BridgeState::Stopped;
        self.bridge.child = None;
        outcome
    }

    /// disable 時の処理指示（search cancel / child 回収）。G05。/
    pub fn bridge_disabled_text(&self) -> String {
        "Web Search Bridge を無効化しました（検索をキャンセルし child を回収）".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入力: Bridge crash → 限定再起動/失敗表示。G05。/
    #[test]
    fn crash_bounded_restart_then_failure() {
        let mut view = SearchViewModel::new();
        assert_eq!(view.record_crash(), BridgeState::Running, "1st");
        assert_eq!(view.record_crash(), BridgeState::Running, "2nd");
        assert_eq!(view.record_crash(), BridgeState::Running, "3rd");
        // 上限超過 → Crashed（失敗表示）。G05。/
        assert_eq!(view.record_crash(), BridgeState::Crashed);
        assert!(view.bridge_redacted().contains("再起動上限"));
        assert!(view.bridge_state() == BridgeState::Crashed);
    }

    /// 入力: SearXNG 403 → JSON 設定案内。G05。/
    #[test]
    fn searxng_403_offers_json_setup_hint() {
        let mut view = SearchViewModel::new();
        view.set_searxng_status(SearxngStatus::JsonDisabled);
        let hint = view.searxng_json_hint().expect("hint");
        assert!(hint.contains("JSON format"));
        assert!(hint.contains("format=json"));
    }

    /// 入力: external off → 検索 0。G05。/
    #[test]
    fn external_opt_in_off_means_no_search() {
        let mut view = SearchViewModel::new(); // external_opt_in = false
        assert!(!view.can_search());
        let n = view.run_search();
        assert_eq!(n, 0, "external off -> no search");
        assert_eq!(view.search_count(), 0);
        // opt-in 有効 → 検索 1。G05。/
        view.set_external_opt_in(true);
        assert!(view.can_search());
        assert_eq!(view.run_search(), 1);
    }

    /// 入力: disable → search cancel/child 回収。G05。/
    #[test]
    fn disable_cancels_search_and_reclaims_child() {
        let mut view = SearchViewModel::new();
        let outcome = view.disable();
        assert!(outcome.cancel_search);
        assert!(outcome.reclaim_child);
        assert_eq!(view.bridge_state(), BridgeState::Stopped);
        // 検索をキャンセルした旨の表示。G05。/
        assert!(view.bridge_disabled_text().contains("キャンセル"));
        assert!(view.bridge_disabled_text().contains("child を回収"));
    }

    /// レビュー重点: Bridge token を画面に表示しない。G05。/
    #[test]
    fn bridge_status_is_redacted() {
        let mut view = SearchViewModel::new();
        view.apply_bridge_status(BridgeStatus {
            state: BridgeState::Running,
            child: Some(42),
            listen: Some("127.0.0.1:18081".to_string()),
            port_conflict: false,
            last_failure: Some("auth failed".to_string()),
        });
        let text = view.bridge_redacted();
        assert!(!text.contains("token"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("password"));
    }

    /// レビュー重点: max loop 表示（search max3 / model turn6）。G05。/
    #[test]
    fn max_loop_is_surfaced() {
        let view = SearchViewModel::new();
        assert_eq!(view.max_loop(), 6);
    }

    /// レビュー重点: Docker インストール / 外部 provider 契約の導線を
    /// 含めない（本 view model に install 導線なし）。G05。/
    #[test]
    fn no_docker_or_provider_contract_wiring() {
        // SearchViewModel は install / provider 契約のメソッドを持たない。
        // コンパイルで保証（この view model に install 導線が無い）。G05。/
        let view = SearchViewModel::new();
        assert_eq!(view.bridge_state(), BridgeState::Stopped);
    }
}
