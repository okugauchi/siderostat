use super::{ChildIdentity, ManagedChild, ProcessControlError, SupervisedChild, SupervisedSlot};
use crate::{
    cluster::{Ds4Command, Ds4LogEvent, LocalStandaloneLifecycle},
    metrics::Metrics,
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;
use url::Url;

#[derive(Clone)]
pub struct StandaloneSupervisor {
    inner: Arc<StandaloneSupervisorInner>,
}

struct StandaloneSupervisorInner {
    command: Ds4Command,
    models_url: Url,
    client: reqwest::Client,
    startup_timeout: Duration,
    poll_interval: Duration,
    stop_timeout: Duration,
    allow_sigkill: bool,
    metrics: Arc<Metrics>,
    child: SupervisedSlot,
}

impl StandaloneSupervisor {
    pub fn new(
        command: Ds4Command,
        models_url: Url,
        startup_timeout: Duration,
        poll_interval: Duration,
        stop_timeout: Duration,
        allow_sigkill: bool,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            inner: Arc::new(StandaloneSupervisorInner {
                command,
                models_url,
                client: reqwest::Client::new(),
                startup_timeout,
                poll_interval,
                stop_timeout,
                allow_sigkill,
                metrics,
                child: SupervisedSlot::new(),
            }),
        }
    }

    pub async fn child_identity(&self) -> Option<ChildIdentity> {
        self.inner.child.child_identity().await
    }

    #[cfg(target_os = "macos")]
    async fn start_inner(&self, generation: u64) -> anyhow::Result<()> {
        let Some(mut slot) = self.inner.child.begin_start().await? else {
            return Ok(());
        };
        let startup_deadline = Instant::now() + self.inner.startup_timeout;
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let (mut logs, mut events, forwarders) = child.start_log_forwarding_with_events(256)?;
        let (dspark_activation, mut dspark_activation_rx) = tokio::sync::watch::channel(false);
        let activation_profile = self.inner.command.profile.profile_id.clone();
        let metrics = self.inner.metrics.clone();
        let log_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = events.recv() => {
                        match event {
                            Ds4LogEvent::DsparkActivated => {
                                dspark_activation.send_replace(true);
                                tracing::info!(
                                    event = "dspark-activated",
                                    profile = %activation_profile,
                                    generation,
                                    "DS4 standalone feature activated"
                                );
                            }
                            Ds4LogEvent::PrefillProgress {
                                current,
                                total,
                                percent,
                                cached,
                                chunk_tps,
                                avg_tps,
                                elapsed_secs,
                            } => {
                                metrics.prefill_progress(crate::metrics::PrefillProgress {
                                    current,
                                    total,
                                    percent,
                                    cached,
                                    chunk_tps,
                                    avg_tps,
                                    elapsed_secs,
                                });
                            }
                            Ds4LogEvent::KvCacheHit { tokens, load_ms } => {
                                metrics.kv_cache_hit(tokens, load_ms);
                            }
                        Ds4LogEvent::GenerationProgress {
                            completion,
                            chunk_tps,
                            avg_tps,
                            elapsed_secs,
                        } => {
                            metrics.generation_progress(
                                completion,
                                chunk_tps,
                                avg_tps,
                                elapsed_secs,
                            );
                            }
                            Ds4LogEvent::HttpListening { .. }
                            | Ds4LogEvent::WorkerRegistered { .. }
                            | Ds4LogEvent::CompleteRouteReady { .. }
                            | Ds4LogEvent::WorkerRemoved { .. }
                            | Ds4LogEvent::RouteIncomplete { .. } => {}
                        }
                    }
                    Some(record) = logs.recv() => {
                        tracing::info!(
                            profile = %record.profile_id,
                            generation = record.generation,
                            pid = record.pid,
                            stream = ?record.stream,
                            truncated = record.truncated,
                            event = ?record.event,
                            line_bytes = record.line.len(),
                            "DS4 child log"
                        );
                    }
                    else => break,
                }
            }
        });
        if let Err(error) = child
            .wait_http_ready(
                &self.inner.client,
                &self.inner.models_url,
                self.inner.startup_timeout,
                self.inner.poll_interval,
            )
            .await
        {
            let _ = child
                .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
                .await;
            log_task.abort();
            return Err(error.into());
        }
        if self.inner.command.profile.dspark_required && !*dspark_activation_rx.borrow() {
            let remaining = startup_deadline.saturating_duration_since(Instant::now());
            let activation_observed = match tokio::time::timeout(
                remaining,
                dspark_activation_rx.wait_for(|activated| *activated),
            )
            .await
            {
                Ok(Ok(activated)) => *activated,
                Ok(Err(_)) | Err(_) => false,
            };
            if !activation_observed {
                let _ = child
                    .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
                    .await;
                log_task.abort();
                return Err(ProcessControlError::DsparkActivationTimeout.into());
            }
        }
        *slot = Some(SupervisedChild {
            child,
            _log_forwarders: forwarders,
            log_task,
        });
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    async fn start_inner(&self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("managed DS4 supervision requires macOS")
    }

    async fn stop_inner(&self) -> anyhow::Result<()> {
        self.inner
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await
    }

    async fn is_running_inner(&self) -> anyhow::Result<bool> {
        self.inner.child.is_running().await
    }
}

impl LocalStandaloneLifecycle for StandaloneSupervisor {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.start_inner(generation).await })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.stop_inner().await })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.is_running_inner().await })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.child_identity().await })
    }
}
