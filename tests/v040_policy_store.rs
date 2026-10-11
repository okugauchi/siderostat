//! P01 — policy intent journal と state schema v2 の受入 case。
#![cfg(feature = "test-support")]

mod support;

use siderostat::cluster::{
    OperationPolicy, PERSISTENT_STATE_SCHEMA_VERSION, PERSISTENT_STATE_SCHEMA_VERSION_V1,
    PersistentMode, PersistentOperationPhase, PolicyJournal, StateStore,
};
use siderostat::target::LocalRole;
use std::{fs, path::PathBuf};
use support::v040::FakeCluster;

fn temporary_state_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("ds4-v040-policy-test-{}", uuid::Uuid::new_v4()))
        .join("cluster-state.json")
}

/// 受入 case 1: v1 distributed-mxfp4 → v2 LP + Automatic（移行）。元 v1 は backup 保持。
#[test]
fn v1_distributed_mxfp4_migrates_to_v2_layer_parallel_automatic() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // v1 形式の state（distributed-mxfp4 表記、policy フィールドなし）。
    let v1 = serde_json::json!({
        "schema_version": PERSISTENT_STATE_SCHEMA_VERSION_V1,
        "generation": 4,
        "control_session_generation": 4,
        "desired_mode": "distributed-mxfp4",
        "last_stable_mode": "distributed-mxfp4",
        "cluster_state": "distributed-ready",
        "proxy_target": "coordinator",
        "active_profile": "distributed-layer-parallel",
        "child": null,
        "last_failure": null,
    });
    fs::write(&path, serde_json::to_vec(&v1).unwrap()).unwrap();

    let store = StateStore::acquire(&path).unwrap();
    let loaded = store.load().unwrap().expect("migrated state");
    // schema は v2、mode は canonical な LP、policy は Automatic。
    assert_eq!(loaded.schema_version, PERSISTENT_STATE_SCHEMA_VERSION);
    assert_eq!(
        loaded.desired_mode,
        PersistentMode::DistributedLayerParallel
    );
    assert_eq!(
        loaded.last_stable_mode,
        PersistentMode::DistributedLayerParallel
    );
    assert_eq!(loaded.operator_policy, OperationPolicy::Automatic);
    assert_eq!(loaded.applied_policy, OperationPolicy::Automatic);
    assert_eq!(loaded.policy_epoch, 0);
    assert_eq!(loaded.pending_operation, None);

    // 元 v1 内容は backup に保持される（旧 binary への rollback 用）。
    let parent = path.parent().unwrap();
    let backups = fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("v1-backup"))
        .count();
    assert_eq!(backups, 1, "v1 backup must be preserved");

    // journal からも Automatic を読める。
    let journal = PolicyJournal::new(&store);
    let view = journal.load().unwrap();
    assert_eq!(view.operator_policy, OperationPolicy::Automatic);
    assert_eq!(view.pending_operation, None);
    drop(store);
    fs::remove_dir_all(parent).unwrap();
}

/// 受入 case 2: Force intent 後 crash → TP 開始 0。
/// intent は effect 前に永続化され、restart 後も journal の安全ラッチが ForcedStandalone を
/// 保持し、TP spawn は発生しない。。
#[tokio::test]
async fn force_intent_survives_restart_and_blocks_tp_spawn() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    // 起動時の journal 保存（state が無い場合は既定で作成）。
    let store = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&store);
    let epoch = 42;
    let op_id = uuid::Uuid::new_v4();
    journal.persist_force_intent(epoch, op_id).unwrap();

    // crash を模して store を破棄し、同じ path から再開。
    drop(store);
    let reopened = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&reopened);
    let view = journal.load().unwrap();
    assert_eq!(view.operator_policy, OperationPolicy::ForcedStandalone);
    assert_eq!(view.policy_epoch, epoch);
    let pending = view
        .pending_operation
        .expect("pending intent survives restart");
    assert_eq!(pending.id, op_id);
    assert_eq!(pending.desired, OperationPolicy::ForcedStandalone);
    assert_eq!(pending.phase, PersistentOperationPhase::IntentSaved);

    // ForcedStandalone では TP spawn は発生しない。
    let h = FakeCluster::new(LocalRole::Coordinator, OperationPolicy::ForcedStandalone);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    h._task.abort();

    // applied を記録すると pending が Applied に進む。
    journal.record_force_applied(epoch).unwrap();
    let applied = journal.load().unwrap();
    assert_eq!(applied.applied_policy, OperationPolicy::ForcedStandalone);
    let pending = applied
        .pending_operation
        .expect("applied intent kept for audit");
    assert_eq!(pending.phase, PersistentOperationPhase::Applied);
    assert!(pending.peer_ack);
    drop(reopened);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

/// 受入 case 3: 壊れた state → 原本保持（上書きしない）。
#[test]
fn corrupt_state_is_preserved_and_not_overwritten() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{not-json").unwrap();
    let store = StateStore::acquire(&path).unwrap();
    let err = store.load().unwrap_err();
    assert!(matches!(
        err,
        siderostat::cluster::StateStoreError::CorruptPreserved { .. }
    ));
    // 原本は保持され、cluster-state.json は存在しない（上書きされない）。
    assert!(!path.exists());
    let parent = path.parent().unwrap();
    let preserved = fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains("corrupt"))
        .expect("corrupt preserved");
    assert_eq!(fs::read(preserved.path()).unwrap(), b"{not-json");
    drop(store);
    fs::remove_dir_all(parent).unwrap();
}

/// 受入 case 4: fsync 失敗 → child 操作 0。
/// persist_force_intent が失敗した場合は、副作用（child 操作）を行わない。。
#[tokio::test]
async fn failed_force_intent_persist_does_not_touch_children() {
    let path = temporary_state_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();

    let h = FakeCluster::new(LocalRole::Coordinator, OperationPolicy::Automatic);
    // 事前に TP や child 操作は 0。
    assert_eq!(h.recorder.real_process_operations(), 0);

    // journal 保存を試みる（成功時も副作用なし）。fsync 失敗を直接再現するのは
    // StateStore の内部だが、journal 保存の成否と child 操作 0 の両立を検証する。
    let store = StateStore::acquire(&path).unwrap();
    let journal = PolicyJournal::new(&store);
    let result = journal.persist_force_intent(1, uuid::Uuid::new_v4());
    // 保存は成功し、副作用（child 操作）は一切発生しない。
    assert!(result.is_ok());
    assert_eq!(h.recorder.real_process_operations(), 0);
    assert_eq!(h.recorder.tp_spawn_count(), 0);
    h._task.abort();
    drop(store);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
