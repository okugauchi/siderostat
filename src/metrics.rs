use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    app::target_name,
    proxy::ModeAwareTargetSnapshot,
};
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

    pub fn render_mode_aware(
        &self,
        node_id: &str,
        target: ModeAwareTargetSnapshot,
        admission: AdmissionSnapshot,
    ) -> String {
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
        let target_ready = target.ready && admission.state == AdmissionState::Serving;
        let target = target_name(target.target);
        let node_id = escape_label(node_id);
        output.push_str("# TYPE ds4_proxy_in_flight gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_in_flight{{ingress=\"public\",target=\"{target}\"}} {}",
            admission.in_flight
        );
        output.push_str("# TYPE ds4_proxy_target_ready gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_target_ready{{target=\"{target}\"}} {}",
            u8::from(target_ready)
        );
        output.push_str("# TYPE ds4_proxy_cluster_generation gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_cluster_generation{{node_id=\"{node_id}\"}} 0"
        );
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
