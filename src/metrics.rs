use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    app::target_name,
    cluster::{ClusterSnapshot, transition_name},
    config::{ModelVariant, Residency},
    proxy::ModeAwareTargetSnapshot,
    target::ClusterState,
};
use std::{
    collections::HashMap,
    fmt::Write,
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Debug, Default, Clone, Copy)]
struct Summary {
    sum: f64,
    count: u64,
}

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<HashMap<String, u64>>,
    summaries: Mutex<HashMap<String, Summary>>,
    in_flight: Mutex<HashMap<(String, String), u64>>,
}

pub struct RequestMetricGuard {
    metrics: Arc<Metrics>,
    ingress: &'static str,
    target: &'static str,
    started: Instant,
    status: u16,
    ttfb_seconds: Option<f64>,
    failure: Option<&'static str>,
    request_id: String,
    method: String,
    path_template: &'static str,
    in_flight_before: u64,
}

impl Metrics {
    pub fn begin_request(
        self: &Arc<Self>,
        ingress: &'static str,
        target: &'static str,
        request_id: String,
        method: String,
        path_template: &'static str,
    ) -> RequestMetricGuard {
        let key = (ingress.to_owned(), target.to_owned());
        let mut gauges = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gauge = gauges.entry(key).or_default();
        let in_flight_before = *gauge;
        *gauge += 1;
        RequestMetricGuard {
            metrics: self.clone(),
            ingress,
            target,
            started: Instant::now(),
            status: 500,
            ttfb_seconds: None,
            failure: None,
            request_id,
            method,
            path_template,
            in_flight_before,
        }
    }

    pub fn transition(
        &self,
        from: ClusterState,
        to: ClusterState,
        result: &'static str,
        reason: &'static str,
        seconds: f64,
    ) {
        self.increment(
            "ds4_proxy_cluster_transitions_total",
            &[
                ("from", from.name()),
                ("to", to.name()),
                ("result", result),
                ("reason", reason),
            ],
        );
        self.observe(
            "ds4_proxy_cluster_transition_duration_seconds",
            &[("transition", transition_name(from, to))],
            seconds,
        );
    }

    pub fn child_restart(&self, profile: &'static str, reason: &'static str) {
        self.increment(
            "ds4_proxy_cluster_child_restarts_total",
            &[("profile", profile), ("reason", reason)],
        );
    }

    pub fn hello(&self, result: &'static str, reason: &'static str) {
        self.increment(
            "ds4_proxy_cluster_hello_total",
            &[("result", result), ("reason", reason)],
        );
    }

    pub fn deployment_mismatch(&self, field: &'static str) {
        self.increment(
            "ds4_proxy_cluster_deployment_mismatch_total",
            &[("field", field)],
        );
    }

    pub fn discovery_event(&self, source: &'static str, result: &'static str) {
        self.increment(
            "ds4_proxy_peer_discovery_events_total",
            &[("source", source), ("result", result)],
        );
    }

    pub fn render_mode_aware(&self, snapshot: MetricSnapshot<'_>) -> String {
        let mut output = String::new();
        render_counters(&mut output, &self.counters);
        render_summaries(&mut output, &self.summaries);

        let target = target_name(snapshot.target.target);
        output.push_str("# TYPE ds4_proxy_in_flight gauge\n");
        let gauges = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut gauges = gauges.iter().collect::<Vec<_>>();
        gauges.sort_by_key(|((ingress, target), _)| (ingress.as_str(), target.as_str()));
        let mut current_public_present = false;
        for ((ingress, gauge_target), value) in gauges {
            current_public_present |= ingress == "public" && gauge_target == target;
            let _ = writeln!(
                output,
                "ds4_proxy_in_flight{{ingress=\"{}\",target=\"{}\"}} {value}",
                escape_label(ingress),
                escape_label(gauge_target)
            );
        }

        let target_ready =
            snapshot.target.ready && snapshot.admission.state == AdmissionState::Serving;
        if !current_public_present {
            let _ = writeln!(
                output,
                "ds4_proxy_in_flight{{ingress=\"public\",target=\"{target}\"}} {}",
                snapshot.admission.in_flight
            );
        }
        let node_id = escape_label(snapshot.node_id);
        output.push_str("# TYPE ds4_proxy_target_ready gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_target_ready{{target=\"{target}\"}} {}",
            u8::from(target_ready)
        );

        let (state, mode, generation) =
            snapshot
                .cluster
                .map_or(("booting", "unknown", 0), |cluster| {
                    (
                        cluster.state.name(),
                        cluster.stable_mode.name(),
                        cluster.generation,
                    )
                });
        output.push_str("# TYPE ds4_proxy_cluster_state gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_cluster_state{{node_id=\"{node_id}\",state=\"{state}\"}} 1"
        );
        output.push_str("# TYPE ds4_proxy_cluster_mode gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_cluster_mode{{node_id=\"{node_id}\",mode=\"{mode}\"}} 1"
        );
        output.push_str("# TYPE ds4_proxy_cluster_generation gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_cluster_generation{{node_id=\"{node_id}\"}} {generation}"
        );
        output.push_str("# TYPE ds4_proxy_cluster_peer_lease_seconds gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_cluster_peer_lease_seconds{{node_id=\"{node_id}\"}} {}",
            snapshot.peer_lease_seconds
        );
        output.push_str("# TYPE ds4_proxy_thunderbolt_ip_state gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_thunderbolt_ip_state{{node_id=\"{node_id}\",state=\"{}\"}} 1",
            escape_label(snapshot.thunderbolt_ip_state)
        );
        output.push_str("# TYPE ds4_proxy_peer_discovery_results gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_peer_discovery_results{{node_id=\"{node_id}\",interface=\"{}\"}} {}",
            escape_label(snapshot.interface),
            snapshot.discovery_results
        );
        output.push_str("# TYPE ds4_proxy_standalone_profile_info gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_standalone_profile_info{{node_id=\"{node_id}\",model_variant=\"{}\",residency=\"{}\"}} 1",
            snapshot.model_variant.name(),
            snapshot.residency.name(),
        );
        output
    }

    fn finish_request(&self, guard: &RequestMetricGuard) {
        let status_class = format!("{}xx", guard.status / 100);
        self.increment(
            "ds4_proxy_requests_total",
            &[
                ("ingress", guard.ingress),
                ("target", guard.target),
                ("status_class", &status_class),
            ],
        );
        self.observe(
            "ds4_proxy_request_duration_seconds",
            &[("target", guard.target)],
            guard.started.elapsed().as_secs_f64(),
        );
        if let Some(seconds) = guard.ttfb_seconds {
            self.observe(
                "ds4_proxy_time_to_first_byte_seconds",
                &[("target", guard.target)],
                seconds,
            );
        }
        if let Some(reason) = guard.failure {
            self.increment(
                "ds4_proxy_upstream_failures_total",
                &[("target", guard.target), ("reason", reason)],
            );
        }
        let key = (guard.ingress.to_owned(), guard.target.to_owned());
        let mut gauges = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(value) = gauges.get_mut(&key) {
            *value = value.saturating_sub(1);
        }
    }

    fn increment(&self, name: &'static str, labels: &[(&'static str, &str)]) {
        let key = metric_key(name, labels);
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *counters.entry(key).or_default() += 1;
    }

    fn observe(&self, name: &'static str, labels: &[(&'static str, &str)], value: f64) {
        let key = metric_key(name, labels);
        let mut summaries = self
            .summaries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let summary = summaries.entry(key).or_default();
        summary.sum += value;
        summary.count += 1;
    }
}

impl RequestMetricGuard {
    pub fn set_status(&mut self, status: u16) {
        self.status = status;
    }

    pub fn set_ttfb(&mut self, seconds: f64) {
        self.ttfb_seconds = Some(seconds);
    }

    pub fn set_failure(&mut self, failure: &'static str) {
        self.failure = Some(failure);
    }
}

impl Drop for RequestMetricGuard {
    fn drop(&mut self) {
        self.metrics.finish_request(self);
        tracing::info!(
            event = "proxy_request",
            request_id = %self.request_id,
            ingress = self.ingress,
            method = %self.method,
            path_template = self.path_template,
            proxy_target = self.target,
            cluster_mode = "unknown",
            cluster_state = "unknown",
            generation = 0_u64,
            in_flight_before = self.in_flight_before,
            upstream_connect_ms = 0_u64,
            response_header_ms = self.ttfb_seconds.map_or(0.0, |seconds| seconds * 1_000.0),
            first_body_byte_ms = self.ttfb_seconds.map_or(0.0, |seconds| seconds * 1_000.0),
            total_ms = self.started.elapsed().as_secs_f64() * 1_000.0,
            status = self.status,
            bytes_in = 0_u64,
            bytes_out = 0_u64,
            error_kind = self.failure.unwrap_or("none"),
            "proxy request completed"
        );
    }
}

pub struct MetricSnapshot<'a> {
    pub node_id: &'a str,
    pub interface: &'a str,
    pub target: ModeAwareTargetSnapshot,
    pub admission: AdmissionSnapshot,
    pub cluster: Option<ClusterSnapshot>,
    pub peer_lease_seconds: f64,
    pub thunderbolt_ip_state: &'static str,
    pub discovery_results: u64,
    pub model_variant: ModelVariant,
    pub residency: Residency,
}

fn render_counters(output: &mut String, counters: &Mutex<HashMap<String, u64>>) {
    for name in [
        "ds4_proxy_requests_total",
        "ds4_proxy_upstream_failures_total",
        "ds4_proxy_peer_discovery_events_total",
        "ds4_proxy_cluster_transitions_total",
        "ds4_proxy_cluster_child_restarts_total",
        "ds4_proxy_cluster_hello_total",
        "ds4_proxy_cluster_deployment_mismatch_total",
    ] {
        let _ = writeln!(output, "# TYPE {name} counter");
    }
    let counters = counters
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut values = counters.iter().collect::<Vec<_>>();
    values.sort_by_key(|(key, _)| *key);
    for (key, value) in values {
        let _ = writeln!(output, "{key} {value}");
    }
}

fn render_summaries(output: &mut String, summaries: &Mutex<HashMap<String, Summary>>) {
    for name in [
        "ds4_proxy_request_duration_seconds",
        "ds4_proxy_time_to_first_byte_seconds",
        "ds4_proxy_cluster_transition_duration_seconds",
    ] {
        let _ = writeln!(output, "# TYPE {name} summary");
    }
    let summaries = summaries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut values = summaries.iter().collect::<Vec<_>>();
    values.sort_by_key(|(key, _)| *key);
    for (key, value) in values {
        let (name, labels) = key
            .split_once('{')
            .map_or((key.as_str(), ""), |(name, labels)| (name, labels));
        let suffix = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}")
        };
        let _ = writeln!(output, "{name}_sum{suffix} {}", value.sum);
        let _ = writeln!(output, "{name}_count{suffix} {}", value.count);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{LocalRole, ProxyTarget};
    use std::io::Write as IoWrite;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl IoWrite for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn snapshot<'a>(node_id: &'a str, interface: &'a str) -> MetricSnapshot<'a> {
        MetricSnapshot {
            node_id,
            interface,
            target: ModeAwareTargetSnapshot {
                target: ProxyTarget::LocalStandalone,
                ready: true,
            },
            admission: AdmissionSnapshot {
                state: AdmissionState::Serving,
                in_flight: 0,
                max_in_flight: 4,
                drain_generation: None,
            },
            cluster: Some(ClusterSnapshot::booting_at(LocalRole::Unknown, 7)),
            peer_lease_seconds: 0.0,
            thunderbolt_ip_state: "unknown",
            discovery_results: 0,
            model_variant: ModelVariant::Q2Q4,
            residency: Residency::SsdStreaming,
        }
    }

    #[test]
    fn metrics_golden_contains_every_spec_family_with_bounded_labels() {
        let rendered = Metrics::default().render_mode_aware(snapshot("node-a", "bridge0"));
        for family in [
            "ds4_proxy_requests_total",
            "ds4_proxy_request_duration_seconds",
            "ds4_proxy_time_to_first_byte_seconds",
            "ds4_proxy_in_flight",
            "ds4_proxy_target_ready",
            "ds4_proxy_upstream_failures_total",
            "ds4_proxy_cluster_state",
            "ds4_proxy_cluster_mode",
            "ds4_proxy_cluster_generation",
            "ds4_proxy_cluster_peer_lease_seconds",
            "ds4_proxy_thunderbolt_ip_state",
            "ds4_proxy_peer_discovery_results",
            "ds4_proxy_peer_discovery_events_total",
            "ds4_proxy_cluster_transitions_total",
            "ds4_proxy_cluster_transition_duration_seconds",
            "ds4_proxy_cluster_child_restarts_total",
            "ds4_proxy_standalone_profile_info",
            "ds4_proxy_cluster_hello_total",
            "ds4_proxy_cluster_deployment_mismatch_total",
        ] {
            assert!(rendered.contains(family), "missing metric family {family}");
        }
        assert!(rendered.contains("ds4_proxy_cluster_generation{node_id=\"node-a\"} 7"));
        assert!(rendered.contains(
            "ds4_proxy_standalone_profile_info{node_id=\"node-a\",model_variant=\"q2-q4\",residency=\"ssd-streaming\"} 1"
        ));
        for forbidden in ["profile_id", "session", "request_id", "pid", "digest"] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn request_guard_records_completion_and_returns_in_flight_to_zero() {
        let metrics = Arc::new(Metrics::default());
        let mut request = metrics.begin_request(
            "public",
            "local-standalone",
            "req-safe".into(),
            "POST".into(),
            "/v1/chat/completions",
        );
        request.set_status(200);
        request.set_ttfb(0.01);
        drop(request);

        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(rendered.contains(
            "ds4_proxy_requests_total{ingress=\"public\",target=\"local-standalone\",status_class=\"2xx\"} 1"
        ));
        assert!(
            rendered
                .contains("ds4_proxy_in_flight{ingress=\"public\",target=\"local-standalone\"} 0")
        );
        assert!(rendered.contains(
            "ds4_proxy_time_to_first_byte_seconds_sum{target=\"local-standalone\"} 0.01"
        ));
    }

    #[test]
    fn label_values_are_prometheus_escaped() {
        let rendered = Metrics::default().render_mode_aware(snapshot("node\"a\n", "bridge\\0"));
        assert!(rendered.contains("node_id=\"node\\\"a\\n\""));
        assert!(rendered.contains("interface=\"bridge\\\\0\""));
    }

    #[test]
    fn request_log_schema_excludes_headers_and_body() {
        let logs = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(logs.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let metrics = Arc::new(Metrics::default());
            let mut request = metrics.begin_request(
                "public",
                "local-standalone",
                "req-safe".into(),
                "POST".into(),
                "/v1/chat/completions",
            );
            request.set_status(200);
            drop(request);
        });
        let output = String::from_utf8(
            logs.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .unwrap();
        assert!(output.contains("proxy_request"));
        for secret in [
            "Authorization",
            "Bearer top-secret-token",
            "Cookie",
            "private prompt body",
            "x-hermes-session-id",
        ] {
            assert!(!output.contains(secret));
        }
    }

    #[test]
    fn cluster_event_counters_use_only_finite_diagnostic_labels() {
        let metrics = Metrics::default();
        metrics.transition(
            ClusterState::PairedStandaloneReady,
            ClusterState::AwaitingWorkerHello,
            "success",
            "state-change",
            0.25,
        );
        metrics.child_restart("standalone", "unexpected-exit");
        metrics.hello("rejected", "deployment-mismatch");
        metrics.deployment_mismatch("model-sha256");
        metrics.discovery_event("bonjour", "accepted");
        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        for expected in [
            "reason=\"state-change\"",
            "transition=\"promote\"",
            "profile=\"standalone\"",
            "reason=\"deployment-mismatch\"",
            "field=\"model-sha256\"",
            "source=\"bonjour\",result=\"accepted\"",
        ] {
            assert!(rendered.contains(expected), "missing {expected}");
        }
    }
}
