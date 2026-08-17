use super::{ChildIdentity, ManagedChild, SupervisedChild, SupervisedSlot};
use crate::{
    cluster::{DistributedWorkerLifecycle, Ds4Command, Ds4LogEvent},
    metrics::Metrics,
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct DistributedWorkerSupervisor {
    inner: Arc<DistributedWorkerSupervisorInner>,
}

struct DistributedWorkerSupervisorInner {
    command: Ds4Command,
    stop_timeout: Duration,
    allow_sigkill: bool,
    metrics: Arc<Metrics>,
    child: SupervisedSlot,
}

impl DistributedWorkerSupervisor {
    pub fn new(
        command: Ds4Command,
        stop_timeout: Duration,
        allow_sigkill: bool,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            inner: Arc::new(DistributedWorkerSupervisorInner {
                command,
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
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let metrics = self.inner.metrics.clone();
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
                if let Some(event) = &record.event {
                    match event {
                        Ds4LogEvent::PrefillProgress {
                            current,
                            total,
                            percent,
                            cached,
                        } => {
                            metrics.prefill_progress(*current, *total, *percent, *cached);
                        }
                        Ds4LogEvent::KvCacheHit { tokens, load_ms } => {
                            metrics.kv_cache_hit(*tokens, *load_ms);
                        }
                        Ds4LogEvent::GenerationProgress {
                            completion,
                            chunk_tps,
                            avg_tps,
                        } => {
                            metrics.generation_progress(*completion, *chunk_tps, *avg_tps);
                        }
                        Ds4LogEvent::HttpListening { .. }
                        | Ds4LogEvent::DsparkActivated
                        | Ds4LogEvent::WorkerRegistered { .. }
                        | Ds4LogEvent::CompleteRouteReady { .. }
                        | Ds4LogEvent::WorkerRemoved { .. }
                        | Ds4LogEvent::RouteIncomplete { .. } => {}
                    }
                }
                tracing::info!(
                    profile = %record.profile_id,
                    generation = record.generation,
                    pid = record.pid,
                    stream = ?record.stream,
                    truncated = record.truncated,
                    event = ?record.event,
                    line_bytes = record.line.len(),
                    "DS4 distributed worker log"
                );
            }
        });
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

impl DistributedWorkerLifecycle for DistributedWorkerSupervisor {
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
