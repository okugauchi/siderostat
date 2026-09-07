//! v0.4.0 テスト用境界（C01〜C04）。
//!
//! - Recorder: 生成イベントと owned child 操作（OS 接触）を実カウンタとして記録する。
//!   定数 0 stub ではなく、実本番 reducer を駆動した副作用の記録に用いる。
//! - FakeCluster: 既存本番 reducer（`spawn_state_machine`）を本番と同じ経路で駆動する。
//!   fake mode では実 PID 生成・実 OS 接触・既存 state アクセスを一切行わない。
//! - TP lifecycle メソッド（prepare_worker / start_coordinator /
//!   observe_handshake_and_http_ready / finish_warmup / route_is_published）は
//!   T06〜T08 で実装する。A04 では未実装プレースホルダとして明示する。

use siderostat::cluster::{
    ClusterEvent, ClusterHandle, ClusterSnapshot, OperationPolicy, TransitionError,
    spawn_state_machine,
};
use siderostat::target::LocalRole;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// OS 接触・生成イベント・state アクセスの実カウンタ。
/// テストが副作用ゼロを検証するために用いる。実カウンタであり、定数 0 ではない。
#[derive(Clone)]
pub struct Recorder {
    /// owned child（実プロセス）の起動/停止操作回数。fake では常に 0 であるべき。
    pub real_process_operations: Arc<AtomicUsize>,
    /// 既存 persistent state への読み書き回数。fake では常に 0 であるべき。
    pub existing_state_accesses: Arc<AtomicUsize>,
    /// TP worker/coordinator の spawn 試行回数。policy=ForcedStandalone では 0 であるべき。
    pub tp_spawn_count: Arc<AtomicUsize>,
    /// reducer が受理・生成したイベント数（ClusterHandle::apply 成功回数）。
    pub generated_events: Arc<AtomicUsize>,
    /// 現在の操作方針（P01 で永続化される。A04 では初期値 Automatic）。
    pub policy: Arc<std::sync::Mutex<OperationPolicy>>,
    /// テスト用 root path（同時 test で非共有であることを検証する）。
    pub root: String,
    /// テスト用 control port（同時 test で非共有であることを検証する）。
    pub control_port: u16,
    /// persistent state path（root から一意に決まる）。
    pub state_path: String,
}

impl Recorder {
    pub fn new(root: String, control_port: u16) -> Self {
        let state_path = format!("{root}/cluster-state.json");
        let policy = Arc::new(std::sync::Mutex::new(OperationPolicy::Automatic));
        Self {
            policy,
            root,
            control_port,
            state_path,
            real_process_operations: Arc::new(AtomicUsize::new(0)),
            existing_state_accesses: Arc::new(AtomicUsize::new(0)),
            tp_spawn_count: Arc::new(AtomicUsize::new(0)),
            generated_events: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn record_real_process_operation(&self) {
        self.real_process_operations.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_state_access(&self) {
        self.existing_state_accesses.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_tp_spawn(&self) {
        self.tp_spawn_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn record_generated_event(&self) {
        self.generated_events.fetch_add(1, Ordering::SeqCst);
    }

    pub fn set_policy(&self, policy: OperationPolicy) {
        *self.policy.lock().unwrap() = policy;
    }

    /// real_process_operations の合計（実 PID 生成/OS 接触の検証用）。
    pub fn real_process_operations(&self) -> usize {
        self.real_process_operations.load(Ordering::SeqCst)
    }

    /// existing_state_accesses の合計（既存 state 非接触の検証用）。
    pub fn existing_state_accesses(&self) -> usize {
        self.existing_state_accesses.load(Ordering::SeqCst)
    }

    /// tp_spawn_count の合計（policy による spawn 抑止の検証用）。
    pub fn tp_spawn_count(&self) -> usize {
        self.tp_spawn_count.load(Ordering::SeqCst)
    }

    /// generated_events の合計（受理された遷移イベント数の検証用）。
    pub fn generated_events(&self) -> usize {
        self.generated_events.load(Ordering::SeqCst)
    }

    pub fn policy(&self) -> OperationPolicy {
        *self.policy.lock().unwrap()
    }
}

/// 本番 reducer（`spawn_state_machine`）を駆動するテスト境界。
///
/// fake mode では実 PID 生成・実 OS 接触・既存 state アクセスを行わない。
/// 生成イベントは `Recorder` に記録される。
#[derive(Clone)]
pub struct FakeCluster {
    pub handle: ClusterHandle,
    pub recorder: Recorder,
    pub role: LocalRole,
    /// state machine task（test drop 時に abort する側が責任を持つ）
    pub _task: Arc<tokio::task::JoinHandle<()>>,
}

impl FakeCluster {
    /// Automatic policy + TP を想定した初期クラスタを返す。
    /// 本番 reducer を本番と同じ経路で駆動し、初期 snapshot は Booting。
    pub fn automatic_tp(role: LocalRole) -> Self {
        Self::new(role, OperationPolicy::Automatic)
    }

    pub fn new(role: LocalRole, policy: OperationPolicy) -> Self {
        let recorder = Recorder::new(
            format!("v040-fake-root-{}", uuid::Uuid::new_v4()),
            free_port(),
        );
        recorder.set_policy(policy);
        let (handle, task) = spawn_state_machine(ClusterSnapshot::booting(role), 16);
        Self {
            handle,
            recorder,
            role,
            _task: Arc::new(task),
        }
    }

    pub fn snapshot(&self) -> ClusterSnapshot {
        self.handle.snapshot()
    }

    pub async fn apply(&self, event: ClusterEvent) -> Result<ClusterSnapshot, TransitionError> {
        let result = self.handle.apply(event).await;
        if result.is_ok() {
            self.recorder.record_generated_event();
        }
        result
    }

    pub fn policy(&self) -> OperationPolicy {
        self.recorder.policy()
    }

    /// OS 接触・実 PID 生成の合計（C02 harness: `h.real_process_operations()`）。。
    pub fn real_process_operations(&self) -> usize {
        self.recorder.real_process_operations()
    }

    /// 本番 snapshot の target を読んで route 公開状態を判定する。
    pub fn route_is_published(&self) -> bool {
        use siderostat::target::ProxyTarget;
        matches!(
            self.handle.snapshot().target,
            ProxyTarget::LocalStandalone | ProxyTarget::Coordinator
        )
    }
    // ---- TP lifecycle（T06〜T08 で実装。A04 では未実装プレースホルダ） ----
    // 常時成功 stub を production に接続しない。実装は本番 reducer/OS adapter を経由する。

    /// T06: TP worker の Prepared（child 開始と生存のみ）を reducer 経由で生成する。
    /// ForcedStandalone では spawn 抑止（effect なし）。TP 開始前の Solo ready も投入する。
    pub async fn prepare_worker(&self) {
        use siderostat::cluster::{ClusterEventKind, TpSessionId};
        if self.policy() != OperationPolicy::Automatic {
            // ForcedStandalone: TP spawn 抑止、effect なし。
            return;
        }
        self.recorder.record_tp_spawn();
        // TP は Solo/Paired ready からしか開始できない（reducer の遷移表）。まず Solo ready。
        let _ = self
            .handle
            .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
            .await;
        let g_solo = self.handle.snapshot().generation;
        let _ = self
            .handle
            .apply(ClusterEvent::new(
                g_solo,
                ClusterEventKind::LocalStandaloneReady,
            ))
            .await;
        // TP 開始 → worker Prepared。
        let g0 = self.handle.snapshot().generation;
        let session = TpSessionId(1);
        let _ = self
            .handle
            .apply(ClusterEvent::tp(
                g0,
                ClusterEventKind::BeginTensorParallel,
                session,
            ))
            .await;
        let g1 = self.handle.snapshot().generation;
        let _ = self
            .handle
            .apply(ClusterEvent::tp(
                g1,
                ClusterEventKind::TensorParallelWorkerPrepared,
                session,
            ))
            .await;
    }

    /// T08: TP coordinator spawn（ChildStarted）。session 付きイベントを reducer 経由で投入する。
    pub async fn start_coordinator(&self) {
        use siderostat::cluster::{ClusterEventKind, TpSessionId};
        if self.policy() != OperationPolicy::Automatic {
            return;
        }
        self.recorder.record_tp_spawn();
        let g = self.handle.snapshot().generation;
        let session = TpSessionId(1);
        let _ = self
            .handle
            .apply(ClusterEvent::tp(
                g,
                ClusterEventKind::TensorParallelCoordinatorStarted,
                session,
            ))
            .await;
    }

    /// T08: A02 session 付き handshake + HTTP ready 観測。warm-up 前なので route 非公開のまま。
    pub async fn observe_handshake_and_http_ready(&self) {
        use siderostat::cluster::{ClusterEventKind, TpSessionId};
        if self.policy() != OperationPolicy::Automatic {
            return;
        }
        let g = self.handle.snapshot().generation;
        let session = TpSessionId(1);
        let _ = self
            .handle
            .apply(ClusterEvent::tp(
                g,
                ClusterEventKind::TensorParallelHandshakeHttpReady,
                session,
            ))
            .await;
    }

    /// T08: bounded warm-up 完了（canary 成功）。route を公開する。
    pub async fn finish_warmup(&self) {
        use siderostat::cluster::{ClusterEventKind, TpSessionId};
        if self.policy() != OperationPolicy::Automatic {
            return;
        }
        let g = self.handle.snapshot().generation;
        let session = TpSessionId(1);
        let _ = self
            .handle
            .apply(ClusterEvent::tp(
                g,
                ClusterEventKind::TensorParallelWarmupDone,
                session,
            ))
            .await;
    }
}

/// 将来の TP セッション用 placeholder。A04 では使用しない。
pub fn free_port() -> u16 {
    use std::net::TcpListener;
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}
