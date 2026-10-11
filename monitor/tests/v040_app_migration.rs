//! G07: 旧インストール移行・SMAppService再登録・復元（integration）。
//!
//! C06 / MigrationDriver・service registration。既存の cutover 状態機械
//! （migration.rs）と service_management の fake 境界を利用し、実プロセス /
//! 実 SMAppService 起動なしで受入 case を検証する。原本データを消さず、
//! unknown PID を停止しない。

use siderostat_monitor::migration::{
    CutoverDriver, CutoverState, LegacyInventory, LegacyJob, run_cutover, select_v1_state_backup,
};
use std::path::PathBuf;

/// 受入 case 検証用の scripted cutover driver。各操作を script し、呼び出し
/// 順序を記録する。実プロセス / 実 SMAppService は使わない（fake 境界）。
struct FakeCutoverDriver {
    drain: Result<(), String>,
    stop: Result<(), String>,
    register: Result<(), String>,
    readiness: Result<(), String>,
    rollback: Result<(), String>,
    conflict: bool,
    calls: Vec<&'static str>,
}

impl Default for FakeCutoverDriver {
    fn default() -> Self {
        Self {
            drain: Ok(()),
            stop: Ok(()),
            register: Ok(()),
            readiness: Ok(()),
            rollback: Ok(()),
            conflict: false,
            calls: Vec::new(),
        }
    }
}

impl CutoverDriver for FakeCutoverDriver {
    fn drain_legacy(&mut self) -> Result<(), String> {
        self.calls.push("drain");
        self.drain.clone()
    }
    fn stop_legacy(&mut self) -> Result<(), String> {
        self.calls.push("stop");
        self.stop.clone()
    }
    fn register_new(&mut self) -> Result<(), String> {
        self.calls.push("register");
        self.register.clone()
    }
    fn check_readiness(&mut self) -> Result<(), String> {
        self.calls.push("readiness");
        self.readiness.clone()
    }
    fn port_conflict(&mut self) -> bool {
        self.calls.push("conflict-check");
        self.conflict
    }
    fn rollback(&mut self) -> Result<(), String> {
        self.calls.push("rollback");
        self.rollback.clone()
    }
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    (dir, root)
}

fn touch(path: &std::path::Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

// ---- 受入 case 1: register拒否 → rollback ----

#[test]
fn register_rejection_rolls_back() {
    // SMAppService の register が拒否される（RequiresApproval / DeniedByUser /
    // framework error を Err に変換）と、cutover は新 runtime を登録せず
    // 旧環境へ rollback する。
    let mut driver = FakeCutoverDriver {
        register: Err("register denied by user".into()),
        ..Default::default()
    };
    let state = run_cutover(&mut driver);
    assert_eq!(state, CutoverState::RolledBack);
    // register は呼ばれたが失敗し、rollback が実行される。
    assert_eq!(driver.calls, vec!["drain", "stop", "register", "rollback"]);
}

// ---- 受入 case 2: unregister遅延 → register先行0 ----

#[test]
fn unregister_late_does_not_register_first() {
    // unregister（stop_legacy）が遅延 / 失敗する場合、register を先行させない。
    // 旧 service の停止が完了するまで新 runtime を登録しない（register 先行 0）。
    let mut driver = FakeCutoverDriver {
        stop: Err("unregister still pending".into()),
        ..Default::default()
    };
    let state = run_cutover(&mut driver);
    assert_eq!(state, CutoverState::RolledBack);
    // stop 失敗後は register を呼ばず rollback する（register 先行なし）。
    assert_eq!(driver.calls, vec!["drain", "stop", "rollback"]);
    assert!(!driver.calls.contains(&"register"));
}

// ---- 受入 case 3: v2 state→旧binary復元 → v1backup使用 ----

#[test]
fn v2_state_restores_from_v1_backup() {
    // state が v2 へ移行済みでも、旧 binary 復元時は v1 state backup を
    // 使用する（v2 state を v1 binary が読むと UnsupportedSchema）。
    let (_dir, root) = fixture();
    let state_dir = root.join("state");
    touch(
        &state_dir.join("cluster_state.json"),
        b"{\"schema_version\":2}",
    );
    touch(
        &state_dir.join("v1-backup-00000000-0000-0000-0000-000000000001"),
        b"v1-state",
    );
    let selected = select_v1_state_backup(&state_dir).expect("v1 backup exists");
    // v2 state ではなく v1 backup を選択する。
    assert!(selected.ends_with("v1-backup-00000000-0000-0000-0000-000000000001"));
}

// ---- 受入 case 4: 重複active → 停止対象identity限定 ----

#[test]
fn duplicate_active_stops_only_verified_identity() {
    // 重複 active（旧 LaunchAgent が複数 / unknown PID を含む）を検出しても、
    // 自動停止の対象は identity 確認済みの job だけに限定する。unknown PID は
    // 停止しない（原本データを消さない）。
    let verified = LegacyJob {
        label: "local.siderostat.runtime",
        identity: None,
        identity_verified: true,
    };
    let unknown = LegacyJob {
        label: "local.siderostat.monitor",
        identity: None,
        identity_verified: false,
    };
    let inventory = LegacyInventory {
        jobs: vec![verified.clone(), unknown.clone()],
        ..Default::default()
    };
    let stoppable = inventory.verifiable_jobs();
    // 停止対象は identity_verified のみ（unknown PID は対象外）。
    assert_eq!(stoppable.len(), 1);
    assert_eq!(stoppable[0].label, "local.siderostat.runtime");
    assert!(
        !stoppable
            .iter()
            .any(|job| job.label == "local.siderostat.monitor")
    );
}
