use siderostat::cluster::{
    ClusterFailure, FailureAction, PromotionFailureTracker, PromotionRetryDecision,
    PromotionTrackerError, failure_action,
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
        // spec.md §31: an unknown HELLO/log schema refuses promotion but stays Paired
        // Standalone. It is not a backoff target.
        (
            ClusterFailure::UnknownDs4Schema,
            FailureAction::PairedStandalone,
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

    // All PromotionBackoff targets are recorded in the same tracker.
    assert_eq!(
        tracker.record(ClusterFailure::HelloTimeout, 2_000).unwrap(),
        PromotionRetryDecision::Backoff {
            retry_at_millis: 2_300
        }
    );
    assert_eq!(
        tracker
            .record(ClusterFailure::CoordinatorStartupTimeout, 2_300)
            .unwrap(),
        PromotionRetryDecision::Backoff {
            retry_at_millis: 2_600
        }
    );
    // Consecutive counts the same failure kind; a different PromotionBackoff kind is also
    // accepted but restarts the consecutive window.
    assert_eq!(tracker.status().consecutive, 1);

    // spec.md §31: unknown schema is Paired Standalone, so the tracker must refuse it rather
    // than counting it as a promotion backoff.
    assert_eq!(
        tracker.record(ClusterFailure::UnknownDs4Schema, 2_600),
        Err(PromotionTrackerError::NotPromotionFailure)
    );
    assert_eq!(tracker.status().consecutive, 1);

    // A successful promotion (or a successful reconnect) resets the consecutive count, so a
    // later promotion failure starts fresh instead of accumulating across reconnects.
    tracker.note_success();
    assert_eq!(tracker.status().consecutive, 0);
    tracker.record(ClusterFailure::HelloTimeout, 3_000).unwrap();
    assert_eq!(tracker.status().consecutive, 1);

    // Reconnect (peer loss) failures are classified Solo Standalone, never recorded as a
    // promotion failure, and promotion failures never enter the reconnect path.
    assert_eq!(
        tracker.record(ClusterFailure::PeerAbsent, 3_000),
        Err(PromotionTrackerError::NotPromotionFailure)
    );
    assert_eq!(tracker.status().consecutive, 1);
}
