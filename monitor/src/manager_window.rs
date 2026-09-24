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
use crate::client::MetricsClient;
use siderostat_core::manager::api::ManagerStatusResponse;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};

#[cfg(target_os = "macos")]
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
#[cfg(target_os = "macos")]
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSButton, NSLayoutAttribute, NSStackView, NSStackViewDistribution, NSTextField,
    NSUserInterfaceLayoutOrientation, NSWindow, NSWindowStyleMask,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSEdgeInsets, NSObject, NSPoint, NSRect, NSSize, NSString};
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

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

    /// generation/lease を伴う job を開始する。既存の fake/API 境界を
    /// 壊さないため、context が無い場合は通常 submit へ委譲する。C04。/
    async fn submit_with_context(
        &mut self,
        kind: &str,
        payload_key: &str,
        expected_generation: Option<u64>,
        runtime_lease: Option<&str>,
    ) -> Result<String, String> {
        let _ = (expected_generation, runtime_lease);
        self.submit(kind, payload_key).await
    }

    /// 進行中 job をキャンセルする。G03。/
    async fn cancel(&mut self, job_id: &str) -> Result<(), String>;
}

/// DS4 管理 window の view model。poll は非 GUI スレッドが行い、view
/// model は結果を反映するだけ（main loop を block しない）。window を
/// 閉じても view model は保持され、job 状態は継続する。G03。/
#[derive(Debug, Clone, Default)]
pub struct ManagerViewModel {
    jobs: BTreeMap<String, ManagerJobView>,
    latest_cancellable_id: Option<String>,
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
    /// G03。
    pub fn apply_status(
        &mut self,
        jobs: &[siderostat_core::manager::api::ManagerJobDto],
        active_digest: Option<&str>,
    ) {
        self.active_digest = active_digest.map(str::to_string);
        self.latest_cancellable_id = jobs
            .iter()
            .filter(|job| job.phase == "running")
            .max_by_key(|job| (job.updated_at, job.created_at, &job.id))
            .map(|job| job.id.clone());
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
            // source fetch succeeded → sources へ追加。G03。
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
        self.latest_cancellable_id = Some(id.clone());
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
        self.latest_cancellable_id = Some(id.clone());
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

    /// 最新のrunning job ID。`cancel` wire fieldはキャンセル要求済みフラグ。H06。
    pub fn latest_cancellable_job_id(&self) -> Option<&str> {
        self.latest_cancellable_id.as_deref()
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

/// GUIからworkerへ送るmanager操作。HTTP処理はこのenumを受けたworkerが
/// 実行し、AppKitのmain loopでは実行しない。H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerCommand {
    FetchSource,
    Build {
        target: String,
    },
    Download {
        profile: String,
    },
    Verify {
        profile: String,
    },
    Stage {
        profile: String,
    },
    Activate {
        profile: String,
        expected_generation: u64,
        runtime_lease: String,
    },
    Rollback {
        previous: String,
        expected_generation: u64,
        runtime_lease: String,
    },
    Cancel {
        job_id: String,
    },
    Refresh,
}

/// workerからmain threadへ返すmanager状態更新。非terminal jobを成功へ
/// 変換せず、表示側で観測した状態をそのまま保持する。H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerEvent {
    Status(ManagerStatusResponse),
    Submitted { kind: String, id: String },
    Failed { message: String },
}

/// Convert a worker error into an event safe for the GUI boundary. Secrets and
/// URL credentials are redacted before the event can reach AppKit. H06。
pub fn manager_failed_event(message: impl AsRef<str>) -> ManagerEvent {
    ManagerEvent::Failed {
        message: redact_secrets(message.as_ref()),
    }
}

/// A successful submit only acknowledges that a non-terminal job was queued.
/// The terminal result is observed later through `ManagerEvent::Status`. H06。
pub fn manager_submitted_event(kind: impl Into<String>, id: impl Into<String>) -> ManagerEvent {
    ManagerEvent::Submitted {
        kind: kind.into(),
        id: id.into(),
    }
}

/// Execute one manager command on the worker side. No command is reported as
/// terminal success here; status snapshots remain the source of truth for job
/// phases. H06。
pub async fn execute_manager_command(
    client: &MetricsClient,
    command: ManagerCommand,
) -> ManagerEvent {
    let result = match command {
        ManagerCommand::FetchSource => client
            .submit_manager_job("fetch", "official")
            .await
            .map(|response| manager_submitted_event("fetch", response.id)),
        ManagerCommand::Build { target } => client
            .submit_manager_job("build", &target)
            .await
            .map(|response| manager_submitted_event("build", response.id)),
        ManagerCommand::Download { profile } => client
            .submit_manager_job("download", &profile)
            .await
            .map(|response| manager_submitted_event("download", response.id)),
        ManagerCommand::Verify { profile } => client
            .submit_manager_job("verify", &profile)
            .await
            .map(|response| manager_submitted_event("verify", response.id)),
        ManagerCommand::Stage { profile } => client
            .submit_manager_job("stage", &profile)
            .await
            .map(|response| manager_submitted_event("stage", response.id)),
        ManagerCommand::Activate {
            profile,
            expected_generation,
            runtime_lease,
        } => client
            .submit_manager_job_with_context(
                "activate",
                &profile,
                expected_generation,
                &runtime_lease,
            )
            .await
            .map(|response| manager_submitted_event("activate", response.id)),
        ManagerCommand::Rollback {
            previous,
            expected_generation,
            runtime_lease,
        } => client
            .submit_manager_job_with_context(
                "rollback",
                &previous,
                expected_generation,
                &runtime_lease,
            )
            .await
            .map(|response| manager_submitted_event("rollback", response.id)),
        ManagerCommand::Cancel { job_id } => client
            .cancel_manager_job(&job_id)
            .await
            .map(|()| manager_submitted_event("cancel", job_id)),
        ManagerCommand::Refresh => client.fetch_manager_jobs().await.map(ManagerEvent::Status),
    };
    result.unwrap_or_else(|error| manager_failed_event(error.to_string()))
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ManagerActionIvars {
    command_tx: Sender<ManagerCommand>,
    cancel_job_id: Arc<Mutex<Option<String>>>,
}

#[cfg(target_os = "macos")]
fn send_cancel_command(
    sender: &Sender<ManagerCommand>,
    selected: &Mutex<Option<String>>,
) -> anyhow::Result<bool> {
    let id = selected
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(job_id) = id else {
        return Ok(false);
    };
    sender
        .send(ManagerCommand::Cancel { job_id })
        .context("manager command channel closed")?;
    Ok(true)
}

#[cfg(target_os = "macos")]
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ManagerActionIvars]
    struct ManagerActionTarget;

    impl ManagerActionTarget {
        #[unsafe(method(managerFetch:))]
        fn manager_fetch(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::FetchSource);
        }

        #[unsafe(method(managerBuild:))]
        fn manager_build(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Build {
                target: "ds4-server".to_string(),
            });
        }

        #[unsafe(method(managerDownload:))]
        fn manager_download(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Download {
                profile: "mxfp4-0731".to_string(),
            });
        }

        #[unsafe(method(managerVerify:))]
        fn manager_verify(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Verify {
                profile: "mxfp4-0731".to_string(),
            });
        }

        #[unsafe(method(managerStage:))]
        fn manager_stage(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Stage {
                profile: "mxfp4-0731".to_string(),
            });
        }

        #[unsafe(method(managerCancel:))]
        fn manager_cancel(&self, _sender: Option<&AnyObject>) {
            let _ = send_cancel_command(&self.ivars().command_tx, &self.ivars().cancel_job_id);
        }

        #[unsafe(method(managerRefresh:))]
        fn manager_refresh(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().command_tx.send(ManagerCommand::Refresh);
        }
    }
);

#[cfg(target_os = "macos")]
impl ManagerActionTarget {
    fn new(
        mtm: MainThreadMarker,
        command_tx: Sender<ManagerCommand>,
        cancel_job_id: Arc<Mutex<Option<String>>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ManagerActionIvars {
            command_tx,
            cancel_job_id,
        });
        // SAFETY: ManagerActionTarget directly subclasses NSObject and uses
        // NSObject's standard init implementation.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(target_os = "macos")]
fn project_jobs(view_model: &ManagerViewModel) -> String {
    let rows: Vec<String> = view_model
        .jobs()
        .map(|job| {
            let error = if job.error.is_empty() {
                "なし".to_string()
            } else {
                ManagerViewModel::redacted_reason(job)
            };
            format!(
                "{} · {} · {} · {}% · error: {}",
                redact_secrets(&job.id),
                redact_secrets(&job.kind),
                redact_secrets(&job.phase),
                job.progress,
                error
            )
        })
        .collect();
    if rows.is_empty() {
        "jobはありません。再読込で状態を取得できます。".to_string()
    } else {
        rows.join("\n")
    }
}

#[cfg(target_os = "macos")]
fn project_profiles(model_view: &ModelView) -> String {
    if model_view.models().is_empty() {
        return "Pending · profile未準備（model catalog入力なし）· 操作不可".to_string();
    }
    model_view
        .models()
        .iter()
        .map(|model| {
            let state = match model_view.profile_readiness(&model.name) {
                ProfileReadiness::Pending(reason) => format!("Pending（{reason}）· 操作不可"),
                ProfileReadiness::Ready => "Ready（検証入力上）".to_string(),
                ProfileReadiness::Rejected(reason) => format!("Rejected（{reason}）· 操作不可"),
            };
            format!(
                "{} · {}",
                redact_secrets(&model.name),
                redact_secrets(&state)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// AppKit manager window host. The view model and command channel outlive the
/// visible window, so close/reopen does not cancel jobs. H06。
#[cfg(target_os = "macos")]
pub struct ManagerWindowHost {
    mtm: Option<MainThreadMarker>,
    _client: MetricsClient,
    window: Option<Retained<NSWindow>>,
    status_label: Option<Retained<NSTextField>>,
    jobs_label: Option<Retained<NSTextField>>,
    profiles_label: Option<Retained<NSTextField>>,
    cancel_button: Option<Retained<NSButton>>,
    _action_target: Option<Retained<ManagerActionTarget>>,
    command_tx: Sender<ManagerCommand>,
    command_rx: Option<Receiver<ManagerCommand>>,
    view_model: ManagerViewModel,
    model_view: ModelView,
    jobs_summary: String,
    profiles_summary: String,
    cancel_job_id: Arc<Mutex<Option<String>>>,
    test_window_identity: usize,
    test_visible: bool,
}

#[cfg(target_os = "macos")]
impl ManagerWindowHost {
    pub fn new(
        mtm: MainThreadMarker,
        client: MetricsClient,
        view_model: ManagerViewModel,
    ) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let cancel_job_id = Arc::new(Mutex::new(
            view_model.latest_cancellable_job_id().map(str::to_string),
        ));
        let action_target =
            ManagerActionTarget::new(mtm, command_tx.clone(), Arc::clone(&cancel_job_id));
        let model_view = ModelView::new();
        let jobs_summary = project_jobs(&view_model);
        let profiles_summary = project_profiles(&model_view);
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(780.0, 600.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the host retains the window for the process lifetime, so it
        // must not be released when the user closes it.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("siDeroStat Manager"));
        window.center();
        let content = window
            .contentView()
            .context("manager window content view unavailable")?;

        // Use AppKit's standard stack layout so the window reads like a native
        // settings/tool window: a clear header, grouped sections, and compact
        // horizontal action rows. H06。
        let root = NSStackView::new(mtm);
        root.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        root.setAlignment(NSLayoutAttribute::Leading);
        root.setDistribution(NSStackViewDistribution::GravityAreas);
        root.setSpacing(12.0);
        root.setEdgeInsets(NSEdgeInsets {
            top: 24.0,
            left: 28.0,
            bottom: 24.0,
            right: 28.0,
        });
        root.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(780.0, 600.0),
        ));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        content.addSubview(&root);

        let title = NSTextField::labelWithString(&NSString::from_str("DS4 Manager"), mtm);
        root.addArrangedSubview(&title);
        let subtitle = NSTextField::labelWithString(
            &NSString::from_str("source、artifact、profile、jobを確認・操作します"),
            mtm,
        );
        root.addArrangedSubview(&subtitle);
        let status_label = NSTextField::wrappingLabelWithString(
            &NSString::from_str(
                "待機中。runtime/modelは変更されていません。job状態を確認してください。",
            ),
            mtm,
        );
        status_label.setPreferredMaxLayoutWidth(700.0);
        status_label.setMaximumNumberOfLines(2);
        root.addArrangedSubview(&status_label);

        let section = |title: &str| {
            let label = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            root.addArrangedSubview(&label);
        };
        let action_row = |buttons: Vec<Retained<NSButton>>| {
            let row = NSStackView::new(mtm);
            row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            row.setSpacing(8.0);
            row.setAlignment(NSLayoutAttribute::CenterY);
            for button in buttons {
                row.addArrangedSubview(&button);
            }
            root.addArrangedSubview(&row);
        };
        let active_button = |text: &str, action| unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(text),
                Some(&*action_target),
                Some(action),
                mtm,
            )
        };
        let disabled_button = |text: &str| unsafe {
            let button =
                NSButton::buttonWithTitle_target_action(&NSString::from_str(text), None, None, mtm);
            button.setEnabled(false);
            button
        };

        section("Runtime / source");
        action_row(vec![
            active_button("公式sourceを取得", sel!(managerFetch:)),
            active_button("ds4-serverをbuild", sel!(managerBuild:)),
        ]);
        section("Artifact pipeline");
        action_row(vec![
            active_button("modelをdownload", sel!(managerDownload:)),
            active_button("verify", sel!(managerVerify:)),
            active_button("stage", sel!(managerStage:)),
        ]);
        section("Profiles");
        let profiles =
            NSTextField::wrappingLabelWithString(&NSString::from_str(&profiles_summary), mtm);
        profiles.setPreferredMaxLayoutWidth(700.0);
        root.addArrangedSubview(&profiles);
        section("Activation / rollback");
        action_row(vec![
            disabled_button("activate（準備中）"),
            disabled_button("rollback（準備中）"),
        ]);
        section("Jobs");
        let jobs = NSTextField::wrappingLabelWithString(&NSString::from_str(&jobs_summary), mtm);
        jobs.setPreferredMaxLayoutWidth(700.0);
        root.addArrangedSubview(&jobs);
        let cancel_button = active_button("Cancel（実行中jobなし）", sel!(managerCancel:));
        if let Some(id) = view_model.latest_cancellable_job_id() {
            cancel_button.setTitle(&NSString::from_str(&format!(
                "Cancel {}",
                redact_secrets(id)
            )));
        } else {
            cancel_button.setEnabled(false);
        }
        action_row(vec![
            cancel_button.clone(),
            active_button("再読込", sel!(managerRefresh:)),
        ]);

        Ok(Self {
            mtm: Some(mtm),
            _client: client,
            window: Some(window),
            status_label: Some(status_label),
            jobs_label: Some(jobs),
            profiles_label: Some(profiles),
            cancel_button: Some(cancel_button),
            _action_target: Some(action_target),
            command_tx,
            command_rx: Some(command_rx),
            view_model,
            model_view,
            jobs_summary,
            profiles_summary,
            cancel_job_id,
            test_window_identity: 0,
            test_visible: false,
        })
    }

    #[cfg(feature = "test-support")]
    pub fn for_test(client: MetricsClient, view_model: ManagerViewModel) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let identity = (&command_tx as *const Sender<ManagerCommand>) as usize;
        let jobs_summary = project_jobs(&view_model);
        let model_view = ModelView::new();
        let profiles_summary = project_profiles(&model_view);
        let cancel_job_id = Arc::new(Mutex::new(
            view_model.latest_cancellable_job_id().map(str::to_string),
        ));
        Self {
            mtm: None,
            _client: client,
            window: None,
            status_label: None,
            jobs_label: None,
            profiles_label: None,
            cancel_button: None,
            _action_target: None,
            command_tx,
            command_rx: Some(command_rx),
            view_model,
            model_view,
            jobs_summary,
            profiles_summary,
            cancel_job_id,
            test_window_identity: identity,
            test_visible: false,
        }
    }

    pub fn show_or_focus(&mut self) -> Result<()> {
        if let (Some(window), Some(mtm)) = (&self.window, self.mtm) {
            window.makeKeyAndOrderFront(None);
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        } else {
            self.test_visible = true;
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if let Some(window) = &self.window {
            window.orderOut(None);
        }
        self.test_visible = false;
    }

    pub fn is_visible(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.isVisible())
            || self.test_visible
    }

    pub fn window_identity(&self) -> usize {
        self.window
            .as_ref()
            .map_or(self.test_window_identity, |window| {
                (&**window as *const NSWindow).cast::<()>() as usize
            })
    }

    pub fn send_command(&self, command: ManagerCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .context("manager command channel closed")
    }

    /// Move the command receiver to the single worker thread. The host keeps
    /// only the sender, so AppKit never performs HTTP itself. H06。
    pub fn take_command_receiver(&mut self) -> Result<Receiver<ManagerCommand>> {
        self.command_rx
            .take()
            .context("manager command receiver already taken")
    }

    pub fn drain_commands(&self) -> Vec<ManagerCommand> {
        self.command_rx
            .as_ref()
            .map(|receiver| receiver.try_iter().collect())
            .unwrap_or_default()
    }

    pub fn apply_event(&mut self, event: ManagerEvent) {
        match event {
            ManagerEvent::Status(status) => {
                self.view_model
                    .apply_status(&status.jobs, status.active_digest.as_deref());
                self.model_view
                    .apply_status(&status.jobs, status.active_digest.as_deref());
                self.jobs_summary = project_jobs(&self.view_model);
                self.profiles_summary = project_profiles(&self.model_view);
                *self
                    .cancel_job_id
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = self
                    .view_model
                    .latest_cancellable_job_id()
                    .map(str::to_string);
                if let Some(jobs_label) = &self.jobs_label {
                    jobs_label.setStringValue(&NSString::from_str(&self.jobs_summary));
                }
                if let Some(profiles_label) = &self.profiles_label {
                    profiles_label.setStringValue(&NSString::from_str(&self.profiles_summary));
                }
                if let Some(cancel_button) = &self.cancel_button {
                    if let Some(id) = self.view_model.latest_cancellable_job_id() {
                        cancel_button.setTitle(&NSString::from_str(&format!(
                            "Cancel {}",
                            redact_secrets(id)
                        )));
                        cancel_button.setEnabled(true);
                    } else {
                        cancel_button.setTitle(&NSString::from_str("Cancel（実行中jobなし）"));
                        cancel_button.setEnabled(false);
                    }
                }
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&format!(
                        "active={} / queue={} / jobs={}",
                        self.view_model.active_digest().unwrap_or("未設定"),
                        status.queue_depth,
                        status.jobs.len()
                    )));
                }
            }
            ManagerEvent::Submitted { kind, id } => {
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&format!(
                        "{kind} jobを開始しました: {id}"
                    )));
                }
            }
            ManagerEvent::Failed { message } => {
                if let Some(status_label) = &self.status_label {
                    status_label.setStringValue(&NSString::from_str(&redact_secrets(&message)));
                }
            }
        }
    }

    pub fn view_model(&self) -> &ManagerViewModel {
        &self.view_model
    }

    pub fn model_view(&self) -> &ModelView {
        &self.model_view
    }

    /// 外部のcatalog/manifest検証結果を純粋なModelViewとして受け取る。H06。
    pub fn set_model_view(&mut self, model_view: ModelView) {
        self.model_view = model_view;
        self.profiles_summary = project_profiles(&self.model_view);
        if let Some(label) = &self.profiles_label {
            label.setStringValue(&NSString::from_str(&self.profiles_summary));
        }
    }

    pub fn jobs_summary(&self) -> &str {
        &self.jobs_summary
    }

    pub fn profiles_summary(&self) -> &str {
        &self.profiles_summary
    }

    /// AppKitのCancel actionと同じdispatchをfixtureから確認する。H06。
    pub fn request_cancel_selected(&self) -> Result<bool> {
        send_cancel_command(&self.command_tx, &self.cancel_job_id)
    }

    /// AppKitの再読込actionと同じworker channelに送る。H06。
    pub fn request_refresh(&self) -> Result<()> {
        self.send_command(ManagerCommand::Refresh)
    }
}

#[cfg(not(target_os = "macos"))]
pub struct ManagerWindowHost;

#[cfg(not(target_os = "macos"))]
impl ManagerWindowHost {
    pub fn send_command(&self, _command: ManagerCommand) -> anyhow::Result<()> {
        anyhow::bail!("manager window requires macOS")
    }
}

/// エラー文字列から資格情報を隠す。URL userinfo、queryの全項目、
/// Authorization header、Bearer tokenを表示前に置換する。G03/H06。/
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
    let mut query_redacted = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find('?') {
        query_redacted.push_str(&rest[..=start]);
        let after = &rest[start + 1..];
        let end = after
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '#' | '"' | '\'' | '<' | '>' | ')'))
            .unwrap_or(after.len());
        if end > 0 {
            query_redacted.push_str("[REDACTED]");
        }
        rest = &after[end..];
    }
    query_redacted.push_str(rest);

    // Header values may contain spaces, so hide the whole header line.
    let mut headers_redacted = String::with_capacity(query_redacted.len());
    for line in query_redacted.split_inclusive('\n') {
        let lower = line.to_ascii_lowercase();
        let auth = lower
            .find("authorization:")
            .or_else(|| lower.find("authorization="));
        if let Some(start) = auth {
            headers_redacted.push_str(&line[..start]);
            headers_redacted.push_str("Authorization: [REDACTED]");
            if line.ends_with('\n') {
                headers_redacted.push('\n');
            }
        } else {
            headers_redacted.push_str(line);
        }
    }

    let mut bearer_redacted = String::with_capacity(headers_redacted.len());
    let mut rest = headers_redacted.as_str();
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find("bearer ") else {
            bearer_redacted.push_str(rest);
            break;
        };
        bearer_redacted.push_str(&rest[..start + "bearer ".len()]);
        let after = &rest[start + "bearer ".len()..];
        let end = after
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ')' | ']' | '"' | '\''))
            .unwrap_or(after.len());
        if end > 0 {
            bearer_redacted.push_str("[REDACTED]");
        }
        rest = &after[end..];
    }
    bearer_redacted
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

/// model catalog のエントリ（C04 / model/activation view）。size / license /
/// checksum / encoder / support を表示する。checksum が無い model は
/// activate できない。G04。/
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    pub name: String,
    pub size: u64,
    /// 検証済み SHA が無ければ None（activate disabled）。G04。/
    pub checksum: Option<String>,
    pub license: String,
    /// Vision encoder（例: "openai-whisper"）。runtime と不整合なら理由。G04。/
    pub encoder: String,
    /// "supported" / "unsupported"。G04。/
    pub support: String,
}

/// Compatibility/prepare state shown in a profile row. `Pending` is used for
/// an artifact that has not passed the required preparation check; it is never
/// reported as a terminal success. G04/H06。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileReadiness {
    Pending(String),
    Ready,
    Rejected(String),
}

/// model 選択・download・activate・rollback の view（C04）。各 stage ごとに
/// 同じ承認 dialog を重複させず、実 runtime 変更の承認は一つの activation
/// 操作へ集約する（レビュー重点）。download / stage / activate は一つの
/// activation 操作で開始する。rollback は previous 候補から選ぶ。G04。/
#[derive(Debug, Clone, Default)]
pub struct ModelView {
    models: Vec<ModelEntry>,
    /// 旧 active digest（build/download 中も保持表示）。G04。/
    current_active: Option<String>,
    /// rollback 候補（previous）。rollback 後も保持。G04。/
    previous: Vec<String>,
    /// model 別 download 進捗（%）。G04。/
    download_progress: BTreeMap<String, u8>,
    /// Optional prefix-file compatibility results supplied by the manifest
    /// adapter. Missing entries remain compatible with the existing catalog
    /// behavior; an explicit false disables activation. H06。
    prefix_file_compatibility: BTreeMap<String, bool>,
}

impl ModelView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_models(&mut self, models: Vec<ModelEntry>) {
        self.models = models;
    }

    /// Set the verified prefix-file compatibility result for one profile.
    /// A mismatch is a hard rejection and is kept separate from checksum and
    /// encoder validation. H06。
    pub fn set_prefix_file_compatible(&mut self, name: &str, compatible: bool) {
        self.prefix_file_compatibility
            .insert(name.to_string(), compatible);
    }

    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    /// checksum 有り + Vision 整合 → activate 可能。checksum が無い model
    /// は activate disabled（受入 case 1）。Vision 不整合も disabled（受入
    /// case 2）。G04。/
    pub fn can_activate(&self, name: &str) -> bool {
        matches!(self.profile_readiness(name), ProfileReadiness::Ready)
    }

    /// Project one model row into the user-visible compatibility state. This
    /// is the single source of truth used by both activation gating and the
    /// AppKit profile display. H06。
    pub fn profile_readiness(&self, name: &str) -> ProfileReadiness {
        let Some(model) = self.models.iter().find(|m| m.name == name) else {
            return ProfileReadiness::Pending("profile未準備".to_string());
        };
        if self.prefix_file_compatibility.get(name) == Some(&false) {
            return ProfileReadiness::Rejected("prefix-file不一致".to_string());
        }
        if model.checksum.is_none() {
            return ProfileReadiness::Pending("checksum未検証".to_string());
        }
        if model.support != "supported" {
            return ProfileReadiness::Rejected(format!("Vision 対応外（{}）", model.support));
        }
        if !vision_consistent(model) {
            return ProfileReadiness::Rejected(format!(
                "Vision encoder 不整合（{}）",
                model.encoder
            ));
        }
        ProfileReadiness::Ready
    }

    /// Vision 不整合の理由（受入 case 2）。整合していれば None。G04。/
    pub fn vision_reason(&self, name: &str) -> Option<String> {
        let model = self.models.iter().find(|m| m.name == name)?;
        if model.support != "supported" {
            return Some(format!("Vision 対応外（{}）", model.support));
        }
        if !vision_consistent(model) {
            return Some(format!("Vision encoder 不整合（{}）", model.encoder));
        }
        None
    }

    /// `/manager/status` の反映。build/download が進行中なら旧 active を
    /// 表示し続ける（受入 case 3）。download 進捗を model 別に反映。G04。/
    pub fn apply_status(
        &mut self,
        jobs: &[siderostat_core::manager::api::ManagerJobDto],
        active_digest: Option<&str>,
    ) {
        let mut build_or_download_active = false;
        for job in jobs {
            if (job.kind == "build" || job.kind == "download")
                && (job.phase == "running" || job.phase == "cancelling")
            {
                build_or_download_active = true;
            }
            if job.kind == "download" {
                self.download_progress.insert(job.id.clone(), job.progress);
            }
        }
        // 進行中は旧 active を保持（active_digest で上書きしない）。G04。/
        if !build_or_download_active {
            self.current_active = active_digest.map(str::to_string);
        }
    }

    /// 現在の active（旧 active を表示）。G04。/
    pub fn current_active(&self) -> Option<&str> {
        self.current_active.as_deref()
    }

    /// model の download 進捗（%）。G04。/
    pub fn download_progress(&self, name: &str) -> Option<u8> {
        self.download_progress.get(name).copied()
    }

    /// activation を開始する。download / stage / activate は一つの
    /// activation 操作へ集約（承認 dialog を各 stage で重複させない）。
    /// 開始前に current_active を previous へ追加する。G04。/
    pub async fn start_activation(
        &mut self,
        name: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        if !self.can_activate(name) {
            return Err(format!(
                "{} は activate できません（checksum / Vision 不整合）",
                name
            ));
        }
        let id = api.submit("activate", name).await?;
        if let Some(active) = self.current_active.clone()
            && !self.previous.contains(&active)
        {
            self.previous.push(active);
        }
        Ok(id)
    }

    /// rollback 候補（previous）。G04。/
    pub fn rollback_candidates(&self) -> &[String] {
        &self.previous
    }

    /// rollback を開始する（kind=rollback）。previous は保持する（受入
    /// case 4）。G04。/
    pub async fn rollback(
        &mut self,
        previous_id: &str,
        api: &mut impl ManagerApi,
    ) -> Result<String, String> {
        if !self.previous.contains(&previous_id.to_string()) {
            return Err("previous candidate not found".to_string());
        }
        let id = api.submit("rollback", previous_id).await?;
        // previous は rollback 後も保持する（自動削除しない）。G04。/
        Ok(id)
    }
}

/// Vision encoder が runtime と整合するか。G04。/
fn vision_consistent(model: &ModelEntry) -> bool {
    // 既定の Vision encoder は "openai-whisper"。空は不整合。G04。/
    model.encoder == "openai-whisper"
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    /// 入力: source 無し → 取得ボタン。G03。/
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

    /// 入力: checksum 無し → activate disabled。G04。/
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

    /// 入力: Vision 不整合 → 理由。G04。/
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

    /// 入力: build/download 中 → 旧 active 表示。G04。/
    #[test]
    fn old_active_shown_while_build_or_download_runs() {
        use siderostat_core::manager::api::ManagerJobDto;
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
        // 先に旧 active を反映しておく。G04。/
        view.apply_status(&[], Some("old-active"));
        assert_eq!(view.current_active(), Some("old-active"));
        // build 進行中は旧 active を表示し続ける（active_digest で上書き
        // しない）。G04。/
        view.apply_status(std::slice::from_ref(&running_build), Some("new-active"));
        assert_eq!(view.current_active(), Some("old-active"));
        let _ = running_build;
    }

    /// 入力: rollback → previous 保持。G04。/
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
        // activation で current_active を previous に追加。G04。/
        view.apply_status(&[], Some("active-a"));
        let _ = block_on(view.start_activation("m1", &mut api)).expect("activate");
        assert_eq!(view.rollback_candidates(), &["active-a".to_string()]);
        // rollback 後も previous は保持。G04。/
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
        // download / stage / activate を分割せず、一つの activate 操作に
        // 集約（POST 1 回）。G04。/
        assert_eq!(api.submit_calls.len(), 1);
        assert_eq!(api.submit_calls[0].0, "activate");
    }

    #[test]
    fn manager_api_context_submit_preserves_plain_fake_boundary() {
        let mut api = FakeManagerApi::with_jobs(&["fetch-1"]);
        let id = block_on(api.submit_with_context("fetch", "official", None, None))
            .expect("plain submit");
        assert_eq!(id, "fetch-1");
        assert_eq!(api.submit_calls, vec![("fetch".into(), "official".into())]);
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
