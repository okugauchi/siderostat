//! Display state held by the monitor and updated from each metrics poll.

use crate::metrics::{MetricsSnapshot, MonitorStatus};

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    pub status: MonitorStatus,
    pub cluster_mode: Option<String>,
    pub cluster_state: Option<String>,
    pub generation: Option<u64>,
    pub target_ready: Option<bool>,
    pub node_id: Option<String>,
    pub prefill_active: bool,
    pub prefill_current: u64,
    pub prefill_total: u64,
    pub prefill_percent: f64,
    pub prefill_cached: u64,
    pub kv_hits_total: u64,
    pub kv_hit_tokens: u64,
    pub kv_load_ms: f64,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            status: MonitorStatus::Offline,
            cluster_mode: None,
            cluster_state: None,
            generation: None,
            target_ready: None,
            node_id: None,
            prefill_active: false,
            prefill_current: 0,
            prefill_total: 0,
            prefill_percent: 0.0,
            prefill_cached: 0,
            kv_hits_total: 0,
            kv_hit_tokens: 0,
            kv_load_ms: 0.0,
        }
    }
}

impl DisplayState {
    /// Apply a successful metrics poll.
    pub fn apply_metrics(&mut self, snapshot: &MetricsSnapshot) {
        self.status = MonitorStatus::Online;
        self.cluster_mode = snapshot.cluster_mode.clone();
        self.cluster_state = snapshot.cluster_state.clone();
        self.generation = snapshot.generation;
        self.target_ready = snapshot.target_ready;
        self.node_id = snapshot.node_id.clone();
        self.prefill_active = snapshot.prefill.active;
        self.prefill_current = snapshot.prefill.current;
        self.prefill_total = snapshot.prefill.total;
        self.prefill_percent = snapshot.prefill.percent;
        self.prefill_cached = snapshot.prefill.cached;
        self.kv_hits_total = snapshot.kv_cache.hits_total;
        self.kv_hit_tokens = snapshot.kv_cache.hit_tokens;
        self.kv_load_ms = snapshot.kv_cache.load_ms;
    }

    /// Mark the monitor as offline after a failed poll.
    pub fn mark_offline(&mut self) {
        self.status = MonitorStatus::Offline;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            cluster_mode: Some("distributed-mxfp4".into()),
            cluster_state: Some("distributed-ready".into()),
            generation: Some(7),
            target_ready: Some(true),
            node_id: Some("macstudio".into()),
            prefill: crate::metrics::PrefillState {
                active: true,
                current: 4096,
                total: 9005,
                percent: 45.5,
                cached: 0,
            },
            kv_cache: crate::metrics::KvCacheState {
                hits_total: 3,
                hit_tokens: 9005,
                load_ms: 12.3,
            },
        }
    }

    #[test]
    fn applies_metrics() {
        let mut state = DisplayState::default();
        state.apply_metrics(&snapshot());
        assert_eq!(state.status, MonitorStatus::Online);
        assert_eq!(state.cluster_mode.as_deref(), Some("distributed-mxfp4"));
        assert_eq!(state.generation, Some(7));
        assert!(state.prefill_active);
        assert_eq!(state.prefill_percent, 45.5);
    }

    #[test]
    fn offline_state_takes_precedence() {
        let mut state = DisplayState::default();
        state.apply_metrics(&snapshot());
        state.mark_offline();
        assert_eq!(state.status, MonitorStatus::Offline);
    }
}
