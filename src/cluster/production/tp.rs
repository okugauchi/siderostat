//! v0.4.0 TP 本番配線の開始 gate（T11 / C02・C03）。
//!
//! typed TP が本番経路へ到達する際、全ての開始点で保護ラッチ（operator_policy）と
//! peer の protocol 交渉を確認する。旧（v1）peer の TP/policy 操作は negotiation で
//! 拒否し、旧 LP 経路のみを残す。ForcedStandalone では TP 開始を抑止し、local
//! Standalone を維持する（C03 の operator_policy 安全ラッチ）。
//!
//! 本 module は純粋判定（副作用なし）で、本番 reducer と同一の policy / protocol 入力
//! から開始可否を返す。実 child 起動・OS 接触は行わない。判定結果は
//! `ProductionClusterRuntime::tp_start_verdict` 経由で本番開始点が参照する。

use super::super::policy::OperationPolicy;

/// TP 本番配線で交渉する protocol version。旧 peer（protocol_version != 1）は
/// TP/policy 操作を拒否する（旧 LP 経路のみ利用可能）。
pub const TP_PRODUCTION_PROTOCOL_VERSION: u16 = 1;

/// TP 開始 gate の判定結果（C02/C03）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpStartVerdict {
    /// 開始してよい。Automatic policy + 交渉一致 peer。
    Allowed,
    /// ForcedStandalone: TP 開始を抑止し local Standalone を維持する。TP retry 0。
    PolicyForcedStandalone,
    /// 旧（v1 不一致）peer: TP 操作を negotiation で拒否。旧 LP 経路のみ。
    UnsupportedPeer,
}

impl TpStartVerdict {
    /// TP 開始を許可するか（`Allowed` のみ true）。。。
    pub fn allows_tp(self) -> bool {
        matches!(self, TpStartVerdict::Allowed)
    }

    /// 安定した有限ラベル（diagnostics / journal 用）。。。
    pub fn name(self) -> &'static str {
        match self {
            TpStartVerdict::Allowed => "tp-start-allowed",
            TpStartVerdict::PolicyForcedStandalone => "tp-start-policy-forced-standalone",
            TpStartVerdict::UnsupportedPeer => "tp-start-unsupported-peer",
        }
    }
}

/// TP 本番配線の開始 gate 判定。全開始点（worker 先行 / coordinator 起動 /
/// retry 再開）がこの判定を確認する。副作用なしの純粋関数。
///
/// - `operator_policy`（C03 の保護ラッチ。P01 journal から復元）: ForcedStandalone
///   なら TP 開始を抑止（TP retry 0）。
/// - `peer_protocol_version`: 交渉不一致（v1 以外）の旧 peer は TP/policy 操作を拒否。
pub fn check_tp_start(
    operator_policy: OperationPolicy,
    peer_protocol_version: Option<u16>,
) -> TpStartVerdict {
    if operator_policy == OperationPolicy::ForcedStandalone {
        return TpStartVerdict::PolicyForcedStandalone;
    }
    if peer_protocol_version.is_some_and(|v| v != TP_PRODUCTION_PROTOCOL_VERSION) {
        return TpStartVerdict::UnsupportedPeer;
    }
    TpStartVerdict::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_policy_and_matching_peer_allows_tp() {
        assert_eq!(
            check_tp_start(
                OperationPolicy::Automatic,
                Some(TP_PRODUCTION_PROTOCOL_VERSION),
            ),
            TpStartVerdict::Allowed
        );
        assert!(check_tp_start(OperationPolicy::Automatic, Some(1)).allows_tp());
    }

    #[test]
    fn forced_standalone_suppresses_tp_start() {
        assert_eq!(
            check_tp_start(OperationPolicy::ForcedStandalone, Some(1)),
            TpStartVerdict::PolicyForcedStandalone
        );
        assert!(!check_tp_start(OperationPolicy::ForcedStandalone, Some(1)).allows_tp());
        // ForcedStandalone は peer 状態より優先して抑止する（安全ラッチ）。。。
        assert_eq!(
            check_tp_start(OperationPolicy::ForcedStandalone, Some(2)),
            TpStartVerdict::PolicyForcedStandalone
        );
    }

    #[test]
    fn mismatched_protocol_peer_is_rejected() {
        assert_eq!(
            check_tp_start(OperationPolicy::Automatic, Some(2)),
            TpStartVerdict::UnsupportedPeer
        );
        assert_eq!(
            check_tp_start(OperationPolicy::Automatic, Some(0)),
            TpStartVerdict::UnsupportedPeer
        );
        assert!(!check_tp_start(OperationPolicy::Automatic, Some(2)).allows_tp());
    }

    #[test]
    fn unknown_peer_protocol_is_not_rejected() {
        // peer 未確認（None）は交渉不一致ではない。Automatic なら開始してよい。
        assert_eq!(
            check_tp_start(OperationPolicy::Automatic, None),
            TpStartVerdict::Allowed
        );
    }

    #[test]
    fn labels_are_stable_finite() {
        assert_eq!(TpStartVerdict::Allowed.name(), "tp-start-allowed");
        assert_eq!(
            TpStartVerdict::PolicyForcedStandalone.name(),
            "tp-start-policy-forced-standalone"
        );
        assert_eq!(
            TpStartVerdict::UnsupportedPeer.name(),
            "tp-start-unsupported-peer"
        );
    }
}
