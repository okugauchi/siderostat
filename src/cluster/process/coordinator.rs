use super::{ChildIdentity, ManagedChild, ProcessControlError, SupervisedChild};
use crate::cluster::{
    DistributedCoordinatorLifecycle, Ds4Command, Ds4LogEvent,
    spawn_child_log_forwarders_with_events,
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use url::Url;

#[derive(Clone)]
pub struct DistributedCoordinatorSupervisor {
    inner: Arc<DistributedCoordinatorSupervisorInner>,
}

struct DistributedCoordinatorSupervisorInner {
    command: Ds4Command,
    models_url: Url,
    client: reqwest::Client,
    http_startup_timeout: Duration,
    poll_interval: Duration,
    stop_timeout: Duration,
    allow_sigkill: bool,
    child: tokio::sync::Mutex<Option<SupervisedChild>>,
    route: tokio::sync::watch::Sender<CoordinatorRouteState>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CoordinatorRouteState {
    http_ready: bool,
    worker_registered: bool,
    complete_route: bool,
}

impl DistributedCoordinatorSupervisor {
    pub fn new(
        command: Ds4Command,
        models_url: Url,
        http_startup_timeout: Duration,
        poll_interval: Duration,
        stop_timeout: Duration,
        allow_sigkill: bool,
    ) -> Self {
        let (route, _) = tokio::sync::watch::channel(CoordinatorRouteState::default());
        Self {
            inner: Arc::new(DistributedCoordinatorSupervisorInner {
                command,
                models_url,
                client: reqwest::Client::new(),
                http_startup_timeout,
                poll_interval,
                stop_timeout,
                allow_sigkill,
                child: tokio::sync::Mutex::new(None),
                route,
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
        self.inner
            .route
            .send_replace(CoordinatorRouteState::default());
        let mut child = ManagedChild::spawn(&self.inner.command, generation).await?;
        let stdout = child
            .take_stdout()
            .ok_or(ProcessControlError::MissingLogPipe)?;
        let stderr = child
            .take_stderr()
            .ok_or(ProcessControlError::MissingLogPipe)?;
        let (mut logs, mut events, forwarders) = spawn_child_log_forwarders_with_events(
            stdout,
            stderr,
            Arc::from(child.identity().profile_id.as_str()),
            child.identity().generation,
            child.identity().pid,
            256,
        );
        let route = self.inner.route.clone();
        let log_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(event) = events.recv() => {
                        let mut state = *route.borrow();
                        match event {
                            Ds4LogEvent::WorkerRegistered { .. } => state.worker_registered = true,
                            Ds4LogEvent::CompleteRouteReady { .. } => state.complete_route = true,
                            Ds4LogEvent::WorkerRemoved { .. } => {
                                state.worker_registered = false;
                                state.complete_route = false;
                            }
                            Ds4LogEvent::RouteIncomplete { .. } => state.complete_route = false,
                            Ds4LogEvent::HttpListening { .. } | Ds4LogEvent::DsparkActivated => {}
                        }
                        route.send_replace(state);
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
                            "DS4 distributed coordinator log"
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
                self.inner.http_startup_timeout,
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
        let mut state = *self.inner.route.borrow();
        state.http_ready = true;
        self.inner.route.send_replace(state);
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
            self.inner
                .route
                .send_replace(CoordinatorRouteState::default());
            return Ok(());
        };
        let result = current
            .child
            .stop(self.inner.stop_timeout, self.inner.allow_sigkill)
            .await;
        current.log_task.abort();
        self.inner
            .route
            .send_replace(CoordinatorRouteState::default());
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

    async fn wait_ready_inner(&self) -> anyhow::Result<()> {
        let mut route = self.inner.route.subscribe();
        loop {
            let state = *route.borrow_and_update();
            if state.http_ready && state.worker_registered && state.complete_route {
                return Ok(());
            }
            if !self.is_running_inner().await? {
                anyhow::bail!("distributed coordinator exited before complete route");
            }
            tokio::select! {
                result = route.changed() => result.map_err(|_| anyhow::anyhow!("route monitor stopped"))?,
                () = tokio::time::sleep(self.inner.poll_interval) => {}
            }
        }
    }

    async fn wait_route_loss_inner(&self) -> anyhow::Result<()> {
        let mut route = self.inner.route.subscribe();
        loop {
            let state = *route.borrow_and_update();
            if !state.http_ready || !state.worker_registered || !state.complete_route {
                return Ok(());
            }
            if !self.is_running_inner().await? {
                return Ok(());
            }
            tokio::select! {
                result = route.changed() => result.map_err(|_| anyhow::anyhow!("route monitor stopped"))?,
                () = tokio::time::sleep(self.inner.poll_interval) => {}
            }
        }
    }
}

impl DistributedCoordinatorLifecycle for DistributedCoordinatorSupervisor {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.start_inner(generation).await })
    }

    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.wait_ready_inner().await })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.stop_inner().await })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let supervisor = self.clone();
        Box::pin(async move { supervisor.wait_route_loss_inner().await })
    }
}
