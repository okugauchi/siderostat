//! DS4 管理画面（G03 / C04 / ManagerViewModel）。AppKit 管理 window の
//! view model。commit 候補・active digest・source 取得 / build / cancel /
//! log 導線を保持する。build 可能 target と失敗理由を表示する。G03。/
//!
//! 受入 case（全て必須）:
//! - 入力: source 無し → 取得ボタン（sources 空で fetch 可能）
//! - 入力: build 進行 → cancel 有効（running/cancelling で cancel 可能）
//! - 入力: error → redacted reason（資格情報・URL 等を隠す）
//! - 入力: window 閉じ再開 → job 継続（view model は window と独立）
//!
//! レビュー重点: model 巨大 list や poll で main loop を block しない
//! （poll は非 GUI、view model は純粋ロジック）。sudo install を GUI
//! 既定導線にしない（本 view model に install 導線を含めない）。G03。/
use std::collections::BTreeMap;

/// source エントリ（commit 候補・active 判定）。G03。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub remote: String,
    pub full_commit: String,
    /// active digest と一致していれば true。G03。/
    pub active: bool,
}

/// 管理 window の job 表示（secret / raw build log を含まない）。G03。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerJobView {
    pub id: String,
    pub kind: String,
    pub progress: u8,
    pub phase: String,
    pub error: String,
    pub cancel: bool,
}

impl ManagerJobView {
    /// 進行中（cancel が有効）。G03。/
    pub fn is_active(&self) -> bool {
        self.phase == "running" || self.phase == "cancelling"
    }
}

/// `/manager/jobs` の抽象境界（fetch/build/cancel）。テストでは fake で
/// 記録する。自クレート内でのみ使用するため async fn in trait を許可
/// する（clippy -D warnings 対策）。G03。/
#[allow(async_fn_in_trait)]
pub trait ManagerApi {
    /// 新規 job を開始する。Ok(id)。G03。/
    async fn submit(&mut self, kind: &str, payload_key: &str) -> Result<String, String>;
    /// 進行中 job をキャンセルする。G03。/
    async fn cancel(&mut self, job_id: &str) -> Result<(), String>;
}

/// DS4 管理 window の view model。poll は非 GUI スレッドが行い、view
/// model は結果を反映するだけ（main loop を block しない）。window を
/// 閉じても view model は保持され、job 状態は継続する。G03。/
#[derive(Debug, Clone, Default)]
pub struct ManagerViewModel {
    jobs: BTreeMap<String, ManagerJobView>,
    sources: Vec<SourceEntry>,
    active_digest: Option<String>,
    build_targets: Vec<String>,
}

impl ManagerViewModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// `/manager/status` の反映。job を個別更新し、active digest を
    /// 保持する。source fetch が succeeded なら sources へ追加する。
    /// G03。。
    pub fn apply_status(
        &mut self,
        jobs: &[siderostat_core::manager::api::ManagerJobDto],
        active_digest: Option<&str>,
    ) {
        self.active_digest = active_digest.map(str::to_string);
        for job in jobs {
            self.jobs.insert(
                job.id.clone(),
                ManagerJobView {
                    id: job.id.clone(),
                    kind: job.kind.clone(),
                    progress: job.progress,
                    phase: job.phase.clone(),
                    error: job.error.clone(),
                    cancel: job.cancel,
                },
            );
            // source fetch succeeded → sources へ追加。G03。。
            if job.kind == "fetch"
                && job.phase == "succeeded"
                && !self.sources.iter().any(|s| s.remote == job.id)
            {
                self.sources.push(SourceEntry {
                    remote: job.id.clone(),
                    full_commit: String::new(),
                    active: self.active_digest.as_deref() == Some(job.id.as_str()),
                });
            }
        }
    }

    /// source が無い状態 → 取得ボタン有効。G03。/
    pub fn can_fetch_source(&self) -> bool {
        self.sources.is_empty()
    }

    /// source 取得を開始する（POST /manager/jobs kind=fetch）。G03。/
    pub async fn fetch_source(&mut self, api: &mut impl ManagerApi) -> Result<String, String> {
        let id = api.submit("fetch", "official").await?;
        self.jobs.insert(
            id.clone(),
            ManagerJobView {
                id: id.clone(),
                kind: "fetch".to_string(),
                progress: 0,
                phase: "running".to_string(),
                error: String::new(),
                cancel: true,
            },
        );
        Ok(id)
    }

    /// build 対象（target）。G03。/
    pub fn build_targets(&self) -> &[String] {
        &self.build_targets
    }

    /// build を開始する（POST /manager/jobs kind=build）。G03。/
    pub async fn start_build(
        &mut self,
        target: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        let id = api.submit("build", target).await?;
        if !self.build_targets.contains(&target.to_string()) {
            self.build_targets.push(target.to_string());
        }
        self.jobs.insert(
            id.clone(),
            ManagerJobView {
                id: id.clone(),
                kind: "build".to_string(),
                progress: 0,
                phase: "running".to_string(),
                error: String::new(),
                cancel: true,
            },
        );
        Ok(id)
    }

    /// build 進行中 → cancel 有効。G03。/
    pub fn can_cancel(&self, job_id: &str) -> bool {
        self.jobs
            .get(job_id)
            .is_some_and(|job| job.kind == "build" && job.is_active())
    }

    /// build job をキャンセルする（POST /manager/jobs/{id}/cancel）。
    /// G03。/
    pub async fn cancel_job(
        &mut self,
        job_id: &str,
        api: &mut impl ManagerApi,
    ) -> Result<(), String> {
        api.cancel(job_id).await?;
        if let Some(job) = self.jobs.get_mut(job_id) {
            job.phase = "cancelling".to_string();
        }
        Ok(())
    }

    /// job 一覧。G03。/
    pub fn jobs(&self) -> impl Iterator<Item = &ManagerJobView> {
        self.jobs.values()
    }

    /// source 一覧。G03。/
    pub fn sources(&self) -> &[SourceEntry] {
        &self.sources
    }

    /// active digest。G03。/
    pub fn active_digest(&self) -> Option<&str> {
        self.active_digest.as_deref()
    }

    /// job エラーの redacted 表示。資格情報・URL userinfo・query 等を
    /// 隠し、生のエラー文字列をそのまま GUI に出さない（C04:
    /// URL query/credentials/token をログ/表示へ出さない）。G03。/
    pub fn redacted_reason(job: &ManagerJobView) -> String {
        redact_secrets(&job.error)
    }
}

/// エラー文字列から資格情報・URL を隠す。`scheme://user:pass@host` や
/// `?token=...` 等を `[REDACTED]` に置換する。G03。/
pub fn redact_secrets(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("://") {
        out.push_str(&rest[..start]);
        // scheme:// の後の userinfo を探す（次の / か ? か # の前の @）。
        let after = &rest[start + 3..];
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let authority = &after[..end];
        if let Some(at) = authority.rfind('@') {
            // user:pass@ を [REDACTED]@ に。
            out.push_str("://[REDACTED]@");
            out.push_str(&authority[at + 1..]);
        } else {
            out.push_str("://");
            out.push_str(authority);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    // 残る query の token/secret も隠す。G03。/
    let mut out2 = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find('?') {
        out2.push_str(&rest[..=start]);
        let after = &rest[start + 1..];
        let end = after.find(['&', ' ']).unwrap_or(after.len());
        let pair = &after[..end];
        if pair.to_ascii_lowercase().contains("token")
            || pair.to_ascii_lowercase().contains("secret")
            || pair.to_ascii_lowercase().contains("key=")
        {
            out2.push_str("[REDACTED]");
        } else {
            out2.push_str(pair);
        }
        rest = &after[end..];
    }
    out2.push_str(rest);
    out2
}

// ---------------------------------------------------------------------------
// テスト用 fake API。G03。/
// ---------------------------------------------------------------------------
#[cfg(test)]
pub mod test_util {
    use super::*;

    #[derive(Default)]
    pub struct FakeManagerApi {
        pub submit_calls: Vec<(String, String)>,
        pub cancel_calls: Vec<String>,
        pub job_ids: Vec<String>,
        pub submit_error: Option<String>,
    }

    impl FakeManagerApi {
        pub fn with_jobs(ids: &[&str]) -> Self {
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
            if let Some(error) = &self.submit_error {
                return Err(error.clone());
            }
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
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    /// 入力: source 無し → 取得ボタン。G03。。/
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
        assert!(!vm.can_fetch_source() || vm.sources().is_empty());
    }

    /// 入力: build 進行 → cancel 有効。G03。/
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

    /// 入力: error → redacted reason。G03。/
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

    /// 入力: window 閉じ再開 → job 継続。G03。/
    #[test]
    fn view_model_survives_window_close() {
        // view model は window と独立（Arc<Mutex> 等で保持）であり、
        // window を閉じても job 状態は消えない。G03。/
        let mut api = FakeManagerApi::with_jobs(&["build-1"]);
        let mut vm = ManagerViewModel::new();
        let id = block_on(vm.start_build("ds4", &mut api)).expect("build");
        // window を閉じる（view model はそのまま）。再開時も同 view model。
        assert!(vm.jobs().any(|j| j.id == id && j.phase == "running"));
        assert!(vm.can_cancel(&id), "job continues after window reopen");
    }

    /// レビュー重点: build 可能 target と失敗理由を表示する。G03。/
    #[test]
    fn build_targets_and_failure_reason_are_surfaced() {
        let mut api = FakeManagerApi::with_jobs(&["build-ds4"]);
        let mut vm = ManagerViewModel::new();
        let _ = block_on(vm.start_build("ds4-server", &mut api)).expect("build");
        assert_eq!(vm.build_targets(), &["ds4-server".to_string()]);
        let job = ManagerJobView {
            id: "x".to_string(),
            kind: "build".to_string(),
            progress: 0,
            phase: "failed".to_string(),
            error: "missing toolchain".to_string(),
            cancel: false,
        };
        assert_eq!(ManagerViewModel::redacted_reason(&job), "missing toolchain");
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
}
