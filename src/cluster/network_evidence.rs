use super::{NetworkSnapshot, ThunderboltIpState};
use crate::target::LocalRole;
use std::{net::Ipv4Addr, sync::RwLock};

/// Shared, latest verified network snapshot for the production control plane (N-02).
///
/// The production handler derives `route_scoped` from this shared evidence instead of passing a
/// hard-coded `true`, so peer-present gating comes from actual production input
/// ([`NetworkObservation`] -> [`NetworkSnapshot`]) rather than a constant. The state is
/// fail-closed by default: until a fresh observation is applied, neither route scoping nor peer
/// presence holds, so `establish`/`renew` are rejected with `ControlError::RouteNotScoped`.
///
/// An observation `epoch` is carried on each snapshot. [`Self::update`] rejects any snapshot
/// strictly older than the latest already applied, so a stale rescan cannot overwrite a newer
/// one (plan N-02 action 3).
#[derive(Debug, Default)]
pub struct NetworkEvidence {
    inner: RwLock<NetworkEvidenceState>,
}

#[derive(Debug, Default)]
struct NetworkEvidenceState {
    snapshot: NetworkSnapshot,
    observed_epoch: u64,
}

impl NetworkEvidence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fresh snapshot. Returns `true` when applied, or `false` when the snapshot is
    /// stale (its `epoch` is strictly older than the latest applied epoch). Stale observations
    /// are rejected and do not change the current evidence.
    pub fn update(&self, snapshot: NetworkSnapshot) -> bool {
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if snapshot.epoch < state.observed_epoch {
            return false;
        }
        state.observed_epoch = snapshot.epoch;
        state.snapshot = snapshot;
        true
    }

    /// Whether the latest verified snapshot has a valid, `bridge0`-scoped peer candidate.
    /// `PeerCandidateFound` and `AuthenticatedPeer` both require `candidate_address ==
    /// expected peer` and `route_scoped_to_interface`, so this is the measured value the
    /// production control handler passes as `route_scoped` (plan N-02 action 2).
    pub fn route_scoped(&self) -> bool {
        let state = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(
            state.snapshot.state,
            ThunderboltIpState::PeerCandidateFound | ThunderboltIpState::AuthenticatedPeer
        )
    }

    /// Whether the latest verified snapshot considers the peer present (only
    /// `AuthenticatedPeer`). Bonjour discovery alone never sets this (plan N-01).
    pub fn peer_present(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
            .peer_present
    }

    /// Latest applied snapshot, including its observation epoch.
    pub fn snapshot(&self) -> NetworkSnapshot {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
    }

    /// Seed the evidence with a synthetic `AuthenticatedPeer` snapshot for dry-run mode.
    ///
    /// Dry-run must not observe the host's live interfaces or processes, but the control plane
    /// is fail-closed and rejects `establish`/`renew` with `RouteNotScoped` unless `route_scoped`
    /// holds. A synthetic bridge0-scoped, authenticated peer keeps the production control plane
    /// and state machine fully functional while never touching a real interface or process. The
    /// epoch is `u64::MAX` so no later (dry-run) update is treated as stale.
    pub fn apply_dry_run(
        &self,
        role: LocalRole,
        local_address: Ipv4Addr,
        expected_peer_address: Ipv4Addr,
    ) {
        let snapshot = NetworkSnapshot {
            epoch: u64::MAX,
            state: ThunderboltIpState::AuthenticatedPeer,
            role,
            local_address: Some(local_address),
            expected_peer_address: Some(expected_peer_address),
            peer_present: true,
        };
        self.update(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::LocalRole;
    use std::net::Ipv4Addr;

    fn snapshot(epoch: u64, state: ThunderboltIpState) -> NetworkSnapshot {
        NetworkSnapshot {
            epoch,
            state,
            role: LocalRole::Worker,
            local_address: Some(Ipv4Addr::new(10, 99, 0, 2)),
            expected_peer_address: Some(Ipv4Addr::new(10, 99, 0, 1)),
            peer_present: state == ThunderboltIpState::AuthenticatedPeer,
        }
    }

    #[test]
    fn defaults_to_fail_closed() {
        let evidence = NetworkEvidence::new();
        assert!(!evidence.route_scoped());
        assert!(!evidence.peer_present());
        assert_eq!(evidence.snapshot().epoch, 0);
    }

    #[test]
    fn apply_dry_run_seeds_an_authenticated_scoped_peer() {
        let evidence = NetworkEvidence::new();
        assert!(!evidence.route_scoped());
        assert!(!evidence.peer_present());

        evidence.apply_dry_run(
            LocalRole::Coordinator,
            Ipv4Addr::new(10, 99, 0, 1),
            Ipv4Addr::new(10, 99, 0, 2),
        );
        assert!(evidence.route_scoped());
        assert!(evidence.peer_present());
        let snapshot = evidence.snapshot();
        assert_eq!(snapshot.epoch, u64::MAX);
        assert_eq!(snapshot.state, ThunderboltIpState::AuthenticatedPeer);
        assert_eq!(snapshot.local_address, Some(Ipv4Addr::new(10, 99, 0, 1)));
        assert_eq!(
            snapshot.expected_peer_address,
            Some(Ipv4Addr::new(10, 99, 0, 2))
        );
    }

    #[test]
    fn route_scoped_only_when_candidate_is_valid_and_scoped() {
        let evidence = NetworkEvidence::new();
        assert!(!evidence.route_scoped());

        // ReadyNoPeer / unavailable states are applied but never route scoped.
        assert!(evidence.update(snapshot(1, ThunderboltIpState::ReadyNoPeer)));
        assert!(!evidence.route_scoped());
        assert!(evidence.update(snapshot(2, ThunderboltIpState::ServiceMissing)));
        assert!(!evidence.route_scoped());

        // A valid, bridge0-scoped candidate enables route scoping even before authentication.
        assert!(evidence.update(snapshot(3, ThunderboltIpState::PeerCandidateFound)));
        assert!(evidence.route_scoped());
        assert!(!evidence.peer_present());

        // AuthenticatedPeer also enables route scoping and peer presence.
        assert!(evidence.update(snapshot(4, ThunderboltIpState::AuthenticatedPeer)));
        assert!(evidence.route_scoped());
        assert!(evidence.peer_present());
    }

    #[test]
    fn rejects_stale_observation_epoch() {
        let evidence = NetworkEvidence::new();
        assert!(evidence.update(snapshot(5, ThunderboltIpState::AuthenticatedPeer)));
        assert!(evidence.route_scoped());

        // A stale observation (epoch 4 < 5) must not overwrite the current evidence.
        assert!(!evidence.update(snapshot(4, ThunderboltIpState::ReadyNoPeer)));
        assert!(evidence.route_scoped());
        assert!(evidence.peer_present());
        assert_eq!(evidence.snapshot().epoch, 5);
    }
}
