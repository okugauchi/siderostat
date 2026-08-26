//! B-02: macOS bundle template の静的検査。
//!
//! bundle template（Info.plist.in、LaunchAgent plist、entitlements.plist）に、
//! user 固有絶対 path、secret、legacy install 参照、`/usr/local/bin`、
//! `get-task-allow` が含まれないことを検証する。plist は署名済み bundle の一部として
//! install / first launch 時に書き換えず、user home の絶対 path を埋め込まない。

use std::path::PathBuf;

/// 検査対象の template file（repository root からの相対 path）。
const TEMPLATES: &[&str] = &[
    "contrib/macos/Info.plist.in",
    "contrib/macos/Uninstaller-Info.plist.in",
    "contrib/macos/dev.siderostat-ds4-proxy.runtime.plist",
    "contrib/macos/entitlements.plist",
];

/// bundle plist に含まれてはならない禁止文字列。
/// - `/Users/` / `$HOME` / `/Users/USERNAME`: user 固有絶対 path
/// - `/usr/local/bin`: legacy install 参照
/// - `local.siderostat`: legacy LaunchAgent label
/// - `get-task-allow`: 配布署名に含めてはならない entitlement
/// - `secret` / `PLACEHOLDER`: secret や credential 参照
const FORBIDDEN: &[&str] = &[
    "/Users/",
    "$HOME",
    "/usr/local/bin",
    "local.siderostat",
    "get-task-allow",
    "secret",
    "PLACEHOLDER",
];

fn repo_root() -> PathBuf {
    // cargo test の cwd は crate root。
    std::env::current_dir().expect("resolve current dir")
}

#[test]
fn bundle_templates_contain_no_forbidden_strings() {
    for relative in TEMPLATES {
        let path = repo_root().join(relative);
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for needle in FORBIDDEN {
            assert!(
                !contents.contains(needle),
                "{} must not contain forbidden string {:?}",
                relative,
                needle
            );
        }
    }
}

#[test]
fn bundle_templates_prescribe_fixed_runtime_program_path() {
    let runtime_plist = repo_root().join("contrib/macos/dev.siderostat-ds4-proxy.runtime.plist");
    let contents = std::fs::read_to_string(&runtime_plist).unwrap();
    // pkg の postinstall から launchctl bootstrap するため、Program は固定の
    // インストール先を明示する。SMAppService からも同じ plist を利用する。
    assert!(contents.contains("<key>Program</key>"));
    assert!(contents.contains("/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime"));
    assert!(!contents.contains("<key>BundleProgram</key>"));
    // ProgramArguments は runtime の固定 subcommand を渡す。
    assert!(contents.contains("<string>siderostat-runtime</string>"));
    assert!(contents.contains("<string>serve</string>"));
    // RunAtLoad / KeepAlive / ThrottleInterval が記載される。
    assert!(contents.contains("<key>RunAtLoad</key>"));
    assert!(contents.contains("<key>KeepAlive</key>"));
    assert!(contents.contains("<key>ThrottleInterval</key>"));
}

#[test]
fn bundle_templates_use_new_identifier_and_version_placeholders() {
    let info = repo_root().join("contrib/macos/Info.plist.in");
    let contents = std::fs::read_to_string(&info).unwrap();
    assert!(contents.contains("<string>dev.siderostat-ds4-proxy</string>"));
    assert!(contents.contains("<key>LSUIElement</key>"));
    // version / build number は builder が置換する placeholder。
    assert!(contents.contains("<string>@VERSION@</string>"));
    assert!(contents.contains("<string>@BUILD_NUMBER@</string>"));

    let runtime = repo_root().join("contrib/macos/dev.siderostat-ds4-proxy.runtime.plist");
    let runtime_contents = std::fs::read_to_string(&runtime).unwrap();
    assert!(runtime_contents.contains("<string>dev.siderostat-ds4-proxy.runtime</string>"));

    let uninstaller = repo_root().join("contrib/macos/Uninstaller-Info.plist.in");
    let uninstaller_contents = std::fs::read_to_string(&uninstaller).unwrap();
    assert!(uninstaller_contents.contains("dev.siderostat-ds4-proxy.uninstaller"));
    assert!(uninstaller_contents.contains("Siderostat Uninstaller"));
    assert!(uninstaller_contents.contains("<key>LSUIElement</key>\n  <false/>"));
    assert!(!uninstaller_contents.contains("CFBundleIconFile"));
    assert!(!uninstaller_contents.contains("AppIcon"));

    let entitlements = repo_root().join("contrib/macos/entitlements.plist");
    let entitlements_contents = std::fs::read_to_string(&entitlements).unwrap();
    assert!(entitlements_contents.contains("<dict>"));
}

#[test]
fn resources_include_license_notices_and_default_config() {
    let resources = repo_root().join("contrib/macos/Resources");
    for name in ["LICENSE", "THIRD-PARTY-NOTICES.md", "default-config.toml"] {
        assert!(
            resources.join(name).is_file(),
            "missing resource {name} under {}",
            resources.display()
        );
    }
    let license = std::fs::read_to_string(resources.join("LICENSE")).unwrap();
    assert!(license.contains("MIT License"));
    for locale in ["en.lproj", "ja.lproj"] {
        let strings = resources.join(locale).join("Localizable.strings");
        assert!(
            strings.is_file(),
            "missing localized resource {}",
            strings.display()
        );
        let contents = std::fs::read_to_string(strings).unwrap();
        assert!(contents.contains("app.name"));
        assert!(contents.contains("menu.settings"));
        assert!(contents.contains("menu.quit"));
        assert!(contents.contains("first_launch.version"));
        assert!(contents.contains("first_launch.approval"));
        assert!(contents.contains("first_launch.model_ready"));
        assert!(contents.contains("uninstaller.confirm.title"));
        assert!(contents.contains("uninstaller.failure.title"));
    }
}
