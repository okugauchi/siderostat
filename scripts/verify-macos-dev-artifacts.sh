#!/usr/bin/env bash
# E-03: certificate-free app/pkg verification for pull-request CI.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.3.1}"
BUILD_NUMBER="${BUILD_NUMBER:-1}"
APP_STAGING="${APP_STAGING:-$ROOT_DIR/build/app-dev-ci}"
OUTPUT_DIR="${OUTPUT_DIR:-$ROOT_DIR/build/pkg-ci-output}"
EXPAND_DIR="${EXPAND_DIR:-$ROOT_DIR/build/pkg-ci-expand}"

fail() {
    echo "E-03 verification failed: $*" >&2
    return 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command is missing: $1"
}

verify_plist() {
    local plist="$1"
    [[ -f "$plist" ]] || { fail "plist is missing: $plist"; return 1; }
    plutil -lint "$plist" >/dev/null || { fail "invalid plist: $plist"; return 1; }
}

verify_bundle_file_list() {
    local app="$1"
    local file rel
    while IFS= read -r -d '' file; do
        rel="${file#"$app"/}"
        case "$rel" in
            *.gguf|*.bin|*.key|*.pem|*.p12|*.p8|*.token|*.secret|*.sqlite|*.db|*.log)
                fail "model or credential-like file in app: $rel" || return 1
                ;;
            *secret*|*password*|*credential*|*Application\ Support*|*Library/Logs*|*models*|*model*|*cache*)
                fail "user-data or model-like path in app: $rel" || return 1
                ;;
        esac
    done < <(find "$app" -type f -print0)
}

verify_bundle() {
    local app="$1"
    local contents="$app/Contents"
    local info="$contents/Info.plist"
    local launch_agent="$contents/Library/LaunchAgents/dev.siderostat-ds4-proxy.runtime.plist"
    local icon="$contents/Resources/AppIcon.icns"
    local main="$contents/MacOS/Siderostat"
    local helper="$contents/Helpers/siderostat-runtime"
    local identifier

    [[ -d "$app" ]] || { fail "app bundle is missing: $app"; return 1; }
    [[ -x "$main" ]] || { fail "main executable is missing or not executable: $main"; return 1; }
    [[ -x "$helper" ]] || { fail "helper executable is missing or not executable: $helper"; return 1; }
    verify_plist "$info" || return 1
    verify_plist "$launch_agent" || return 1
    grep -Fq '<key>Program</key>' "$launch_agent" || {
        fail "runtime LaunchAgent must use Program for direct pkg bootstrap"
        return 1
    }
    grep -Fq '/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime' "$launch_agent" || {
        fail "runtime LaunchAgent is missing the fixed installed helper path"
        return 1
    }
    if grep -Fq '<key>BundleProgram</key>' "$launch_agent"; then
        fail "runtime LaunchAgent must not use BundleProgram for direct pkg bootstrap"
        return 1
    fi
    [[ -f "$icon" ]] || { fail "app icon is missing: $icon"; return 1; }
    file "$icon" | grep -Eq 'Mac OS X icon|Apple Icon' || {
        fail "app icon is not an ICNS file: $icon"
        return 1
    }

    identifier="$(plutil -extract CFBundleIdentifier raw -o - "$info")" || return 1
    [[ "$identifier" == "dev.siderostat-ds4-proxy" ]] || {
        fail "unexpected bundle identifier: $identifier"
        return 1
    }

    if grep -Eq '/Users/|/home/|/usr/local/bin|USERNAME|SECRET|PASSWORD|get-task-allow' \
        "$launch_agent"; then
        fail "forbidden absolute path or secret marker in launch agent"
        return 1
    fi

    codesign --verify --strict "$helper" >/dev/null 2>&1 || \
        { fail "nested helper signature is invalid: $helper"; return 1; }
    codesign --verify --strict "$main" >/dev/null 2>&1 || \
        { fail "main executable signature is invalid: $main"; return 1; }
    codesign --verify --deep --strict "$app" >/dev/null 2>&1 || \
        { fail "bundle signature is invalid: $app"; return 1; }
    verify_bundle_file_list "$app" || return 1
}

verify_expanded_package() {
    local expand_dir="$1"
    local payload_dir payload_entry file rel scripts_dir script package_info
    local payload_dirs=()
    local entries=()
    local scripts_dirs=()
    local script_files=()

    while IFS= read -r -d '' payload_dir; do
        payload_dirs+=("$payload_dir")
    done < <(find "$expand_dir" -type d -name Payload -print0)
    [[ "${#payload_dirs[@]}" -eq 1 ]] || {
        fail "expected exactly one expanded Payload directory, found ${#payload_dirs[@]}"
        return 1
    }
    payload_dir="${payload_dirs[0]}"
    package_info="$(dirname "$payload_dir")/PackageInfo"
    [[ -f "$package_info" ]] || {
        fail "component PackageInfo is missing: $package_info"
        return 1
    }
    grep -Eq 'install-location="/Applications"' "$package_info" || {
        fail "package install location is not /Applications"
        return 1
    }

    shopt -s nullglob
    entries=("$payload_dir"/*)
    shopt -u nullglob
    [[ "${#entries[@]}" -eq 1 ]] || {
        fail "expected exactly one package payload entry, found ${#entries[@]}"
        return 1
    }
    payload_entry="${entries[0]##*/}"
    [[ "$payload_entry" == "Siderostat.app" ]] || {
        fail "unexpected package payload entry: $payload_entry"
        return 1
    }
    [[ -d "$payload_dir/Siderostat.app" ]] || {
        fail "Siderostat.app is missing from package payload"
        return 1
    }

    while IFS= read -r -d '' file; do
        rel="${file#"$payload_dir"/}"
        case "$rel" in
            Siderostat.app|Siderostat.app/*) ;;
            *) fail "forbidden package payload path: $rel" || return 1 ;;
        esac
    done < <(find "$payload_dir" -type f -print0)

    while IFS= read -r -d '' scripts_dir; do
        scripts_dirs+=("$scripts_dir")
    done < <(find "$expand_dir" -type d -name Scripts -print0)
    [[ "${#scripts_dirs[@]}" -eq 1 ]] || {
        fail "expected exactly one Scripts directory, found ${#scripts_dirs[@]}"
        return 1
    }
    while IFS= read -r -d '' script; do
        script_files+=("$script")
    done < <(find "${scripts_dirs[0]}" -type f -print0)
    [[ "${#script_files[@]}" -eq 2 ]] || {
        fail "expected exactly two installer scripts, found ${#script_files[@]}"
        return 1
    }
    local preinstall_script=""
    local postinstall_script=""
    for script in "${script_files[@]}"; do
        case "${script##*/}" in
            preinstall) preinstall_script="$script" ;;
            postinstall) postinstall_script="$script" ;;
            *)
                fail "unexpected installer script: $script"
                return 1
                ;;
        esac
    done
    [[ -n "$preinstall_script" && -n "$postinstall_script" ]] || {
        fail "installer scripts must include preinstall and postinstall"
        return 1
    }
    sh -n "$preinstall_script" || {
        fail "preinstall script has invalid shell syntax: $preinstall_script"
        return 1
    }
    sh -n "$postinstall_script" || {
        fail "postinstall script has invalid shell syntax: $postinstall_script"
        return 1
    }
    grep -Fq "/Applications/Siderostat.app/Contents/MacOS/Siderostat" "$preinstall_script" || {
        fail "preinstall script is missing the exact Monitor path"
        return 1
    }
    grep -Fq "/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime" "$preinstall_script" || {
        fail "preinstall script is missing the exact runtime path"
        return 1
    }
    grep -Fq "dev.siderostat-ds4-proxy.runtime" "$preinstall_script" || {
        fail "preinstall script is missing the product runtime LaunchAgent label"
        return 1
    }
    grep -Fq "CONSOLE_UID" "$preinstall_script" || {
        fail "preinstall script is missing GUI-session scoping"
        return 1
    }
    grep -Fq "/bin/launchctl bootout" "$preinstall_script" || {
        fail "preinstall script is missing runtime LaunchAgent stop"
        return 1
    }
    grep -Fq "/bin/launchctl kill SIGKILL" "$preinstall_script" || {
        fail "preinstall script is missing runtime force-stop fallback"
        return 1
    }
    local runtime_stop_line monitor_stop_line
    runtime_stop_line="$(grep -n '^stop_runtime_job$' "$preinstall_script" | tail -1 | cut -d: -f1)"
    monitor_stop_line="$(grep -n '^stop_monitor$' "$preinstall_script" | tail -1 | cut -d: -f1)"
    [[ -n "$runtime_stop_line" && -n "$monitor_stop_line" && "$runtime_stop_line" -lt "$monitor_stop_line" ]] || {
        fail "preinstall script must stop runtime before Monitor"
        return 1
    }
    grep -Fq "/bin/kill -TERM" "$preinstall_script" || {
        fail "preinstall script is missing SIGTERM handling"
        return 1
    }
    grep -Fq "/bin/kill -KILL" "$preinstall_script" || {
        fail "preinstall script is missing SIGKILL fallback"
        return 1
    }
    if grep -Eq 'killall|pkill|pkgutil --forget|rm -rf|Application Support|/usr/local/bin' "$preinstall_script"; then
        fail "preinstall script contains an unsafe broad process or user-data operation"
        return 1
    fi
    grep -Fq "/bin/launchctl asuser" "$postinstall_script" || {
        fail "postinstall script is missing the active-user launch request"
        return 1
    }
    grep -Fq "/bin/launchctl print-disabled" "$postinstall_script" || {
        fail "postinstall script is missing the runtime registration-state check"
        return 1
    }
    grep -Fq "/bin/launchctl bootstrap" "$postinstall_script" || {
        fail "postinstall script is missing the enabled runtime bootstrap"
        return 1
    }
    grep -Fq "dev.siderostat-ds4-proxy.runtime.plist" "$postinstall_script" || {
        fail "postinstall script is missing the runtime LaunchAgent plist"
        return 1
    }
    local bootstrap_line app_launch_line
    bootstrap_line="$(grep -n '/bin/launchctl bootstrap' "$postinstall_script" | tail -1 | cut -d: -f1)"
    app_launch_line="$(grep -n '/usr/bin/open -a' "$postinstall_script" | tail -1 | cut -d: -f1)"
    [[ -n "$bootstrap_line" && -n "$app_launch_line" && "$bootstrap_line" -lt "$app_launch_line" ]] || {
        fail "postinstall script must bootstrap runtime before launching the app"
        return 1
    }
    grep -Fq "/usr/bin/open -a" "$postinstall_script" || {
        fail "postinstall script is missing the app launch request"
        return 1
    }
    grep -Fq "/Applications/Siderostat.app" "$postinstall_script" || {
        fail "postinstall script is missing the exact app path"
        return 1
    }
    if grep -Eq 'killall|pkill|ds4-server|Application Support|/usr/local/bin|pkgutil --forget|launchctl disable|SMAppService\.unregister' "$postinstall_script"; then
        fail "postinstall script contains an out-of-scope process, service, or user-data operation"
        return 1
    fi

    for script in "$expand_dir/.preinstall" "$expand_dir/.postinstall"; do
        [[ ! -e "$script" ]] || { fail "installer script found: $script"; return 1; }
    done

    while IFS= read -r -d '' file; do
        rel="${file#"$expand_dir"/}"
        case "$rel" in
            *.gguf|*.key|*.pem|*.p12|*.p8|*.token|*.secret|*.sqlite|*.db|*.log|*secret*|*password*|*credential*|*Application\ Support*|*Library/Logs*|*models*|*model*|*cache*)
                fail "model, credential, or user-data-like file in package: $rel" || return 1
                ;;
        esac
    done < <(find "$expand_dir" -type f -print0)
}

expect_failure() {
    local description="$1"
    shift
    if "$@"; then
        fail "fixture unexpectedly passed: $description"
        return 1
    fi
    echo "fixture failure confirmed: $description"
}

run_fixture_tests() {
    local fixture
    fixture="$(mktemp -d "${TMPDIR:-/tmp}/siderostat-e03-fixtures.XXXXXX")"

    printf '%s\n' '<plist><dict>' > "$fixture/broken.plist"
    expect_failure "broken plist" verify_plist "$fixture/broken.plist"

    mkdir -p "$fixture/extra-payload/Payload/Applications" \
        "$fixture/extra-payload/Payload/usr/local/bin"
    printf '%s\n' '<pkg-info install-location="/Applications"/>' \
        > "$fixture/extra-payload/PackageInfo"
    touch "$fixture/extra-payload/Payload/usr/local/bin/rogue"
    expect_failure "extra package payload" verify_expanded_package "$fixture/extra-payload"

    mkdir -p "$fixture/valid-package/Payload/Siderostat.app" \
        "$fixture/valid-package/Scripts"
    printf '%s\n' '<pkg-info install-location="/Applications"/>' \
        > "$fixture/valid-package/PackageInfo"
    printf '%s\n' \
        '#!/bin/sh' \
        "APP_EXECUTABLE='/Applications/Siderostat.app/Contents/MacOS/Siderostat'" \
        "RUNTIME_EXECUTABLE='/Applications/Siderostat.app/Contents/Helpers/siderostat-runtime'" \
        "RUNTIME_LABEL='dev.siderostat-ds4-proxy.runtime'" \
        'CONSOLE_UID="$(/usr/bin/id -u)"' \
        'stop_runtime_job() { :; }' \
        'stop_monitor() { :; }' \
        '/bin/launchctl bootout "gui/$CONSOLE_UID/$RUNTIME_LABEL"' \
        '/bin/launchctl kill SIGKILL "gui/$CONSOLE_UID/$RUNTIME_LABEL"' \
        '/bin/kill -TERM "$pid"' \
        '/bin/kill -KILL "$pid"' \
        'stop_runtime_job' \
        'stop_monitor' \
        > "$fixture/valid-package/Scripts/preinstall"
    printf '%s\n' \
        '#!/bin/sh' \
        "APP='/Applications/Siderostat.app'" \
        'printf "%s\\n" "dev.siderostat-ds4-proxy.runtime => enabled"' \
        '/bin/launchctl print-disabled "gui/$CONSOLE_UID"' \
        '/bin/launchctl bootstrap "gui/$CONSOLE_UID" "$APP/Contents/Library/LaunchAgents/dev.siderostat-ds4-proxy.runtime.plist"' \
        '/bin/launchctl asuser "$CONSOLE_UID" /usr/bin/open -a "$APP"' \
        'exit 0' \
        > "$fixture/valid-package/Scripts/postinstall"
    chmod +x "$fixture/valid-package/Scripts/preinstall" \
        "$fixture/valid-package/Scripts/postinstall"
    verify_expanded_package "$fixture/valid-package"

    mkdir -p "$fixture/unsigned-helper/Contents/Helpers" \
        "$fixture/unsigned-helper/Contents/MacOS" \
        "$fixture/unsigned-helper/Contents/Library/LaunchAgents" \
        "$fixture/unsigned-helper/Contents/Resources"
    printf '%s\n' \
        '<?xml version="1.0" encoding="UTF-8"?>' \
        '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
        '<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>dev.siderostat-ds4-proxy</string></dict></plist>' \
        > "$fixture/unsigned-helper/Contents/Info.plist"
    printf '%s\n' \
        '<?xml version="1.0" encoding="UTF-8"?>' \
        '<plist version="1.0"><dict><key>Label</key><string>dev.siderostat-ds4-proxy.runtime</string></dict></plist>' \
        > "$fixture/unsigned-helper/Contents/Library/LaunchAgents/dev.siderostat-ds4-proxy.runtime.plist"
    printf '#!/bin/sh\n' > "$fixture/unsigned-helper/Contents/MacOS/Siderostat"
    printf '#!/bin/sh\n' > "$fixture/unsigned-helper/Contents/Helpers/siderostat-runtime"
    printf 'icns\000\000\000\020icp4\000\000\000\010' \
        > "$fixture/unsigned-helper/Contents/Resources/AppIcon.icns"
    chmod +x "$fixture/unsigned-helper/Contents/MacOS/Siderostat" \
        "$fixture/unsigned-helper/Contents/Helpers/siderostat-runtime"
    expect_failure "unsigned helper" verify_bundle "$fixture/unsigned-helper"

    rm -rf "$fixture"
}

main() {
    for command in cargo file plutil codesign pkgutil find grep; do
        require_command "$command"
    done

    run_fixture_tests

    mkdir -p "$OUTPUT_DIR"
    rm -rf "$APP_STAGING" "$EXPAND_DIR"
    cargo xtask app-dev \
        --version "$VERSION" \
        --build-number "$BUILD_NUMBER" \
        --staging "$APP_STAGING" \
        --verify
    verify_bundle "$APP_STAGING/Siderostat.app"

    cargo xtask pkg-dev \
        --app-dir "$APP_STAGING" \
        --version "$VERSION" \
        --output-dir "$OUTPUT_DIR"
    local package="$OUTPUT_DIR/Siderostat-$VERSION.pkg"
    [[ -f "$package" ]] || { fail "package is missing: $package"; return 1; }
    pkgutil --expand-full "$package" "$EXPAND_DIR"
    verify_expanded_package "$EXPAND_DIR"

    echo "E-03 verification: PASS"
    echo "app: $APP_STAGING/Siderostat.app"
    echo "pkg: $package"
}

main "$@"
