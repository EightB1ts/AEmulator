# shellcheck shell=bash
# _common.sh — shared helpers for the oracle scripts.
#
# Sourced by detect-baseline.sh and qemu-hvf-boot.sh. NOT executable on its own.
# Targets macOS's stock bash 3.2 (no associative arrays, no `mapfile`).

# ---- logging ---------------------------------------------------------------

# ANSI colours only when stdout is a TTY.
if [ -t 1 ]; then
    _C_RED=$'\033[31m'; _C_YEL=$'\033[33m'; _C_GRN=$'\033[32m'
    _C_BLU=$'\033[34m'; _C_DIM=$'\033[2m'; _C_RST=$'\033[0m'
else
    _C_RED=; _C_YEL=; _C_GRN=; _C_BLU=; _C_DIM=; _C_RST=
fi

log()  { printf '%s[oracle]%s %s\n'  "$_C_BLU" "$_C_RST" "$*"; }
ok()   { printf '%s[ ok  ]%s %s\n'   "$_C_GRN" "$_C_RST" "$*"; }
warn() { printf '%s[warn ]%s %s\n'   "$_C_YEL" "$_C_RST" "$*" >&2; }
err()  { printf '%s[error]%s %s\n'   "$_C_RED" "$_C_RST" "$*" >&2; }
die()  { err "$*"; exit 1; }
step() { printf '\n%s==>%s %s\n' "$_C_BLU" "$_C_RST" "$*"; }

# ---- paths -----------------------------------------------------------------

# Absolute path to the AEmulator repo root (parent of oracle/).
repo_root() {
    local here
    here=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
    printf '%s\n' "$here"
}

ORACLE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(repo_root)"
REPORTS_DIR="$ORACLE_DIR/reports"
DOWNLOADS_DIR="$ORACLE_DIR/downloads"

# ---- Android SDK discovery -------------------------------------------------

# Resolve the Android SDK root from the environment or the macOS default.
sdk_root() {
    local candidates=(
        "${ANDROID_SDK_ROOT:-}"
        "${ANDROID_HOME:-}"
        "$HOME/Library/Android/sdk"
        "$HOME/Android/Sdk"
    )
    local c
    for c in "${candidates[@]}"; do
        [ -n "$c" ] && [ -d "$c" ] && { printf '%s\n' "$c"; return 0; }
    done
    return 1
}

# Locate the adb binary. Echoes the path on success.
locate_adb() {
    if command -v adb >/dev/null 2>&1; then command -v adb; return 0; fi
    local root; root=$(sdk_root) || return 1
    [ -x "$root/platform-tools/adb" ] && { printf '%s\n' "$root/platform-tools/adb"; return 0; }
    return 1
}

# Locate the Android Studio emulator binary. Echoes the path on success.
locate_emulator() {
    local root; root=$(sdk_root) || true
    if [ -n "$root" ] && [ -x "$root/emulator/emulator" ]; then
        printf '%s\n' "$root/emulator/emulator"; return 0
    fi
    command -v emulator >/dev/null 2>&1 && { command -v emulator; return 0; }
    return 1
}

# Locate avdmanager / sdkmanager (may be absent — used only to print create hints).
locate_avdmanager() {
    local root; root=$(sdk_root) || return 1
    local p
    for p in \
        "$root/cmdline-tools/latest/bin/avdmanager" \
        "$root/cmdline-tools/"*/bin/avdmanager \
        "$root/tools/bin/avdmanager"; do
        [ -x "$p" ] && { printf '%s\n' "$p"; return 0; }
    done
    command -v avdmanager >/dev/null 2>&1 && { command -v avdmanager; return 0; }
    return 1
}

locate_sdkmanager() {
    local root; root=$(sdk_root) || return 1
    local p
    for p in \
        "$root/cmdline-tools/latest/bin/sdkmanager" \
        "$root/cmdline-tools/"*/bin/sdkmanager \
        "$root/tools/bin/sdkmanager"; do
        [ -x "$p" ] && { printf '%s\n' "$p"; return 0; }
    done
    command -v sdkmanager >/dev/null 2>&1 && { command -v sdkmanager; return 0; }
    return 1
}

# ---- misc ------------------------------------------------------------------

# Poll `cmd` until it prints `want`, or `deadline_s` seconds elapse.
# Usage: wait_for <deadline_s> <want> <cmd...>
wait_for() {
    local deadline=$1 want=$2; shift 2
    local start; start=$(date +%s)
    while :; do
        if [ "$("$@" 2>/dev/null | tr -d '\r')" = "$want" ]; then return 0; fi
        local now; now=$(date +%s)
        if [ $((now - start)) -ge "$deadline" ]; then return 1; fi
        sleep 2
    done
}

# ISO-8601 UTC timestamp.
now_utc() { date -u +'%Y-%m-%dT%H:%M:%SZ'; }
