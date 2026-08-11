use super::{ChildIdentity, ManagedChild, SupervisedChild};
use crate::cluster::{DistributedWorkerLifecycle, Ds4Command};
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
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
}

impl DistributedWorkerSupervisor {
    pub fn new(command: Ds4Command, stop_timeout: Duration, allow_sigkill: bool) -> Self {
        Self {
            inner: Arc::new(DistributedWorkerSupervisorInner {
                command,
                stop_timeout,
                allow_sigkill,
                child: tokio::sync::Mutex::new(None),
            }),
        }
    }

    pub async fn child_identity(&self) -> Option<ChildIdentity> {
        self.inner
            .child
            .lock()
            .await
            .as_ref()
            .map(|current| current.child.identity().clone())
    }

    #[cfg(target_os = "macos")]
    async fn start_inner(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.inner.child.lock().await;
        if let Some(current) = slot.as_mut()
            && current.child.try_wait()?.is_none()
        {
            return Ok(());
        }
        if let Some(stale) = slot.take() {
            stale.log_task.abort();
        }
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
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
        let mut slot = self.inner.child.lock().await;
        let Some(mut current) = slot.take() else {
            return Ok(());
        };
        let result = current
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await;
        current.log_task.abort();
        result?;
        Ok(())
    }

    async fn is_running_inner(&self) -> anyhow::Result<bool> {
        let mut slot = self.inner.child.lock().await;
        let Some(current) = slot.as_mut() else {
            return Ok(false);
        };
        Ok(current.child.try_wait()?.is_none())
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
}
