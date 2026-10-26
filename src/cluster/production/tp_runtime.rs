//! Real TP orchestration; the existing reducer is the sole readiness state machine.
use super::tp_control::{TpCommand, TpRequest, TpResponse};
use super::*;
use crate::cluster::{TpLocalLifecycle, TpLocalStatus, TpSessionId};
use crate::target::{ProxyTarget, UnavailableReason};

#[derive(Clone, Copy)]
struct Session {
    id: u64,
    operation: uuid::Uuid,
    epoch: u64,
}
pub(super) struct TpRuntime {
    pub child: Arc<dyn TpLocalLifecycle>,
    pub effects: Mutex<()>,
    session: std::sync::Mutex<Option<Session>>,
    completed: std::sync::Mutex<Option<Session>>,
    pub journal: std::sync::Mutex<crate::cluster::state_store::PersistentTpState>,
    retry_after: AtomicU64,
    startup_deadline: std::sync::Mutex<Option<tokio::time::Instant>>,
}
impl TpRuntime {
    pub fn new(child: Arc<dyn TpLocalLifecycle>) -> Self {
        Self {
            child,
            effects: Mutex::new(()),
            session: std::sync::Mutex::new(None),
            completed: std::sync::Mutex::new(None),
            journal: std::sync::Mutex::new(Default::default()),
            retry_after: AtomicU64::new(0),
            startup_deadline: std::sync::Mutex::new(None),
        }
    }
    pub(super) fn session_id(&self) -> Option<u64> {
        self.session().map(|s| s.id)
    }
    fn session(&self) -> Option<Session> {
        *self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl ProductionClusterRuntime {
    pub(super) fn tp_runtime(&self) -> anyhow::Result<&TpRuntime> {
        self.inner.tp.as_ref().context("TP runtime unavailable")
    }
    fn tp_address(&self) -> anyhow::Result<IpAddr> {
        self.inner
            .config
            .ds4
            .distributed
            .rdma_address
            .as_deref()
            .context("TP rdma_address missing")?
            .parse()
            .map_err(Into::into)
    }
    fn tp_request(
        &self,
        command: TpCommand,
        session: Option<Session>,
    ) -> anyhow::Result<TpRequest> {
        let s = session.unwrap_or(Session {
            id: 0,
            operation: uuid::Uuid::new_v4(),
            epoch: self.policy_epoch(),
        });
        Ok(TpRequest {
            version: 1,
            deployment: self.inner.manifest.deployment_id()?,
            epoch: s.epoch,
            session: s.id,
            operation: s.operation,
            coordinator_address: self.tp_address()?,
            command,
        })
    }
    async fn tp_peer(&self, request: &TpRequest) -> anyhow::Result<TpResponse> {
        let reply = self.inner.client.tp(request).await?;
        self.inner.lease.expires_at.store(
            now_millis() + self.inner.config.cluster.timeouts.control_lease.as_millis() as u64,
            Ordering::Release,
        );
        Ok(reply)
    }
    fn tp_note_failure(&self, session: u64, reserved: bool) -> anyhow::Result<()> {
        let tp = self.tp_runtime()?;
        {
            let mut journal = tp.journal.lock().unwrap_or_else(|e| e.into_inner());
            if session > journal.last_failed_session {
                journal.last_failed_session = session;
                if !reserved {
                    journal.failed_attempts = journal.failed_attempts.saturating_add(1);
                }
            }
        }
        self.persist_tp_journal()
    }
    fn tp_policy_check(&self, epoch: u64) -> anyhow::Result<()> {
        ensure!(
            !self.planned_restart_active(),
            "TP start blocked by planned restart"
        );
        ensure!(
            self.operator_policy() == OperationPolicy::Automatic
                && !self.policy_pending()
                && self.policy_epoch() == epoch,
            "TP operation superseded by policy"
        );
        ensure!(
            self.inner.network.route_scoped(),
            "TP control route is not verified"
        );
        Ok(())
    }
    async fn tp_event(
        &self,
        kind: ClusterEventKind,
        session: u64,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let current = self.inner.mode.snapshot();
        let event = if matches!(
            kind,
            ClusterEventKind::BeginTensorParallel
                | ClusterEventKind::TensorParallelWorkerPrepared
                | ClusterEventKind::TensorParallelCoordinatorStarted
                | ClusterEventKind::TensorParallelHandshakeHttpReady
                | ClusterEventKind::TensorParallelWarmupDone
                | ClusterEventKind::BeginTensorParallelDemotion
        ) {
            ClusterEvent::tp(current.generation, kind, TpSessionId(session))
        } else {
            ClusterEvent::new(current.generation, kind)
        };
        Ok(self.inner.mode.cluster_handle().apply(event).await?)
    }
    async fn tp_pair_local(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        if self.inner.mode.snapshot().state == ClusterState::SoloStandaloneReady {
            self.tp_event(ClusterEventKind::BeginPairing, 0).await?;
            self.tp_event(ClusterEventKind::PairingReady, 0).await?;
        }
        ensure!(
            self.inner.mode.snapshot().state == ClusterState::PairedStandaloneReady,
            "TP pairing requires standalone readiness"
        );
        self.inner
            .proxy
            .set_target(self.inner.mode.snapshot().target, true);
        self.inner.proxy.admission().start_serving();
        Ok(self.inner.mode.snapshot())
    }
    pub(super) async fn tp_pair(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "only coordinator pairs TP"
        );
        let tp = self.tp_runtime()?;
        let _effect = tp.effects.lock().await;
        self.tp_policy_check(self.policy_epoch())?;
        let peer = self
            .tp_peer(&self.tp_request(TpCommand::Node, None)?)
            .await?;
        tp.child.preflight(Some(peer.address)).await?;
        self.tp_peer(&self.tp_request(TpCommand::Pair, None)?)
            .await?;
        self.tp_pair_local().await
    }
    pub(super) async fn handle_tp(&self, r: TpRequest) -> anyhow::Result<TpResponse> {
        let tp = self.tp_runtime()?;
        ensure!(
            r.version == 1 && r.deployment == self.inner.manifest.deployment_id()?,
            "TP deployment/protocol mismatch"
        );
        if r.command != TpCommand::Stop {
            self.tp_policy_check(r.epoch)?;
        }
        let status = match r.command {
            TpCommand::Node => {
                if tp.session().is_none() {
                    tp.child.preflight(None).await?;
                }
                if let Some(s) = tp.session() {
                    tp.child.status(s.id).await?
                } else {
                    TpLocalStatus::default()
                }
            }
            TpCommand::Pair => {
                let _effect = tp.effects.lock().await;
                self.tp_pair_local().await?;
                TpLocalStatus::default()
            }
            TpCommand::Prepare => {
                ensure!(
                    self.inner.role == LocalRole::Worker,
                    "TP prepare requires worker"
                );
                let _effect = tp.effects.lock().await;
                if let Some(current) = tp.session() {
                    ensure!(
                        current.id == r.session && current.operation == r.operation,
                        "another TP session is active"
                    );
                } else {
                    ensure!(
                        r.session
                            > tp.journal
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .last_session,
                        "stale TP prepare"
                    );
                    tp.child.preflight(Some(r.coordinator_address)).await?;
                    let s = Session {
                        id: r.session,
                        operation: r.operation,
                        epoch: r.epoch,
                    };
                    self.tp_begin(s).await?;
                    if let Err(error) = tp.child.start(s.id, r.coordinator_address).await {
                        self.tp_fallback_locked().await?;
                        return Err(error);
                    }
                    ensure!(
                        tp.child.status(s.id).await?.running,
                        "TP worker exited during prepare"
                    );
                    self.tp_event(ClusterEventKind::TensorParallelWorkerPrepared, s.id)
                        .await?;
                }
                tp.child.status(r.session).await?
            }
            TpCommand::Status | TpCommand::Ready => {
                let _effect = tp.effects.lock().await;
                self.tp_match(&r)?;
                *tp.startup_deadline
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(
                    tokio::time::Instant::now() + self.inner.config.cluster.timeouts.complete_route,
                );
                let status = tp.child.status(r.session).await?;
                if r.command == TpCommand::Ready {
                    ensure!(
                        status.running && status.connected,
                        "TP worker handshake is incomplete"
                    );
                    if self.inner.mode.snapshot().state != ClusterState::TensorParallelReady {
                        self.tp_event(
                            ClusterEventKind::TensorParallelCoordinatorStarted,
                            r.session,
                        )
                        .await?;
                        self.tp_event(
                            ClusterEventKind::TensorParallelHandshakeHttpReady,
                            r.session,
                        )
                        .await?;
                        self.tp_event(ClusterEventKind::TensorParallelWarmupDone, r.session)
                            .await?;
                    }
                    self.inner.proxy.set_target(ProxyTarget::Coordinator, true);
                    self.inner.proxy.admission().start_serving();
                }
                status
            }
            TpCommand::Stop => {
                let _effect = tp.effects.lock().await;
                if tp.session().is_none() {
                    let completed = *tp.completed.lock().unwrap_or_else(|e| e.into_inner());
                    ensure!(
                        completed.is_some_and(|s| s.id == r.session && s.operation == r.operation),
                        "stale TP stop"
                    );
                } else {
                    self.tp_match(&r)?;
                    self.tp_fallback_locked().await?;
                }
                TpLocalStatus::default()
            }
        };
        self.inner.lease.expires_at.store(
            now_millis() + self.inner.config.cluster.timeouts.control_lease.as_millis() as u64,
            Ordering::Release,
        );
        Ok(TpResponse {
            version: 1,
            deployment: r.deployment,
            epoch: r.epoch,
            session: if r.command == TpCommand::Node {
                tp.journal
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_session
            } else {
                r.session
            },
            operation: r.operation,
            address: self.tp_address()?,
            status,
        })
    }
    fn tp_match(&self, r: &TpRequest) -> anyhow::Result<()> {
        let s = self
            .tp_runtime()?
            .session()
            .context("no active TP session")?;
        ensure!(
            s.id == r.session && s.operation == r.operation && s.epoch == r.epoch,
            "stale TP session"
        );
        Ok(())
    }
    async fn tp_begin(&self, s: Session) -> anyhow::Result<()> {
        let tp = self.tp_runtime()?;
        self.tp_policy_check(s.epoch)?;
        self.inner
            .proxy
            .admission()
            .drain(
                self.inner.mode.snapshot().generation,
                self.inner.config.cluster.timeouts.drain,
            )
            .await?;
        self.inner.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            false,
        );
        tp.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_session = s.id;
        self.persist_tp_journal()?;
        *tp.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
        *tp.startup_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Some(tokio::time::Instant::now() + self.inner.config.cluster.timeouts.complete_route);
        self.tp_event(ClusterEventKind::BeginTensorParallel, s.id)
            .await?;
        self.inner.standalone.stop().await?;
        Ok(())
    }
    pub(super) async fn tp_promote(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let this = self.clone();
        tokio::spawn(async move { this.tp_promote_owned().await }).await?
    }
    async fn tp_promote_owned(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        ensure!(
            self.inner.role == LocalRole::Coordinator,
            "only coordinator promotes TP"
        );
        let tp = self.tp_runtime()?;
        let _effect = tp.effects.lock().await;
        self.tp_policy_check(self.policy_epoch())?;
        ensure!(
            self.inner.config.cluster.policy.auto_promote,
            "TP promotion is disabled"
        );
        ensure!(
            self.inner.mode.snapshot().state == ClusterState::PairedStandaloneReady,
            "TP requires paired standalone"
        );
        ensure!(
            tp.journal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failed_attempts
                < 3,
            "TP retry budget exhausted; explicit operator action required"
        );
        ensure!(
            now_millis() >= tp.retry_after.load(Ordering::Acquire),
            "TP promotion is in backoff"
        );
        let peer = self
            .tp_peer(&self.tp_request(TpCommand::Node, None)?)
            .await?;
        tp.child.preflight(Some(peer.address)).await?;
        let id = tp
            .journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_session
            .max(peer.session)
            .checked_add(1)
            .context("TP session exhausted")?;
        let s = Session {
            id,
            operation: uuid::Uuid::new_v4(),
            epoch: self.policy_epoch(),
        };
        // Reserve attempts before effects; a process crash cannot restore this budget.
        {
            let mut journal = tp.journal.lock().unwrap_or_else(|e| e.into_inner());
            journal.failed_attempts += 1;
            journal.last_session = id;
        }
        self.persist_tp_journal()?;
        let result = async {
            self.tp_begin(s).await?;
            let prepared = self.tp_peer(&self.tp_request(TpCommand::Prepare, Some(s))?).await?;
            ensure!(prepared.status.running, "TP worker is not prepared");
            self.tp_event(ClusterEventKind::TensorParallelWorkerPrepared, id).await?;
            self.tp_policy_check(s.epoch)?;
            tp.child.start(id, self.tp_address()?).await?;
            self.tp_event(ClusterEventKind::TensorParallelCoordinatorStarted, id).await?;
            let deadline = tokio::time::Instant::now() + self.inner.config.cluster.timeouts.complete_route;
            loop {
                self.tp_policy_check(s.epoch)?;
                let local = tp.child.status(id).await?;
                let remote = self.tp_peer(&self.tp_request(TpCommand::Status, Some(s))?).await?;
                ensure!(local.running && remote.status.running, "TP child exited before handshake");
                if local.connected && local.http_ready && remote.status.connected { break; }
                ensure!(tokio::time::Instant::now() < deadline, "TP handshake timeout");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.tp_event(ClusterEventKind::TensorParallelHandshakeHttpReady, id).await?;
            {
                let _permit = self.inner.proxy.admission().try_acquire_recovery(true)?;
                tokio::time::timeout(Duration::from_secs(300), async {
                    let warmup = tp.child.warmup();
                    tokio::pin!(warmup);
                    loop {
                        tokio::select! {
                            result = &mut warmup => return result,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                self.tp_policy_check(s.epoch)?;
                                let remote = self.tp_peer(&self.tp_request(TpCommand::Status, Some(s))?).await?;
                                ensure!(remote.status.running && remote.status.connected, "TP peer lost during warm-up");
                            }
                        }
                    }
                }).await??;
            }
            self.tp_policy_check(s.epoch)?;
            let local = tp.child.status(id).await?;
            ensure!(local.running && local.connected && local.http_ready, "TP child lost after warm-up");
            let remote = self.tp_peer(&self.tp_request(TpCommand::Ready, Some(s))?).await?;
            ensure!(remote.status.running && remote.status.connected, "TP peer lost before publication");
            self.tp_policy_check(s.epoch)?;
            let ready = self.tp_event(ClusterEventKind::TensorParallelWarmupDone, id).await?;
            self.inner.proxy.set_target(ready.target, true);
            self.inner.proxy.admission().start_serving();
            Ok(ready)
        }.await;
        if result.is_err() {
            self.tp_note_failure(s.id, true)?;
            self.inner.proxy.admission().block();
            let _ = self
                .tp_peer(&self.tp_request(TpCommand::Stop, Some(s))?)
                .await;
            self.tp_fallback_locked().await?;
            tp.retry_after.store(
                now_millis()
                    + self
                        .inner
                        .config
                        .cluster
                        .policy
                        .promotion_backoff
                        .as_millis() as u64,
                Ordering::Release,
            );
        } else {
            let mut journal = tp.journal.lock().unwrap_or_else(|e| e.into_inner());
            journal.failed_attempts = journal.failed_attempts.saturating_sub(1);
            drop(journal);
            self.persist_tp_journal()?;
        }
        result
    }
    pub(super) async fn tp_fallback_locked(
        &self,
    ) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let result = self.tp_fallback_effects().await;
        if result.is_err() {
            self.inner.proxy.admission().block();
            self.tp_event(ClusterEventKind::RequireManualIntervention, 0)
                .await?;
        }
        result
    }
    async fn tp_fallback_effects(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let tp = self.tp_runtime()?;
        self.inner.proxy.admission().block();
        self.inner.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            false,
        );
        let previous = tp.session();
        let session = previous.map(|s| s.id).unwrap_or(0);
        if matches!(
            self.inner.mode.snapshot().state,
            ClusterState::TensorParallelReady
                | ClusterState::TensorParallelStarting
                | ClusterState::AwaitingTensorParallelWorkerHello
        ) {
            self.tp_event(ClusterEventKind::BeginTensorParallelDemotion, session)
                .await?;
        }
        self.inner
            .proxy
            .admission()
            .wait_for_zero(self.inner.config.cluster.timeouts.drain)
            .await?;
        tp.child.stop().await?;
        *tp.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.tp_event(ClusterEventKind::BeginSoloStandalone, session)
            .await?;
        if let Err(error) = self
            .inner
            .standalone
            .start(self.inner.mode.snapshot().generation)
            .await
        {
            self.tp_event(ClusterEventKind::RequireManualIntervention, session)
                .await?;
            return Err(error);
        }
        ensure!(
            self.inner.standalone.is_running().await?,
            "Standalone fallback is not running"
        );
        let ready = self
            .tp_event(ClusterEventKind::LocalStandaloneReady, session)
            .await?;
        self.inner.proxy.admission().reset_blocked_generation();
        self.inner.proxy.set_target(ready.target, true);
        self.inner.proxy.admission().start_serving();
        *tp.completed.lock().unwrap_or_else(|e| e.into_inner()) = previous;
        Ok(ready)
    }
    pub(super) async fn tp_stop_for_restart(&self) -> anyhow::Result<()> {
        let tp = self.tp_runtime()?;
        let _effect = tp.effects.lock().await;
        self.inner.proxy.admission().block();
        self.inner
            .proxy
            .admission()
            .wait_for_zero(self.inner.config.cluster.timeouts.drain)
            .await?;
        if let Some(session) = tp.session() {
            if self.inner.role == LocalRole::Coordinator {
                let _ = self
                    .tp_peer(&self.tp_request(TpCommand::Stop, Some(session))?)
                    .await;
            }
            self.tp_event(ClusterEventKind::BeginTensorParallelDemotion, session.id)
                .await?;
        }
        tp.child.stop().await?;
        self.inner.standalone.stop().await?;
        *tp.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.inner.proxy.set_target(
            ProxyTarget::Unavailable {
                reason: UnavailableReason::Transition,
            },
            false,
        );
        self.reset_tp_budget()?;
        Ok(())
    }
    pub(super) async fn tp_shutdown(&self) -> anyhow::Result<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let tp = this.tp_runtime()?;
            let _effect = tp.effects.lock().await;
            this.inner.proxy.admission().block();
            this.inner.proxy.set_target(
                ProxyTarget::Unavailable {
                    reason: UnavailableReason::Transition,
                },
                false,
            );
            this.inner
                .proxy
                .admission()
                .wait_for_zero(this.inner.config.cluster.timeouts.drain)
                .await?;
            tp.child.stop().await?;
            *tp.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }
    pub(super) async fn tp_demote(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let this = self.clone();
        tokio::spawn(async move {
            let tp = this.tp_runtime()?;
            let _effect = tp.effects.lock().await;
            this.inner.proxy.admission().block();
            if this.inner.role == LocalRole::Coordinator {
                if let Some(s) = tp.session() {
                    let _ = this
                        .tp_peer(&this.tp_request(TpCommand::Stop, Some(s))?)
                        .await;
                }
            }
            this.tp_fallback_locked().await
        })
        .await?
    }
    pub(super) async fn tp_reconcile(&self) -> anyhow::Result<crate::cluster::ClusterSnapshot> {
        let tp = self.tp_runtime()?;
        // Startup owns this lock until all effects finish. Never inspect a half-started child.
        let Ok(_effect) = tp.effects.try_lock() else {
            return Ok(self.inner.mode.snapshot());
        };
        let state = self.inner.mode.snapshot().state;
        if state == ClusterState::PairedStandaloneReady {
            if self
                .tp_peer(&self.tp_request(TpCommand::Node, None)?)
                .await
                .is_err()
            {
                self.inner.proxy.admission().block();
                if self.inner.config.cluster.policy.auto_demote {
                    return self.tp_fallback_locked().await;
                }
                self.tp_event(ClusterEventKind::RequireManualIntervention, 0)
                    .await?;
            }
            return Ok(self.inner.mode.snapshot());
        }
        if matches!(
            state,
            ClusterState::TensorParallelReady
                | ClusterState::TensorParallelStarting
                | ClusterState::AwaitingTensorParallelWorkerHello
        ) {
            let s = tp.session().context("TP state without session")?;
            let local = tp.child.status(s.id).await;
            let peer = self.tp_peer(&self.tp_request(TpCommand::Node, None)?).await;
            let ready = state == ClusterState::TensorParallelReady;
            let startup_expired = !ready
                && tp
                    .startup_deadline
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none_or(|deadline| tokio::time::Instant::now() >= deadline);
            let failed = startup_expired
                || local
                    .as_ref()
                    .map_or(true, |l| !l.running || (ready && !l.connected))
                || peer.as_ref().map_or(true, |p| {
                    ready && (!p.status.running || !p.status.connected || p.session != s.id)
                });
            if failed {
                self.inner.proxy.admission().block();
                self.tp_note_failure(s.id, false)?;
                if self.inner.config.cluster.policy.auto_demote {
                    if self.inner.role == LocalRole::Coordinator {
                        let _ = self
                            .tp_peer(&self.tp_request(TpCommand::Stop, Some(s))?)
                            .await;
                    }
                    return self.tp_fallback_locked().await;
                }
                self.tp_event(ClusterEventKind::RequireManualIntervention, s.id)
                    .await?;
            }
        }
        Ok(self.inner.mode.snapshot())
    }
    pub(super) fn restore_tp_state(
        &self,
        store: &StateStore,
    ) -> anyhow::Result<crate::cluster::state_store::PersistentTpState> {
        use sha2::{Digest, Sha256};
        let c = &self.inner.config.ds4.distributed;
        let identity =
            crate::cluster::manifest::lower_hex(&Sha256::digest(serde_json::to_vec(&(
                self.inner.manifest.deployment_id()?,
                &c.rdma_member_interface,
                &c.rdma_address,
                &c.rdma_device,
                &c.rdma_gid,
            ))?));
        let mut saved = store.load()?.and_then(|s| s.tp).unwrap_or_default();
        if saved.identity != identity {
            saved.identity = identity;
            saved.failed_attempts = 0;
        }
        Ok(saved)
    }
    pub(super) fn reset_tp_budget(&self) -> anyhow::Result<()> {
        if let Some(tp) = &self.inner.tp {
            tp.journal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failed_attempts = 0;
            tp.retry_after.store(0, Ordering::Release);
            self.persist_tp_journal()?;
        }
        Ok(())
    }
    pub(super) fn persist_tp_journal(&self) -> anyhow::Result<()> {
        let Some(tp) = &self.inner.tp else {
            return Ok(());
        };
        let _guard = self.policy_persistence_guard();
        if let Some(store) = self.policy_store() {
            use crate::cluster::state_store::{
                PERSISTENT_STATE_SCHEMA_VERSION, PersistentClusterState, PersistentMode,
                PersistentProxyTarget,
            };
            let current = self.inner.mode.snapshot();
            let mut state = store.load()?.unwrap_or(PersistentClusterState {
                tp: None,
                schema_version: PERSISTENT_STATE_SCHEMA_VERSION,
                generation: current.generation,
                control_session_generation: current.generation,
                desired_mode: PersistentMode::SoloStandalone,
                last_stable_mode: PersistentMode::SoloStandalone,
                cluster_state: current.state.name().into(),
                proxy_target: PersistentProxyTarget::Unavailable,
                active_profile: None,
                child: None,
                last_failure: None,
                operator_policy: self.operator_policy(),
                applied_policy: self.applied_policy(),
                policy_epoch: self.policy_epoch(),
                pending_operation: None,
            });
            state.tp = Some(tp.journal.lock().unwrap_or_else(|e| e.into_inner()).clone());
            store.save(&state)?;
        }
        Ok(())
    }
}
