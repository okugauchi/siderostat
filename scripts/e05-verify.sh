#!/usr/bin/env bash
# E-05: 実機検証ヘルパー（migration / upgrade / rollback / uninstall）
#
# 本スクリプトは E-05 の実機検証時に、状態の採取・比較・検証を行う read-only helper である。
# クラスタの停止・変更は行わない（change window 中にユーザー/agent が明示的に操作する）。
#
# 使い方:
#   ./scripts/e05-verify.sh snapshot <label>         現在の状態を採取して保存
#   ./scripts/e05-verify.sh compare <a> <b>          label a/b の user data digest を比較
#   ./scripts/e05-verify.sh processes                現在の runtime/ds4 process 一覧
#   ./scripts/e05-verify.sh ports                    現在の関連 port LISTEN 一覧
#   ./scripts/e05-verify.sh jobs                     launchctl の siderostat job 状態
#   ./scripts/e05-verify.sh readiness                /healthz /readyz を表示
#   ./scripts/e05-verify.sh all <label>              snapshot + processes + ports + jobs + readiness
set -euo pipefail

# SSH non-interactive shells may not source Homebrew's shell profile.
PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"
export PATH

LABELS_DIR="${E05_LABELS_DIR:-/tmp/siderostat-e05}"
mkdir -p "$LABELS_DIR"

PORTS_RE="18080|18081|9920|18082|9911|8000"
APPSUPPORT="$HOME/Library/Application Support/siderostat"

snapshot() {
    local label="$1"
    local dir="$LABELS_DIR/$label"
    mkdir -p "$dir"
    {
        echo "host: $(hostname)"
        echo "date: $(date '+%Y-%m-%d %H:%M:%S %z')"
        echo "macos: $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
        echo "app: $(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' /Applications/Siderostat.app/Contents/Info.plist 2>/dev/null || echo missing)"
        echo "app-build: $(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' /Applications/Siderostat.app/Contents/Info.plist 2>/dev/null || echo missing)"
    } > "$dir/env.txt"

    # user data digest（secret 本文は含めない。mode のみ）
    (
        shasum -a 256 "$APPSUPPORT/config.toml" 2>/dev/null || echo "config: MISSING"
        shasum -a 256 "$APPSUPPORT/cluster-state.json" 2>/dev/null || echo "cluster-state: MISSING"
        for m in "$APPSUPPORT"/manifests/*.json; do
            [[ -f "$m" ]] && shasum -a 256 "$m"
        done
        for s in "$APPSUPPORT"/secrets/*; do
            [[ -e "$s" ]] && stat -f "mode: %Sp %N" "$s"
        done
    ) > "$dir/userdata.txt"

    # processes / ports / jobs / readiness
    pgrep -fl "siderostat|ds4-server" > "$dir/processes.txt" 2>&1 || true
    lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | grep -E "$PORTS_RE" > "$dir/ports.txt" || true
    {
        launchctl print "gui/$(id -u)/dev.siderostat-ds4-proxy.runtime" 2>&1 | sed -n '1,8p'
        launchctl print "gui/$(id -u)/dev.siderostat-ds4-proxy.monitor" 2>&1 | sed -n '1,5p' || true
        launchctl print "gui/$(id -u)/local.siderostat.runtime" 2>&1 | sed -n '1,5p' || true
    } > "$dir/jobs.txt" 2>&1 || true
    {
        echo "healthz: $(curl -fsS http://127.0.0.1:18081/healthz 2>&1 || echo unreachable)"
        echo "readyz: $(curl -fsS http://127.0.0.1:18081/readyz 2>&1 || echo unreachable)"
    } > "$dir/readiness.txt" 2>&1 || true

    echo "snapshot saved to $dir"
    echo "--- env ---"; cat "$dir/env.txt"
    echo "--- readiness ---"; cat "$dir/readiness.txt"
}

compare() {
    local a="$1" b="$2"
    local da="$LABELS_DIR/$a/userdata.txt"
    local db="$LABELS_DIR/$b/userdata.txt"
    if [[ ! -f "$da" || ! -f "$db" ]]; then
        echo "missing snapshot (need both $a and $b)" >&2
        exit 1
    fi
    if diff -u "$da" "$db" > "$LABELS_DIR/compare-$a-$b.diff"; then
        echo "PASS: user data digest/mode identical between '$a' and '$b'"
    else
        echo "DIFF: user data changed between '$a' and '$b' — see $LABELS_DIR/compare-$a-$b.diff"
    fi
}

processes() {
    echo "=== runtime / ds4 processes ==="
    pgrep -fl "siderostat|ds4-server" || echo "(none)"
}

ports() {
    echo "=== related LISTEN ports ==="
    lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | grep -E "$PORTS_RE" || echo "(none)"
}

jobs() {
    echo "=== new runtime job ==="
    launchctl print "gui/$(id -u)/dev.siderostat-ds4-proxy.runtime" 2>&1 | sed -n '1,8p'
    echo "=== legacy runtime job (should be absent after migration) ==="
    launchctl print "gui/$(id -u)/local.siderostat.runtime" 2>&1 | sed -n '1,5p' || echo "(not present)"
}

readiness() {
    echo "healthz: $(curl -fsS http://127.0.0.1:18081/healthz 2>&1 || echo unreachable)"
    echo "readyz: $(curl -fsS http://127.0.0.1:18081/readyz 2>&1 || echo unreachable)"
}

cmd="${1:-}"
case "$cmd" in
    snapshot) snapshot "${2:?label required}" ;;
    compare) compare "${2:?label a required}" "${3:?label b required}" ;;
    processes) processes ;;
    ports) ports ;;
    jobs) jobs ;;
    readiness) readiness ;;
    all) snapshot "${2:?label required}"; echo; processes; echo; ports; echo; jobs; echo; readiness ;;
    *) echo "usage: $0 {snapshot <label>|compare <a> <b>|processes|ports|jobs|readiness|all <label>}" >&2; exit 1 ;;
esac
