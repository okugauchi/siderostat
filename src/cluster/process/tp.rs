//! Shared owned-child supervision for the two TP roles. No TP socket probes.
use super::{ChildIdentity, Ds4Command, SupervisedSlot};
#[cfg(target_os = "macos")]
use super::{ManagedChild, SupervisedChild};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Default, Clone)]
pub struct TpLogReadiness {
    peer: bool,
    bound: bool,
    failed: bool,
}
impl TpLogReadiness {
    pub fn observe(&mut self, line: &str) {
        if line.contains("TP gate exchange failed")
            || line.contains("tp hello exchange failed")
            || line.contains("tp: prefill synchronization failed")
            || line.contains("big gate payload exchange failed")
        {
            self.failed = true;
        }
        if line.starts_with("ds4-tp: worker connected, transport=rdma ")
            || line.starts_with("ds4-tp: leader connected, transport=rdma ")
        {
            self.peer = true;
        }
        // Upstream ds4_log writes this marker without a newline. The next
        // stderr message can therefore share the same forwarded record.
        if [0, 1].into_iter().any(|rank| {
            line.contains(&format!(
                "tensor parallelism bound: rank {rank}, 50/50 expert ownership, rdma transport"
            ))
        }) {
            self.bound = true;
        }
    }
    pub fn connected(&self) -> bool {
        self.peer && self.bound && !self.failed
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TpLocalStatus {
    pub running: bool,
    pub connected: bool,
    pub http_ready: bool,
}

pub trait TpLocalLifecycle: Send + Sync + 'static {
    fn preflight(&self, peer: Option<std::net::IpAddr>) -> BoxFuture<'static, anyhow::Result<()>>;
    fn start(
        &self,
        session: u64,
        coordinator: std::net::IpAddr,
    ) -> BoxFuture<'static, anyhow::Result<()>>;
    fn status(&self, session: u64) -> BoxFuture<'static, anyhow::Result<TpLocalStatus>>;
    fn warmup(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

#[derive(Clone)]
pub struct TpProcessSupervisor {
    inner: Arc<TpProcessInner>,
}
struct TpProcessInner {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    command: Ds4Command,
    child: SupervisedSlot,
    observed: Arc<Mutex<(u64, TpLogReadiness)>>,
    config: crate::config::ModeAwareConfig,
    coordinator: bool,
}
impl TpProcessSupervisor {
    pub fn new(
        command: Ds4Command,
        config: crate::config::ModeAwareConfig,
        coordinator: bool,
    ) -> Self {
        Self {
            inner: Arc::new(TpProcessInner {
                command,
                child: SupervisedSlot::new(),
                observed: Arc::new(Mutex::new((0, TpLogReadiness::default()))),
                config,
                coordinator,
            }),
        }
    }
    #[cfg(target_os = "macos")]
    async fn start_inner(&self, session: u64, coordinator: std::net::IpAddr) -> anyhow::Result<()> {
        let mut slot = self
            .inner
            .child
            .begin_start()
            .await?
            .ok_or_else(|| anyhow::anyhow!("TP child already running"))?;
        *self
            .inner
            .observed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (session, TpLogReadiness::default());
        let mut command = self.inner.command.clone();
        if let Some(index) = command
            .argv
            .iter()
            .position(|a| a == "--coordinator" || a == "--listen")
        {
            command.argv[index + 1] = coordinator.to_string().into();
        }
        let mut child = ManagedChild::spawn(&command, session).await?;
        let (mut logs, forwarders) = child.start_log_forwarding(256)?;
        let observed = self.inner.observed.clone();
        let log_task = tokio::spawn(async move {
            while let Some(record) = logs.recv().await {
                let mut state = observed.lock().unwrap_or_else(|e| e.into_inner());
                if state.0 == session && record.generation == session && !record.truncated {
                    state.1.observe(&record.line);
                }
                // Deliberately do not log raw model output.
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
    async fn start_inner(
        &self,
        _session: u64,
        _coordinator: std::net::IpAddr,
    ) -> anyhow::Result<()> {
        anyhow::bail!("TP supervision requires macOS")
    }
    fn endpoint(&self, suffix: &str) -> String {
        format!(
            "http://{}:{}/v1/{suffix}",
            self.inner.config.ds4.http_host, self.inner.config.ds4.http_port
        )
    }
}
impl TpLocalLifecycle for TpProcessSupervisor {
    fn preflight(&self, peer: Option<std::net::IpAddr>) -> BoxFuture<'static, anyhow::Result<()>> {
        let config = self.inner.config.clone();
        Box::pin(async move {
            #[cfg(target_os = "macos")]
            {
                super::super::platform::rdma::probe_system(&config, peer).await
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (config, peer);
                anyhow::bail!("TP RDMA requires macOS")
            }
        })
    }
    fn start(
        &self,
        session: u64,
        coordinator: std::net::IpAddr,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        let this = self.clone();
        Box::pin(async move { this.start_inner(session, coordinator).await })
    }
    fn status(&self, session: u64) -> BoxFuture<'static, anyhow::Result<TpLocalStatus>> {
        let this = self.clone();
        Box::pin(async move {
            let connected = {
                let s = this
                    .inner
                    .observed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                s.0 == session && s.1.connected()
            };
            let running = this.inner.child.is_running().await?;
            if running {
                if let Some(identity) = this.inner.child.child_identity().await {
                    super::platform_process_controller().verify(&identity)?;
                } else {
                    anyhow::bail!("TP child identity unavailable");
                }
            }
            let http_ready = if this.inner.coordinator && running && connected {
                reqwest::Client::new()
                    .get(this.endpoint("models"))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
            } else {
                false
            };
            Ok(TpLocalStatus {
                running,
                connected: connected && running,
                http_ready,
            })
        })
    }
    fn warmup(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let this = self.clone();
        Box::pin(async move {
            anyhow::ensure!(this.inner.coordinator, "only the coordinator can warm up");
            let mut response = reqwest::Client::new()
                .get(this.endpoint("models"))
                .timeout(Duration::from_secs(2))
                .send()
                .await?
                .error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                anyhow::ensure!(
                    bytes.len() + chunk.len() <= 65536,
                    "TP model list too large"
                );
                bytes.extend(chunk);
            }
            let models: serde_json::Value = serde_json::from_slice(&bytes)?;
            let model = models["data"][0]["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("TP coordinator did not advertise a model"))?;
            let mut response = reqwest::Client::new().post(this.endpoint("chat/completions"))
                .timeout(Duration::from_secs(300)).json(&serde_json::json!({"model":model, "messages":[{"role":"user","content":"Reply OK."}],"max_tokens":1,"temperature":0,"stream":false}))
                .send().await?.error_for_status()?;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                anyhow::ensure!(
                    bytes.len() + chunk.len() <= 65536,
                    "TP warm-up response too large"
                );
                bytes.extend(chunk);
            }
            let body: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                body.get("error").is_none()
                    && body["choices"].as_array().is_some_and(|a| !a.is_empty())
                    && body["usage"]["completion_tokens"]
                        .as_u64()
                        .is_some_and(|n| n > 0),
                "TP warm-up did not complete generation"
            );
            Ok(())
        })
    }
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let this = self.clone();
        Box::pin(async move {
            self::invalidate(&this.inner.observed);
            this.inner
                .child
                .stop(
                    this.inner.config.cluster.timeouts.stop,
                    this.inner.config.ds4.allow_sigkill,
                )
                .await
        })
    }
    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        let this = self.clone();
        Box::pin(async move { this.inner.child.child_identity().await })
    }
}
fn invalidate(observed: &Mutex<(u64, TpLogReadiness)>) {
    observed.lock().unwrap_or_else(|e| e.into_inner()).1.failed = true;
}

/// Explicit dry-run boundary; only lifecycle observations are simulated.
#[derive(Default)]
pub(crate) struct SimulatedTp {
    session: std::sync::atomic::AtomicU64,
}
impl TpLocalLifecycle for SimulatedTp {
    fn preflight(&self, _: Option<std::net::IpAddr>) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn start(&self, session: u64, _: std::net::IpAddr) -> BoxFuture<'static, anyhow::Result<()>> {
        self.session
            .store(session, std::sync::atomic::Ordering::Release);
        Box::pin(async { Ok(()) })
    }
    fn status(&self, session: u64) -> BoxFuture<'static, anyhow::Result<TpLocalStatus>> {
        let running =
            self.session.load(std::sync::atomic::Ordering::Acquire) == session && session != 0;
        Box::pin(async move {
            Ok(TpLocalStatus {
                running,
                connected: running,
                http_ready: running,
            })
        })
    }
    fn warmup(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        self.session.store(0, std::sync::atomic::Ordering::Release);
        Box::pin(async { Ok(()) })
    }
}
