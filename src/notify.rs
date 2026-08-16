//! Desktop notification layer for macOS.
//!
//! State-transition notifications are a behavior-neutral add-on: they never
//! block or alter the cluster state machine, proxy, or persistence. Startup
//! cleanup uses the same posting primitive for its short timed restart notice;
//! notification failures remain non-fatal.
//!
//! The macOS implementation posts notifications through `/usr/bin/osascript`
//! (`display notification`), which works from a LaunchAgent running in the
//! user's `gui/<uid>` (Aqua) session. Non-macOS targets are a no-op.

use crate::target::ClusterState;
use futures::future::BoxFuture;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Minimum interval between desktop notifications. High-frequency transitions
/// such as restart loops are absorbed by this throttle.
pub const NOTIFICATION_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Sound played by the default macOS notification.
pub const NOTIFICATION_SOUND: &str = "Glass";

/// A single desktop notification payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
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
    #[error("osascript failed: {0}")]
    Osascript(String),
    #[error("osascript is unavailable: {0}")]
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

/// macOS implementation that posts through `/usr/bin/osascript`.
#[derive(Debug, Clone)]
pub struct OsascriptNotifier {
    enabled: bool,
    sound: bool,
}

impl OsascriptNotifier {
    pub fn new(enabled: bool, sound: bool) -> Self {
        Self { enabled, sound }
    }

    fn command(&self, notification: &Notification) -> std::process::Command {
        let mut command = std::process::Command::new("/usr/bin/osascript");
        command.arg("-e");
        let mut script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape_apple_script(&notification.body),
            escape_apple_script(&notification.title),
        );
        if self.sound {
            script.push_str(&format!(" sound name \"{NOTIFICATION_SOUND}\""));
        }
        command.arg(script);
        command
    }
}

impl DesktopNotifier for OsascriptNotifier {
    fn notify(&self, notification: Notification) -> BoxFuture<'static, Result<(), NotifyError>> {
        if !self.enabled {
            return Box::pin(async { Ok(()) });
        }
        let command = self.command(&notification);
        Box::pin(async move {
            let output = tokio::process::Command::from(command)
                .output()
                .await
                .map_err(NotifyError::Io)?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(NotifyError::Osascript(stderr.trim().to_string()));
            }
            Ok(())
        })
    }
}

/// Escape a string for use inside a double-quoted AppleScript literal.
fn escape_apple_script(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build a notifier from the resolved notification configuration.
pub fn build_notifier(
    enabled: bool,
    sound: bool,
    platform: NotifyPlatform,
) -> Arc<dyn DesktopNotifier> {
    match platform {
        NotifyPlatform::MacOs => Arc::new(OsascriptNotifier::new(enabled, sound)),
        NotifyPlatform::Other => Arc::new(NoopNotifier),
    }
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
    last_sent_at: Option<Instant>,
    session_check: SessionCheck,
}

impl DesktopNotificationService {
    pub fn new(notifier: Arc<dyn DesktopNotifier>) -> Self {
        Self {
            notifier,
            last_sent_at: None,
            session_check: Box::new(|| Box::pin(gui_session_available())),
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
            last_sent_at: None,
            session_check,
        }
    }

    /// Handle one observed transition. Only stable or important states are
    /// reported; intermediate transitions such as `Promoting` are skipped.
    pub fn observe_transition(&mut self, previous: ClusterState, current: ClusterState) {
        if let Some(notification) = notification_for_transition(previous, current) {
            self.maybe_notify(notification);
        }
    }

    /// Handle a startup notification explicitly. The transition monitor only
    /// subscribes after boot, so boot transitions are reported here.
    pub fn observe_startup(&mut self, state: ClusterState) {
        let notification = match state {
            ClusterState::SoloStandaloneReady => {
                Notification::new("siderostat 起動", "SoloStandalone 起動完了")
            }
            ClusterState::ManualInterventionRequired => {
                Notification::new("siderostat 起動", "要手動対応")
            }
            _ => Notification::new("siderostat 起動", "起動中"),
        };
        self.maybe_notify(notification);
    }

    /// Handle a standalone child restart detected by the local monitor.
    pub fn observe_child_restart(&mut self) {
        self.maybe_notify(Notification::new(
            "siderostat",
            "Standalone が再起動されました",
        ));
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
fn notification_for_transition(
    previous: ClusterState,
    current: ClusterState,
) -> Option<Notification> {
    match (previous, current) {
        (ClusterState::Demoting, ClusterState::PairedStandaloneReady) => Some(Notification::new(
            "siderostat",
            "Distributed 停止・Paired へ復帰",
        )),
        (_, ClusterState::SoloStandaloneReady) => {
            Some(Notification::new("siderostat", "Standalone 準備完了"))
        }
        (_, ClusterState::PairedStandaloneReady) => Some(Notification::new(
            "siderostat",
            "Standalone がペアリングされました",
        )),
        (ClusterState::DistributedStarting, ClusterState::DistributedReady) => {
            Some(Notification::new("siderostat", "Distributed 準備完了"))
        }
        (_, ClusterState::Backoff) => Some(Notification::new(
            "siderostat",
            "プロモーション失敗・バックオフ",
        )),
        (_, ClusterState::ManualInterventionRequired) => {
            Some(Notification::new("siderostat", "要手動対応"))
        }
        _ => None,
    }
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
        service.observe_transition(previous.state, current.state);
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterSnapshot;
    use crate::target::{LocalRole, StableMode};
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

    #[tokio::test]
    async fn noop_notifier_never_fails() {
        let notifier = NoopNotifier;
        let result = notifier.notify(Notification::new("title", "body")).await;
        assert!(result.is_ok());
    }

    #[test]
    fn osascript_command_posts_a_sounding_banner() {
        let notifier = OsascriptNotifier::new(true, true);
        let command = notifier.command(&Notification::new(
            "siderostat",
            "再起動が必要です（5秒後）",
        ));
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args.first().map(String::as_str), Some("-e"));
        assert!(args[1].contains("display notification"));
        assert!(args[1].contains("sound name \"Glass\""));
        assert!(args[1].contains("再起動が必要です（5秒後）"));
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
                Some("Standalone 準備完了"),
            ),
            (
                ClusterState::SoloStandaloneReady,
                ClusterState::Pairing,
                None,
            ),
            (
                ClusterState::Pairing,
                ClusterState::PairedStandaloneReady,
                Some("Standalone がペアリングされました"),
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
                Some("Distributed 準備完了"),
            ),
            (
                ClusterState::PairedStandaloneReady,
                ClusterState::Backoff,
                Some("プロモーション失敗・バックオフ"),
            ),
            (
                ClusterState::Backoff,
                ClusterState::ManualInterventionRequired,
                Some("要手動対応"),
            ),
            (ClusterState::DistributedReady, ClusterState::Demoting, None),
            (
                ClusterState::Demoting,
                ClusterState::PairedStandaloneReady,
                Some("Distributed 停止・Paired へ復帰"),
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
            "SoloStandalone 起動完了"
        );

        let manual_notifier = Arc::new(RecordingNotifier::default());
        let mut manual_service = service(manual_notifier.clone());
        manual_service.observe_startup(ClusterState::ManualInterventionRequired);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(manual_notifier.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            manual_notifier.notifications.lock().unwrap()[0].body,
            "要手動対応"
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
            "SoloStandalone 起動完了"
        );
    }

    #[test]
    fn escape_apple_script_quotes_and_backslashes() {
        assert_eq!(escape_apple_script(r#"a"b\c"#), r#"a\"b\\c"#);
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

    #[test]
    fn stable_mode_names_are_stable() {
        assert_eq!(StableMode::SoloStandalone.name(), "solo-standalone");
        assert_eq!(StableMode::PairedStandalone.name(), "paired-standalone");
        assert_eq!(StableMode::DistributedMxfp4.name(), "distributed-mxfp4");
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
