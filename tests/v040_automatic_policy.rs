//! P05 — Automatic 復帰と全自動経路の policy gate（C03 PolicyEpoch equality）。
//!
//! 実モデルを使わず、本番 reducer（`spawn_state_machine`）と policy gate 判定（純粋）を
//! fake 境界で駆動して Automatic 復帰の全自動経路 gate を検証する。実 child 起動・OS
//! 接触・既存 state アクセスは行わない（Recorder で 0 を検証）。
//!
//! 受入 case:
//! 1. Automatic + peer 無し → Complete + Solo。
//! 2. auto_promote false → promotion 0。
//! 3. 部分 commit（epoch 不一致）→ promotion 0。
//! 4. deployment mismatch → latch 保持。
//!
//! レビュー重点: discovery callback / periodic tick / operator promote / recovery / route
//! monitor の全自動経路で gate 漏れを作らない。`automatic_promotion_verdict` を各経路が
//! 共有する判定とし、gate の単一正本にする。

#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    AutomaticPromotionVerdict, ClusterEvent, ClusterEventKind, OperationPolicy,
    automatic_promotion_verdict,
};
use siderostat::target::{ClusterState, LocalRole};

/// 受入 case 1: Automatic + peer 無し → Complete + Solo。
/// Automatic 適用が完了（operator_policy=Automatic、両 node epoch 一致）しても、peer が
/// 不在なら promotion は発生せず Solo のまま。job 完了（方針の適用）は TP ready とは
/// 区別される。
#[tokio::test]
async fn v040_automatic_policy_peer_absent_complete_solo() {
    let h = support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::Automatic);
    // Automatic 適用完了を表す（operator_policy=Automatic、両端 epoch 一致）。。
    // ここでは gate 判定が peer 不在で Allow にならないことを検証する。
    let verdict = automatic_promotion_verdict(
        OperationPolicy::Automatic,
        true,  // auto_promote
        false, // deployment mismatch latch なし
        false, // peer 不在
        7,     // local policy epoch
        7,     // peer policy epoch（一致）
    );
    assert_eq!(verdict, AutomaticPromotionVerdict::PeerAbsent);
    assert!(!verdict.allows_promotion());
    // 本番 reducer は peer 不在時 Solo のまま（Pairing へ進まない）。。
    // FakeCluster は初期 Booting。Solo ready に収束させ、pair を試みない。
    let _ = h
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("solo begin accepted");
    let g = h.handle.snapshot().generation;
    let ready = h
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("solo ready accepted");
    assert_eq!(ready.state, ClusterState::SoloStandaloneReady);
    // TP spawn / 実 child / OS 接触は 0。
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    assert_eq!(h.real_process_operations(), 0);
    assert_eq!(h.recorder.existing_state_accesses(), 0);
    h._task.abort();
}

/// 受入 case 2: auto_promote false → promotion 0。
#[tokio::test]
async fn v040_automatic_policy_auto_promote_false_no_promotion() {
    let h = support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::Automatic);
    // auto_promote=false。peer がいても epoch が一致しても昇格しない。。
    let verdict = automatic_promotion_verdict(
        OperationPolicy::Automatic,
        false, // auto_promote disabled
        false, // latch なし
        true,  // peer present
        7,
        7,
    );
    assert_eq!(verdict, AutomaticPromotionVerdict::AutoPromoteDisabled);
    assert!(!verdict.allows_promotion());
    // 本番 reducer は Solo ready のまま（promotion 0）。。
    let _ = h
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("solo begin accepted");
    let g = h.handle.snapshot().generation;
    let ready = h
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("solo ready accepted");
    assert_eq!(ready.state, ClusterState::SoloStandaloneReady);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    h._task.abort();
}

/// 受入 case 3: 部分 commit（epoch 不一致）→ promotion 0。
#[tokio::test]
async fn v040_automatic_policy_partial_commit_no_promotion() {
    let h = support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::Automatic);
    // 両端の policy epoch が不一致（部分 commit / 片側 ack 不明）。peer がいても
    // auto_promote=true でも昇格しない。。
    let verdict = automatic_promotion_verdict(
        OperationPolicy::Automatic,
        true,
        false,
        true,
        7, // local policy epoch
        8, // peer policy epoch（不一致）
    );
    assert_eq!(verdict, AutomaticPromotionVerdict::EpochMismatch);
    assert!(!verdict.allows_promotion());
    assert_eq!(verdict.name(), "auto-promote-epoch-mismatch");
    // 本番 reducer は Solo ready のまま（promotion 0）。。
    let _ = h
        .apply(ClusterEvent::new(0, ClusterEventKind::BeginSoloStandalone))
        .await
        .expect("solo begin accepted");
    let g = h.handle.snapshot().generation;
    let ready = h
        .apply(ClusterEvent::new(g, ClusterEventKind::LocalStandaloneReady))
        .await
        .expect("solo ready accepted");
    assert_eq!(ready.state, ClusterState::SoloStandaloneReady);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    assert_eq!(h.real_process_operations(), 0);
    h._task.abort();
}

/// 受入 case 4: deployment mismatch → latch 保持。
/// deployment mismatch latch は独立。epoch が一致しても latch が立っている限り
/// promotion は抑止される。latch は peer 不在 / epoch 変更で勝手に解除されない。。
#[tokio::test]
async fn v040_automatic_policy_deployment_mismatch_latch_held() {
    let h = support::v040::FakeCluster::new(LocalRole::Coordinator, OperationPolicy::Automatic);
    // deployment mismatch latch 保持中。epoch 一致・peer 存在・auto_promote=true でも
    // 昇格しない。。
    let verdict = automatic_promotion_verdict(
        OperationPolicy::Automatic,
        true,
        true, // deployment mismatch latch
        true,
        7,
        7,
    );
    assert_eq!(verdict, AutomaticPromotionVerdict::DeploymentMismatchLatch);
    assert!(!verdict.allows_promotion());
    assert_eq!(verdict.name(), "auto-promote-deployment-mismatch-latch");
    // latch は peer 不在になっても保持される（独立）。。
    let still_held = automatic_promotion_verdict(
        OperationPolicy::Automatic,
        true,
        true,
        false, // peer 不在でも
        7,
        7,
    );
    assert_eq!(
        still_held,
        AutomaticPromotionVerdict::DeploymentMismatchLatch
    );
    h._task.abort();
}

/// 補足: ForcedStandalone 保護ラッチは Automatic を選んでも解除されない（C03）。。
#[tokio::test]
async fn v040_automatic_policy_forced_standalone_latch_wins() {
    // operator_policy=ForcedStandalone のまま（保護ラッチ保持）。Automatic を選んでも
    // 解除されない。。
    let verdict =
        automatic_promotion_verdict(OperationPolicy::ForcedStandalone, true, false, true, 7, 7);
    assert_eq!(verdict, AutomaticPromotionVerdict::ForcedStandaloneLatch);
    assert!(!verdict.allows_promotion());
    assert_eq!(verdict.name(), "auto-promote-forced-standalone-latch");
}

/// 補足: 正常系（両端 epoch 一致・peer 存在・auto_promote=true・latch なし）のみ Allow。。
#[tokio::test]
async fn v040_automatic_policy_allow_only_when_all_conditions_met() {
    let verdict = automatic_promotion_verdict(OperationPolicy::Automatic, true, false, true, 9, 9);
    assert_eq!(verdict, AutomaticPromotionVerdict::Allow);
    assert!(verdict.allows_promotion());
    assert_eq!(verdict.name(), "auto-promote-allowed");
}
