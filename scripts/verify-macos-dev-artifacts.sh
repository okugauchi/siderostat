#!/usr/bin/env bash
# E-03: certificate-free app/pkg verification for pull-request CI.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.3.0}"
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
    [[ "${#script_files[@]}" -eq 1 ]] || {
        fail "expected exactly one installer script, found ${#script_files[@]}"
        return 1
    }
    script="${script_files[0]}"
    [[ "${script##*/}" == "preinstall" ]] || {
        fail "unexpected installer script: $script"
        return 1
    }
    sh -n "$script" || {
        fail "preinstall script has invalid shell syntax: $script"
        return 1
    }
    grep -Fq "/Applications/Siderostat.app/Contents/MacOS/Siderostat" "$script" || {
        fail "preinstall script is missing the exact Monitor path"
        return 1
    }
    grep -Fq "/bin/kill -TERM" "$script" || {
        fail "preinstall script is missing SIGTERM handling"
        return 1
    }
    grep -Fq "/bin/kill -KILL" "$script" || {
        fail "preinstall script is missing SIGKILL fallback"
        return 1
    }
    if grep -Eq 'killall|pkill|LaunchAgents|launchctl|ds4-server|siderostat-runtime' "$script"; then
        fail "preinstall script contains an out-of-scope process or service operation"
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
