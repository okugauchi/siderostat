use super::{
    ClusterEvent, ClusterEventKind, ClusterHandle, ClusterSnapshot, CoordinatorControl, PeerLease,
    TransitionError, WorkerControl, spawn_state_machine,
};
use crate::{
    admission::DrainError,
    proxy::ModeAwareProxyState,
    target::{ClusterState, LocalRole, ProxyTarget},
};
use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};
use thiserror::Error;

pub trait LocalStandaloneLifecycle: Send + Sync + 'static {
    fn start(&self, generation: u64) -> BoxFuture<'static, anyhow::Result<()>>;
    fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>>;
    fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>>;
}

pub trait RuntimePeerControl {
    fn peer_lease(&self) -> &PeerLease;
    fn advance_generation(&mut self, generation: u64);
}

impl RuntimePeerControl for CoordinatorControl {
    fn peer_lease(&self) -> &PeerLease {
        self.peer_lease()
    }

    fn advance_generation(&mut self, generation: u64) {
        self.advance_generation(generation);
    }
}

impl RuntimePeerControl for WorkerControl {
    fn peer_lease(&self) -> &PeerLease {
        self.peer_lease()
    }

    fn advance_generation(&mut self, generation: u64) {
        self.advance_generation(generation);
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error(transparent)]
    Drain(#[from] DrainError),
    #[error("local standalone lifecycle failed: {0}")]
    LocalLifecycle(#[source] anyhow::Error),
}

pub struct ModeRuntime {
    role: LocalRole,
    cluster: ClusterHandle,
    state_task: tokio::task::JoinHandle<()>,
    proxy: Arc<ModeAwareProxyState>,
    local: Arc<dyn LocalStandaloneLifecycle>,
    drain_timeout: Duration,
}

impl ModeRuntime {
    pub async fn spawn_ready(
        role: LocalRole,
        proxy: Arc<ModeAwareProxyState>,
        local: Arc<dyn LocalStandaloneLifecycle>,
        drain_timeout: Duration,
    ) -> Result<Self, RuntimeError> {
        Self::spawn_ready_at(role, proxy, local, drain_timeout, 0).await
    }

    pub async fn spawn_ready_at(
        role: LocalRole,
        proxy: Arc<ModeAwareProxyState>,
        local: Arc<dyn LocalStandaloneLifecycle>,
        drain_timeout: Duration,
        baseline_generation: u64,
    ) -> Result<Self, RuntimeError> {
        let (cluster, state_task) =
            spawn_state_machine(ClusterSnapshot::booting_at(role, baseline_generation), 16);
        let startup = async {
            let starting = cluster
                .apply(ClusterEvent {
                    expected_generation: baseline_generation,
                    kind: ClusterEventKind::BeginSoloStandalone,
                })
                .await?;
            local
                .start(starting.generation)
                .await
                .map_err(RuntimeError::LocalLifecycle)?;
            cluster
                .apply(ClusterEvent {
                    expected_generation: starting.generation,
                    kind: ClusterEventKind::LocalStandaloneReady,
                })
                .await
                .map_err(RuntimeError::Transition)
        }
        .await;
        let ready = match startup {
            Ok(ready) => ready,
            Err(error) => {
                let _ = local.stop().await;
                state_task.abort();
                return Err(error);
            }
        };
        apply_proxy_snapshot(&proxy, ready);
        proxy.admission().start_serving();
        Ok(Self {
            role,
            cluster,
            state_task,
            proxy,
            local,
            drain_timeout,
        })
    }

    pub async fn spawn_manual_at(
        role: LocalRole,
        proxy: Arc<ModeAwareProxyState>,
        local: Arc<dyn LocalStandaloneLifecycle>,
        drain_timeout: Duration,
        baseline_generation: u64,
    ) -> Result<Self, RuntimeError> {
        let (cluster, state_task) =
            spawn_state_machine(ClusterSnapshot::booting_at(role, baseline_generation), 16);
        let manual = match cluster
            .apply(ClusterEvent {
                expected_generation: baseline_generation,
                kind: ClusterEventKind::RequireManualIntervention,
            })
            .await
        {
            Ok(manual) => manual,
            Err(error) => {
                state_task.abort();
                return Err(error.into());
            }
        };
        apply_proxy_snapshot(&proxy, manual);
        proxy.admission().block();
        Ok(Self {
            role,
            cluster,
            state_task,
            proxy,
            local,
            drain_timeout,
        })
    }

    pub fn snapshot(&self) -> ClusterSnapshot {
        self.cluster.snapshot()
    }

    pub fn cluster_handle(&self) -> ClusterHandle {
        self.cluster.clone()
    }

    pub async fn reconcile_peer<C: RuntimePeerControl>(
        &self,
        control: &mut C,
        now_millis: u64,
    ) -> Result<ClusterSnapshot, RuntimeError> {
        let current = self.cluster.snapshot();
        if self.role == LocalRole::Unknown {
            return Ok(current);
        }
        let peer_present = control.peer_lease().peer_present(now_millis);
        let next = match (current.state, peer_present) {
            (ClusterState::SoloStandaloneReady, true) => self.form_pair(current).await,
            (
                ClusterState::Pairing
                | ClusterState::PairedStandaloneReady
                | ClusterState::AwaitingWorkerHello
                | ClusterState::Promoting
                | ClusterState::DistributedStarting
                | ClusterState::DistributedReady
                | ClusterState::Demoting,
                false,
            ) => self.fallback_to_solo(current).await,
            _ => Ok(current),
        }?;
        Ok(next)
    }

    pub async fn reconcile_local(&self) -> Result<ClusterSnapshot, RuntimeError> {
        let current = self.cluster.snapshot();
        if current.state != ClusterState::SoloStandaloneReady
            || self
                .local
                .is_running()
                .await
                .map_err(RuntimeError::LocalLifecycle)?
        {
            return Ok(current);
        }
        self.proxy.admission().block();
        self.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            false,
        );
        let starting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::LocalStandaloneLost,
            })
            .await?;
        self.local
            .start(starting.generation)
            .await
            .map_err(RuntimeError::LocalLifecycle)?;
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::LocalStandaloneReady,
            })
            .await?;
        apply_proxy_snapshot(&self.proxy, ready);
        self.proxy.admission().start_serving();
        Ok(ready)
    }

    async fn form_pair(&self, current: ClusterSnapshot) -> Result<ClusterSnapshot, RuntimeError> {
        let pairing = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::BeginPairing,
            })
            .await?;
        if self.role == LocalRole::Worker {
            self.proxy
                .admission()
                .drain(pairing.generation, self.drain_timeout)
                .await?;
            self.local
                .stop()
                .await
                .map_err(RuntimeError::LocalLifecycle)?;
        } else {
            self.proxy.admission().block();
        }
        let paired = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: pairing.generation,
                kind: ClusterEventKind::PairingReady,
            })
            .await?;
        apply_proxy_snapshot(&self.proxy, paired);
        self.proxy.admission().start_serving();
        Ok(paired)
    }

    async fn fallback_to_solo(
        &self,
        current: ClusterSnapshot,
    ) -> Result<ClusterSnapshot, RuntimeError> {
        self.proxy.admission().block();
        self.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            false,
        );
        let starting = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: current.generation,
                kind: ClusterEventKind::PeerLost,
            })
            .await?;
        if self.role == LocalRole::Worker {
            self.local
                .start(starting.generation)
                .await
                .map_err(RuntimeError::LocalLifecycle)?;
        }
        let ready = self
            .cluster
            .apply(ClusterEvent {
                expected_generation: starting.generation,
                kind: ClusterEventKind::LocalStandaloneReady,
            })
            .await?;
        apply_proxy_snapshot(&self.proxy, ready);
        self.proxy.admission().start_serving();
        Ok(ready)
    }
}

impl Drop for ModeRuntime {
    fn drop(&mut self) {
        self.state_task.abort();
    }
}

fn apply_proxy_snapshot(proxy: &ModeAwareProxyState, snapshot: ClusterSnapshot) {
    let ready = !matches!(snapshot.target, ProxyTarget::Unavailable { .. });
    proxy.set_target(snapshot.target, ready);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster::{
            AuthenticatedPeer, ControlCommand, ControlEndpoint, ControlMessage, ControlMode,
            ControlRole, NodeDescriptor, WorkerControl,
        },
        proxy::{ModeAwareProxyOptions, ModeAwareTargetSnapshot},
        target::{StableMode, UnavailableReason},
    };
    use std::{
        net::IpAddr,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Lifecycle {
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        running: Arc<AtomicBool>,
    }

    impl LocalStandaloneLifecycle for Lifecycle {
        fn start(&self, _generation: u64) -> BoxFuture<'static, anyhow::Result<()>> {
            let starts = self.starts.clone();
            let running = self.running.clone();
            Box::pin(async move {
                starts.fetch_add(1, Ordering::Relaxed);
                running.store(true, Ordering::Relaxed);
                Ok(())
            })
        }

        fn stop(&self) -> BoxFuture<'static, anyhow::Result<()>> {
            let stops = self.stops.clone();
            let running = self.running.clone();
            Box::pin(async move {
                stops.fetch_add(1, Ordering::Relaxed);
                running.store(false, Ordering::Relaxed);
                Ok(())
            })
        }

        fn is_running(&self) -> BoxFuture<'static, anyhow::Result<bool>> {
            let running = self.running.clone();
            Box::pin(async move { Ok(running.load(Ordering::Relaxed)) })
        }
    }

    fn descriptor(role: ControlRole, node_id: &str) -> NodeDescriptor {
        NodeDescriptor {
            protocol_version: 1,
            node_id: node_id.into(),
            role,
            generation: 2,
            mode: ControlMode::SoloStandalone,
            deployment_id: None,
        }
    }

    fn authenticated() -> AuthenticatedPeer {
        AuthenticatedPeer::new_for_test("coordinator", IpAddr::from([10, 99, 0, 1]), 1_000)
    }

    fn pair(request_id: &str) -> ControlMessage {
        ControlMessage {
            request_id: request_id.into(),
            generation: 2,
            deployment_id: None,
            command: ControlCommand::Pair {
                descriptor: descriptor(ControlRole::Coordinator, "coordinator"),
            },
        }
    }

    fn proxy() -> Arc<ModeAwareProxyState> {
        Arc::new(
            ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:8000").unwrap(),
                url::Url::parse("http://10.99.0.1:18082").unwrap(),
                ModeAwareProxyOptions {
                    max_in_flight: 4,
                    request_body_limit_bytes: 4096,
                    response_header_timeout: Duration::from_secs(1),
                    first_body_byte_timeout: Duration::from_secs(1),
                    stream_idle_timeout: Duration::from_secs(1),
                    connect_timeout: Duration::from_secs(1),
                },
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn worker_converges_solo_paired_solo_and_reconnects() {
        let proxy = proxy();
        let lifecycle = Arc::new(Lifecycle::default());
        let runtime = ModeRuntime::spawn_ready(
            LocalRole::Worker,
            proxy.clone(),
            lifecycle.clone(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut control = WorkerControl::new(
            descriptor(ControlRole::Worker, "worker"),
            Duration::from_millis(150),
            Duration::from_millis(20),
        )
        .unwrap();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-1"),
                &authenticated(),
                true,
                1_000,
            )
            .unwrap();

        assert_eq!(
            runtime
                .reconcile_peer(&mut control, 1_019)
                .await
                .unwrap()
                .stable_mode,
            StableMode::SoloStandalone
        );
        let permit = proxy.admission().try_acquire(true).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(permit);
        });
        let paired = runtime.reconcile_peer(&mut control, 1_020).await.unwrap();
        assert_eq!(paired.stable_mode, StableMode::PairedStandalone);
        assert_eq!(
            proxy.target_snapshot(),
            ModeAwareTargetSnapshot {
                target: ProxyTarget::Coordinator,
                ready: true,
            }
        );
        assert_eq!(lifecycle.stops.load(Ordering::Relaxed), 1);

        // Bonjour result loss alone does not alter the authenticated lease.
        assert_eq!(
            runtime
                .reconcile_peer(&mut control, 1_100)
                .await
                .unwrap()
                .stable_mode,
            StableMode::PairedStandalone
        );

        let solo = runtime.reconcile_peer(&mut control, 1_150).await.unwrap();
        assert_eq!(solo.stable_mode, StableMode::SoloStandalone);
        assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
        assert_eq!(lifecycle.starts.load(Ordering::Relaxed), 2);

        control
            .handle(
                ControlEndpoint::Pair,
                ControlMessage {
                    generation: 6,
                    command: ControlCommand::Pair {
                        descriptor: NodeDescriptor {
                            generation: 6,
                            ..descriptor(ControlRole::Coordinator, "coordinator")
                        },
                    },
                    ..pair("pair-2")
                },
                &authenticated(),
                true,
                1_200,
            )
            .unwrap();
        assert_eq!(
            runtime
                .reconcile_peer(&mut control, 1_219)
                .await
                .unwrap()
                .stable_mode,
            StableMode::SoloStandalone
        );
        assert_eq!(
            runtime
                .reconcile_peer(&mut control, 1_220)
                .await
                .unwrap()
                .stable_mode,
            StableMode::PairedStandalone
        );
        assert_eq!(lifecycle.stops.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn route_loss_blocks_future_admission_before_fallback() {
        let proxy = proxy();
        let runtime = ModeRuntime::spawn_ready(
            LocalRole::Worker,
            proxy.clone(),
            Arc::new(Lifecycle::default()),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let mut control = WorkerControl::new(
            descriptor(ControlRole::Worker, "worker"),
            Duration::from_secs(15),
            Duration::ZERO,
        )
        .unwrap();
        control
            .handle(
                ControlEndpoint::Pair,
                pair("pair-route"),
                &authenticated(),
                true,
                2_000,
            )
            .unwrap();
        runtime.reconcile_peer(&mut control, 2_000).await.unwrap();
        control.invalidate_route();
        let snapshot = runtime.reconcile_peer(&mut control, 2_001).await.unwrap();
        assert_eq!(snapshot.stable_mode, StableMode::SoloStandalone);
        assert!(!matches!(
            proxy.target_snapshot().target,
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition
            }
        ));
    }

    #[tokio::test]
    async fn local_crash_blocks_admission_until_restart_is_ready() {
        let proxy = proxy();
        let lifecycle = Arc::new(Lifecycle::default());
        let runtime = ModeRuntime::spawn_ready(
            LocalRole::Coordinator,
            proxy.clone(),
            lifecycle.clone(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(lifecycle.starts.load(Ordering::Relaxed), 1);
        lifecycle.running.store(false, Ordering::Relaxed);

        let recovered = runtime.reconcile_local().await.unwrap();
        assert_eq!(recovered.state, ClusterState::SoloStandaloneReady);
        assert_eq!(lifecycle.starts.load(Ordering::Relaxed), 2);
        assert_eq!(proxy.target_snapshot().target, ProxyTarget::LocalStandalone);
        assert_eq!(
            proxy.admission().snapshot().state,
            crate::admission::AdmissionState::Serving
        );
    }
}
