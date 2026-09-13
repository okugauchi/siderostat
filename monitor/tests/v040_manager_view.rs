//! G03 / C04 / ManagerViewModel: DS4 管理画面の受入 case を公開 API
//! （ManagerViewModel / ManagerApi）経由で検証する。実 network は使わず、
//! fake ManagerApi 境界で fetch/build/cancel を記録する。レビュー重点:
//! view model は純粋ロジック（poll は非 GUI、main loop を block しない）、
//! sudo install 導線を含めない。
use siderostat_core::manager::api::ManagerJobDto;
use siderostat_monitor::manager_window::{
    ManagerApi, ManagerJobView, ManagerViewModel, redact_secrets,
};

/// POST /manager/jobs と cancel を記録する fake API。G03。/
#[derive(Default)]
struct FakeManagerApi {
    submit_calls: Vec<(String, String)>,
    cancel_calls: Vec<String>,
    job_ids: Vec<String>,
}

impl FakeManagerApi {
    fn with_jobs(ids: &[&str]) -> Self {
        Self {
            job_ids: ids.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
}

impl ManagerApi for FakeManagerApi {
    async fn submit(&mut self, kind: &str, payload_key: &str) -> Result<String, String> {
        self.submit_calls
            .push((kind.to_string(), payload_key.to_string()));
        let id = self
            .job_ids
            .get(self.submit_calls.len() - 1)
            .cloned()
            .unwrap_or_else(|| format!("job-{}", self.submit_calls.len()));
        Ok(id)
    }

    async fn cancel(&mut self, job_id: &str) -> Result<(), String> {
        self.cancel_calls.push(job_id.to_string());
        Ok(())
    }
}

fn block_on<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    runtime.block_on(future)
}

fn dto(id: &str, kind: &str, phase: &str, error: &str, cancel: bool) -> ManagerJobDto {
    ManagerJobDto {
        id: id.to_string(),
        kind: kind.to_string(),
        progress: 0,
        phase: phase.to_string(),
        error: error.to_string(),
        created_at: 0,
        updated_at: 0,
        cancel,
    }
}

/// 受入 case 1: source 無し → 取得ボタン。G03。/
#[test]
fn empty_sources_enable_fetch_button() {
    let mut api = FakeManagerApi::with_jobs(&["fetch-1"]);
    let mut vm = ManagerViewModel::new();
    assert!(vm.can_fetch_source(), "no source -> fetch enabled");
    let id = block_on(vm.fetch_source(&mut api)).expect("fetch");
    assert_eq!(id, "fetch-1");
    assert_eq!(
        api.submit_calls,
        vec![("fetch".to_string(), "official".to_string())]
    );
    assert!(vm.jobs().any(|j| j.id == "fetch-1" && j.phase == "running"));
}

/// 受入 case 2: build 進行 → cancel 有効。G03。/
#[test]
fn running_build_enables_cancel() {
    let mut api = FakeManagerApi::with_jobs(&["build-1"]);
    let mut vm = ManagerViewModel::new();
    let id = block_on(vm.start_build("ds4", &mut api)).expect("build");
    assert!(vm.can_cancel(&id));
    block_on(vm.cancel_job(&id, &mut api)).expect("cancel");
    assert_eq!(api.cancel_calls, vec!["build-1".to_string()]);
    assert_eq!(vm.jobs().find(|j| j.id == id).unwrap().phase, "cancelling");
}

/// 受入 case 3: error → redacted reason。G03。/
#[test]
fn error_is_redacted() {
    let job = ManagerJobView {
        id: "b".to_string(),
        kind: "build".to_string(),
        progress: 0,
        phase: "failed".to_string(),
        error: "fetch failed for https://user:secret@example.com/repo?token=abc123".to_string(),
        cancel: false,
    };
    let redacted = ManagerViewModel::redacted_reason(&job);
    assert!(
        !redacted.contains("secret"),
        "password must be hidden: {redacted}"
    );
    assert!(
        !redacted.contains("abc123"),
        "token must be hidden: {redacted}"
    );
    assert!(
        redacted.contains("[REDACTED]"),
        "redaction marker present: {redacted}"
    );
    assert!(
        redacted.contains("example.com"),
        "host preserved: {redacted}"
    );
}

/// 受入 case 4: window 閉じ再開 → job 継続。G03。/
#[test]
fn view_model_survives_window_close() {
    let mut api = FakeManagerApi::with_jobs(&["build-1"]);
    let mut vm = ManagerViewModel::new();
    let id = block_on(vm.start_build("ds4", &mut api)).expect("build");
    // window を閉じても view model は保持され、再開時に job は継続。G03。/
    assert!(vm.jobs().any(|j| j.id == id && j.phase == "running"));
    assert!(vm.can_cancel(&id), "job continues after window reopen");
}

/// apply_status が /manager/status（ManagerJobDto）を個別反映し、
/// active digest と build 可能 target / 失敗理由を保持する。G03。/
#[test]
fn apply_status_reflects_jobs_and_active_digest() {
    let mut vm = ManagerViewModel::new();
    let jobs = vec![
        dto("fetch-1", "fetch", "succeeded", "", false),
        dto("build-1", "build", "failed", "missing toolchain", false),
    ];
    vm.apply_status(&jobs, Some("fetch-1"));
    assert_eq!(vm.active_digest(), Some("fetch-1"));
    assert!(
        vm.sources()
            .iter()
            .any(|s| s.remote == "fetch-1" && s.active)
    );
    let build = vm.jobs().find(|j| j.id == "build-1").unwrap();
    assert_eq!(build.phase, "failed");
    assert_eq!(
        ManagerViewModel::redacted_reason(build),
        "missing toolchain"
    );
}

/// redact_secrets が URL userinfo / query token を隠し、host を残す。G03。/
#[test]
fn redact_secrets_hides_credentials_keeps_host() {
    let redacted = redact_secrets("https://alice:pw@git.example.com/x.git?api_key=k123&ref=main");
    assert!(!redacted.contains("pw"));
    assert!(!redacted.contains("k123"));
    assert!(redacted.contains("git.example.com"));
    assert!(redacted.contains("[REDACTED]"));
}
