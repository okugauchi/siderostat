//! Dry-run cluster lifecycle implementations.
//!
//! These simulate the DS4 child process lifecycles so that the Siderostat clustering
//! (state machine, control plane, discovery, pairing, promotion, demotion, recovery) can be
//! exercised without spawning or stopping a real ds4-server process. They are injected into
//! `ProductionClusterRuntime` via a dry-run constructor; the clustering implementation itself
//! is unchanged.

use super::{
    ChildIdentity, DistributedCoordinatorLifecycle, DistributedWorkerLifecycle, Ds4Hello,
    build_hello_frame,
};
use futures::future::BoxFuture;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// A probe that reports whether the simulated distributed route is currently complete.
///
/// Production observes the route from the coordinator DS4 log. Dry-run has no real DS4 log, so
/// the coordinator dry-run lifecycle consults the control plane instead: the route is complete
/// while the worker control phase is `WorkerReady` and the peer lease is present.
pub(crate) trait DryRunRouteProbe: Send + Sync + 'static {
    fn probe(&self) -> BoxFuture<'static, bool>;
}

#[derive(Clone)]
pub(crate) struct DryRunCoordinatorLifecycle {
    running: Arc<AtomicBool>,
    probe: Arc<dyn DryRunRouteProbe>,
    poll_interval: Duration,
}

impl DryRunCoordinatorLifecycle {
    pub(crate) fn new(probe: Arc<dyn DryRunRouteProbe>) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            probe,
            poll_interval: Duration::from_millis(50),
        }
    }
}

impl DistributedCoordinatorLifecycle for DryRunCoordinatorLifecycle {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        Box::pin(async move {
            running.store(true, Ordering::Release);
            tracing::info!(
                event = "dry-run-coordinator-start",
                generation,
                "simulated distributed coordinator start"
            );
            Ok(())
        })
    }

    fn wait_ready(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let probe = self.probe.clone();
        let poll_interval = self.poll_interval;
        Box::pin(async move {
            loop {
                if !running.load(Ordering::Acquire) {
                    anyhow::bail!("simulated coordinator exited before ready");
                }
                if probe.probe().await {
                    return Ok(());
                }
                tokio::time::sleep(poll_interval).await;
            }
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        Box::pin(async move {
            running.store(false, Ordering::Release);
            tracing::info!("simulated distributed coordinator stop");
            Ok(())
        })
    }

    fn wait_route_loss(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let probe = self.probe.clone();
        let poll_interval = self.poll_interval;
        Box::pin(async move {
            loop {
                if !running.load(Ordering::Acquire) {
                    return Ok(());
                }
                if !probe.probe().await {
                    return Ok(());
                }
                tokio::time::sleep(poll_interval).await;
            }
        })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::Acquire)) })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

/// Parameters a dry-run worker needs to simulate the distributed DS4 HELLO to the coordinator's
/// rendezvous listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DryRunHello {
    pub coordinator_address: IpAddr,
    pub worker_address: IpAddr,
    pub distributed_port: u16,
    pub layer_start: u32,
    pub context_size: u32,
    pub model_name: String,
    pub listen_port: u16,
}

#[derive(Clone)]
pub(crate) struct DryRunWorkerLifecycle {
    running: Arc<AtomicBool>,
    hello: DryRunHello,
}

impl DryRunWorkerLifecycle {
    pub(crate) fn new(hello: DryRunHello) -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            hello,
        }
    }
}

impl DistributedWorkerLifecycle for DryRunWorkerLifecycle {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let hello = self.hello.clone();
        Box::pin(async move {
            running.store(true, Ordering::Release);
            let task_hello = hello.clone();
            tokio::spawn(async move {
                if let Err(error) = send_dry_run_hello(&task_hello).await {
                    tracing::warn!(
                        error = %error,
                        generation,
                        "dry-run worker HELLO simulation failed"
                    );
                }
            });
            tracing::info!(
                event = "dry-run-worker-start",
                generation,
                "simulated distributed worker start"
            );
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        Box::pin(async move {
            running.store(false, Ordering::Release);
            tracing::info!("simulated distributed worker stop");
            Ok(())
        })
    }

    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::Acquire)) })
    }

    fn child_identity(&self) -> BoxFuture<'static, Option<ChildIdentity>> {
        Box::pin(async { None })
    }
}

/// Open a source-bound connection to the coordinator's rendezvous listener and send a simulated
/// DS4 HELLO. The source is bound to the worker address so the rendezvous `WrongSource` check
/// accepts it, exactly as a real worker DS4 would connect.
async fn send_dry_run_hello(hello: &DryRunHello) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind(SocketAddr::new(hello.worker_address, 0))?;
    let mut stream = socket
        .connect(SocketAddr::new(
            hello.coordinator_address,
            hello.distributed_port,
        ))
        .await?;
    // Simulated model shape: the coordinator's `WorkerHelloExpectation` is strict only about
    // `layer_start` (derived from config) and the output-end relation `layer_end + 1 ==
    // layer_count`. The concrete 42/43 values below are therefore arbitrary simulation
    // constants, independent of the real model's layer count; they are kept stable so the
    // rendezvous output-end validation always passes.
    let frame = build_hello_frame(&Ds4Hello {
        model_id: 1,
        quant_bits: 2,
        layer_start: hello.layer_start,
        layer_end: 42,
        has_output: true,
        has_hidden: true,
        context_size: hello.context_size,
        layer_count: 43,
        listen_port: hello.listen_port,
        model_name: hello.model_name.clone(),
    });
    stream.write_all(&frame).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A probe whose result is controlled by a shared flag, so tests can drive
    /// wait_ready / wait_route_loss deterministically.
    #[derive(Clone)]
    struct FlagProbe {
        value: Arc<AtomicBool>,
    }

    impl FlagProbe {
        fn new(value: bool) -> Self {
            Self {
                value: Arc::new(AtomicBool::new(value)),
            }
        }
    }

    impl DryRunRouteProbe for FlagProbe {
        fn probe(&self) -> BoxFuture<'static, bool> {
            let value = self.value.clone();
            Box::pin(async move { value.load(Ordering::Acquire) })
        }
    }

    #[tokio::test]
    async fn dry_run_coordinator_tracks_the_route_probe() {
        let probe = FlagProbe::new(false);
        let coordinator = DryRunCoordinatorLifecycle::new(Arc::new(probe.clone()));
        assert!(!coordinator.is_running().await.unwrap());

        coordinator.start(7).await.unwrap();
        assert!(coordinator.is_running().await.unwrap());

        // Route not complete yet: wait_ready must not resolve.
        let ready = coordinator.wait_ready();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!futures::poll!(Box::pin(ready)).is_ready());

        // Route becomes complete -> wait_ready resolves.
        probe.value.store(true, Ordering::Release);
        tokio::time::timeout(std::time::Duration::from_secs(1), coordinator.wait_ready())
            .await
            .expect("wait_ready should resolve once the route probe reports true")
            .unwrap();

        // Route loss: wait_route_loss resolves when the probe goes false.
        probe.value.store(false, Ordering::Release);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            coordinator.wait_route_loss(),
        )
        .await
        .expect("wait_route_loss should resolve once the route probe reports false")
        .unwrap();

        coordinator.stop().await.unwrap();
        assert!(!coordinator.is_running().await.unwrap());
        assert!(coordinator.child_identity().await.is_none());
    }

    #[tokio::test]
    async fn dry_run_coordinator_wait_ready_errors_after_stop() {
        let coordinator = DryRunCoordinatorLifecycle::new(Arc::new(FlagProbe::new(false)));
        coordinator.start(1).await.unwrap();
        coordinator.stop().await.unwrap();
        assert!(coordinator.wait_ready().await.is_err());
    }

    #[tokio::test]
    async fn dry_run_worker_start_stop_toggles_running() {
        let worker = DryRunWorkerLifecycle::new(DryRunHello {
            coordinator_address: IpAddr::from([10, 99, 0, 1]),
            worker_address: IpAddr::from([10, 99, 0, 2]),
            distributed_port: 9911,
            layer_start: 20,
            context_size: 262_144,
            model_name: "deepseek-v4-flash".into(),
            listen_port: 9911,
        });
        assert!(!worker.is_running().await.unwrap());
        worker.start(5).await.unwrap();
        assert!(worker.is_running().await.unwrap());
        worker.stop().await.unwrap();
        assert!(!worker.is_running().await.unwrap());
        assert!(worker.child_identity().await.is_none());
    }

    #[test]
    fn dry_run_hello_builds_a_frame_the_rendezvous_parser_accepts() {
        let hello = DryRunHello {
            coordinator_address: IpAddr::from([10, 99, 0, 1]),
            worker_address: IpAddr::from([10, 99, 0, 2]),
            distributed_port: 9911,
            layer_start: 20,
            context_size: 262_144,
            model_name: "deepseek-v4-flash".into(),
            listen_port: 9911,
        };
        let frame = build_hello_frame(&super::Ds4Hello {
            model_id: 1,
            quant_bits: 2,
            layer_start: hello.layer_start,
            layer_end: 42,
            has_output: true,
            has_hidden: true,
            context_size: hello.context_size,
            layer_count: 43,
            listen_port: hello.listen_port,
            model_name: hello.model_name.clone(),
        });
        let parsed = crate::cluster::parse_hello_frame(&frame).unwrap();
        assert_eq!(parsed.layer_start, 20);
        assert_eq!(parsed.layer_end, 42);
        assert_eq!(parsed.layer_count, 43);
        assert!(parsed.has_output);
        assert_eq!(parsed.context_size, 262_144);
        assert_eq!(parsed.model_name, "deepseek-v4-flash");
    }
}
