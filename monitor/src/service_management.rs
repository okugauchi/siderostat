//! SMAppService adapter (C-01).
//!
//! A small interface that lets the menu / first-launch UI read the platform
//! status of the runtime LaunchAgent helper and the main-app login item
//! without touching framework objects directly. This task implements status
//! reads only; registration state is NOT changed here (C-02 adds register /
//! unregister).
//!
//! The macOS implementation uses the ServiceManagement framework via objc2.
//! The test fake is a separate type so UI logic can be exercised without a
//! framework session.
//!
//! The adapter is exercised from the first-launch / menu UI in C-05; until it
//! is wired to production code the module is dead code from the crate's
//! perspective, so allow dead_code here.
#![allow(dead_code)]

#[cfg(target_os = "macos")]
use core::ffi::c_uint;
use std::fmt;

/// Platform status of a service, mapped from `SMAppService.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Not registered with Service Management.
    NotRegistered,
    /// Registered and eligible to run.
    Enabled,
    /// Registered but the user must approve it in System Settings first.
    RequiresApproval,
    /// An error occurred and no such service could be found.
    NotFound,
    /// A platform error was reported while querying status.
    Error,
}

impl fmt::Display for ServiceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ServiceStatus::NotRegistered => "not_registered",
            ServiceStatus::Enabled => "enabled",
            ServiceStatus::RequiresApproval => "requires_approval",
            ServiceStatus::NotFound => "not_found",
            ServiceStatus::Error => "error",
        };
        f.write_str(name)
    }
}

/// Identifies a service managed through Service Management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    /// The runtime LaunchAgent helper (`dev.siderostat-ds4-proxy.runtime`).
    RuntimeAgent,
    /// The main application as a login item (menu bar at login).
    MainAppLoginItem,
}

impl ServiceKind {
    /// The bundle-internal LaunchAgent plist name for the runtime helper.
    pub const RUNTIME_PLIST_NAME: &'static str = "dev.siderostat-ds4-proxy.runtime.plist";

    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::RuntimeAgent => "runtime-background-service",
            ServiceKind::MainAppLoginItem => "main-app-login-item",
        }
    }
}

/// A small adapter over Service Management.
///
/// The macOS implementation reads `SMAppService` directly. The `Fake`
/// implementation is used by tests and, in bundle mode, keeps UI logic
/// decoupled from framework objects.
pub trait ServiceManagementAdapter: Send {
    /// Read the current platform status. This does not change registration.
    fn status(&self, kind: ServiceKind) -> ServiceStatus;

    /// Register a service so it may launch, subject to user consent (C-02).
    /// After a successful platform registration the status is re-read and an
    /// approval shortage is reported as `RequiresApproval`, never as success.
    fn register(&self, kind: ServiceKind) -> RegisterOutcome;

    /// Unregister a service. Running it twice is a safe no-op: the second call
    /// observes `NotRegistered` and returns `Unregistered`.
    fn unregister(&self, kind: ServiceKind) -> UnregisterOutcome;
}

/// Result of a `register` attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Platform registration succeeded and the status is now `Enabled`.
    Registered,
    /// Platform registration succeeded but the user must approve in System
    /// Settings before it can run. Not treated as success.
    RequiresApproval,
    /// The user denied the launch in System Settings.
    DeniedByUser,
    /// A platform error occurred.
    Error(String),
}

/// Result of an `unregister` attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnregisterOutcome {
    /// The service was unregistered.
    Unregistered,
    /// The service was already not registered (safe no-op).
    AlreadyNotRegistered,
    /// A platform error occurred.
    Error(String),
}

/// Real macOS implementation backed by `SMAppService`.
///
/// `SMAppService` is only usable from the main thread on macOS; callers must
/// ensure this adapter is driven from the AppKit main thread.
#[cfg(target_os = "macos")]
pub struct ServiceManagement {
    // Holds nothing beyond the framework objects created per call; kept as a
    // marker so the type is distinct from the fake and carries no global state.
    _private: (),
}

#[cfg(target_os = "macos")]
impl ServiceManagement {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

#[cfg(target_os = "macos")]
impl Default for ServiceManagement {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
impl ServiceManagementAdapter for ServiceManagement {
    fn status(&self, kind: ServiceKind) -> ServiceStatus {
        // SAFETY: these framework calls are made on the main thread (the caller
        // drives this from AppKit). The returned status is copied out of the
        // autoreleased object before it is dropped.
        match kind {
            ServiceKind::RuntimeAgent => {
                let plist = objc2_foundation::NSString::from_str(ServiceKind::RUNTIME_PLIST_NAME);
                let service = unsafe {
                    objc2_service_management::SMAppService::agentServiceWithPlistName(&plist)
                };
                map_status(unsafe { service.status() })
            }
            ServiceKind::MainAppLoginItem => {
                let service = unsafe { objc2_service_management::SMAppService::mainAppService() };
                map_status(unsafe { service.status() })
            }
        }
    }

    fn register(&self, kind: ServiceKind) -> RegisterOutcome {
        let service = self.service(kind);
        // SAFETY: main-thread framework call (see status).
        if let Err(error) = unsafe { service.registerAndReturnError() } {
            return classify_register_error(&error);
        }
        // Do not fake approval shortages as success: re-read the real status.
        match self.status(kind) {
            ServiceStatus::Enabled => RegisterOutcome::Registered,
            ServiceStatus::RequiresApproval => RegisterOutcome::RequiresApproval,
            other => RegisterOutcome::Error(format!("unexpected status after register: {other}")),
        }
    }

    fn unregister(&self, kind: ServiceKind) -> UnregisterOutcome {
        let service = self.service(kind);
        // SAFETY: main-thread framework call.
        if let Err(error) = unsafe { service.unregisterAndReturnError() } {
            return classify_unregister_error(&error);
        }
        UnregisterOutcome::Unregistered
    }
}

#[cfg(target_os = "macos")]
impl ServiceManagement {
    /// Build the `SMAppService` handle for a service kind.
    fn service(
        &self,
        kind: ServiceKind,
    ) -> objc2::rc::Retained<objc2_service_management::SMAppService> {
        match kind {
            ServiceKind::RuntimeAgent => {
                let plist = objc2_foundation::NSString::from_str(ServiceKind::RUNTIME_PLIST_NAME);
                // SAFETY: main-thread framework call; the retained object outlives
                // the autorelease scope of the caller.
                unsafe { objc2_service_management::SMAppService::agentServiceWithPlistName(&plist) }
            }
            ServiceKind::MainAppLoginItem => {
                // SAFETY: main-thread framework call.
                unsafe { objc2_service_management::SMAppService::mainAppService() }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn classify_register_error(error: &objc2_foundation::NSError) -> RegisterOutcome {
    classify_register_code(error.code() as c_uint)
}

#[cfg(target_os = "macos")]
fn classify_register_code(code: c_uint) -> RegisterOutcome {
    use objc2_service_management::kSMErrorLaunchDeniedByUser;
    if code == kSMErrorLaunchDeniedByUser {
        RegisterOutcome::DeniedByUser
    } else {
        RegisterOutcome::Error(format!("register failed (code {code})"))
    }
}

#[cfg(target_os = "macos")]
fn classify_unregister_error(error: &objc2_foundation::NSError) -> UnregisterOutcome {
    classify_unregister_code(error.code() as c_uint)
}

#[cfg(target_os = "macos")]
fn classify_unregister_code(code: c_uint) -> UnregisterOutcome {
    use objc2_service_management::kSMErrorJobNotFound;
    // Unregistering an already-not-registered service reports job-not-found;
    // treat that as a safe no-op.
    if code == kSMErrorJobNotFound {
        UnregisterOutcome::AlreadyNotRegistered
    } else {
        UnregisterOutcome::Error(format!("unregister failed (code {code})"))
    }
}

#[cfg(target_os = "macos")]
fn map_status(status: objc2_service_management::SMAppServiceStatus) -> ServiceStatus {
    use objc2_service_management::SMAppServiceStatus as S;
    match status {
        S::NotRegistered => ServiceStatus::NotRegistered,
        S::Enabled => ServiceStatus::Enabled,
        S::RequiresApproval => ServiceStatus::RequiresApproval,
        S::NotFound => ServiceStatus::NotFound,
        // A future/unknown raw value maps to the safe "error" bucket.
        _ => ServiceStatus::Error,
    }
}

/// Test double: returns a scripted status per service kind and scripted
/// register / unregister outcomes.
#[derive(Debug, Clone, Default)]
pub struct FakeServiceManagement {
    statuses: std::collections::HashMap<ServiceKind, ServiceStatus>,
    register_outcomes: std::collections::HashMap<ServiceKind, RegisterOutcome>,
    unregister_outcomes: std::collections::HashMap<ServiceKind, UnregisterOutcome>,
}

impl FakeServiceManagement {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a status for a service kind.
    pub fn set_status(&mut self, kind: ServiceKind, status: ServiceStatus) {
        self.statuses.insert(kind, status);
    }

    /// Script the outcome of `register` for a kind.
    pub fn set_register_outcome(&mut self, kind: ServiceKind, outcome: RegisterOutcome) {
        self.register_outcomes.insert(kind, outcome);
    }

    /// Script the outcome of `unregister` for a kind.
    pub fn set_unregister_outcome(&mut self, kind: ServiceKind, outcome: UnregisterOutcome) {
        self.unregister_outcomes.insert(kind, outcome);
    }
}

impl ServiceManagementAdapter for FakeServiceManagement {
    fn status(&self, kind: ServiceKind) -> ServiceStatus {
        self.statuses
            .get(&kind)
            .copied()
            .unwrap_or(ServiceStatus::NotRegistered)
    }

    fn register(&self, kind: ServiceKind) -> RegisterOutcome {
        self.register_outcomes
            .get(&kind)
            .cloned()
            .unwrap_or(RegisterOutcome::Registered)
    }

    fn unregister(&self, kind: ServiceKind) -> UnregisterOutcome {
        self.unregister_outcomes
            .get(&kind)
            .cloned()
            .unwrap_or(UnregisterOutcome::Unregistered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_for(kind: ServiceKind, status: ServiceStatus) -> ServiceStatus {
        let mut fake = FakeServiceManagement::new();
        fake.set_status(kind, status);
        fake.status(kind)
    }

    #[test]
    fn status_display_names_are_stable_snake_case() {
        assert_eq!(ServiceStatus::NotRegistered.to_string(), "not_registered");
        assert_eq!(ServiceStatus::Enabled.to_string(), "enabled");
        assert_eq!(
            ServiceStatus::RequiresApproval.to_string(),
            "requires_approval"
        );
        assert_eq!(ServiceStatus::NotFound.to_string(), "not_found");
        assert_eq!(ServiceStatus::Error.to_string(), "error");
    }

    #[test]
    fn service_kind_labels_are_stable() {
        assert_eq!(
            ServiceKind::RuntimeAgent.label(),
            "runtime-background-service"
        );
        assert_eq!(ServiceKind::MainAppLoginItem.label(), "main-app-login-item");
        assert_eq!(
            ServiceKind::RUNTIME_PLIST_NAME,
            "dev.siderostat-ds4-proxy.runtime.plist"
        );
    }

    #[test]
    fn fake_returns_scripted_status_per_kind() {
        assert_eq!(
            status_for(ServiceKind::RuntimeAgent, ServiceStatus::Enabled),
            ServiceStatus::Enabled
        );
        assert_eq!(
            status_for(
                ServiceKind::MainAppLoginItem,
                ServiceStatus::RequiresApproval
            ),
            ServiceStatus::RequiresApproval
        );
    }

    #[test]
    fn fake_defaults_to_not_registered() {
        let fake = FakeServiceManagement::new();
        assert_eq!(
            fake.status(ServiceKind::RuntimeAgent),
            ServiceStatus::NotRegistered
        );
        assert_eq!(
            fake.status(ServiceKind::MainAppLoginItem),
            ServiceStatus::NotRegistered
        );
    }

    #[test]
    fn runtime_and_login_item_are_independent() {
        let mut fake = FakeServiceManagement::new();
        fake.set_status(ServiceKind::RuntimeAgent, ServiceStatus::Enabled);
        fake.set_status(ServiceKind::MainAppLoginItem, ServiceStatus::NotRegistered);
        assert_eq!(
            fake.status(ServiceKind::RuntimeAgent),
            ServiceStatus::Enabled
        );
        assert_eq!(
            fake.status(ServiceKind::MainAppLoginItem),
            ServiceStatus::NotRegistered
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn map_status_covers_all_platform_statuses() {
        use objc2_service_management::SMAppServiceStatus as S;
        assert_eq!(map_status(S::NotRegistered), ServiceStatus::NotRegistered);
        assert_eq!(map_status(S::Enabled), ServiceStatus::Enabled);
        assert_eq!(
            map_status(S::RequiresApproval),
            ServiceStatus::RequiresApproval
        );
        assert_eq!(map_status(S::NotFound), ServiceStatus::NotFound);
        // Unknown raw values fall into the safe error bucket.
        assert_eq!(map_status(S(S::NotFound.0 + 1)), ServiceStatus::Error);
    }

    // ---- C-02: register / unregister wiring ----

    #[test]
    fn register_reports_success_when_status_becomes_enabled() {
        let mut fake = FakeServiceManagement::new();
        fake.set_status(ServiceKind::RuntimeAgent, ServiceStatus::Enabled);
        fake.set_register_outcome(ServiceKind::RuntimeAgent, RegisterOutcome::Registered);
        assert_eq!(
            fake.register(ServiceKind::RuntimeAgent),
            RegisterOutcome::Registered
        );
    }

    #[test]
    fn register_reports_requires_approval_when_status_needs_approval() {
        let mut fake = FakeServiceManagement::new();
        fake.set_register_outcome(ServiceKind::RuntimeAgent, RegisterOutcome::RequiresApproval);
        fake.set_status(ServiceKind::RuntimeAgent, ServiceStatus::RequiresApproval);
        assert_eq!(
            fake.register(ServiceKind::RuntimeAgent),
            RegisterOutcome::RequiresApproval
        );
    }

    #[test]
    fn register_reports_denied_by_user() {
        let mut fake = FakeServiceManagement::new();
        fake.set_register_outcome(ServiceKind::RuntimeAgent, RegisterOutcome::DeniedByUser);
        assert_eq!(
            fake.register(ServiceKind::RuntimeAgent),
            RegisterOutcome::DeniedByUser
        );
    }

    #[test]
    fn register_reports_framework_error() {
        let mut fake = FakeServiceManagement::new();
        fake.set_register_outcome(
            ServiceKind::RuntimeAgent,
            RegisterOutcome::Error("register failed".into()),
        );
        assert_eq!(
            fake.register(ServiceKind::RuntimeAgent),
            RegisterOutcome::Error("register failed".into())
        );
    }

    #[test]
    fn unregister_reports_success() {
        let mut fake = FakeServiceManagement::new();
        fake.set_status(ServiceKind::RuntimeAgent, ServiceStatus::Enabled);
        fake.set_unregister_outcome(ServiceKind::RuntimeAgent, UnregisterOutcome::Unregistered);
        assert_eq!(
            fake.unregister(ServiceKind::RuntimeAgent),
            UnregisterOutcome::Unregistered
        );
    }

    #[test]
    fn double_unregister_is_a_safe_no_op() {
        // 二重 unregister: 2回目は AlreadyNotRegistered を返す安全な no-op。
        let mut fake = FakeServiceManagement::new();
        fake.set_unregister_outcome(
            ServiceKind::RuntimeAgent,
            UnregisterOutcome::AlreadyNotRegistered,
        );
        assert_eq!(
            fake.unregister(ServiceKind::RuntimeAgent),
            UnregisterOutcome::AlreadyNotRegistered
        );
        // 再実行しても同じ no-op が返る（状態を mutation しない）。
        assert_eq!(
            fake.unregister(ServiceKind::RuntimeAgent),
            UnregisterOutcome::AlreadyNotRegistered
        );
    }

    #[test]
    fn unregister_reports_framework_error() {
        let mut fake = FakeServiceManagement::new();
        fake.set_unregister_outcome(
            ServiceKind::RuntimeAgent,
            UnregisterOutcome::Error("unregister failed".into()),
        );
        assert_eq!(
            fake.unregister(ServiceKind::RuntimeAgent),
            UnregisterOutcome::Error("unregister failed".into())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn register_error_classification_denied_and_generic() {
        use objc2_service_management::kSMErrorLaunchDeniedByUser;
        // kSMErrorLaunchDeniedByUser -> DeniedByUser。
        assert_eq!(
            classify_register_code(kSMErrorLaunchDeniedByUser),
            RegisterOutcome::DeniedByUser
        );
        // その他の error code は generic Error へ。
        assert!(matches!(
            classify_register_code(999),
            RegisterOutcome::Error(msg) if msg.contains("register failed")
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unregister_error_classification_job_not_found_and_generic() {
        use objc2_service_management::kSMErrorJobNotFound;
        // kSMErrorJobNotFound -> 安全な no-op（二重 unregister）。
        assert_eq!(
            classify_unregister_code(kSMErrorJobNotFound),
            UnregisterOutcome::AlreadyNotRegistered
        );
        // その他の error code は generic Error へ。
        assert!(matches!(
            classify_unregister_code(999),
            UnregisterOutcome::Error(msg) if msg.contains("unregister failed")
        ));
    }
}
