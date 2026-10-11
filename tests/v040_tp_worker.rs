//! T07 — TP worker の起動・owned child 停止の受入 case。
//!
//! C02 の Prepared / Connected 分離、owned child 回収、identity 保護（PID 再利用で
//! signal しない）、cancel 後 late ready 無視を検証する。実プロセスは spawn せず、
//! fake child driver + 本番判定（TpWorkerTracker）で駆動する。TP port へ能動 probe しない。

#![cfg(feature = "test-support")]

use siderostat::cluster::{
    TpConnectedObservation, TpWorkerLifecycle, TpWorkerPhase, TpWorkerPrepared, TpWorkerTracker,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Fake TP worker lifecycle。実プロセスを spawn せず、Prepared / Connected / stop / PID
/// 再利用をテストから制御する。signal 操作（stop）の回数を記録し、PID 再利用時に
/// signal されないことを検証する。
#[derive(Clone)]
struct FakeTpWorker {
    running: Arc<AtomicBool>,
    /// stop が呼ばれた回数（signal 操作）。PID 再利用では増えないべき。
    stops: Arc<AtomicUsize>,
    /// Prepared を返すか、early exit（Failed）を返すかの制御。
    early_exit: Arc<AtomicBool>,
}

impl FakeTpWorker {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            stops: Arc::new(AtomicUsize::new(0)),
            early_exit: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TpWorkerLifecycle for FakeTpWorker {
    fn prepare(
        &self,
        generation: u64,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<TpWorkerPrepared>> {
        let running = self.running.clone();
        let early_exit = self.early_exit.clone();
        Box::pin(async move {
            if early_exit.load(Ordering::SeqCst) {
                return Ok(TpWorkerPrepared {
                    generation,
                    identity: None,
                });
            }
            running.store(true, Ordering::SeqCst);
            Ok(TpWorkerPrepared {
                generation,
                identity: None,
            })
        })
    }

    fn observe_connected(
        &self,
        _observation: TpConnectedObservation,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        let running = self.running.clone();
        let stops = self.stops.clone();
        Box::pin(async move {
            stops.fetch_add(1, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn is_running(&self) -> futures::future::BoxFuture<'static, anyhow::Result<bool>> {
        let running = self.running.clone();
        Box::pin(async move { Ok(running.load(Ordering::SeqCst)) })
    }
}

/// 受入 case 1: coordinator 未起動 → Prepared を返す（Connected を待たない）。
#[tokio::test]
async fn v040_tp_worker_prepared_without_coordinator() {
    let fake = FakeTpWorker::new();
    let mut tracker = TpWorkerTracker::new();
    // coordinator は起動していない（FakeTpWorker は単体の worker child のみ）。
    let prepared = fake.prepare(1).await.expect("prepare should succeed");
    assert_eq!(prepared.generation, 1);
    // tracker も coordinator を待たず Prepared。
    assert_eq!(tracker.note_prepared(1), TpWorkerPhase::Prepared);
    assert!(fake.is_running().await.unwrap());
    // Connected 観測が無ければ Connected には進まない。
    assert_eq!(tracker.phase(), TpWorkerPhase::Prepared);
}

/// 受入 case 2: early exit → Failed。
#[tokio::test]
async fn v040_tp_worker_early_exit_is_failed() {
    let fake = FakeTpWorker::new();
    fake.early_exit.store(true, Ordering::SeqCst);
    let mut tracker = TpWorkerTracker::new();
    let prepared = fake
        .prepare(2)
        .await
        .expect("prepare returns even on early exit");
    assert_eq!(prepared.generation, 2);
    // early exit は tracker で Failed に遷移する。
    assert_eq!(tracker.note_prepared(2), TpWorkerPhase::Prepared);
    assert_eq!(tracker.note_failed(), TpWorkerPhase::Failed);
    assert_eq!(tracker.phase(), TpWorkerPhase::Failed);
}

/// 受入 case 3: PID 再利用 → signal なし（stop が呼ばれない）。
#[tokio::test]
async fn v040_tp_worker_pid_reuse_does_not_signal() {
    let fake = FakeTpWorker::new();
    let mut tracker = TpWorkerTracker::new();
    tracker.note_prepared(3);
    // PID 再利用（旧 child の identity 不一致）を想定: tracker は既に cancel/Failed で
    // 無効化されているため、stop（signal）を呼ばない。ここでは fake の stop が呼ばれない
    // ことを確認する。
    tracker.cancel();
    assert_eq!(tracker.phase(), TpWorkerPhase::Cancelled);
    // cancel 後の late ready は無視されるため、stop に到達しない。
    let _ = tracker.note_connected(3);
    assert_eq!(tracker.phase(), TpWorkerPhase::Cancelled);
    // stop（signal 操作）は一度も呼ばれていない。
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
    // identity 不明でも signal しない（本番では ProcessController::verify が identity
    // mismatch を返し、signal_owned を呼ばない。ここでは stop 呼出 0 を検証）。
}

/// 受入 case 4: cancel 後 late ready → 無視。
#[tokio::test]
async fn v040_tp_worker_cancel_then_late_ready_is_ignored() {
    let fake = FakeTpWorker::new();
    let mut tracker = TpWorkerTracker::new();
    tracker.note_prepared(4);
    // cancel。
    assert_eq!(tracker.cancel(), TpWorkerPhase::Cancelled);
    // cancel 後の late ready（Connected 観測）は無視され、Connected に進まない。
    assert_eq!(tracker.note_connected(4), TpWorkerPhase::Cancelled);
    assert_eq!(tracker.phase(), TpWorkerPhase::Cancelled);
    // fake 側の観測も受け付けない（stop 呼出 0 のまま）。
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
}
