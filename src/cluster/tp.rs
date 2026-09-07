//! TP（Tensor Parallelism）セッションの純粋状態機械（C02）。
//!
//! クラスタ reducer（`state.rs`）が TP 専用イベントを受理したとき、どの準備要素
//! （worker Prepared / coordinator started / handshake+HTTP ready / warm-up 完了）が
//! 現在の TP セッションで揃っているかを追跡し、全て揃った場合のみ route 公開
//! （TensorParallelReady）を許可する。
//!
//! - `TpSessionState` は現在の TP セッションの準備状態を表す不変値。Copy で、
//!   reducer の `ClusterSnapshot` に格納される（第二の状態機械を作らない）。
//! - セッション照合は `ClusterEvent::tp_session`（`TpSessionId`）で行う。control
//!   generation（`expected_generation`）とは別型として扱う（C02・C03）。
//! - 旧セッションの成功は受理しない。TP と LP のイベントは相互流用しない。
//!
//! 本モジュールは純粋な状態判定のみ。実際の child 起動・停止・観測は T07〜T11 が
//! 所有し、ここでは fake/本番の両方で同じ reducer を駆動するための判定を提供する。

use crate::cluster::operation::TpSessionId;

/// TP セッションの準備要素を表すビット集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TpReadiness {
    /// worker の Prepared（child 開始と生存のみ観測）。
    pub worker_prepared: bool,
    /// coordinator の child 起動（handshake 前段）。
    pub coordinator_started: bool,
    /// A02 session 付き handshake + HTTP ready の観測。
    pub handshake_http_ready: bool,
    /// bounded warm-up の完了。
    pub warmup_done: bool,
}

impl TpReadiness {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全ての準備要素が揃っているか。揃うまで route は非公開。
    pub fn is_complete(&self) -> bool {
        self.worker_prepared
            && self.coordinator_started
            && self.handshake_http_ready
            && self.warmup_done
    }

    /// 要素を一つ進める。既に立っている要素は冪等に維持する。
    pub fn with(&self, event: TpReadinessEvent) -> Self {
        let mut next = *self;
        match event {
            TpReadinessEvent::WorkerPrepared => next.worker_prepared = true,
            TpReadinessEvent::CoordinatorStarted => next.coordinator_started = true,
            TpReadinessEvent::HandshakeHttpReady => next.handshake_http_ready = true,
            TpReadinessEvent::WarmupDone => next.warmup_done = true,
        }
        next
    }
}

/// TP セッションが受理できる準備イベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpReadinessEvent {
    WorkerPrepared,
    CoordinatorStarted,
    HandshakeHttpReady,
    WarmupDone,
}

/// 現在の TP セッションの準備状態。`ClusterSnapshot` に格納される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpSessionState {
    /// 現在の TP セッション ID。role swap / child 交換後に旧セッションを拒否する。
    pub session: TpSessionId,
    /// このセッションで揃った準備要素。
    pub readiness: TpReadiness,
}

impl TpSessionState {
    pub fn new(session: TpSessionId) -> Self {
        Self {
            session,
            readiness: TpReadiness::new(),
        }
    }

    /// 現在セッションで全要素が揃っているか（route 公開の許可）。
    pub fn route_ready(&self) -> bool {
        self.readiness.is_complete()
    }

    /// 指定セッションのイベントを適用する。
    ///
    /// - セッションが一致しない場合は `None`（旧/未知セッション → 無視）。
    /// - 一致する場合は準備要素を進めた新しい状態を返す。
    pub fn apply_event(&self, session: TpSessionId, event: TpReadinessEvent) -> Option<Self> {
        if self.session != session {
            return None;
        }
        Some(Self {
            session,
            readiness: self.readiness.with(event),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u64) -> TpSessionId {
        TpSessionId(n)
    }

    #[test]
    fn readiness_starts_incomplete() {
        assert!(!TpReadiness::new().is_complete());
        let state = TpSessionState::new(sid(1));
        assert!(!state.route_ready());
    }

    #[test]
    fn route_only_publishes_when_all_elements_collected() {
        // 一つずつ立てて、途中は non-ready、最後に ready。
        let mut state = TpSessionState::new(sid(1));
        for ev in [
            TpReadinessEvent::WorkerPrepared,
            TpReadinessEvent::CoordinatorStarted,
            TpReadinessEvent::HandshakeHttpReady,
        ] {
            state = state.apply_event(sid(1), ev).unwrap();
            assert!(!state.route_ready());
        }
        state = state
            .apply_event(sid(1), TpReadinessEvent::WarmupDone)
            .unwrap();
        assert!(state.route_ready());
    }

    #[test]
    fn old_session_event_is_ignored() {
        let state = TpSessionState::new(sid(2));
        // 旧セッションの warm-up 完了は受理しない。
        let result = state.apply_event(sid(1), TpReadinessEvent::WarmupDone);
        assert!(result.is_none());
        assert!(!state.route_ready());
    }

    #[test]
    fn same_session_events_accumulate_idempotently() {
        let state = TpSessionState::new(sid(3));
        let once = state
            .apply_event(sid(3), TpReadinessEvent::WorkerPrepared)
            .unwrap();
        let twice = once
            .apply_event(sid(3), TpReadinessEvent::WorkerPrepared)
            .unwrap();
        assert_eq!(once.readiness, twice.readiness);
    }
}
