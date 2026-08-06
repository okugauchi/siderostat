use ds4_smart_proxy::cluster::{
    ClusterFailure, FailureAction, PromotionFailureTracker, PromotionRetryDecision, failure_action,
};
use std::time::Duration;

#[test]
fn failure_table_and_finite_promotion_retry_match_policy() {
    let rows = [
        (ClusterFailure::PeerAbsent, FailureAction::SoloStandalone),
        (
            ClusterFailure::BridgeUnavailable,
            FailureAction::SoloStandalone,
        ),
        (
            ClusterFailure::BridgeAddressInvalid,
            FailureAction::SoloStandalone,
        ),
        (
            ClusterFailure::BonjourUnavailable {
                static_fallback: true,
            },
            FailureAction::RetryStaticDiscovery,
        ),
        (
            ClusterFailure::BonjourUnavailable {
                static_fallback: false,
            },
            FailureAction::SoloStandalone,
        ),
        (
            ClusterFailure::UnauthenticatedDiscovery,
            FailureAction::MaintainCurrent,
        ),
        (
            ClusterFailure::InvalidControlHmac,
            FailureAction::RejectRequest,
        ),
        (
            ClusterFailure::InvalidPeerProxyToken,
            FailureAction::RejectRequest,
        ),
        (
            ClusterFailure::DeploymentMismatch,
            FailureAction::PairedStandalone,
        ),
        (
            ClusterFailure::ManifestStale,
            FailureAction::PairedStandalone,
        ),
        (
            ClusterFailure::HelloTimeout,
            FailureAction::PromotionBackoff,
        ),
        (
            ClusterFailure::UnknownDs4Schema,
            FailureAction::PromotionBackoff,
        ),
        (
            ClusterFailure::CoordinatorStartupTimeout,
            FailureAction::PromotionBackoff,
        ),
        (
            ClusterFailure::RouteIncomplete,
            FailureAction::PairedStandalone,
        ),
        (ClusterFailure::PeerLeaseLost, FailureAction::SoloStandalone),
        (
            ClusterFailure::ChildIdentityUnknown,
            FailureAction::ManualIntervention,
        ),
        (
            ClusterFailure::StandaloneStartFailed,
            FailureAction::Unavailable,
        ),
        (
            ClusterFailure::DrainTimeout,
            FailureAction::ManualIntervention,
        ),
        (
            ClusterFailure::StateCorrupt {
                standalone_safe: true,
            },
            FailureAction::SoloStandalone,
        ),
        (
            ClusterFailure::StateCorrupt {
                standalone_safe: false,
            },
            FailureAction::ManualIntervention,
        ),
    ];
    for (failure, expected) in rows {
        assert_eq!(failure_action(failure), expected, "{failure:?}");
    }

    let mut tracker = PromotionFailureTracker::new(Duration::from_millis(300), 3).unwrap();
    assert_eq!(
        tracker
            .record(ClusterFailure::CoordinatorStartupTimeout, 1_000)
            .unwrap(),
        PromotionRetryDecision::Backoff {
            retry_at_millis: 1_300
        }
    );
    assert!(!tracker.can_retry(1_299));
    assert!(tracker.can_retry(1_300));
    assert_eq!(
        tracker
            .record(ClusterFailure::CoordinatorStartupTimeout, 1_300)
            .unwrap(),
        PromotionRetryDecision::Backoff {
            retry_at_millis: 1_600
        }
    );
    assert_eq!(
        tracker
            .record(ClusterFailure::CoordinatorStartupTimeout, 1_600)
            .unwrap(),
        PromotionRetryDecision::ManualIntervention
    );
    assert!(!tracker.can_retry(u64::MAX));

    tracker.operator_reconcile();
    assert!(tracker.can_retry(0));
    assert_eq!(tracker.status().consecutive, 0);
    tracker.record(ClusterFailure::HelloTimeout, 2_000).unwrap();
    tracker
        .record(ClusterFailure::UnknownDs4Schema, 2_300)
        .unwrap();
    assert_eq!(tracker.status().consecutive, 1);
}
