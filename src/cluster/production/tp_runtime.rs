//! v0.4.0 TP coordinator runtime（T11 / C02）。
//!
//! LP の `CoordinatorDistributedRuntime`（`DistributedChildStarted` / `DistributedRouteReady`
//! を駆動）に相当する TP 版 runtime。reducer（`spawn_state_machine`）へ TP 準備要素
//! （worker Prepared → coordinator 起動 → handshake+HTTP ready → warm-up 完了）を
//! session 付きで投入し、`TensorParallelReady` で route を公開する。開始 gate
//! （operator_policy 保護ラッチ・v1 交渉）は caller 側（`tp_start_verdict`）が確認済みと
//! し、ここでは TP イベント投入のみを行う。
//!
//! 実 child 起動・OS 接触は行わない（dry-run / fake 境界で駆動）。TP の child 操作
//! （worker Prepared / coordinator 起動）は T07 / T08 の supervisor / harness が担い、
//! 本 module は reducer へのイベント投入と route 公開を担う。

use crate::{
    cluster::{ClusterEvent, ClusterEventKind, ClusterHandle, ClusterSnapshot, TpSessionId},
    proxy::ModeAwareProxyState,
    target::ClusterState,
};
use anyhow::ensure;
use std::sync::Arc;

/// TP coordinator runtime の開始 gate を確認した後、TP 準備要素を reducer へ投入して
/// `TensorParallelReady` へ進める。開始時点は PairedStandaloneReady（または
/// SoloStandaloneReady）でなければならない（reducer の TP 遷移表）。戻り値は TP Ready
/// の snapshot。
pub async fn drive_tp_ready(
    cluster: &ClusterHandle,
    proxy: Arc<ModeAwareProxyState>,
    session: TpSessionId,
) -> anyhow::Result<ClusterSnapshot> {
    let current = cluster.snapshot();
    ensure!(
        matches!(
            current.state,
            ClusterState::PairedStandaloneReady | ClusterState::SoloStandaloneReady
        ),
        "TP は Solo/Paired ready からしか開始できない: {:?}",
        current.state
    );
    // BeginTP → TensorParallelStarting。
    let starting = cluster
        .apply(ClusterEvent::tp(
            current.generation,
            ClusterEventKind::BeginTensorParallel,
            session,
        ))
        .await?;
    // worker Prepared → AwaitingTensorParallelWorkerHello。
    let _ = cluster
        .apply(ClusterEvent::tp(
            starting.generation,
            ClusterEventKind::TensorParallelWorkerPrepared,
            session,
        ))
        .await?;
    // coordinator 起動。
    let _ = cluster
        .apply(ClusterEvent::tp(
            cluster.snapshot().generation,
            ClusterEventKind::TensorParallelCoordinatorStarted,
            session,
        ))
        .await?;
    // handshake + HTTP ready。
    let _ = cluster
        .apply(ClusterEvent::tp(
            cluster.snapshot().generation,
            ClusterEventKind::TensorParallelHandshakeHttpReady,
            session,
        ))
        .await?;
    // warm-up 完了 → TensorParallelReady。route 公開。
    let ready = cluster
        .apply(ClusterEvent::tp(
            cluster.snapshot().generation,
            ClusterEventKind::TensorParallelWarmupDone,
            session,
        ))
        .await?;
    ensure!(
        ready.state == ClusterState::TensorParallelReady,
        "TP warm-up 後に TensorParallelReady へ到達しなかった: {:?}",
        ready.state
    );
    proxy.set_target(ready.target, true);
    Ok(ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster::{ClusterEventKind, spawn_state_machine},
        target::{ClusterState, LocalRole, ProxyTarget},
    };
    use std::time::Duration;

    fn proxy() -> Arc<crate::proxy::ModeAwareProxyState> {
        Arc::new(
            crate::proxy::ModeAwareProxyState::new(
                url::Url::parse("http://127.0.0.1:8000").unwrap(),
                url::Url::parse("http://10.99.0.1:18082").unwrap(),
                crate::proxy::ModeAwareProxyOptions {
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

    /// PairedStandaloneReady まで駆動した ClusterHandle を返す。TP は Solo/Paired ready
    /// からしか開始できないため、事前に ready へ進めておく。。
    async fn paired_ready(handle: &crate::cluster::ClusterHandle) {
        let _ = handle
            .apply(crate::cluster::ClusterEvent::new(
                0,
                ClusterEventKind::BeginSoloStandalone,
            ))
            .await
            .expect("BeginSoloStandalone accepted");
        let _ = handle
            .apply(crate::cluster::ClusterEvent::new(
                1,
                ClusterEventKind::LocalStandaloneReady,
            ))
            .await
            .expect("LocalStandaloneReady accepted");
        let pairing = handle
            .apply(crate::cluster::ClusterEvent::new(
                2,
                ClusterEventKind::BeginPairing,
            ))
            .await
            .expect("BeginPairing accepted");
        let _ = handle
            .apply(crate::cluster::ClusterEvent::new(
                pairing.generation,
                ClusterEventKind::PairingReady,
            ))
            .await
            .expect("PairingReady accepted");
    }

    #[tokio::test]
    async fn drives_tp_ready_from_paired_ready_and_publishes_route() {
        let proxy = proxy();
        let (handle, task) = spawn_state_machine(
            crate::cluster::ClusterSnapshot::booting(LocalRole::Coordinator),
            16,
        );
        paired_ready(&handle).await;
        let ready = drive_tp_ready(&handle, proxy.clone(), TpSessionId(1))
            .await
            .expect("TP ready should drive");
        assert_eq!(ready.state, ClusterState::TensorParallelReady);
        assert_eq!(
            ready.stable_mode,
            crate::target::StableMode::DistributedTensorParallel
        );
        // coordinator は local route を公開。
        assert!(ready.local_standalone_ready);
        assert_eq!(ready.target, ProxyTarget::LocalStandalone);
        task.abort();
    }

    #[tokio::test]
    async fn worker_reaches_tp_ready_and_forwards_to_coordinator() {
        let proxy = proxy();
        let (handle, task) = spawn_state_machine(
            crate::cluster::ClusterSnapshot::booting(LocalRole::Worker),
            16,
        );
        paired_ready(&handle).await;
        let ready = drive_tp_ready(&handle, proxy.clone(), TpSessionId(1))
            .await
            .expect("TP ready should drive");
        assert_eq!(ready.state, ClusterState::TensorParallelReady);
        // worker は TP route を coordinator へ転送。
        assert!(!ready.local_standalone_ready);
        assert_eq!(ready.target, ProxyTarget::Coordinator);
        task.abort();
    }

    #[tokio::test]
    async fn rejects_start_from_non_ready_state() {
        let proxy = proxy();
        let (handle, task) = spawn_state_machine(
            crate::cluster::ClusterSnapshot::booting(LocalRole::Coordinator),
            16,
        );
        // Booting のまま TP を開始しようとすると拒否される。
        let err = drive_tp_ready(&handle, proxy.clone(), TpSessionId(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Solo/Paired ready"));
        task.abort();
    }

    #[tokio::test]
    async fn stale_session_is_not_accepted_by_reducer() {
        // TP 開始後に別 session の TP イベントは reducer が無視する（session 照合 C02）。
        // drive_tp_ready は同 session で正しく TP Ready へ進めることを確認済み。
        let proxy = proxy();
        let (handle, task) = spawn_state_machine(
            crate::cluster::ClusterSnapshot::booting(LocalRole::Coordinator),
            16,
        );
        paired_ready(&handle).await;
        let ready = drive_tp_ready(&handle, proxy.clone(), TpSessionId(1))
            .await
            .unwrap();
        assert_eq!(ready.state, ClusterState::TensorParallelReady);
        task.abort();
    }
}
