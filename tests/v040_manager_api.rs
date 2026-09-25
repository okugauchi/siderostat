//! DS4 Manager API（M10 / C04）の受入テスト。
//!
//! 公開 API（`manager::api`）を介して C04 の ManagerApi 契約を検証する。
//! dry-run 方針に従い、実プロセス・実ネットワークを使わず JobJournal を
//! fake 境界として駆動する。secret / raw build log を公開 DTO に含めない
//! ことも確認する。M10。
use siderostat::manager::{
    JobJournal, JobKind, JobPhase, ManagerJob,
    api::{
        JobSubmitRequest, ManagerApiError, cancel, get, parse_kind, status, submit, submit_json,
    },
};

#[cfg(feature = "test-support")]
mod routes {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use siderostat::{
        app::{AppState, admin_router},
        cluster::{AdminAction, AdminController, AdminExecutor, AdminFuture, encode_token},
        config::ModeAwareConfig,
        manager::executor::{
            FixtureManagerBackend, ManagerExecutionBackend, ManagerExecutionError,
            ManagerExecutionOutcome, ManagerExecutionRequest, RuntimeManagerBackend,
        },
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    struct UnusedAdminExecutor;

    impl AdminExecutor for UnusedAdminExecutor {
        fn execute(&self, _action: AdminAction) -> AdminFuture {
            Box::pin(async { Ok(serde_json::json!({})) })
        }
    }

    fn state_with_backend<B: ManagerExecutionBackend>(backend: B) -> Arc<AppState> {
        let mut config = ModeAwareConfig::parse(include_str!("../siderostat.example.toml"))
            .expect("parse test config");
        config.cluster.enabled = false;
        let admin =
            AdminController::new(vec![3; 32], Arc::new(UnusedAdminExecutor)).expect("admin");
        AppState::from_config_with_manager_backend(config, backend, admin).expect("manager state")
    }

    fn state() -> Arc<AppState> {
        state_with_backend(FixtureManagerBackend::new())
    }

    struct WaitForCancelBackend {
        started: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    impl ManagerExecutionBackend for WaitForCancelBackend {
        fn execute(
            &self,
            _request: ManagerExecutionRequest,
            cancel: Arc<AtomicBool>,
        ) -> Result<ManagerExecutionOutcome, ManagerExecutionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.store(true, Ordering::SeqCst);
            while !cancel.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // Even a late success after cancellation must remain failed.
            Ok(ManagerExecutionOutcome { progress: 100 })
        }
    }

    async fn request(
        state: Arc<AppState>,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let bearer = format!("Bearer {}", encode_token(&[3; 32]));
        let response = admin_router(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", bearer)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("route");
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        (status, serde_json::from_slice(&body).expect("json"))
    }

    #[tokio::test]
    async fn routes_preserve_admin_auth_and_strict_payload() {
        let state = state();
        let unauthenticated = admin_router(state.clone())
            .oneshot(
                Request::post("/manager/jobs")
                    .body(Body::from(
                        r#"{"kind":"fetch","payload_key":"fixture-fetch"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("route");
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        let (invalid_status, _) = request(
            state,
            "POST",
            "/manager/jobs",
            r#"{"kind":"fetch","payload_key":"fixture-fetch","unknown":true}"#,
        )
        .await;
        assert_eq!(invalid_status, StatusCode::BAD_REQUEST);
    }

    async fn terminal(state: Arc<AppState>, id: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let (status, job) =
                request(state.clone(), "GET", &format!("/manager/jobs/{id}"), "").await;
            assert_eq!(status, StatusCode::OK);
            if matches!(job["phase"].as_str(), Some("succeeded" | "failed")) {
                return job;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "job stayed nonterminal: {job}"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn fixture_submit_reaches_terminal_through_routes() {
        let state = state();
        let (status, body) = request(
            state.clone(),
            "POST",
            "/manager/jobs",
            r#"{"kind":"fetch","payload_key":"fixture-fetch"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = body["id"].as_str().expect("job id");
        let job = terminal(state.clone(), id).await;
        assert_eq!(job["phase"], "succeeded");
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/manager/jobs/{id}/cancel"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, after_cancel) = request(state, "GET", &format!("/manager/jobs/{id}"), "").await;
        assert_eq!(after_cancel["phase"], "succeeded");
    }

    #[tokio::test]
    async fn duplicate_submit_runs_once_and_route_cancel_wins() {
        let started = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let state = state_with_backend(WaitForCancelBackend {
            started: started.clone(),
            calls: calls.clone(),
        });
        let body = r#"{"kind":"fetch","payload_key":"fixture-fetch"}"#;
        let (first_status, first) = request(state.clone(), "POST", "/manager/jobs", body).await;
        assert_eq!(first_status, StatusCode::ACCEPTED);
        let id = first["id"].as_str().expect("id").to_string();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend started");
        let (duplicate_status, duplicate) =
            request(state.clone(), "POST", "/manager/jobs", body).await;
        assert_eq!(duplicate_status, StatusCode::ACCEPTED);
        assert_eq!(duplicate["id"], id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (cancel_status, _) = request(
            state.clone(),
            "POST",
            &format!("/manager/jobs/{id}/cancel"),
            "",
        )
        .await;
        assert_eq!(cancel_status, StatusCode::OK);
        let job = terminal(state, &id).await;
        assert_eq!(job["phase"], "failed");
        assert_eq!(job["cancel"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closed_queue_fails_only_new_job() {
        let state = state();
        state.shutdown_manager_executor_for_test();
        let (status, body) = request(
            state.clone(),
            "POST",
            "/manager/jobs",
            r#"{"kind":"fetch","payload_key":"fixture-fetch"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = body["id"].as_str().expect("job id");
        assert_eq!(body.as_object().expect("response object").len(), 1);
        let (get_status, job) =
            request(state.clone(), "GET", &format!("/manager/jobs/{id}"), "").await;
        assert_eq!(get_status, StatusCode::OK);
        assert_eq!(job["phase"], "failed");
        assert_eq!(job["error"], "manager executor queue is closed");
        let (_, status_body) = request(state, "GET", "/manager/status", "").await;
        assert_eq!(status_body["queue_depth"], 0);
        assert_eq!(status_body["jobs"][0]["phase"], "failed");
    }

    #[tokio::test]
    async fn persistence_failure_returns_503_without_publishing_a_job() {
        let state = state();
        let store = state.manager_store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = store.lock().expect("store lock");
            panic!("poison store lock for persistence failure test");
        })
        .join();

        let (status, body) = request(
            state.clone(),
            "POST",
            "/manager/jobs",
            r#"{"kind":"fetch","payload_key":"must-not-publish"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.get("id").is_none());
        let (_, status_body) = request(state, "GET", "/manager/status", "").await;
        assert_eq!(status_body["jobs"].as_array().expect("jobs").len(), 0);
    }

    #[tokio::test]
    async fn production_backend_with_no_registered_plan_never_succeeds() {
        let state = state_with_backend(RuntimeManagerBackend::without_model_catalog());
        let (status, body) = request(
            state.clone(),
            "POST",
            "/manager/jobs",
            r#"{"kind":"activate","payload_key":"unknown","expected_generation":1,"runtime_lease":"lease"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = body["id"].as_str().expect("id");
        let job = terminal(state, id).await;
        assert_eq!(job["phase"], "failed");
    }
}

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

#[test]
fn interrupted_job_dto_is_explicit_and_contains_no_raw_output_fields() {
    let job = ManagerJob {
        id: "verify-7".into(),
        kind: JobKind::Verify,
        progress: 37,
        phase: JobPhase::Interrupted,
        error: "manager process restarted; work was not resumed".into(),
        created_at: 1,
        updated_at: 2,
        cancel: false,
    };
    let dto = siderostat::manager::api::ManagerJobDto::from(&job);
    let value = serde_json::to_value(dto).expect("serialize DTO");
    assert_eq!(value["phase"], "interrupted");
    assert_eq!(value.as_object().expect("object").len(), 8);
    let json = value.to_string();
    assert!(!json.contains("secret"));
    assert!(!json.contains("raw_output"));
    assert!(!json.contains("build_log"));
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

#[test]
fn cancel_does_not_reopen_a_completed_job() {
    let mut journal = JobJournal::new();
    let id = submit(&mut journal, req("fetch", "completed"))
        .expect("submit")
        .id;
    journal.succeed(&id).expect("complete");
    cancel(&mut journal, &id).expect("idempotent cancel");
    let dto = get(&journal, &id).expect("get");
    assert_eq!(dto.phase, "succeeded");
    assert!(!dto.cancel);
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
