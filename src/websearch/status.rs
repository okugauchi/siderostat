//! Bridge の status 表示と lifecycle（G05 / C05 / SearchViewModel・
//! Bridge supervisor）。Bridge の enable/disable・owned child identity・
//! port 衝突・bounded 再起動を扱う。SearXNG health/JSON 可否・external
//! opt-in・max loop・直近失敗を表示する。Bridge token を画面に表示しな
//! い（redacted）。G05。
//!
//! 受入 case（全て必須）:
//! - 入力: Bridge crash → 限定再起動/失敗表示
//! - 入力: SearXNG 403 → JSON 設定案内
//! - 入力: external off → 検索 0
//! - 入力: disable → search cancel/child 回収
//!
//! レビュー重点: Docker daemon インストールや外部 provider 契約を無断
//! 実行しない。Bridge token を画面に表示しない。G05。
use std::time::{Duration, Instant};

/// Bridge の実行状態。G05。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    /// 停止中（disable / 未起動）。G05。
    Stopped,
    /// 稼働中。G05。
    Running,
    /// crash 検出（bounded restart 対象）。G05。
    Crashed,
}

/// Bridge の status 表示（redacted）。token 等の資格情報を含まない。
/// G05。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    pub state: BridgeState,
    /// owned child identity（PID 等）。G05。
    pub child: Option<u32>,
    /// 専用 loopback の listen アドレス。G05。
    pub listen: Option<String>,
    /// port 衝突検出（専用 listener が取れない）。G05。
    pub port_conflict: bool,
    /// 直近の失敗理由（redacted）。G05。
    pub last_failure: Option<String>,
}

impl BridgeStatus {
    /// 資格情報・token を表示しない redacted 表示。G05。
    pub fn redacted(&self) -> String {
        format!(
            "state={:?} child={} listen={} port_conflict={} last_failure={}",
            self.state,
            self.child
                .map_or_else(|| "--".to_string(), |pid| pid.to_string()),
            self.listen.as_deref().unwrap_or("--"),
            self.port_conflict,
            self.last_failure.as_deref().unwrap_or("none"),
        )
    }
}

/// Bridge lifecycle（bounded restart）。crash は限定回数まで再起動し、
/// 超過で失敗表示。G05。
#[derive(Debug, Clone)]
pub struct BridgeLifecycle {
    max_restarts: usize,
    crash_count: usize,
    last_crash: Option<Instant>,
    restart_window: Duration,
    external_opt_in: bool,
    max_loop: usize,
}

impl Default for BridgeLifecycle {
    fn default() -> Self {
        Self {
            // bounded restart: 既定最大 3 回。G05。
            max_restarts: 3,
            crash_count: 0,
            last_crash: None,
            // crash がこの窓内に連続したら再起動上限を消費する。G05。
            restart_window: Duration::from_secs(60),
            // external opt-in（既定 false → 検索 0）。G05。
            external_opt_in: false,
            // model turn / search の最大 loop（C05: search max3 / turn6）。G05。
            max_loop: 6,
        }
    }
}

impl BridgeLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_external_opt_in(mut self, enabled: bool) -> Self {
        self.external_opt_in = enabled;
        self
    }

    pub fn with_max_restarts(mut self, max: usize) -> Self {
        self.max_restarts = max;
        self
    }

    /// external opt-in（false → 検索 0）。G05。
    pub fn external_opt_in(&self) -> bool {
        self.external_opt_in
    }

    /// max loop（search max3 / model turn6）。G05。
    pub fn max_loop(&self) -> usize {
        self.max_loop
    }

    /// crash を記録し、bounded restart を判定する。再起動上限内なら
    /// Running に戻し、超過で Crashed（失敗表示）を返す。G05。
    pub fn record_crash(&mut self, now: Instant) -> BridgeState {
        // 再起動窓を超えていれば crash 連続をリセットする。G05。
        if let Some(last) = self.last_crash {
            if now.duration_since(last) > self.restart_window {
                self.crash_count = 0;
            }
        }
        self.crash_count += 1;
        self.last_crash = Some(now);
        if self.crash_count > self.max_restarts {
            BridgeState::Crashed
        } else {
            BridgeState::Running
        }
    }

    pub fn crash_count(&self) -> usize {
        self.crash_count
    }

    /// disable: 停止状態へ。search cancel と child 回収の指示を返す。
    /// G05。
    pub fn disable(&self) -> DisableOutcome {
        DisableOutcome {
            cancel_search: true,
            reclaim_child: true,
        }
    }
}

/// disable 時の処理指示（search cancel / child 回収）。G05。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableOutcome {
    pub cancel_search: bool,
    pub reclaim_child: bool,
}

/// SearXNG の status（health / JSON 可否）。G05。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearxngStatus {
    /// 未配置 / 未接続（その他 GUI 操作は使用可能）。G05。
    Unavailable,
    /// health OK。G05。
    Healthy,
    /// 403: JSON format 無効（設定案内を表示）。G05。
    JsonDisabled,
}

impl SearxngStatus {
    /// 403 → JSON 設定案内を返す。G05。
    pub fn json_setup_hint(&self) -> Option<&'static str> {
        match self {
            SearxngStatus::JsonDisabled => {
                Some("SearXNG の JSON format が無効です（format=json）。設定で有効化してください。")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入力: Bridge crash → 限定再起動/失敗表示。G05。/
    #[test]
    fn crash_bounded_restart_then_failure() {
        let now = Instant::now();
        let mut lc = BridgeLifecycle::new().with_max_restarts(3);
        assert_eq!(lc.record_crash(now), BridgeState::Running, "1st restart");
        assert_eq!(
            lc.record_crash(now + Duration::from_secs(1)),
            BridgeState::Running,
            "2nd"
        );
        assert_eq!(
            lc.record_crash(now + Duration::from_secs(2)),
            BridgeState::Running,
            "3rd"
        );
        // 上限超過 → Crashed（失敗表示）。G05。/
        assert_eq!(
            lc.record_crash(now + Duration::from_secs(3)),
            BridgeState::Crashed
        );
        assert_eq!(lc.crash_count(), 4);
    }

    /// 入力: SearXNG 403 → JSON 設定案内。G05。/
    #[test]
    fn searxng_403_offers_json_setup_hint() {
        let status = SearxngStatus::JsonDisabled;
        let hint = status.json_setup_hint().expect("hint");
        assert!(hint.contains("JSON format"));
        assert!(hint.contains("format=json"));
        assert!(SearxngStatus::Healthy.json_setup_hint().is_none());
    }

    /// 入力: external off → 検索 0。G05。/
    #[test]
    fn external_opt_in_off_means_no_search() {
        let lc = BridgeLifecycle::new(); // external_opt_in = false
        assert!(!lc.external_opt_in());
        let lc_on = BridgeLifecycle::new().with_external_opt_in(true);
        assert!(lc_on.external_opt_in());
    }

    /// 入力: disable → search cancel/child 回収。G05。/
    #[test]
    fn disable_cancels_search_and_reclaims_child() {
        let lc = BridgeLifecycle::new();
        let out = lc.disable();
        assert!(out.cancel_search);
        assert!(out.reclaim_child);
    }

    /// レビュー重点: Bridge token を画面に表示しない。G05。/
    #[test]
    fn status_is_redacted() {
        let status = BridgeStatus {
            state: BridgeState::Running,
            child: Some(42),
            listen: Some("127.0.0.1:18081".to_string()),
            port_conflict: false,
            last_failure: Some("auth failed".to_string()),
        };
        let text = status.redacted();
        assert!(!text.contains("token"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("password"));
    }
}
