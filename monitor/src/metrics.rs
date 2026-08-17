//! Minimal Prometheus text-format parser for the siderostat `/metrics`
//! endpoint.
//!
//! Only the families the monitor displays are parsed; unknown families are
//! ignored so the monitor remains forward-compatible with metric additions.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    Online,
    Offline,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrefillState {
    pub active: bool,
    pub current: u64,
    pub total: u64,
    pub percent: f64,
    pub cached: u64,
    pub chunk_tps: f64,
    pub avg_tps: f64,
    pub elapsed_secs: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct KvCacheState {
    pub hits_total: u64,
    pub hit_tokens: u64,
    pub load_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodeState {
    pub completion: u64,
    pub chunk_tps: f64,
    pub avg_tps: f64,
    pub elapsed_secs: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetricsSnapshot {
    pub cluster_mode: Option<String>,
    pub cluster_state: Option<String>,
    pub generation: Option<u64>,
    pub target_ready: Option<bool>,
    pub node_id: Option<String>,
    pub prefill: PrefillState,
    pub kv_cache: KvCacheState,
    pub decode: DecodeState,
}

/// Parse a Prometheus text exposition into a `MetricsSnapshot`. Unknown or
/// malformed samples are skipped; a missing family simply stays `None`.
pub fn parse_metrics(text: &str) -> MetricsSnapshot {
    let mut samples = HashMap::new();
    let mut metric_type = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut parts = rest.split_whitespace();
                if let (Some(name), Some(ty)) = (parts.next(), parts.next()) {
                    metric_type.insert(name.to_string(), ty.to_string());
                }
            }
            continue;
        }
        let Some((name, _, value)) = split_metric_line(line) else {
            continue;
        };
        let value = match value.parse::<f64>() {
            Ok(value) => value,
            Err(_) => continue,
        };
        samples.insert(name, value);
    }

    let cluster_mode = label_from_sample(text, "ds4_proxy_cluster_mode", "mode");
    let cluster_state = label_from_sample(text, "ds4_proxy_cluster_state", "state");
    let node_id = label_from_sample(text, "ds4_proxy_cluster_mode", "node_id");

    MetricsSnapshot {
        cluster_mode,
        cluster_state,
        generation: samples
            .get("ds4_proxy_cluster_generation")
            .map(|v| *v as u64),
        target_ready: samples.get("ds4_proxy_target_ready").map(|v| *v != 0.0),
        node_id,
        prefill: PrefillState {
            active: samples
                .get("ds4_proxy_ds4_prefill_active")
                .is_some_and(|v| *v != 0.0),
            current: samples
                .get("ds4_proxy_ds4_prefill_current")
                .map(|v| *v as u64)
                .unwrap_or(0),
            total: samples
                .get("ds4_proxy_ds4_prefill_total")
                .map(|v| *v as u64)
                .unwrap_or(0),
            percent: samples
                .get("ds4_proxy_ds4_prefill_percent")
                .copied()
                .unwrap_or(0.0),
            cached: samples
                .get("ds4_proxy_ds4_prefill_cached")
                .map(|v| *v as u64)
                .unwrap_or(0),
            chunk_tps: samples
                .get("ds4_proxy_ds4_prefill_chunk_tps")
                .copied()
                .unwrap_or(0.0),
            avg_tps: samples
                .get("ds4_proxy_ds4_prefill_avg_tps")
                .copied()
                .unwrap_or(0.0),
            elapsed_secs: samples
                .get("ds4_proxy_ds4_prefill_elapsed_seconds")
                .copied()
                .unwrap_or(0.0),
        },
        kv_cache: KvCacheState {
            hits_total: samples
                .get("ds4_proxy_ds4_kv_cache_hits_total")
                .map(|v| *v as u64)
                .unwrap_or(0),
            hit_tokens: samples
                .get("ds4_proxy_ds4_kv_cache_hit_tokens")
                .map(|v| *v as u64)
                .unwrap_or(0),
            load_ms: samples
                .get("ds4_proxy_ds4_kv_cache_load_ms")
                .copied()
                .unwrap_or(0.0),
        },
        decode: DecodeState {
            completion: samples
                .get("ds4_proxy_ds4_generation_completion")
                .map(|v| *v as u64)
                .unwrap_or(0),
            chunk_tps: samples
                .get("ds4_proxy_ds4_generation_chunk_tps")
                .copied()
                .unwrap_or(0.0),
            avg_tps: samples
                .get("ds4_proxy_ds4_generation_avg_tps")
                .copied()
                .unwrap_or(0.0),
            elapsed_secs: samples
                .get("ds4_proxy_ds4_generation_elapsed_seconds")
                .copied()
                .unwrap_or(0.0),
        },
    }
}

/// Extract a label value from the first sample line of a family.
fn label_from_sample(text: &str, family: &str, label: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with(family) {
            continue;
        }
        let (name, labels, _) = split_metric_line(line)?;
        if name != family {
            continue;
        }
        return labels.get(label).cloned();
    }
    None
}

/// Split a sample line into (family name, label map, value).
fn split_metric_line(line: &str) -> Option<(String, HashMap<String, String>, &str)> {
    let (name_and_labels, value) = line.split_once(' ')?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(open) = name_and_labels.find('{') {
        let name = name_and_labels[..open].trim().to_string();
        let close = name_and_labels.rfind('}')?;
        if open >= close || name.is_empty() {
            return None;
        }
        let labels = &name_and_labels[open + 1..close];
        let mut map = HashMap::new();
        for part in labels.split(',') {
            let part = part.trim();
            let Some((label_name, label_value)) = part.split_once('=') else {
                continue;
            };
            let label_value = label_value.trim().trim_matches('"');
            map.insert(label_name.trim().to_string(), label_value.to_string());
        }
        Some((name, map, value))
    } else {
        Some((name_and_labels.trim().to_string(), HashMap::new(), value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metrics() -> &'static str {
        r#"
# TYPE ds4_proxy_cluster_mode gauge
ds4_proxy_cluster_mode{node_id="macstudio",mode="solo-standalone"} 1
# TYPE ds4_proxy_cluster_state gauge
ds4_proxy_cluster_state{node_id="macstudio",state="solo-standalone-ready"} 1
# TYPE ds4_proxy_cluster_generation gauge
ds4_proxy_cluster_generation{node_id="macstudio"} 42
# TYPE ds4_proxy_target_ready gauge
ds4_proxy_target_ready{target="local-standalone"} 1
"#
    }

    #[test]
    fn parses_existing_cluster_families() {
        let snapshot = parse_metrics(sample_metrics());
        assert_eq!(snapshot.cluster_mode.as_deref(), Some("solo-standalone"));
        assert_eq!(
            snapshot.cluster_state.as_deref(),
            Some("solo-standalone-ready")
        );
        assert_eq!(snapshot.generation, Some(42));
        assert_eq!(snapshot.target_ready, Some(true));
        assert_eq!(snapshot.node_id.as_deref(), Some("macstudio"));
        assert!(!snapshot.prefill.active);
    }

    #[test]
    fn parses_prefill_and_kv_cache_families() {
        let text = r#"
# TYPE ds4_proxy_ds4_prefill_active gauge
ds4_proxy_ds4_prefill_active 1
# TYPE ds4_proxy_ds4_prefill_current gauge
ds4_proxy_ds4_prefill_current 4096
# TYPE ds4_proxy_ds4_prefill_total gauge
ds4_proxy_ds4_prefill_total 9005
# TYPE ds4_proxy_ds4_prefill_percent gauge
ds4_proxy_ds4_prefill_percent 45.5
# TYPE ds4_proxy_ds4_prefill_cached gauge
ds4_proxy_ds4_prefill_cached 0
# TYPE ds4_proxy_ds4_prefill_chunk_tps gauge
ds4_proxy_ds4_prefill_chunk_tps 123.4
# TYPE ds4_proxy_ds4_prefill_avg_tps gauge
ds4_proxy_ds4_prefill_avg_tps 100.0
# TYPE ds4_proxy_ds4_prefill_elapsed_seconds gauge
ds4_proxy_ds4_prefill_elapsed_seconds 10.0
# TYPE ds4_proxy_ds4_kv_cache_hits_total counter
ds4_proxy_ds4_kv_cache_hits_total 7
# TYPE ds4_proxy_ds4_kv_cache_hit_tokens gauge
ds4_proxy_ds4_kv_cache_hit_tokens 9005
# TYPE ds4_proxy_ds4_kv_cache_load_ms gauge
ds4_proxy_ds4_kv_cache_load_ms 12.3
# TYPE ds4_proxy_ds4_generation_completion gauge
ds4_proxy_ds4_generation_completion 42
# TYPE ds4_proxy_ds4_generation_chunk_tps gauge
ds4_proxy_ds4_generation_chunk_tps 32.1
# TYPE ds4_proxy_ds4_generation_avg_tps gauge
ds4_proxy_ds4_generation_avg_tps 28.5
# TYPE ds4_proxy_ds4_generation_elapsed_seconds gauge
ds4_proxy_ds4_generation_elapsed_seconds 1.5
"#;
        let snapshot = parse_metrics(text);
        assert!(snapshot.prefill.active);
        assert_eq!(snapshot.prefill.current, 4096);
        assert_eq!(snapshot.prefill.total, 9005);
        assert_eq!(snapshot.prefill.percent, 45.5);
        assert_eq!(snapshot.prefill.cached, 0);
        assert_eq!(snapshot.prefill.chunk_tps, 123.4);
        assert_eq!(snapshot.prefill.avg_tps, 100.0);
        assert_eq!(snapshot.prefill.elapsed_secs, 10.0);
        assert_eq!(snapshot.kv_cache.hits_total, 7);
        assert_eq!(snapshot.kv_cache.hit_tokens, 9005);
        assert_eq!(snapshot.kv_cache.load_ms, 12.3);
        assert_eq!(snapshot.decode.completion, 42);
        assert_eq!(snapshot.decode.chunk_tps, 32.1);
        assert_eq!(snapshot.decode.avg_tps, 28.5);
        assert_eq!(snapshot.decode.elapsed_secs, 1.5);
    }

    #[test]
    fn ignores_unknown_families_and_malformed_lines() {
        let text = r#"
# TYPE ds4_proxy_unknown gauge
ds4_proxy_unknown 1
not-a-sample
ds4_proxy_cluster_generation{node_id="macstudio"} not-a-number
"#;
        let snapshot = parse_metrics(text);
        assert_eq!(snapshot.generation, None);
        assert_eq!(snapshot.cluster_mode, None);
    }

    #[test]
    fn parses_labels_with_quoted_values() {
        let text = r#"ds4_proxy_cluster_mode{node_id="node-a",mode="distributed-mxfp4"} 1"#;
        let snapshot = parse_metrics(text);
        assert_eq!(snapshot.cluster_mode.as_deref(), Some("distributed-mxfp4"));
        assert_eq!(snapshot.node_id.as_deref(), Some("node-a"));
    }
}
