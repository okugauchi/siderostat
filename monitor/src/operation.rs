//! User-visible status for asynchronous menu operations.
//!
//! The menu event handler performs mutating work away from the AppKit loop.
//! This small state model lets the main-thread tray refresh show whether an
//! operation is running, succeeded, or failed without touching tray objects
//! from a worker thread.

use crate::localization::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    RuntimeRestart,
    BackgroundStart,
    BackgroundStop,
    OpenConfig,
    OpenLoginItems,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationOutcome {
    Succeeded,
    RequiresApproval,
    Denied,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OperationState {
    #[default]
    Idle,
    Running(OperationKind),
    Finished {
        kind: OperationKind,
        outcome: OperationOutcome,
    },
}

impl OperationState {
    /// Begin an operation unless another operation is already running.
    pub fn begin(&mut self, kind: OperationKind) -> bool {
        if self.is_busy() {
            return false;
        }
        *self = Self::Running(kind);
        true
    }

    /// Record an outcome only for the currently running operation.
    pub fn finish(&mut self, kind: OperationKind, outcome: OperationOutcome) {
        if *self == Self::Running(kind) {
            *self = Self::Finished { kind, outcome };
        }
    }

    pub fn is_busy(self) -> bool {
        matches!(self, Self::Running(_))
    }

    /// Render a short, localized status line for the menu.
    pub fn menu_text(self) -> String {
        match self {
            Self::Idle => text("operation.idle", "操作: 待機中"),
            Self::Running(kind) => match kind {
                OperationKind::RuntimeRestart => text(
                    "operation.restart.running",
                    "操作: siderostat-runtimeを再起動中…",
                ),
                OperationKind::BackgroundStart => text(
                    "operation.background_enable.running",
                    "操作: siderostat-runtimeを起動して自動起動を有効化中…",
                ),
                OperationKind::BackgroundStop => text(
                    "operation.background_disable.running",
                    "操作: siderostat-runtimeを停止して自動起動を無効化中…",
                ),
                OperationKind::OpenConfig => text(
                    "operation.open_config.running",
                    "操作: 設定ファイルを開いています…",
                ),
                OperationKind::OpenLoginItems => text(
                    "operation.open_login_items.running",
                    "操作: ログイン項目を開いています…",
                ),
            },
            Self::Finished { kind, outcome } => match (kind, outcome) {
                (OperationKind::RuntimeRestart, OperationOutcome::Succeeded) => text(
                    "operation.restart.succeeded",
                    "操作: siderostat-runtimeの再起動を要求しました",
                ),
                (OperationKind::RuntimeRestart, OperationOutcome::Denied) => text(
                    "operation.restart.denied",
                    "操作: siderostat-runtimeの再起動が拒否されました",
                ),
                (OperationKind::RuntimeRestart, _) => text(
                    "operation.restart.failed",
                    "操作: siderostat-runtimeの再起動に失敗しました",
                ),
                (OperationKind::BackgroundStart, OperationOutcome::Succeeded) => text(
                    "operation.background_enable.succeeded",
                    "操作: siderostat-runtimeを起動し、自動起動を有効化しました",
                ),
                (OperationKind::BackgroundStart, OperationOutcome::RequiresApproval) => text(
                    "operation.background_enable.approval",
                    "操作: siderostat-runtimeの自動起動にログイン項目の承認が必要です",
                ),
                (OperationKind::BackgroundStart, OperationOutcome::Denied) => text(
                    "operation.background_enable.denied",
                    "操作: siderostat-runtimeの自動起動が拒否されました",
                ),
                (OperationKind::BackgroundStart, _) => text(
                    "operation.background_enable.failed",
                    "操作: siderostat-runtimeの起動と自動起動の有効化に失敗しました",
                ),
                (OperationKind::BackgroundStop, OperationOutcome::Succeeded) => text(
                    "operation.background_disable.succeeded",
                    "操作: siderostat-runtimeを停止し、自動起動を無効化しました",
                ),
                (OperationKind::BackgroundStop, _) => text(
                    "operation.background_disable.failed",
                    "操作: siderostat-runtimeの停止と自動起動の無効化に失敗しました",
                ),
                (OperationKind::OpenConfig, OperationOutcome::Succeeded) => text(
                    "operation.open_config.succeeded",
                    "操作: 設定ファイルを開きました",
                ),
                (OperationKind::OpenConfig, _) => text(
                    "operation.open_config.failed",
                    "操作: 設定ファイルを開けませんでした",
                ),
                (OperationKind::OpenLoginItems, OperationOutcome::Succeeded) => text(
                    "operation.open_login_items.succeeded",
                    "操作: ログイン項目を開きました",
                ),
                (OperationKind::OpenLoginItems, _) => text(
                    "operation.open_login_items.failed",
                    "操作: ログイン項目を開けませんでした",
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_cannot_be_started_twice() {
        let mut state = OperationState::default();
        assert!(state.begin(OperationKind::RuntimeRestart));
        assert!(!state.begin(OperationKind::BackgroundStop));
        assert_eq!(
            state,
            OperationState::Running(OperationKind::RuntimeRestart)
        );
    }

    #[test]
    fn stale_completion_does_not_replace_current_operation() {
        let mut state = OperationState::default();
        assert!(state.begin(OperationKind::RuntimeRestart));
        state.finish(OperationKind::BackgroundStart, OperationOutcome::Succeeded);
        assert_eq!(
            state,
            OperationState::Running(OperationKind::RuntimeRestart)
        );
    }

    #[test]
    fn finished_operation_can_be_replaced_by_a_new_operation() {
        let mut state = OperationState::default();
        assert!(state.begin(OperationKind::RuntimeRestart));
        state.finish(OperationKind::RuntimeRestart, OperationOutcome::Succeeded);
        assert!(state.begin(OperationKind::BackgroundStop));
        assert_eq!(
            state,
            OperationState::Running(OperationKind::BackgroundStop)
        );
    }

    #[test]
    fn menu_text_distinguishes_progress_and_outcomes() {
        let mut state = OperationState::default();
        assert_eq!(state.menu_text(), "操作: 待機中");
        state.begin(OperationKind::RuntimeRestart);
        assert_eq!(state.menu_text(), "操作: siderostat-runtimeを再起動中…");
        state.finish(OperationKind::RuntimeRestart, OperationOutcome::Succeeded);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeの再起動を要求しました"
        );
        state.begin(OperationKind::RuntimeRestart);
        state.finish(OperationKind::RuntimeRestart, OperationOutcome::Failed);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeの再起動に失敗しました"
        );
    }

    #[test]
    fn approval_and_denial_are_not_reported_as_success() {
        let mut state = OperationState::default();
        state.begin(OperationKind::BackgroundStart);
        state.finish(
            OperationKind::BackgroundStart,
            OperationOutcome::RequiresApproval,
        );
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeの自動起動にログイン項目の承認が必要です"
        );
        state.begin(OperationKind::BackgroundStart);
        state.finish(OperationKind::BackgroundStart, OperationOutcome::Denied);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeの自動起動が拒否されました"
        );

        state.begin(OperationKind::RuntimeRestart);
        state.finish(OperationKind::RuntimeRestart, OperationOutcome::Denied);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeの再起動が拒否されました"
        );
    }

    #[test]
    fn background_toggle_text_describes_service_lifecycle() {
        let mut state = OperationState::default();
        state.begin(OperationKind::BackgroundStart);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeを起動して自動起動を有効化中…"
        );
        state.finish(OperationKind::BackgroundStart, OperationOutcome::Succeeded);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeを起動し、自動起動を有効化しました"
        );

        state.begin(OperationKind::BackgroundStop);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeを停止して自動起動を無効化中…"
        );
        state.finish(OperationKind::BackgroundStop, OperationOutcome::Succeeded);
        assert_eq!(
            state.menu_text(),
            "操作: siderostat-runtimeを停止し、自動起動を無効化しました"
        );
    }
}
