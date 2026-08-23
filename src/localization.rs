//! Localized strings shared by runtime and menu-bar notifications.

/// Resolve a UI string from the application bundle, falling back to `fallback`.
pub(crate) fn text(key: &str, fallback: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSBundle, NSString, NSUserDefaults};
        use std::{env, path::PathBuf};

        // The runtime is launched from Contents/Helpers, so mainBundle can
        // describe the helper executable rather than the containing app.
        // Resolve the parent app bundle first and retain mainBundle as the
        // fallback for developer binaries launched outside an app bundle.
        let app_bundle_path = env::current_exe().ok().and_then(|executable| {
            executable
                .ancestors()
                .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("Contents"))
                .and_then(|contents| contents.parent())
                .map(PathBuf::from)
        });
        let bundle = app_bundle_path
            .and_then(|path| {
                let path = NSString::from_str(&path.to_string_lossy());
                NSBundle::bundleWithPath(&path)
            })
            .unwrap_or_else(NSBundle::mainBundle);
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
