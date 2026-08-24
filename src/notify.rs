//! Desktop notification layer for macOS.
//!
//! State-transition notifications are a behavior-neutral add-on: they never
//! block or alter the cluster state machine, proxy, or persistence. Startup
//! cleanup uses the same posting primitive for its short timed restart notice;
//! notification failures remain non-fatal.
//!
//! The macOS implementation posts through `UNUserNotificationCenter` from the
//! signed Siderostat.app process. The runtime helper is not itself an app
//! bundle, so it forwards notification payloads over a per-user Unix socket to
//! the menu-bar monitor. Non-macOS targets are a no-op.

use crate::{
    cluster::{ClusterFailure, ClusterSnapshot},
    localization::text,
    metrics::Metrics,
    target::ClusterState,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use std::{collections::BTreeSet, sync::Arc};
use tokio::sync::watch;

/// Minimum interval between desktop notifications. High-frequency transitions
/// such as restart loops are absorbed by this throttle.
pub const NOTIFICATION_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Sound played by the default macOS notification.
pub const NOTIFICATION_SOUND: &str = "Glass";

/// A single desktop notification payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NotificationKind {
    SoloStandaloneReady,
    PairedStandaloneReady,
    StandaloneRestart,
    DistributedReady,
    Backoff,
    ManualInterventionRequired,
    DeploymentMismatch,
}

impl NotificationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SoloStandaloneReady => "solo-standalone-ready",
            Self::PairedStandaloneReady => "paired-standalone-ready",
            Self::StandaloneRestart => "standalone-restart",
            Self::DistributedReady => "distributed-ready",
            Self::Backoff => "backoff",
            Self::ManualInterventionRequired => "manual-intervention-required",
            Self::DeploymentMismatch => "deployment-mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationEvent {
    kind: NotificationKind,
    notification: Notification,
}

impl NotificationEvent {
    fn new(kind: NotificationKind, notification: Notification) -> Self {
        Self { kind, notification }
    }
}

/// Keep stable standalone notifications at most once per recovery epoch.
///
/// A peer-discovery retry can move the runtime between Solo and Paired stable
/// states more than once. The state machine remains authoritative, but the
/// desktop notification layer should not turn those retries into a banner
/// loop. DistributedReady closes the current epoch and prepares the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationEpoch {
    recovery_id: String,
    generation: u64,
}

const IMPLICIT_EPOCH_ID: &str = "cluster-transition";

#[derive(Debug, Default)]
struct NotificationDeduplicator {
    emitted_stable_states: BTreeSet<NotificationKind>,
    epoch: Option<NotificationEpoch>,
}

impl NotificationDeduplicator {
    fn ensure_implicit_epoch(&mut self, generation: u64) {
        if self.epoch.is_none() {
            self.epoch = Some(NotificationEpoch {
                recovery_id: IMPLICIT_EPOCH_ID.to_string(),
                generation,
            });
        }
    }

    fn begin_recovery_epoch(&mut self, recovery_id: impl Into<String>, generation: u64) {
        let epoch = NotificationEpoch {
            recovery_id: recovery_id.into(),
            generation,
        };
        if self.epoch.as_ref() != Some(&epoch) {
            self.emitted_stable_states.clear();
            self.epoch = Some(epoch);
        }
    }

    fn epoch(&self) -> Option<&NotificationEpoch> {
        self.epoch.as_ref()
    }

    fn end_epoch(&mut self) {
        self.epoch = None;
        self.emitted_stable_states.clear();
    }

    fn should_emit(&mut self, kind: NotificationKind) -> bool {
        if kind == NotificationKind::DistributedReady {
            self.emitted_stable_states.clear();
            return true;
        }
        if matches!(
            kind,
            NotificationKind::SoloStandaloneReady | NotificationKind::PairedStandaloneReady
        ) {
            return self.emitted_stable_states.insert(kind);
        }
        true
    }
}

impl Notification {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

/// Error returned when a desktop notification could not be posted.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("desktop notification session is unavailable")]
    SessionUnavailable,
    #[error("native macOS notification failed: {0}")]
    Native(String),
    #[error("notification relay failed: {0}")]
    Relay(String),
    #[error("notification relay I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Platform-independent posting contract. Tests inject a fake implementation.
pub trait DesktopNotifier: Send + Sync + 'static {
    /// Post a notification asynchronously. The returned future resolves when
    /// the notification has been submitted or failed; failures are non-fatal.
    fn notify(&self, notification: Notification) -> BoxFuture<'static, Result<(), NotifyError>>;
}

/// A notifier that does nothing. Used on non-macOS platforms and when
/// notifications are disabled by configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopNotifier;

impl DesktopNotifier for NoopNotifier {
    fn notify(&self, _notification: Notification) -> BoxFuture<'static, Result<(), NotifyError>> {
        Box::pin(async { Ok(()) })
    }
}

/// macOS implementation used by the signed Siderostat.app process.
#[derive(Debug, Clone)]
struct UserNotificationsNotifier {
    enabled: bool,
    sound: bool,
}

impl UserNotificationsNotifier {
    pub fn new(enabled: bool, sound: bool) -> Self {
        Self { enabled, sound }
    }

    #[cfg(target_os = "macos")]
    fn post(&self, notification: Notification) -> Result<(), NotifyError> {
        if !self.enabled {
            return Ok(());
        }
        post_user_notification(notification, self.sound)
    }

    #[cfg(not(target_os = "macos"))]
    fn post(&self, _notification: Notification) -> Result<(), NotifyError> {
        Ok(())
    }
}

impl DesktopNotifier for UserNotificationsNotifier {
    fn notify(&self, notification: Notification) -> BoxFuture<'static, Result<(), NotifyError>> {
        if !self.enabled {
            return Box::pin(async { Ok(()) });
        }
        let notifier = self.clone();
        Box::pin(async move { notifier.post(notification) })
    }
}

/// Runtime-to-monitor payload sent over the local notification relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NotificationEnvelope {
    notification: Notification,
    sound: bool,
}

/// Runtime-side notifier that forwards payloads to the menu-bar monitor.
#[derive(Debug, Clone)]
struct NotificationRelayNotifier {
    enabled: bool,
    sound: bool,
}

impl NotificationRelayNotifier {
    fn new(enabled: bool, sound: bool) -> Self {
        Self { enabled, sound }
    }

    async fn send(&self, notification: Notification) -> Result<(), NotifyError> {
        if !self.enabled {
            return Ok(());
        }

        #[cfg(target_os = "macos")]
        {
            let envelope = NotificationEnvelope {
                notification,
                sound: self.sound,
            };
            send_notification_envelope(envelope).await
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = notification;
            Ok(())
        }
    }
}

impl DesktopNotifier for NotificationRelayNotifier {
    fn notify(&self, notification: Notification) -> BoxFuture<'static, Result<(), NotifyError>> {
        let notifier = self.clone();
        Box::pin(async move { notifier.send(notification).await })
    }
}

/// Build a notifier from the resolved notification configuration.
pub fn build_notifier(
    enabled: bool,
    sound: bool,
    platform: NotifyPlatform,
) -> Arc<dyn DesktopNotifier> {
    match platform {
        NotifyPlatform::MacOs => {
            if is_siderostat_app_process() {
                Arc::new(UserNotificationsNotifier::new(enabled, sound))
            } else {
                Arc::new(NotificationRelayNotifier::new(enabled, sound))
            }
        }
        NotifyPlatform::Other => Arc::new(NoopNotifier),
    }
}

/// Start the monitor-side notification relay.
///
/// The relay is intentionally owned by the menu-bar app. Calling
/// `UNUserNotificationCenter` from `Contents/Helpers/siderostat-runtime` does
/// not provide the app bundle identity that Notification Center needs, while
/// the monitor is the signed `Siderostat.app` executable.
#[cfg(target_os = "macos")]
pub fn start_notification_relay(
    enabled: bool,
    sound: bool,
) -> Result<std::thread::JoinHandle<()>, NotifyError> {
    use std::os::unix::net::UnixListener;

    let path = notification_relay_path().ok_or_else(|| {
        NotifyError::Relay("HOME is unavailable; cannot locate notification relay".to_string())
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if path.exists() {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => {
                return Err(NotifyError::Relay(
                    "another notification relay is already running".to_string(),
                ));
            }
            Err(_) => std::fs::remove_file(&path)?,
        }
    }

    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| NotifyError::Relay(error.to_string()))?;

    let handle = std::thread::Builder::new()
        .name("siderostat-notification-relay".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                match tokio::net::UnixListener::from_std(listener) {
                    Ok(listener) => notification_relay_loop(listener, enabled, sound).await,
                    Err(error) => {
                        tracing::warn!(error = %error, "notification relay listener setup failed");
                    }
                }
            });
            let _ = std::fs::remove_file(&path);
        })
        .map_err(NotifyError::Io)?;
    Ok(handle)
}

#[cfg(target_os = "macos")]
async fn notification_relay_loop(listener: tokio::net::UnixListener, enabled: bool, sound: bool) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(error = %error, "notification relay accept failed");
                break;
            }
        };
        tokio::spawn(handle_notification_connection(stream, enabled, sound));
    }
}

#[cfg(target_os = "macos")]
async fn handle_notification_connection(
    mut stream: tokio::net::UnixStream,
    enabled: bool,
    sound: bool,
) {
    use tokio::io::AsyncReadExt;

    let mut payload = Vec::new();
    if let Err(error) = stream.read_to_end(&mut payload).await {
        tracing::warn!(error = %error, "notification relay read failed");
        return;
    }
    let envelope = match serde_json::from_slice::<NotificationEnvelope>(&payload) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(error = %error, "notification relay payload was invalid");
            return;
        }
    };
    if let Err(error) =
        UserNotificationsNotifier::new(enabled, sound && envelope.sound).post(envelope.notification)
    {
        tracing::warn!(error = %error, "native macOS notification failed");
    }
}

#[cfg(target_os = "macos")]
async fn send_notification_envelope(envelope: NotificationEnvelope) -> Result<(), NotifyError> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let path = notification_relay_path().ok_or_else(|| {
        NotifyError::Relay("HOME is unavailable; cannot locate notification relay".to_string())
    })?;
    let payload =
        serde_json::to_vec(&envelope).map_err(|error| NotifyError::Relay(error.to_string()))?;
    let retry_delays = [
        Duration::from_millis(0),
        Duration::from_millis(100),
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ];
    let mut last_error = None;
    for delay in retry_delays {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match UnixStream::connect(&path).await {
            Ok(mut stream) => {
                stream.write_all(&payload).await?;
                stream.shutdown().await?;
                return Ok(());
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(NotifyError::Relay(format!(
        "could not connect to {} after retries: {}",
        path.display(),
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown connection error".to_string())
    )))
}

#[cfg(target_os = "macos")]
fn notification_relay_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join("Library/Application Support/siderostat/notifications.sock")
    })
}

#[cfg(target_os = "macos")]
fn is_siderostat_app_process() -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    executable.file_name().and_then(|name| name.to_str()) == Some("Siderostat")
        && executable.ancestors().any(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("app")
                && path.join("Contents/Info.plist").is_file()
        })
}

#[cfg(target_os = "macos")]
fn post_user_notification(notification: Notification, sound: bool) -> Result<(), NotifyError> {
    use objc2_foundation::NSString;
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };
    use std::sync::atomic::{AtomicU8, Ordering};

    static AUTHORIZATION_STATE: AtomicU8 = AtomicU8::new(0);
    const AUTHORIZATION_UNREQUESTED: u8 = 0;
    const AUTHORIZATION_PENDING: u8 = 1;
    const AUTHORIZATION_GRANTED: u8 = 2;
    const AUTHORIZATION_DENIED: u8 = 3;

    let center = UNUserNotificationCenter::currentNotificationCenter();
    let content = UNMutableNotificationContent::new();
    let title = NSString::from_str(&notification.title);
    let body = NSString::from_str(&notification.body);
    content.setTitle(&title);
    content.setBody(&body);
    if sound {
        let sound_name = NSString::from_str(NOTIFICATION_SOUND);
        let sound = UNNotificationSound::soundNamed(&sound_name);
        content.setSound(Some(&sound));
    }
    let identifier = NSString::from_str(&uuid::Uuid::new_v4().to_string());
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);

    match AUTHORIZATION_STATE.compare_exchange(
        AUTHORIZATION_UNREQUESTED,
        AUTHORIZATION_PENDING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            let authorization_options = if sound {
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound
            } else {
                UNAuthorizationOptions::Alert
            };
            let completion: block2::RcBlock<
                dyn Fn(objc2::runtime::Bool, *mut objc2_foundation::NSError),
            > = block2::RcBlock::new(
                move |granted: objc2::runtime::Bool, _error: *mut objc2_foundation::NSError| {
                    if granted.is_true() {
                        AUTHORIZATION_STATE.store(AUTHORIZATION_GRANTED, Ordering::Release);
                        let center = UNUserNotificationCenter::currentNotificationCenter();
                        center.addNotificationRequest_withCompletionHandler(&request, None);
                    } else {
                        AUTHORIZATION_STATE.store(AUTHORIZATION_DENIED, Ordering::Release);
                        tracing::warn!(
                            "Siderostat desktop notification permission was not granted"
                        );
                    }
                },
            );
            center.requestAuthorizationWithOptions_completionHandler(
                authorization_options,
                &completion,
            );
        }
        Err(AUTHORIZATION_GRANTED) => {
            center.addNotificationRequest_withCompletionHandler(&request, None);
        }
        Err(AUTHORIZATION_PENDING | AUTHORIZATION_DENIED) => {}
        Err(state) => {
            return Err(NotifyError::Native(format!(
                "unexpected notification authorization state {state}"
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn start_notification_relay(
    _enabled: bool,
    _sound: bool,
) -> Result<std::thread::JoinHandle<()>, NotifyError> {
    Err(NotifyError::Relay(
        "notification relay is only available on macOS".to_string(),
    ))
}

/// Platform classification used to choose a notifier implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyPlatform {
    MacOs,
    Other,
}

impl NotifyPlatform {
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
}

/// Whether the current process runs in a GUI (Aqua) session that can show
/// notifications. On non-macOS platforms this is always true so that a
/// no-op notifier is not repeatedly warned about a missing session; the
/// platform check is applied when the notifier implementation is chosen.
pub async fn gui_session_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        match tokio::process::Command::new("/bin/launchctl")
            .arg("managername")
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                let name = String::from_utf8_lossy(&output.stdout);
                name.trim() == "Aqua"
            }
            _ => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Event selection and throttling for the desktop notification service.
type SessionCheck = Box<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;

pub struct DesktopNotificationService {
    notifier: Arc<dyn DesktopNotifier>,
    metrics: Arc<Metrics>,
    last_sent_at: Option<Instant>,
    session_check: SessionCheck,
    deduplicator: NotificationDeduplicator,
}

impl DesktopNotificationService {
    pub fn new(notifier: Arc<dyn DesktopNotifier>) -> Self {
        Self::with_session_check_and_metrics(
            notifier,
            Box::new(|| Box::pin(gui_session_available())),
            Arc::new(Metrics::default()),
        )
    }

    pub fn with_metrics(notifier: Arc<dyn DesktopNotifier>, metrics: Arc<Metrics>) -> Self {
        Self::with_session_check_and_metrics(
            notifier,
            Box::new(|| Box::pin(gui_session_available())),
            metrics,
        )
    }

    pub fn with_session_check_and_metrics(
        notifier: Arc<dyn DesktopNotifier>,
        session_check: SessionCheck,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            notifier,
            metrics,
            last_sent_at: None,
            session_check,
            deduplicator: NotificationDeduplicator::default(),
        }
    }

    /// Log whether desktop notifications can reach the user's GUI session.
    /// Called once at startup so an operator notices a deployment outside the
    /// `gui/<uid>` Aqua domain (for example a LaunchDaemon or `user/<uid>`
    /// agent) instead of silently dropping notifications.
    pub async fn log_session_status(&self) {
        let available = (self.session_check)().await;
        if available {
            tracing::info!("desktop notifications available in this Aqua GUI session");
        } else {
            tracing::warn!(
                "desktop notifications unavailable: not running in an Aqua GUI session; \
                 deploy as a LaunchAgent under gui/<uid> to receive notifications"
            );
        }
    }

    /// Construct a service with an injected GUI session check, for tests.
    pub fn with_session_check(
        notifier: Arc<dyn DesktopNotifier>,
        session_check: SessionCheck,
    ) -> Self {
        Self {
            notifier,
            metrics: Arc::new(Metrics::default()),
            last_sent_at: None,
            session_check,
            deduplicator: NotificationDeduplicator::default(),
        }
    }

    /// Handle one observed transition. Only stable or important states are
    /// reported; intermediate transitions such as `Promoting` are skipped.
    pub fn observe_transition(&mut self, previous: ClusterState, current: ClusterState) {
        if let Some(event) = notification_event_for_snapshot_transition(previous, current, None) {
            self.observe_event(event);
        }
    }

    /// Handle a transition while retaining the failure attached to the resulting snapshot.
    /// This lets a recoverable state transition and a non-repairable failure use different
    /// user-facing messages even when both end in Solo Standalone.
    pub fn observe_snapshot_transition(
        &mut self,
        previous: ClusterSnapshot,
        current: ClusterSnapshot,
    ) {
        self.deduplicator.ensure_implicit_epoch(current.generation);
        if let Some(event) = notification_event_for_snapshots(previous, current) {
            self.observe_event(event);
        }
        if current.state == ClusterState::DistributedReady {
            self.deduplicator.end_epoch();
        }
    }

    /// Handle a startup notification explicitly. The transition monitor only
    /// subscribes after boot, so boot transitions are reported here.
    pub fn observe_startup(&mut self, state: ClusterState) {
        let event = match state {
            ClusterState::SoloStandaloneReady => Some(NotificationEvent::new(
                NotificationKind::SoloStandaloneReady,
                Notification::new(
                    text("notification.startup.title", "ds4-serverの起動"),
                    text(
                        "notification.standalone.started",
                        "ds4-serverをStandaloneモードで起動しました",
                    ),
                ),
            )),
            ClusterState::ManualInterventionRequired => Some(NotificationEvent::new(
                NotificationKind::ManualInterventionRequired,
                Notification::new(
                    text("notification.startup.title", "ds4-serverの起動"),
                    text(
                        "notification.manual_intervention",
                        "ds4-serverのモード変更に失敗しました。手動で復旧してください",
                    ),
                ),
            )),
            _ => None,
        };
        if let Some(event) = event {
            self.observe_event(event);
        } else {
            self.maybe_notify(Notification::new(
                text("notification.startup.title", "ds4-serverの起動"),
                text("notification.startup.starting", "ds4-serverを起動中"),
            ));
        }
    }

    /// Handle a standalone child restart detected by the local monitor.
    pub fn observe_child_restart(&mut self) {
        self.maybe_notify(Notification::new(
            text("app.name", "Siderostat"),
            text(
                "notification.standalone.restarted",
                "ds4-serverをStandaloneモードで再起動しました",
            ),
        ));
    }

    /// Start a notification epoch for one H-phase recovery job. The job
    /// identity is retained only in structured logs; it is never a metric
    /// label or part of the user-facing notification text.
    pub fn observe_recovery_started(&mut self, recovery_id: &str, generation: u64) {
        self.deduplicator
            .begin_recovery_epoch(recovery_id.to_owned(), generation);
        tracing::info!(
            event = "notification-recovery-epoch-started",
            recovery_id,
            generation,
            "desktop notification recovery epoch started"
        );
    }

    pub fn observe_recovery_completed(&mut self, recovery_id: &str, generation: u64) {
        tracing::info!(
            event = "notification-recovery-epoch-completed",
            recovery_id,
            generation,
            "desktop notification recovery epoch completed"
        );
    }

    pub fn observe_recovery_failed(&mut self, recovery_id: &str, generation: u64, reason: &str) {
        tracing::warn!(
            event = "notification-recovery-epoch-failed",
            recovery_id,
            generation,
            reason,
            "desktop notification recovery epoch failed"
        );
    }

    /// Post one notification through the same session check and throttle used
    /// by state-transition notifications. Callers remain responsible for
    /// deciding whether a state change is worth reporting.
    pub fn notify(&mut self, notification: Notification) {
        self.maybe_notify(notification);
    }

    fn observe_event(&mut self, event: NotificationEvent) {
        if self.deduplicator.should_emit(event.kind) {
            self.maybe_notify(event.notification);
        } else {
            self.metrics.notification_suppressed(event.kind.as_str());
            if let Some(epoch) = self.deduplicator.epoch() {
                tracing::debug!(
                    kind = event.kind.as_str(),
                    recovery_id = %epoch.recovery_id,
                    generation = epoch.generation,
                    "desktop notification suppressed within recovery epoch"
                );
            } else {
                tracing::debug!(
                    kind = event.kind.as_str(),
                    "desktop notification suppressed within recovery epoch"
                );
            }
        }
    }

    fn maybe_notify(&mut self, notification: Notification) {
        let now = Instant::now();
        if self
            .last_sent_at
            .is_some_and(|last| now.duration_since(last) < NOTIFICATION_MIN_INTERVAL)
        {
            tracing::debug!(
                title = %notification.title,
                "desktop notification throttled"
            );
            return;
        }
        self.last_sent_at = Some(now);
        let notifier = self.notifier.clone();
        let session = (self.session_check)();
        tokio::spawn(async move {
            if !session.await {
                tracing::warn!(
                    title = %notification.title,
                    "desktop notification skipped: not in an Aqua GUI session"
                );
                return;
            }
            if let Err(error) = notifier.notify(notification.clone()).await {
                tracing::warn!(error = %error, title = %notification.title, "desktop notification failed");
            }
        });
    }
}

/// Map a transition to an optional notification. Only stable states and
/// important lifecycle transitions are reported.
#[cfg(test)]
fn notification_for_transition(
    previous: ClusterState,
    current: ClusterState,
) -> Option<Notification> {
    notification_event_for_snapshot_transition(previous, current, None)
        .map(|event| event.notification)
}

#[cfg(test)]
fn notification_for_snapshot_transition(
    previous: ClusterState,
    current: ClusterState,
    failure: Option<ClusterFailure>,
) -> Option<Notification> {
    notification_event_for_snapshot_transition(previous, current, failure)
        .map(|event| event.notification)
}

fn notification_event_for_snapshot_transition(
    previous: ClusterState,
    current: ClusterState,
    failure: Option<ClusterFailure>,
) -> Option<NotificationEvent> {
    if current == ClusterState::SoloStandaloneReady
        && failure == Some(ClusterFailure::DeploymentMismatch)
    {
        return Some(NotificationEvent::new(
            NotificationKind::DeploymentMismatch,
            Notification::new(
                text("app.name", "Siderostat"),
                text(
                    "notification.deployment_mismatch",
                    "2台のds4-serverの設定が一致しないためDistributed（layer-parallel）モードを開始できません。設定を確認して再試行してください。Standaloneモードで待機します",
                ),
            ),
        ));
    }

    match (previous, current) {
        (ClusterState::Demoting, ClusterState::PairedStandaloneReady) => {
            Some(NotificationEvent::new(
                NotificationKind::StandaloneRestart,
                Notification::new(
                    text("app.name", "Siderostat"),
                    text(
                        "notification.standalone.restarted",
                        "ds4-serverをStandaloneモードで再起動しました",
                    ),
                ),
            ))
        }
        (_, ClusterState::SoloStandaloneReady) => Some(NotificationEvent::new(
            NotificationKind::SoloStandaloneReady,
            Notification::new(
                text("app.name", "Siderostat"),
                text(
                    "notification.standalone.started",
                    "ds4-serverをStandaloneモードで起動しました",
                ),
            ),
        )),
        (_, ClusterState::PairedStandaloneReady) => Some(NotificationEvent::new(
            NotificationKind::PairedStandaloneReady,
            Notification::new(
                text("app.name", "Siderostat"),
                text(
                    "notification.standalone.peer_detected",
                    "ネットワーク上の別のds4-serverを検出しました",
                ),
            ),
        )),
        (ClusterState::DistributedStarting, ClusterState::DistributedReady) => {
            Some(NotificationEvent::new(
                NotificationKind::DistributedReady,
                Notification::new(
                    text("app.name", "Siderostat"),
                    text(
                        "notification.distributed.ready",
                        "2台のds4-serverをDistributed（layer-parallel）モードに切り替えました",
                    ),
                ),
            ))
        }
        (_, ClusterState::Backoff) => Some(NotificationEvent::new(
            NotificationKind::Backoff,
            Notification::new(
                text("app.name", "Siderostat"),
                text(
                    "notification.distributed.backoff",
                    "ds4-serverのDistributed（layer-parallel）モードへの切替に失敗しました。Standaloneモードで待機します",
                ),
            ),
        )),
        (_, ClusterState::ManualInterventionRequired) => Some(NotificationEvent::new(
            NotificationKind::ManualInterventionRequired,
            Notification::new(
                text("app.name", "Siderostat"),
                text(
                    "notification.manual_intervention",
                    "ds4-serverのモード変更に失敗しました。手動で復旧してください",
                ),
            ),
        )),
        _ => None,
    }
}

fn notification_event_for_snapshots(
    previous: ClusterSnapshot,
    current: ClusterSnapshot,
) -> Option<NotificationEvent> {
    let event = notification_event_for_snapshot_transition(
        previous.state,
        current.state,
        current.last_failure,
    )?;
    if event.kind == NotificationKind::PairedStandaloneReady
        && current.role == crate::target::LocalRole::Worker
        && !current.local_standalone_ready
    {
        return None;
    }
    Some(event)
}

/// Subscribe to the cluster state watch channel and forward important
/// transitions to the notification service. Runs until the channel closes.
pub async fn run_desktop_notifier(
    mut snapshots: watch::Receiver<crate::cluster::ClusterSnapshot>,
    mut service: DesktopNotificationService,
) {
    let mut previous = *snapshots.borrow_and_update();
    while snapshots.changed().await.is_ok() {
        let current = *snapshots.borrow_and_update();
        service.observe_snapshot_transition(previous, current);
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{LocalRole, StableMode};
    use crate::{
        admission::{AdmissionSnapshot, AdmissionState},
        cluster::ClusterSnapshot,
        metrics::{MetricSnapshot, Metrics},
        proxy::ModeAwareTargetSnapshot,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingNotifier {
        notifications: std::sync::Mutex<Vec<Notification>>,
        calls: AtomicUsize,
    }

    impl DesktopNotifier for RecordingNotifier {
        fn notify(
            &self,
            notification: Notification,
        ) -> BoxFuture<'static, Result<(), NotifyError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.notifications.lock().unwrap().push(notification);
            Box::pin(async { Ok(()) })
        }
    }

    fn service(notifier: Arc<dyn DesktopNotifier>) -> DesktopNotificationService {
        DesktopNotificationService::with_session_check(
            notifier,
            Box::new(|| Box::pin(async { true })),
        )
    }

    fn service_with_metrics(
        notifier: Arc<dyn DesktopNotifier>,
        metrics: Arc<Metrics>,
    ) -> DesktopNotificationService {
        DesktopNotificationService::with_session_check_and_metrics(
            notifier,
            Box::new(|| Box::pin(async { true })),
            metrics,
        )
    }

    fn render_metrics(metrics: &Metrics) -> String {
        metrics.render_mode_aware(MetricSnapshot {
            node_id: "test-node",
            interface: "bridge0",
            target: ModeAwareTargetSnapshot {
                target: crate::target::ProxyTarget::LocalStandalone,
                ready: true,
            },
            admission: AdmissionSnapshot {
                state: AdmissionState::Serving,
                in_flight: 0,
                max_in_flight: 4,
                drain_generation: None,
            },
            cluster: Some(ClusterSnapshot::booting(LocalRole::Unknown)),
            peer_lease_seconds: 0.0,
            thunderbolt_ip_state: "unknown",
            discovery_results: 0,
            quantization: crate::config::Quantization::Q2Q4,
            speculative_support: crate::config::SpeculativeSupport::None,
            residency: crate::config::Residency::SsdStreaming,
        })
    }

    #[tokio::test]
    async fn noop_notifier_never_fails() {
        let notifier = NoopNotifier;
        let result = notifier.notify(Notification::new("title", "body")).await;
        assert!(result.is_ok());
    }

    #[test]
    fn notification_envelope_preserves_payload_and_sound_preference() {
        let envelope = NotificationEnvelope {
            notification: Notification::new(
                "Siderostat",
                "5秒後にsiderostat-runtimeを再起動します",
            ),
            sound: true,
        };
        let encoded = serde_json::to_vec(&envelope).unwrap();
        let decoded: NotificationEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn transition_table_selects_stable_and_important_states() {
        let rows = [
            (
                ClusterState::Booting,
                ClusterState::SoloStandaloneStarting,
                None,
            ),
            (
                ClusterState::SoloStandaloneStarting,
                ClusterState::SoloStandaloneReady,
                Some("ds4-serverをStandaloneモードで起動しました"),
            ),
            (
                ClusterState::SoloStandaloneReady,
                ClusterState::Pairing,
                None,
            ),
            (
                ClusterState::Pairing,
                ClusterState::PairedStandaloneReady,
                Some("ネットワーク上の別のds4-serverを検出しました"),
            ),
            (
                ClusterState::PairedStandaloneReady,
                ClusterState::AwaitingWorkerHello,
                None,
            ),
            (
                ClusterState::Promoting,
                ClusterState::DistributedStarting,
                None,
            ),
            (
                ClusterState::DistributedStarting,
                ClusterState::DistributedReady,
                Some("2台のds4-serverをDistributed（layer-parallel）モードに切り替えました"),
            ),
            (
                ClusterState::PairedStandaloneReady,
                ClusterState::Backoff,
                Some(
                    "ds4-serverのDistributed（layer-parallel）モードへの切替に失敗しました。Standaloneモードで待機します",
                ),
            ),
            (
                ClusterState::Backoff,
                ClusterState::ManualInterventionRequired,
                Some("ds4-serverのモード変更に失敗しました。手動で復旧してください"),
            ),
            (ClusterState::DistributedReady, ClusterState::Demoting, None),
            (
                ClusterState::Demoting,
                ClusterState::PairedStandaloneReady,
                Some("ds4-serverをStandaloneモードで再起動しました"),
            ),
        ];
        for (previous, current, expected) in rows {
            let notification = notification_for_transition(previous, current);
            assert_eq!(
                notification.map(|n| n.body),
                expected.map(str::to_string),
                "{previous:?} -> {current:?}"
            );
        }
    }

    #[test]
    fn deployment_mismatch_notification_explains_solo_fallback() {
        let notification = notification_for_snapshot_transition(
            ClusterState::SoloStandaloneStarting,
            ClusterState::SoloStandaloneReady,
            Some(ClusterFailure::DeploymentMismatch),
        )
        .expect("deployment mismatch should notify");

        assert_eq!(
            notification.body,
            "2台のds4-serverの設定が一致しないためDistributed（layer-parallel）モードを開始できません。設定を確認して再試行してください。Standaloneモードで待機します"
        );
    }

    #[tokio::test]
    async fn throttle_suppresses_notifications_within_min_interval() {
        let notifier = Arc::new(RecordingNotifier::default());
        let mut service = service(notifier.clone());
        service.observe_transition(
            ClusterState::SoloStandaloneStarting,
            ClusterState::SoloStandaloneReady,
        );
        service.observe_transition(
            ClusterState::SoloStandaloneReady,
            ClusterState::SoloStandaloneReady,
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(notifier.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn startup_notification_matches_boot_state() {
        let ready_notifier = Arc::new(RecordingNotifier::default());
        let mut ready_service = service(ready_notifier.clone());
        ready_service.observe_startup(ClusterState::SoloStandaloneReady);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(ready_notifier.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            ready_notifier.notifications.lock().unwrap()[0].body,
            "ds4-serverをStandaloneモードで起動しました"
        );

        let manual_notifier = Arc::new(RecordingNotifier::default());
        let mut manual_service = service(manual_notifier.clone());
        manual_service.observe_startup(ClusterState::ManualInterventionRequired);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(manual_notifier.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            manual_notifier.notifications.lock().unwrap()[0].body,
            "ds4-serverのモード変更に失敗しました。手動で復旧してください"
        );
    }

    #[tokio::test]
    async fn non_gui_session_skips_notification() {
        let notifier = Arc::new(RecordingNotifier::default());
        let mut service = DesktopNotificationService::with_session_check(
            notifier.clone(),
            Box::new(|| Box::pin(async { false })),
        );
        service.observe_startup(ClusterState::SoloStandaloneReady);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(notifier.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn session_status_logs_warning_outside_aqua_session() {
        let service = DesktopNotificationService::with_session_check(
            Arc::new(NoopNotifier),
            Box::new(|| Box::pin(async { false })),
        );
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .with_max_level(tracing::Level::INFO)
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(service.log_session_status());
        });
        let output = String::from_utf8(log.lines.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("desktop notifications unavailable"),
            "expected unavailable warning, got: {output}"
        );
        assert!(
            output.contains("gui/<uid>"),
            "expected deployment hint, got: {output}"
        );
    }

    #[test]
    fn session_status_logs_available_in_aqua_session() {
        let service = DesktopNotificationService::with_session_check(
            Arc::new(NoopNotifier),
            Box::new(|| Box::pin(async { true })),
        );
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .with_max_level(tracing::Level::INFO)
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(service.log_session_status());
        });
        let output = String::from_utf8(log.lines.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("desktop notifications available"),
            "expected availability info, got: {output}"
        );
    }

    #[tokio::test]
    async fn gui_session_posts_notification() {
        let notifier = Arc::new(RecordingNotifier::default());
        let mut service = DesktopNotificationService::with_session_check(
            notifier.clone(),
            Box::new(|| Box::pin(async { true })),
        );
        service.observe_startup(ClusterState::SoloStandaloneReady);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(notifier.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            notifier.notifications.lock().unwrap()[0].body,
            "ds4-serverをStandaloneモードで起動しました"
        );
    }

    #[tokio::test]
    async fn run_desktop_notifier_forwards_relevant_transitions() {
        let notifier = Arc::new(RecordingNotifier::default());
        let service = service(notifier.clone());
        let (handle, task) = crate::cluster::spawn_state_machine(
            ClusterSnapshot::booting(LocalRole::Coordinator),
            4,
        );
        let subscribe = handle.subscribe();
        let notifier_task = tokio::spawn(async move {
            run_desktop_notifier(subscribe, service).await;
        });
        handle
            .apply(crate::cluster::ClusterEvent {
                expected_generation: 0,
                kind: crate::cluster::ClusterEventKind::BeginSoloStandalone,
            })
            .await
            .unwrap();
        handle
            .apply(crate::cluster::ClusterEvent {
                expected_generation: 1,
                kind: crate::cluster::ClusterEventKind::LocalStandaloneReady,
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(notifier.calls.load(Ordering::Relaxed), 1);
        task.abort();
        notifier_task.abort();
    }

    #[derive(Default)]
    struct FailingNotifier {
        calls: AtomicUsize,
    }

    impl DesktopNotifier for FailingNotifier {
        fn notify(
            &self,
            _notification: Notification,
        ) -> BoxFuture<'static, Result<(), NotifyError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Err(NotifyError::Native("test sender failure".to_string())) })
        }
    }

    #[tokio::test]
    async fn sender_failure_does_not_prevent_watch_task_from_closing() {
        let notifier = Arc::new(FailingNotifier::default());
        let service = service(notifier.clone());
        let (sender, receiver) = watch::channel(snapshot(
            LocalRole::Coordinator,
            ClusterState::Booting,
            1,
            false,
        ));
        let task = tokio::spawn(run_desktop_notifier(receiver, service));
        tokio::task::yield_now().await;
        sender
            .send(snapshot(
                LocalRole::Coordinator,
                ClusterState::SoloStandaloneReady,
                2,
                true,
            ))
            .unwrap();
        drop(sender);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(notifier.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn watch_channel_close_is_a_normal_notification_task_exit() {
        let service = service(Arc::new(NoopNotifier));
        let (sender, receiver) = watch::channel(snapshot(
            LocalRole::Coordinator,
            ClusterState::Booting,
            1,
            false,
        ));
        let task = tokio::spawn(run_desktop_notifier(receiver, service));
        drop(sender);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn worker_temporary_pairing_without_local_standalone_does_not_notify_peer_detection() {
        let notifier = Arc::new(RecordingNotifier::default());
        let mut service = service(notifier.clone());
        let previous = snapshot(LocalRole::Worker, ClusterState::Pairing, 20, false);
        let current = snapshot(
            LocalRole::Worker,
            ClusterState::PairedStandaloneReady,
            21,
            false,
        );

        service.observe_snapshot_transition(previous, current);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert_eq!(notifier.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn service_records_suppressed_stable_notification_with_bounded_metric_labels() {
        let notifier = Arc::new(RecordingNotifier::default());
        let metrics = Arc::new(Metrics::default());
        let mut service = service_with_metrics(notifier, metrics.clone());

        service.observe_transition(
            ClusterState::SoloStandaloneStarting,
            ClusterState::SoloStandaloneReady,
        );
        service.observe_transition(
            ClusterState::SoloStandaloneReady,
            ClusterState::SoloStandaloneReady,
        );

        tokio::task::yield_now().await;
        let rendered = render_metrics(&metrics);
        assert!(rendered.contains(
            "ds4_proxy_notification_events_total{event=\"suppressed\",kind=\"solo-standalone-ready\"} 1"
        ));
    }

    #[test]
    fn stable_mode_names_are_stable() {
        assert_eq!(StableMode::SoloStandalone.name(), "solo-standalone");
        assert_eq!(StableMode::PairedStandalone.name(), "paired-standalone");
        assert_eq!(
            StableMode::DistributedLayerParallel.name(),
            "distributed-layer-parallel"
        );
    }

    #[test]
    fn stable_standalone_notifications_are_once_per_epoch() {
        let mut deduplicator = NotificationDeduplicator::default();
        assert!(deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(deduplicator.should_emit(NotificationKind::PairedStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::PairedStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::PairedStandaloneReady));
    }

    #[test]
    fn stable_notifications_remain_once_through_180_transition_retries() {
        let mut deduplicator = NotificationDeduplicator::default();
        for attempt in 0..180 {
            assert_eq!(
                deduplicator.should_emit(NotificationKind::SoloStandaloneReady),
                attempt == 0
            );
            assert_eq!(
                deduplicator.should_emit(NotificationKind::PairedStandaloneReady),
                attempt == 0
            );
        }
    }

    #[test]
    fn distributed_ready_rolls_the_standalone_notification_epoch() {
        let mut deduplicator = NotificationDeduplicator::default();
        assert!(deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(deduplicator.should_emit(NotificationKind::PairedStandaloneReady));
        assert!(deduplicator.should_emit(NotificationKind::DistributedReady));
        assert!(deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(deduplicator.should_emit(NotificationKind::PairedStandaloneReady));
    }

    #[test]
    fn explicit_recovery_epoch_rollover_allows_stable_notifications_again() {
        let mut deduplicator = NotificationDeduplicator::default();
        assert!(deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::SoloStandaloneReady));

        deduplicator.begin_recovery_epoch("recovery-42", 101);

        assert!(deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
        assert!(!deduplicator.should_emit(NotificationKind::SoloStandaloneReady));
    }

    #[test]
    fn recovery_epoch_lifecycle_logs_redaction_safe_identity() {
        let mut service = service(Arc::new(NoopNotifier));
        let log = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .with_max_level(tracing::Level::INFO)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            service.observe_recovery_started("recovery-42", 101);
            service.observe_recovery_completed("recovery-42", 102);
            service.observe_recovery_failed("recovery-42", 103, "canary-failed");
        });

        let output = String::from_utf8(log.lines.lock().unwrap().clone()).unwrap();
        assert!(output.contains("notification-recovery-epoch-started"));
        assert!(output.contains("notification-recovery-epoch-completed"));
        assert!(output.contains("notification-recovery-epoch-failed"));
        assert!(output.contains("recovery-42"));
        assert!(output.contains("generation=101"));
        assert!(output.contains("generation=102"));
        assert!(output.contains("generation=103"));
        assert!(output.contains("canary-failed"));
    }

    #[test]
    fn important_failure_and_restart_notifications_are_not_deduplicated() {
        let mut deduplicator = NotificationDeduplicator::default();
        for kind in [
            NotificationKind::StandaloneRestart,
            NotificationKind::Backoff,
            NotificationKind::ManualInterventionRequired,
            NotificationKind::DeploymentMismatch,
        ] {
            assert!(deduplicator.should_emit(kind));
            assert!(deduplicator.should_emit(kind));
        }
    }

    fn snapshot(
        role: LocalRole,
        state: ClusterState,
        generation: u64,
        local_standalone_ready: bool,
    ) -> ClusterSnapshot {
        ClusterSnapshot {
            generation,
            role,
            stable_mode: StableMode::PairedStandalone,
            state,
            target: crate::target::ProxyTarget::Unavailable {
                reason: crate::target::UnavailableReason::Transition,
            },
            local_standalone_ready,
            last_failure: None,
        }
    }

    #[derive(Clone, Default)]
    struct SharedLog {
        lines: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.lines.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
