//! Localized strings for the menu bar application.
//!
//! The source fallback is Japanese so unit tests and non-bundled developer
//! runs remain usable. In the `.app` bundle, `NSBundle` selects the matching
//! `Localizable.strings` resource according to the macOS language settings.

#[cfg(target_os = "macos")]
use objc2_foundation::{NSBundle, NSString, NSUserDefaults};

/// Resolve a UI string from the application bundle, falling back to `fallback`.
pub fn text(key: &str, fallback: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        let bundle = NSBundle::mainBundle();
        let key = NSString::from_str(key);
        let fallback = NSString::from_str(fallback);
        let preferences_key = NSString::from_str("AppleLanguages");
        let preferences =
            NSUserDefaults::standardUserDefaults().stringArrayForKey(&preferences_key);
        let localizations = bundle.localizations();
        let preferred = NSBundle::preferredLocalizationsFromArray_forPreferences(
            &localizations,
            preferences.as_deref(),
        );
        bundle
            .localizedStringForKey_value_table_localizations(
                &key,
                Some(&fallback),
                None,
                &preferred,
            )
            .to_string()
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = key;
        fallback.to_owned()
    }
}

/// App version/build metadata from the bundle or developer fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppMetadata {
    pub version: String,
    pub build: String,
}

/// Return the app version/build metadata shown during first launch and used by
/// the runtime handshake. The ad-hoc developer binary is also usable outside
/// an app bundle, so the package version and an `unknown` build fallback are
/// intentionally retained.
pub fn app_metadata_info() -> AppMetadata {
    #[cfg(target_os = "macos")]
    {
        let bundle = NSBundle::mainBundle();
        let value = |key: &str| {
            let key = NSString::from_str(key);
            bundle
                .objectForInfoDictionaryKey(&key)
                .and_then(|object| object.downcast::<NSString>().ok())
                .map(|value| value.to_string())
        };
        let version = value("CFBundleShortVersionString")
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        let build = value("CFBundleVersion").unwrap_or_else(|| "unknown".to_string());
        AppMetadata { version, build }
    }

    #[cfg(not(target_os = "macos"))]
    {
        AppMetadata {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: "unknown".to_string(),
        }
    }
}

pub fn app_metadata() -> String {
    let metadata = app_metadata_info();
    format!("version={} build={}", metadata.version, metadata.build)
}

#[cfg(test)]
mod tests {
    use super::text;

    #[test]
    fn missing_key_uses_source_fallback() {
        assert_eq!(
            text("missing.localization.key", "フォールバック"),
            "フォールバック"
        );
    }
}
