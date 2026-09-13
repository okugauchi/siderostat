//! DS4 Manager API（M10 / C04）の受入テスト。
//!
//! 公開 API（`manager::api`）を介して C04 の ManagerApi 契約を検証する。
//! dry-run 方針に従い、実プロセス・実ネットワークを使わず JobJournal を
//! fake 境界として駆動する。secret / raw build log を公開 DTO に含めない
//! ことも確認する。M10。
use siderostat::manager::{
    JobJournal,
    api::{
        JobSubmitRequest, ManagerApiError, cancel, get, parse_kind, status, submit, submit_json,
    },
};

fn req(kind: &str, key: &str) -> JobSubmitRequest {
    JobSubmitRequest {
        kind: kind.to_string(),
        payload_key: key.to_string(),
        expected_generation: 0,
        runtime_lease: None,
    }
}

fn activate_req(key: &str) -> JobSubmitRequest {
    JobSubmitRequest {
        kind: "activate".to_string(),
        payload_key: key.to_string(),
        expected_generation: 3,
        runtime_lease: Some("lease-3".to_string()),
    }
}

/// 受入: 入力 fetch→build→download→verify→activate→rollback → 状態一致。M10。
#[test]
fn full_pipeline_states_match() {
    let mut journal = JobJournal::new();
    for (kind, key) in [
        ("fetch", "src-a"),
        ("build", "src-a"),
        ("download", "model-a"),
        ("verify", "model-a"),
        ("stage", "profile-a"),
    ] {
        let id = submit(&mut journal, req(kind, key)).expect("submit").id;
        assert_eq!(parse_kind(kind), Some(journal.get(&id).expect("job").kind));
        journal.succeed(&id).expect("succeed");
    }
    let id = submit(&mut journal, activate_req("profile-a"))
        .expect("activate")
        .id;
    journal.succeed(&id).expect("succeed");
    let mut rb = activate_req("profile-a");
    rb.kind = "rollback".to_string();
    let id = submit(&mut journal, rb).expect("rollback").id;
    journal.succeed(&id).expect("succeed");
    let jobs = journal.all();
    assert_eq!(jobs.len(), 7);
    assert!(
        jobs.iter()
            .all(|j| j.phase == siderostat::manager::jobs::JobPhase::Succeeded)
    );
    // status の queue_depth は 0（全成功）。M10。
    let s = status(&journal, Some("old-digest".to_string()));
    assert_eq!(s.queue_depth, 0);
    assert_eq!(s.active_digest.as_deref(), Some("old-digest"));
}

/// 受入: cancel 後 poll → terminal 保持。M10。
#[test]
fn cancel_keeps_terminal_phase() {
    let mut journal = JobJournal::new();
    let id = submit(&mut journal, req("fetch", "src-b"))
        .expect("submit")
        .id;
    cancel(&mut journal, &id).expect("cancel");
    // poll しても terminal（cancelling）を保持。M10。
    let dto = get(&journal, &id).expect("get");
    assert_eq!(dto.phase, "cancelling");
    assert!(dto.cancel);
    // 存在しない job の cancel → NotFound。M10。
    assert_eq!(cancel(&mut journal, "nope"), Err(ManagerApiError::NotFound));
}

/// 受入: 不正 job / unknown field → 400。M10。
#[test]
fn invalid_job_and_unknown_field_rejected() {
    let mut journal = JobJournal::new();
    assert!(matches!(
        submit(&mut journal, req("publish", "x")),
        Err(ManagerApiError::BadRequest(_))
    ));
    assert!(matches!(
        submit_json(
            &mut journal,
            r#"{"kind":"fetch","payload_key":"y","bogus":1}"#
        ),
        Err(ManagerApiError::BadRequest(_))
    ));
    assert!(matches!(
        submit_json(&mut journal, "not-json"),
        Err(ManagerApiError::BadRequest(_))
    ));
}

/// 受入: activate busy → 409。M10。
#[test]
fn activate_busy_conflicts() {
    let mut journal = JobJournal::new();
    let _ = submit(&mut journal, activate_req("profile-b")).expect("first activate");
    assert!(matches!(
        submit(&mut journal, activate_req("profile-c")),
        Err(ManagerApiError::Conflict(_))
    ));
}

/// activate/rollback は generation + lease を要求（C04）。M10。
#[test]
fn activate_requires_generation_and_lease() {
    let mut journal = JobJournal::new();
    let mut a = activate_req("profile-d");
    a.expected_generation = 0;
    assert!(matches!(
        submit(&mut journal, a),
        Err(ManagerApiError::BadRequest(_))
    ));
    let mut a = activate_req("profile-e");
    a.runtime_lease = None;
    assert!(matches!(
        submit(&mut journal, a),
        Err(ManagerApiError::BadRequest(_))
    ));
    // rollback も generation+lease 必須。M10。
    let mut rb = activate_req("profile-f");
    rb.kind = "rollback".to_string();
    rb.expected_generation = 0;
    assert!(matches!(
        submit(&mut journal, rb),
        Err(ManagerApiError::BadRequest(_))
    ));
}

/// 公開 DTO に secret / raw build log を含めない。M10。
#[test]
fn dto_excludes_secret_and_raw_build_log() {
    let mut journal = JobJournal::new();
    let id = submit(&mut journal, req("build", "src-c"))
        .expect("submit")
        .id;
    let dto = get(&journal, &id).expect("get");
    let json = serde_json::to_string(&dto).expect("json");
    assert!(!json.contains("secret"));
    assert!(!json.contains("api_key"));
    assert!(!json.contains("build_log"));
    assert!(!json.contains("token"));
    // 進行中の job は queue_depth に数えられる。M10。
    let s = status(&journal, None);
    assert_eq!(s.queue_depth, 1);
    assert_eq!(s.jobs.len(), 1);
}
