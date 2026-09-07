use super::{ChildIdentity, ManagedChild, SupervisedChild, SupervisedSlot};
use crate::{
    cluster::{
        DistributedWorkerLifecycle, Ds4Command, Ds4LogEvent, TpConnectedObservation,
        TpWorkerLifecycle, TpWorkerPrepared,
    },
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
                            chunk_tps,
                            avg_tps,
                            elapsed_secs,
                        } => {
                            metrics.prefill_progress(crate::metrics::PrefillProgress {
                                current: *current,
                                total: *total,
                                percent: *percent,
                                cached: *cached,
                                chunk_tps: *chunk_tps,
                                avg_tps: *avg_tps,
                                elapsed_secs: *elapsed_secs,
                            });
                        }
                        Ds4LogEvent::KvCacheHit { tokens, load_ms } => {
                            metrics.kv_cache_hit(*tokens, *load_ms);
                        }
                        Ds4LogEvent::GenerationProgress {
                            completion,
                            chunk_tps,
                            avg_tps,
                            elapsed_secs,
                        } => {
                            metrics.generation_progress(
                                *completion,
                                *chunk_tps,
                                *avg_tps,
                                *elapsed_secs,
                            );
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

/// TP worker の supervised lifecycle。Prepared（child 生成・生存のみ）と Connected
/// （DS4 の session 完了観測）を分離する（C02）。coordinator 起動を待たない。
/// child 操作は既存の SupervisedSlot / ManagedChild を再利用し、identity 保護
/// （PID 再利用・不一致では signal しない）を迂回しない。
#[derive(Clone)]
pub struct TpWorkerSupervisor {
    inner: Arc<TpWorkerSupervisorInner>,
}

struct TpWorkerSupervisorInner {
    command: Ds4Command,
    stop_timeout: Duration,
    allow_sigkill: bool,
    metrics: Arc<Metrics>,
    child: SupervisedSlot,
    /// Connected 観測の有無。実 Ds4LogEvent 観測でのみ立てる（固定値禁止）。
    connected: Arc<std::sync::atomic::AtomicBool>,
}

impl TpWorkerSupervisor {
    pub fn new(
        command: Ds4Command,
        stop_timeout: Duration,
        allow_sigkill: bool,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            inner: Arc::new(TpWorkerSupervisorInner {
                command,
                stop_timeout,
                allow_sigkill,
                metrics,
                child: SupervisedSlot::new(),
                connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
        }
    }

    pub async fn child_identity(&self) -> Option<ChildIdentity> {
        self.inner.child.child_identity().await
    }

    pub async fn is_connected(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.inner.connected.load(Ordering::Acquire)
    }

    #[cfg(target_os = "macos")]
    async fn prepare_inner(&self, generation: u64) -> anyhow::Result<TpWorkerPrepared> {
        let Some(mut slot) = self.inner.child.begin_start().await? else {
            // 既に起動中。Prepared を維持し、既存 child の identity を返す。
            return Ok(TpWorkerPrepared {
                generation,
                identity: self.inner.child.child_identity().await,
            });
        };
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let identity = child.identity().clone();
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let metrics = self.inner.metrics.clone();
        let connected = self.inner.connected.clone();
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
                if let Some(event) = &record.event {
                    match event {
                        Ds4LogEvent::WorkerRegistered { .. }
                        | Ds4LogEvent::CompleteRouteReady { .. } => {
                            connected.store(true, std::sync::atomic::Ordering::Release);
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
                                current: *current,
                                total: *total,
                                percent: *percent,
                                cached: *cached,
                                chunk_tps: *chunk_tps,
                                avg_tps: *avg_tps,
                                elapsed_secs: *elapsed_secs,
                            });
                        }
                        Ds4LogEvent::KvCacheHit { tokens, load_ms } => {
                            metrics.kv_cache_hit(*tokens, *load_ms);
                        }
                        Ds4LogEvent::GenerationProgress {
                            completion,
                            chunk_tps,
                            avg_tps,
                            elapsed_secs,
                        } => {
                            metrics.generation_progress(
                                *completion,
                                *chunk_tps,
                                *avg_tps,
                                *elapsed_secs,
                            );
                        }
                        Ds4LogEvent::HttpListening { .. }
                        | Ds4LogEvent::DsparkActivated
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
                    "DS4 TP worker log"
                );
            }
        });
        *slot = Some(SupervisedChild {
            child,
            _log_forwarders: forwarders,
            log_task,
        });
        self.inner
            .connected
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(TpWorkerPrepared {
            generation,
            identity: Some(identity),
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn prepare_inner(&self, _generation: u64) -> anyhow::Result<TpWorkerPrepared> {
        anyhow::bail!("managed DS4 TP worker supervision requires macOS")
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

    async fn observe_connected_inner(
        &self,
        _observation: TpConnectedObservation,
    ) -> anyhow::Result<()> {
        // 実 Ds4LogEvent 観測（WorkerRegistered / CompleteRouteReady）は log forwarder で
        // connected フラグを立てる。ここでは明示的な観測受領のみを記録する。
        self.inner
            .connected
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

impl TpWorkerLifecycle for TpWorkerSupervisor {
    fn prepare(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<TpWorkerPrepared>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.prepare_inner(generation).await })
    }

    fn observe_connected(
        &self,
        observation: TpConnectedObservation,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.observe_connected_inner(observation).await })
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
