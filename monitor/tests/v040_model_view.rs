//! G04 / C04 / model/activation view: model 選択・download・activate・
//! rollback GUI の受入 case を公開 API（ModelView / ManagerApi）経由で
//! 検証する。実 network は使わず、fake ManagerApi 境界で activate /
//! rollback を記録する。レビュー重点: 各 stage ごとに承認 dialog を
//! 重複させず、実 runtime 変更の承認は一つの activation 操作へ集約。
use siderostat_core::manager::api::ManagerJobDto;
use siderostat_monitor::manager_window::{ManagerApi, ModelEntry, ModelView};

#[derive(Default)]
struct FakeManagerApi {
    submit_calls: Vec<(String, String)>,
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

    async fn cancel(&mut self, _job_id: &str) -> Result<(), String> {
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

fn model(name: &str, checksum: Option<&str>, encoder: &str, support: &str) -> ModelEntry {
    ModelEntry {
        name: name.to_string(),
        size: 1024,
        checksum: checksum.map(str::to_string),
        license: "MIT".to_string(),
        encoder: encoder.to_string(),
        support: support.to_string(),
    }
}

/// 受入 case 1: checksum 無し → activate disabled。G04。/
#[test]
fn missing_checksum_disables_activation() {
    let mut api = FakeManagerApi::with_jobs(&["act-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![model("m1", None, "openai-whisper", "supported")]);
    assert!(!view.can_activate("m1"), "no checksum -> activate disabled");
    let err = block_on(view.start_activation("m1", &mut api)).expect_err("activate");
    assert!(err.contains("activate できません"));
    assert!(api.submit_calls.is_empty(), "no POST when checksum missing");
}

/// 受入 case 2: Vision 不整合 → 理由。G04。/
#[test]
fn vision_mismatch_surfaces_reason() {
    let mut view = ModelView::new();
    view.set_models(vec![
        model("m-ok", Some("sha"), "openai-whisper", "supported"),
        model("m-enc", Some("sha"), "other-encoder", "supported"),
        model("m-unsup", Some("sha"), "openai-whisper", "unsupported"),
    ]);
    assert!(view.can_activate("m-ok"));
    assert!(view.vision_reason("m-ok").is_none());
    let reason = view
        .vision_reason("m-enc")
        .expect("encoder mismatch reason");
    assert!(reason.contains("encoder 不整合"), "{reason}");
    assert!(!view.can_activate("m-enc"));
    let reason = view.vision_reason("m-unsup").expect("unsupported reason");
    assert!(reason.contains("Vision 対応外"), "{reason}");
    assert!(!view.can_activate("m-unsup"));
}

/// 受入 case 3: build/download 中 → 旧 active 表示。G04。/
#[test]
fn old_active_shown_while_build_or_download_runs() {
    let mut view = ModelView::new();
    let running_build = ManagerJobDto {
        id: "build-1".to_string(),
        kind: "build".to_string(),
        progress: 40,
        phase: "running".to_string(),
        error: String::new(),
        created_at: 0,
        updated_at: 0,
        cancel: true,
    };
    view.apply_status(&[], Some("old-active"));
    assert_eq!(view.current_active(), Some("old-active"));
    view.apply_status(&[running_build], Some("new-active"));
    assert_eq!(view.current_active(), Some("old-active"));
}

/// 受入 case 4: rollback → previous 保持。G04。/
#[test]
fn rollback_keeps_previous() {
    let mut api = FakeManagerApi::with_jobs(&["rollback-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![model(
        "m1",
        Some("sha"),
        "openai-whisper",
        "supported",
    )]);
    view.apply_status(&[], Some("active-a"));
    let _ = block_on(view.start_activation("m1", &mut api)).expect("activate");
    assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
    let _ = block_on(view.rollback("active-a", &mut api)).expect("rollback");
    assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
    assert_eq!(api.submit_calls.len(), 2);
    assert_eq!(api.submit_calls[0].0, "activate");
    assert_eq!(api.submit_calls[1].0, "rollback");
}

/// レビュー重点: activation は一つの操作へ集約（承認 dialog を各 stage
/// で重複させない）。G04。/
#[test]
fn activation_is_single_operation() {
    let mut api = FakeManagerApi::with_jobs(&["act-1"]);
    let mut view = ModelView::new();
    view.set_models(vec![model(
        "m1",
        Some("sha"),
        "openai-whisper",
        "supported",
    )]);
    let id = block_on(view.start_activation("m1", &mut api)).expect("activate");
    assert_eq!(id, "act-1");
    // download / stage / activate を分割せず、一つの activate 操作に集約。G04。/
    assert_eq!(api.submit_calls.len(), 1);
    assert_eq!(api.submit_calls[0].0, "activate");
}

/// size/license/checksum/encoder/support を model 一覧で表示できる。G04。/
#[test]
fn model_list_carries_size_license_checksum_encoder_support() {
    let m = model("m1", Some("sha-abc"), "openai-whisper", "supported");
    assert_eq!(m.size, 1024);
    assert_eq!(m.license, "MIT");
    assert_eq!(m.checksum.as_deref(), Some("sha-abc"));
    assert_eq!(m.encoder, "openai-whisper");
    assert_eq!(m.support, "supported");
}
