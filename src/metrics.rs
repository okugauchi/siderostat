use crate::{
    admission::{AdmissionSnapshot, AdmissionState},
    app::target_name,
    cluster::{ClusterSnapshot, transition_name},
    config::{Quantization, Residency, SpeculativeSupport},
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

#[derive(Debug, Default, Clone)]
struct ProgressFreshness {
    last_progress_at: Option<Instant>,
    last_tokens: Option<u64>,
    token_delta: u64,
}

impl ProgressFreshness {
    fn record(&mut self, now: Instant, tokens: u64) {
        self.token_delta = match self.last_tokens {
            Some(previous) if tokens >= previous => tokens - previous,
            Some(_) | None => tokens,
        };
        self.last_tokens = Some(tokens);
        self.last_progress_at = Some(now);
    }

    fn age_seconds(&self, now: Instant, active: bool) -> f64 {
        if !active {
            return 0.0;
        }
        self.last_progress_at.map_or(0.0, |last_progress_at| {
            now.saturating_duration_since(last_progress_at)
                .as_secs_f64()
        })
    }

    fn observed(&self) -> bool {
        self.last_progress_at.is_some()
    }

    fn token_delta(&self) -> u64 {
        self.token_delta
    }
}

#[derive(Debug, Default, Clone)]
struct Ds4MetricsState {
    prefill_active: bool,
    prefill_current: u64,
    prefill_total: u64,
    prefill_percent: f64,
    prefill_cached: u64,
    prefill_chunk_tps: f64,
    prefill_avg_tps: f64,
    prefill_elapsed_secs: f64,
    kv_cache_hits: u64,
    kv_cache_hit_tokens: u64,
    kv_cache_load_ms: f64,
    generation_completion: u64,
    generation_chunk_tps: f64,
    generation_avg_tps: f64,
    generation_elapsed_secs: f64,
    prefill_progress: ProgressFreshness,
    generation_progress: ProgressFreshness,
}

#[derive(Debug, Clone, Copy)]
pub struct PrefillProgress {
    pub current: u64,
    pub total: u64,
    pub percent: f64,
    pub cached: u64,
    pub chunk_tps: f64,
    pub avg_tps: f64,
    pub elapsed_secs: f64,
}

/// Redaction-safe metrics state used by the recovery diagnostic snapshot. It
/// contains aggregate progress only; request identifiers and payload data are
/// intentionally not represented by this type.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressDiagnosticSnapshot {
    pub active: bool,
    pub progress_observed: bool,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub percent: f64,
    pub cached: Option<u64>,
    pub chunk_tps: f64,
    pub avg_tps: f64,
    pub elapsed_secs: f64,
    pub age_secs: Option<f64>,
    pub token_delta: u64,
}

impl Default for ProgressDiagnosticSnapshot {
    fn default() -> Self {
        Self {
            active: false,
            progress_observed: false,
            current: None,
            total: None,
            percent: 0.0,
            cached: None,
            chunk_tps: 0.0,
            avg_tps: 0.0,
            elapsed_secs: 0.0,
            age_secs: None,
            token_delta: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricsDiagnosticSnapshot {
    pub in_flight: u64,
    pub generation_in_flight: u64,
    pub prefill: ProgressDiagnosticSnapshot,
    pub generation: ProgressDiagnosticSnapshot,
}

#[derive(Default)]
pub struct Metrics {
    counters: Mutex<HashMap<String, u64>>,
    summaries: Mutex<HashMap<String, Summary>>,
    in_flight: Mutex<HashMap<(String, String), u64>>,
    generation_in_flight: Mutex<u64>,
    generation_started_at: Mutex<Option<Instant>>,
    ds4: Mutex<Ds4MetricsState>,
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
    tracks_generation: bool,
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
        let in_flight_before = {
            let mut gauges = self
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let gauge = gauges.entry(key).or_default();
            let in_flight_before = *gauge;
            *gauge += 1;
            in_flight_before
        };
        let tracks_generation = is_generation_path(path_template);
        if tracks_generation {
            let mut generation_in_flight = self
                .generation_in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *generation_in_flight == 0 {
                *self
                    .generation_started_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
            }
            *generation_in_flight += 1;
        }
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
            tracks_generation,
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

    pub fn recovery_started(&self, trigger: &str) {
        self.recovery_event("started", trigger);
    }

    pub fn recovery_completed(&self, trigger: &str) {
        self.recovery_event("completed", trigger);
    }

    pub fn recovery_failed(&self, reason: &str) {
        self.recovery_event("failed", reason);
    }

    pub fn recovery_suppressed(&self, reason: &str) {
        self.recovery_event("suppressed", reason);
    }

    pub fn notification_suppressed(&self, kind: &'static str) {
        self.increment(
            "ds4_proxy_notification_events_total",
            &[("event", "suppressed"), ("kind", kind)],
        );
    }

    fn recovery_event(&self, event: &'static str, reason: &str) {
        self.increment(
            "ds4_proxy_recovery_events_total",
            &[("event", event), ("reason", reason)],
        );
    }

    /// Record DS4 prefill progress. When `current >= total` the prefill is
    /// considered finished and the active flag is cleared.
    pub fn prefill_progress(&self, progress: PrefillProgress) {
        let mut ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ds4.prefill_progress
            .record(Instant::now(), progress.current);
        ds4.prefill_active = progress.current < progress.total;
        ds4.prefill_current = progress.current;
        ds4.prefill_total = progress.total;
        ds4.prefill_percent = progress.percent;
        ds4.prefill_cached = progress.cached;
        ds4.prefill_chunk_tps = progress.chunk_tps;
        ds4.prefill_avg_tps = progress.avg_tps;
        ds4.prefill_elapsed_secs = progress.elapsed_secs;
    }

    /// Record a DS4 KV cache hit.
    pub fn kv_cache_hit(&self, tokens: u64, load_ms: f64) {
        let mut ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ds4.kv_cache_hits = ds4.kv_cache_hits.saturating_add(1);
        ds4.kv_cache_hit_tokens = tokens;
        ds4.kv_cache_load_ms = load_ms;
    }

    /// Record the latest DS4 generation progress values for the monitor.
    pub fn generation_progress(
        &self,
        completion: u64,
        chunk_tps: f64,
        avg_tps: f64,
        elapsed_secs: f64,
    ) {
        let mut ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ds4.generation_progress.record(Instant::now(), completion);
        ds4.generation_completion = completion;
        ds4.generation_chunk_tps = chunk_tps;
        ds4.generation_avg_tps = avg_tps;
        ds4.generation_elapsed_secs = elapsed_secs;
    }

    fn reset_generation(&self) {
        let mut ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ds4.generation_completion = 0;
        ds4.generation_chunk_tps = 0.0;
        ds4.generation_avg_tps = 0.0;
        ds4.generation_elapsed_secs = 0.0;
        ds4.generation_progress = ProgressFreshness::default();
    }

    /// Return aggregate, redaction-safe metrics for a diagnostic snapshot.
    /// Request IDs, paths, headers, and payloads are not retained here.
    pub fn diagnostic_snapshot(&self) -> MetricsDiagnosticSnapshot {
        let in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .copied()
            .sum();
        let generation_in_flight = *self
            .generation_in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        MetricsDiagnosticSnapshot {
            in_flight,
            generation_in_flight,
            prefill: ProgressDiagnosticSnapshot {
                active: ds4.prefill_active,
                progress_observed: ds4.prefill_progress.observed(),
                current: Some(ds4.prefill_current),
                total: Some(ds4.prefill_total),
                percent: ds4.prefill_percent,
                cached: Some(ds4.prefill_cached),
                chunk_tps: ds4.prefill_chunk_tps,
                avg_tps: ds4.prefill_avg_tps,
                elapsed_secs: ds4.prefill_elapsed_secs,
                age_secs: ds4
                    .prefill_active
                    .then(|| ds4.prefill_progress.age_seconds(now, true)),
                token_delta: ds4.prefill_progress.token_delta(),
            },
            generation: ProgressDiagnosticSnapshot {
                active: generation_in_flight > 0,
                progress_observed: ds4.generation_progress.observed(),
                current: Some(ds4.generation_completion),
                total: None,
                percent: 0.0,
                cached: None,
                chunk_tps: ds4.generation_chunk_tps,
                avg_tps: ds4.generation_avg_tps,
                elapsed_secs: ds4.generation_elapsed_secs,
                age_secs: (generation_in_flight > 0)
                    .then(|| ds4.generation_progress.age_seconds(now, true)),
                token_delta: ds4.generation_progress.token_delta(),
            },
        }
    }

    pub fn generation_active_age_secs(&self) -> Option<f64> {
        let active = *self
            .generation_in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            > 0;
        active.then(|| {
            self.generation_started_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map_or(0.0, |started| started.elapsed().as_secs_f64())
        })
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
            "ds4_proxy_standalone_profile_info{{node_id=\"{node_id}\",quantization=\"{}\",speculative_support=\"{}\",residency=\"{}\"}} 1",
            snapshot.quantization.name(),
            snapshot.speculative_support.name(),
            snapshot.residency.name(),
        );
        let generation_active = *self
            .generation_in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            > 0;
        let ds4 = self
            .ds4
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        output.push_str("# TYPE ds4_proxy_ds4_prefill_active gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_active {}",
            u8::from(ds4.prefill_active)
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_current gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_current {}",
            ds4.prefill_current
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_total gauge\n");
        let _ = writeln!(output, "ds4_proxy_ds4_prefill_total {}", ds4.prefill_total);
        output.push_str("# TYPE ds4_proxy_ds4_prefill_percent gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_percent {}",
            ds4.prefill_percent
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_cached gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_cached {}",
            ds4.prefill_cached
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_chunk_tps gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_chunk_tps {}",
            ds4.prefill_chunk_tps
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_avg_tps gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_avg_tps {}",
            ds4.prefill_avg_tps
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_elapsed_seconds gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_elapsed_seconds {}",
            ds4.prefill_elapsed_secs
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_last_progress_age_seconds gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_last_progress_age_seconds {}",
            ds4.prefill_progress.age_seconds(now, ds4.prefill_active)
        );
        output.push_str("# TYPE ds4_proxy_ds4_prefill_progress_token_delta gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_prefill_progress_token_delta {}",
            ds4.prefill_progress.token_delta()
        );
        output.push_str("# TYPE ds4_proxy_ds4_kv_cache_hits_total counter\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_kv_cache_hits_total {}",
            ds4.kv_cache_hits
        );
        output.push_str("# TYPE ds4_proxy_ds4_kv_cache_hit_tokens gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_kv_cache_hit_tokens {}",
            ds4.kv_cache_hit_tokens
        );
        output.push_str("# TYPE ds4_proxy_ds4_kv_cache_load_ms gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_kv_cache_load_ms {}",
            ds4.kv_cache_load_ms
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_active gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_active {}",
            u8::from(generation_active)
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_completion gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_completion {}",
            ds4.generation_completion
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_chunk_tps gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_chunk_tps {}",
            ds4.generation_chunk_tps
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_avg_tps gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_avg_tps {}",
            ds4.generation_avg_tps
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_elapsed_seconds gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_elapsed_seconds {}",
            ds4.generation_elapsed_secs
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_progress_observed gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_progress_observed {}",
            u8::from(ds4.generation_progress.observed())
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_last_progress_age_seconds gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_last_progress_age_seconds {}",
            ds4.generation_progress.age_seconds(now, generation_active)
        );
        output.push_str("# TYPE ds4_proxy_ds4_generation_progress_token_delta gauge\n");
        let _ = writeln!(
            output,
            "ds4_proxy_ds4_generation_progress_token_delta {}",
            ds4.generation_progress.token_delta()
        );
        drop(ds4);
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
        drop(gauges);

        if guard.tracks_generation {
            let generation_finished = {
                let mut generation_in_flight = self
                    .generation_in_flight
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *generation_in_flight = generation_in_flight.saturating_sub(1);
                *generation_in_flight == 0
            };
            if generation_finished {
                self.reset_generation();
                *self
                    .generation_started_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
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

fn is_generation_path(path_template: &str) -> bool {
    matches!(path_template, "/v1/chat/completions" | "/v1/completions")
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
    pub quantization: Quantization,
    pub speculative_support: SpeculativeSupport,
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
        "ds4_proxy_recovery_events_total",
        "ds4_proxy_notification_events_total",
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
    use std::{
        io::Write as IoWrite,
        time::{Duration, Instant},
    };

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
            quantization: Quantization::Q2Q4,
            speculative_support: SpeculativeSupport::Dspark,
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
            "ds4_proxy_recovery_events_total",
            "ds4_proxy_ds4_generation_active",
            "ds4_proxy_ds4_generation_progress_observed",
            "ds4_proxy_ds4_generation_last_progress_age_seconds",
            "ds4_proxy_ds4_generation_progress_token_delta",
            "ds4_proxy_ds4_prefill_last_progress_age_seconds",
            "ds4_proxy_ds4_prefill_progress_token_delta",
        ] {
            assert!(rendered.contains(family), "missing metric family {family}");
        }
        assert!(rendered.contains("ds4_proxy_cluster_generation{node_id=\"node-a\"} 7"));
        assert!(rendered.contains(
            "ds4_proxy_standalone_profile_info{node_id=\"node-a\",quantization=\"q2-q4\",speculative_support=\"dspark\",residency=\"ssd-streaming\"} 1"
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

    #[test]
    fn renders_ds4_prefill_and_kv_cache_gauges() {
        let metrics = Arc::new(Metrics::default());
        metrics.prefill_progress(PrefillProgress {
            current: 4096,
            total: 9005,
            percent: 45.5,
            cached: 0,
            chunk_tps: 123.4,
            avg_tps: 100.0,
            elapsed_secs: 10.0,
        });
        metrics.kv_cache_hit(9005, 12.3);
        metrics.kv_cache_hit(1024, 3.1);
        let request = metrics.begin_request(
            "public",
            "local-standalone",
            "req-generation".into(),
            "POST".into(),
            "/v1/chat/completions",
        );
        metrics.generation_progress(42, 32.1, 28.5, 1.5);
        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_active 1"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_current 4096"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_total 9005"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_percent 45.5"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_cached 0"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_chunk_tps 123.4"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_avg_tps 100"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_elapsed_seconds 10"));
        assert!(rendered.contains("ds4_proxy_ds4_kv_cache_hits_total 2"));
        assert!(rendered.contains("ds4_proxy_ds4_kv_cache_hit_tokens 1024"));
        assert!(rendered.contains("ds4_proxy_ds4_kv_cache_load_ms 3.1"));
        assert!(rendered.contains("ds4_proxy_ds4_generation_completion 42"));
        assert!(rendered.contains("ds4_proxy_ds4_generation_chunk_tps 32.1"));
        assert!(rendered.contains("ds4_proxy_ds4_generation_avg_tps 28.5"));
        assert!(rendered.contains("ds4_proxy_ds4_generation_elapsed_seconds 1.5"));
        assert!(rendered.contains("ds4_proxy_ds4_generation_active 1"));

        drop(request);
        let cleared = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(cleared.contains("ds4_proxy_ds4_generation_active 0"));
        assert!(cleared.contains("ds4_proxy_ds4_generation_completion 0"));
        assert!(cleared.contains("ds4_proxy_ds4_generation_avg_tps 0"));
    }

    #[test]
    fn renders_bounded_recovery_events_without_recovery_id_labels() {
        let metrics = Metrics::default();
        metrics.recovery_started("low-decode-tps");
        metrics.recovery_completed("low-decode-tps");
        metrics.recovery_failed("canary-failed");
        metrics.recovery_suppressed("cooldown");

        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        for expected in [
            "ds4_proxy_recovery_events_total{event=\"started\",reason=\"low-decode-tps\"} 1",
            "ds4_proxy_recovery_events_total{event=\"completed\",reason=\"low-decode-tps\"} 1",
            "ds4_proxy_recovery_events_total{event=\"failed\",reason=\"canary-failed\"} 1",
            "ds4_proxy_recovery_events_total{event=\"suppressed\",reason=\"cooldown\"} 1",
        ] {
            assert!(rendered.contains(expected), "missing {expected}");
        }
        assert!(!rendered.contains("recovery_id"));
    }

    #[test]
    fn renders_bounded_notification_suppression_events_without_epoch_labels() {
        let metrics = Metrics::default();
        metrics.notification_suppressed("solo-standalone-ready");
        metrics.notification_suppressed("solo-standalone-ready");
        metrics.notification_suppressed("paired-standalone-ready");

        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));

        assert!(rendered.contains(
            "ds4_proxy_notification_events_total{event=\"suppressed\",kind=\"solo-standalone-ready\"} 2"
        ));
        assert!(rendered.contains(
            "ds4_proxy_notification_events_total{event=\"suppressed\",kind=\"paired-standalone-ready\"} 1"
        ));
        assert!(!rendered.contains("recovery_id"));
        assert!(!rendered.contains("generation=\""));
    }

    #[test]
    fn prefill_completion_clears_active_flag() {
        let metrics = Metrics::default();
        metrics.prefill_progress(PrefillProgress {
            current: 9005,
            total: 9005,
            percent: 100.0,
            cached: 0,
            chunk_tps: 123.4,
            avg_tps: 100.0,
            elapsed_secs: 10.0,
        });
        let rendered = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_active 0"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_last_progress_age_seconds 0"));
        assert!(rendered.contains("ds4_proxy_ds4_prefill_progress_token_delta 9005"));

        metrics.prefill_progress(PrefillProgress {
            current: 4,
            total: 100,
            percent: 4.0,
            cached: 0,
            chunk_tps: 123.4,
            avg_tps: 100.0,
            elapsed_secs: 1.0,
        });
        let next_request = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(next_request.contains("ds4_proxy_ds4_prefill_active 1"));
        assert!(next_request.contains("ds4_proxy_ds4_prefill_progress_token_delta 4"));
    }

    #[test]
    fn progress_freshness_uses_monotonic_age_and_handles_counter_reset() {
        let start = Instant::now();
        let mut progress = ProgressFreshness::default();

        assert!(!progress.observed());
        assert_eq!(
            progress.age_seconds(start + Duration::from_secs(5), true),
            0.0
        );

        progress.record(start, 100);
        assert!(progress.observed());
        assert_eq!(progress.token_delta(), 100);
        assert_eq!(
            progress.age_seconds(start + Duration::from_secs(5), true),
            5.0
        );
        assert_eq!(
            progress.age_seconds(start + Duration::from_secs(5), false),
            0.0
        );

        progress.record(start + Duration::from_secs(6), 104);
        assert_eq!(progress.token_delta(), 4);
        progress.record(start + Duration::from_secs(7), 3);
        assert_eq!(progress.token_delta(), 3);
    }

    #[test]
    fn generation_progress_exposes_first_token_waiting_and_resets_on_completion() {
        let metrics = Arc::new(Metrics::default());
        let request = metrics.begin_request(
            "public",
            "local-standalone",
            "req-first-token".into(),
            "POST".into(),
            "/v1/chat/completions",
        );
        let waiting = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(waiting.contains("ds4_proxy_ds4_generation_active 1"));
        assert!(waiting.contains("ds4_proxy_ds4_generation_progress_observed 0"));
        assert!(waiting.contains("ds4_proxy_ds4_generation_last_progress_age_seconds 0"));

        metrics.generation_progress(2, 8.0, 8.0, 0.25);
        let progressing = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(progressing.contains("ds4_proxy_ds4_generation_progress_observed 1"));
        assert!(progressing.contains("ds4_proxy_ds4_generation_progress_token_delta 2"));

        drop(request);
        let idle = metrics.render_mode_aware(snapshot("node-a", "bridge0"));
        assert!(idle.contains("ds4_proxy_ds4_generation_active 0"));
        assert!(idle.contains("ds4_proxy_ds4_generation_progress_observed 0"));
        assert!(idle.contains("ds4_proxy_ds4_generation_last_progress_age_seconds 0"));
        assert!(idle.contains("ds4_proxy_ds4_generation_progress_token_delta 0"));
    }
}
