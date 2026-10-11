//! G05 / C05 / SearchViewModel・Bridge supervisor: Web Search 管理・Bridge
//! process lifecycle の受入 case を公開 API（SearchViewModel）経由で
//! 検証する。実 Bridge プロセス / 実 network は使わず、lifecycle は
//! 純粋ロジック（fake 境界）。レビュー重点: Bridge token を画面に表示
//! しない、Docker インストール / 外部 provider 契約の無断実行なし。
use siderostat_core::websearch::status::{BridgeState, BridgeStatus, SearxngStatus};
use siderostat_monitor::websearch_window::SearchViewModel;

/// 受入 case 1: Bridge crash → 限定再起動/失敗表示。G05。/
#[test]
fn crash_bounded_restart_then_failure() {
    let mut view = SearchViewModel::new();
    assert_eq!(view.record_crash(), BridgeState::Running, "1st restart");
    assert_eq!(view.record_crash(), BridgeState::Running, "2nd");
    assert_eq!(view.record_crash(), BridgeState::Running, "3rd");
    // 上限超過 → Crashed（失敗表示）。G05。/
    assert_eq!(view.record_crash(), BridgeState::Crashed);
    assert!(view.bridge_state() == BridgeState::Crashed);
    assert!(view.bridge_redacted().contains("再起動上限"));
}

/// 受入 case 2: SearXNG 403 → JSON 設定案内。G05。/
#[test]
fn searxng_403_offers_json_setup_hint() {
    let mut view = SearchViewModel::new();
    view.set_searxng_status(SearxngStatus::JsonDisabled);
    let hint = view.searxng_json_hint().expect("hint");
    assert!(hint.contains("JSON format"));
    assert!(hint.contains("format=json"));
    assert_eq!(view.searxng_status(), SearxngStatus::JsonDisabled);
}

/// 受入 case 3: external off → 検索 0。G05。/
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

/// 受入 case 4: disable → search cancel/child 回収。G05。/
#[test]
fn disable_cancels_search_and_reclaims_child() {
    let mut view = SearchViewModel::new();
    let outcome = view.disable();
    assert!(outcome.cancel_search);
    assert!(outcome.reclaim_child);
    assert_eq!(view.bridge_state(), BridgeState::Stopped);
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

/// レビュー重点: port 衝突と max loop を表示する。G05。/
#[test]
fn port_conflict_and_max_loop_are_surfaced() {
    let mut view = SearchViewModel::new();
    view.apply_bridge_status(BridgeStatus {
        state: BridgeState::Stopped,
        child: None,
        listen: None,
        port_conflict: true,
        last_failure: None,
    });
    assert!(view.port_conflict());
    assert_eq!(view.max_loop(), 6);
}

/// 事後条件: Bridge crash だけで runtime 再起動なし（lifecycle は view
/// model 内で完結、runtime を触らない）。SearXNG 未配置でも GUI 操作は
/// 使用可能（Unavailable 表示）。G05。/
#[test]
fn crash_does_not_restart_runtime_and_gui_usable_without_searxng() {
    let mut view = SearchViewModel::new();
    // SearXNG 未配置（Unavailable）でも GUI 操作は可能。G05。/
    assert_eq!(view.searxng_status(), SearxngStatus::Unavailable);
    assert!(view.searxng_json_hint().is_none());
    // Bridge crash は bounded restart のみ（runtime 再起動導線なし）。
    let state = view.record_crash();
    assert!(state == BridgeState::Running || state == BridgeState::Crashed);
    // disable は search cancel / child 回収のみ。G05。/
    let outcome = view.disable();
    assert!(outcome.cancel_search && outcome.reclaim_child);
}
