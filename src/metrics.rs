use crate::{affinity::AffinityStore, backend::BackendRegistry};
use std::{collections::HashMap, fmt::Write, sync::Mutex};

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<HashMap<String, u64>>,
    duration_sum: Mutex<f64>,
    duration_count: Mutex<u64>,
    ttfb_sum: Mutex<f64>,
    ttfb_count: Mutex<u64>,
}

impl Metrics {
    pub fn increment(&self, name: &str, labels: &[(&str, &str)]) {
        let key = metric_key(name, labels);
        if let Ok(mut counters) = self.counters.lock() {
            *counters.entry(key).or_insert(0) += 1;
        }
    }

    pub fn observe_duration(&self, seconds: f64) {
        if let Ok(mut value) = self.duration_sum.lock() {
            *value += seconds;
        }
        if let Ok(mut count) = self.duration_count.lock() {
            *count += 1;
        }
    }

    pub fn observe_ttfb(&self, seconds: f64) {
        if let Ok(mut value) = self.ttfb_sum.lock() {
            *value += seconds;
        }
        if let Ok(mut count) = self.ttfb_count.lock() {
            *count += 1;
        }
    }

    pub fn render(&self, registry: &BackendRegistry, affinity: &AffinityStore) -> String {
        let mut output = String::new();
        output.push_str("# TYPE ds4_proxy_requests_total counter\n");
        if let Ok(counters) = self.counters.lock() {
            let mut values = counters.iter().collect::<Vec<_>>();
            values.sort_by_key(|(key, _)| *key);
            for (key, value) in values {
                let _ = writeln!(output, "{key} {value}");
            }
        }
        output.push_str("# TYPE ds4_proxy_request_duration_seconds summary\n");
        let duration_sum = self.duration_sum.lock().map_or(0.0, |value| *value);
        let duration_count = self.duration_count.lock().map_or(0, |value| *value);
        let _ = writeln!(
            output,
            "ds4_proxy_request_duration_seconds_sum {duration_sum}"
        );
        let _ = writeln!(
            output,
            "ds4_proxy_request_duration_seconds_count {duration_count}"
        );
        output.push_str("# TYPE ds4_proxy_time_to_first_byte_seconds summary\n");
        let ttfb_sum = self.ttfb_sum.lock().map_or(0.0, |value| *value);
        let ttfb_count = self.ttfb_count.lock().map_or(0, |value| *value);
        let _ = writeln!(
            output,
            "ds4_proxy_time_to_first_byte_seconds_sum {ttfb_sum}"
        );
        let _ = writeln!(
            output,
            "ds4_proxy_time_to_first_byte_seconds_count {ttfb_count}"
        );

        output.push_str("# TYPE ds4_proxy_in_flight gauge\n");
        output.push_str("# TYPE ds4_proxy_backend_health gauge\n");
        output.push_str("# TYPE ds4_proxy_affinity_entries gauge\n");
        for backend in registry.all() {
            let id = escape_label(&backend.config.id);
            let state = backend.snapshot();
            let _ = writeln!(
                output,
                "ds4_proxy_in_flight{{backend=\"{id}\"}} {}",
                backend.in_flight()
            );
            for health in [
                "unknown", "alive", "suspect", "offline", "cooldown", "disabled",
            ] {
                let value = u8::from(state.health.as_str() == health);
                let _ = writeln!(
                    output,
                    "ds4_proxy_backend_health{{backend=\"{id}\",state=\"{health}\"}} {value}"
                );
            }
            let entries = affinity
                .counts_by_backend()
                .get(&backend.config.id)
                .copied()
                .unwrap_or(0);
            let _ = writeln!(
                output,
                "ds4_proxy_affinity_entries{{backend=\"{id}\"}} {entries}"
            );
        }
        output
    }
}

fn metric_key(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let labels = labels
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\"", escape_label(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}{{{labels}}}")
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
