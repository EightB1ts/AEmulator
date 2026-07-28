#!/usr/bin/env bash
#
# detect-baseline.sh — run the AEmulator detection catalogue against a LIVE stock
# Android Studio emulator (goldfish/ranchu) and record which of our signals fire.
#
# This proves the catalogue in `aemulator-detection` against ground truth: the stock
# ranchu emulator SHOULD trip a large set of Tier-A signals. A green AEmulator build
# must later trip ZERO of them.
#
# Behaviour:
#   * Locates adb + emulator (Android SDK, macOS default ~/Library/Android/sdk).
#   * `emulator -list-avds`; picks an arm64 AVD (override with AEMU_AVD=<name>).
#   * If an arm64 AVD exists: boots it headless (generous timeout), waits for boot,
#     runs the getprop / device-node / sensor / cpuinfo / network probes, and writes
#     oracle/reports/baseline-avd.md with a PASS/FAIL-per-signal table. Then shuts the
#     emulator down.
#   * If NO arm64 AVD exists: prints the exact sdkmanager/avdmanager commands to make
#     one, writes a short report, and exits 0 (never hangs).
#
# Env knobs:
#   AEMU_AVD=<name>        force a specific AVD
#   AEMU_BOOT_TIMEOUT=<s>  boot deadline in seconds (default 480)
#   AEMU_KEEP_RUNNING=1    do not kill the emulator on exit (for manual poking)
#   AEMU_GPU=<mode>        emulator -gpu mode (default swiftshader_indirect)
#
# NOTE: run on a host with the Android SDK installed. Safe to run with no AVD present.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=_common.sh
source "$SCRIPT_DIR/_common.sh"

BOOT_TIMEOUT="${AEMU_BOOT_TIMEOUT:-480}"
GPU_MODE="${AEMU_GPU:-swiftshader_indirect}"
REPORT="$REPORTS_DIR/baseline-avd.md"
mkdir -p "$REPORTS_DIR"

# ---------------------------------------------------------------------------
# Locate tools
# ---------------------------------------------------------------------------
step "Locating Android SDK tools"
ADB="$(locate_adb)"     || die "adb not found. Install platform-tools or set ANDROID_SDK_ROOT."
EMULATOR="$(locate_emulator)" || die "emulator not found. Install the Android emulator or set ANDROID_SDK_ROOT."
ok "adb:      $ADB"
ok "emulator: $EMULATOR"
ADB_VERSION="$("$ADB" version 2>/dev/null | head -1)"
EMU_VERSION="$("$EMULATOR" -version 2>/dev/null | head -1)"
log "$ADB_VERSION"
log "$EMU_VERSION"

# ---------------------------------------------------------------------------
# Enumerate AVDs and select an arm64 one
# ---------------------------------------------------------------------------
step "Listing AVDs (emulator -list-avds)"
AVDS="$("$EMULATOR" -list-avds 2>/dev/null | sed '/^$/d')"
if [ -n "$AVDS" ]; then
    printf '%s\n' "$AVDS" | sed 's/^/  - /'
else
    log "(none)"
fi

avd_is_arm64() {
    # Inspect the AVD's config.ini for an arm64/aarch64 ABI marker.
    local name="$1"
    local avd_home="${ANDROID_AVD_HOME:-$HOME/.android/avd}"
    local cfg="$avd_home/$name.avd/config.ini"
    [ -f "$cfg" ] || return 1
    grep -Eiq 'arm64|aarch64' "$cfg"
}

SELECTED_AVD=""
if [ -n "${AEMU_AVD:-}" ]; then
    SELECTED_AVD="$AEMU_AVD"
    log "Using AEMU_AVD override: $SELECTED_AVD"
elif [ -n "$AVDS" ]; then
    # Prefer an AVD whose config.ini says arm64; else fall back to the first AVD
    # (on Apple Silicon a bootable AVD is arm64 in practice) with a warning.
    while IFS= read -r name; do
        [ -z "$name" ] && continue
        if avd_is_arm64 "$name"; then SELECTED_AVD="$name"; break; fi
    done <<EOF
$AVDS
EOF
    if [ -z "$SELECTED_AVD" ]; then
        SELECTED_AVD="$(printf '%s\n' "$AVDS" | head -1)"
        warn "No AVD config.ini advertised arm64; falling back to first AVD: $SELECTED_AVD"
        warn "(override with AEMU_AVD=<name> if this is an x86 image)"
    fi
fi

# ---------------------------------------------------------------------------
# No-AVD path: print the create commands and exit 0 (do NOT hang)
# ---------------------------------------------------------------------------
if [ -z "$SELECTED_AVD" ]; then
    SDKMANAGER="$(locate_sdkmanager || true)"
    AVDMANAGER="$(locate_avdmanager || true)"
    SDK="$(sdk_root || echo "$HOME/Library/Android/sdk")"
    IMG="system-images;android-34;google_apis;arm64-v8a"

    step "No arm64 AVD found — nothing to boot"
    cat <<EOF
To create one on Apple Silicon (arm64), run:

  # 1. Ensure command-line tools exist (sdkmanager/avdmanager). If missing:
  #    Android Studio > SDK Manager > SDK Tools > "Android SDK Command-line Tools",
  #    or download from https://developer.android.com/studio#command-line-tools-only
  #    and unzip to \$ANDROID_SDK_ROOT/cmdline-tools/latest/

  export ANDROID_SDK_ROOT="$SDK"

  # 2. Install a stock arm64 system image (ranchu):
  ${SDKMANAGER:-\$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager} "$IMG"

  # 3. Create the AVD:
  ${AVDMANAGER:-\$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/avdmanager} \\
      create avd -n ranchu_arm64 -k "$IMG" -d pixel_6

  # 4. Re-run this script (it will pick up the new AVD):
  bash oracle/scripts/detect-baseline.sh
EOF

    {
        echo "# Baseline — stock Android emulator detection signals"
        echo
        echo "_Generated: $(now_utc)_"
        echo
        echo "## Result: NO arm64 AVD installed — live baseline not captured"
        echo
        echo "The detection catalogue was **not** run against a live emulator because no AVD"
        echo "is installed on this host. Create one and re-run \`detect-baseline.sh\`:"
        echo
        echo '```bash'
        echo "export ANDROID_SDK_ROOT=\"$SDK\""
        echo "\$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager \"$IMG\""
        echo "\$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/avdmanager create avd -n ranchu_arm64 -k \"$IMG\" -d pixel_6"
        echo "bash oracle/scripts/detect-baseline.sh"
        echo '```'
        echo
        echo "## Probe environment (this run)"
        echo
        echo "| Item | Value |"
        echo "|------|-------|"
        echo "| adb | \`$ADB\` |"
        echo "| adb version | $ADB_VERSION |"
        echo "| emulator | \`$EMULATOR\` |"
        echo "| emulator version | $EMU_VERSION |"
        echo "| AVDs found | none |"
        echo "| sdkmanager | \`${SDKMANAGER:-not installed}\` |"
        echo "| avdmanager | \`${AVDMANAGER:-not installed}\` |"
        echo
        echo "See \`oracle/reports/baseline-avd.EXPECTED.md\` for the catalogue of signals a"
        echo "stock ranchu image is expected to trip, pending a live run."
    } > "$REPORT"

    ok "Wrote $REPORT (no-AVD note)"
    exit 0
fi

# ---------------------------------------------------------------------------
# Boot the selected AVD headless
# ---------------------------------------------------------------------------
step "Booting AVD '$SELECTED_AVD' headless (timeout ${BOOT_TIMEOUT}s, gpu=$GPU_MODE)"
EMU_LOG="$DOWNLOADS_DIR/emulator-$SELECTED_AVD.log"
mkdir -p "$DOWNLOADS_DIR"

"$EMULATOR" -avd "$SELECTED_AVD" \
    -no-window -no-audio -no-boot-anim -no-snapshot \
    -gpu "$GPU_MODE" \
    >"$EMU_LOG" 2>&1 &
EMU_PID=$!
log "emulator pid=$EMU_PID (log: $EMU_LOG)"

cleanup() {
    if [ "${AEMU_KEEP_RUNNING:-0}" = "1" ]; then
        warn "AEMU_KEEP_RUNNING=1 — leaving emulator pid=$EMU_PID running"
        return
    fi
    log "Shutting emulator down"
    "$ADB" emu kill >/dev/null 2>&1 || true
    kill "$EMU_PID" >/dev/null 2>&1 || true
    wait "$EMU_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

log "Waiting for device (adb wait-for-device)"
# adb wait-for-device itself can block forever; bound it by racing the deadline.
DEADLINE=$(( $(date +%s) + BOOT_TIMEOUT ))
"$ADB" start-server >/dev/null 2>&1 || true
( "$ADB" wait-for-device ) &
WAIT_PID=$!
while kill -0 "$WAIT_PID" 2>/dev/null; do
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        kill "$WAIT_PID" 2>/dev/null || true
        die "Timed out waiting for device after ${BOOT_TIMEOUT}s (see $EMU_LOG)"
    fi
    # emulator process died?
    if ! kill -0 "$EMU_PID" 2>/dev/null; then
        die "Emulator process exited before device came up (see $EMU_LOG)"
    fi
    sleep 2
done
ok "Device connected"

log "Waiting for sys.boot_completed=1"
if wait_for "$(( DEADLINE - $(date +%s) ))" "1" "$ADB" shell getprop sys.boot_completed; then
    ok "Boot completed"
else
    die "Timed out waiting for sys.boot_completed after ${BOOT_TIMEOUT}s (see $EMU_LOG)"
fi
# Small settle for late-init props/HALs.
sleep 3

# ---------------------------------------------------------------------------
# Probe: pull all system properties once, then evaluate signals
# ---------------------------------------------------------------------------
step "Running detection catalogue against the live device"
PROP_DUMP="$DOWNLOADS_DIR/getprop-$SELECTED_AVD.txt"
"$ADB" shell getprop 2>/dev/null | tr -d '\r' > "$PROP_DUMP"
log "Dumped $(wc -l < "$PROP_DUMP" | tr -d ' ') properties to $PROP_DUMP"

# getprop value extractor (output format: "[key]: [value]")
gp() { sed -n "s/^\[$1\]: \[\(.*\)\]$/\1/p" "$PROP_DUMP" | head -1; }

FIRED=0; CLEAR=0; INFO=0
ROWS=""
row() { # sig probe value verdict
    local sig="$1" probe="$2" val="$3" verdict="$4" mark
    val=$(printf '%s' "$val" | sed 's/|/\\|/g')
    case "$verdict" in
        FIRE)  mark='🔴 FIRES'; FIRED=$((FIRED+1)) ;;
        CLEAR) mark='🟢 clear'; CLEAR=$((CLEAR+1)) ;;
        *)     mark='⚪ info';  INFO=$((INFO+1)) ;;
    esac
    ROWS="$ROWS| \`$sig\` | $probe | \`${val:-<empty>}\` | $mark |
"
}

# --- Build & system-property tier (research: hw-identifiers + detection-adversary) ---
sig_eq()      { local v; v=$(gp "$1"); if [ "$v" = "$2" ]; then row "$1" "== $2" "$v" FIRE; else row "$1" "== $2" "$v" CLEAR; fi; }
sig_ne()      { local v; v=$(gp "$1"); if [ -n "$v" ] && [ "$v" != "$2" ]; then row "$1" "!= $2 (retail=$2)" "$v" FIRE; else row "$1" "!= $2 (retail=$2)" "$v" CLEAR; fi; }
sig_match()   { local v; v=$(gp "$1"); if printf '%s' "$v" | grep -Eqi "$2"; then row "$1" "=~ /$2/" "$v" FIRE; else row "$1" "=~ /$2/" "$v" CLEAR; fi; }
sig_present() { local v; v=$(gp "$1"); if [ -n "$v" ]; then row "$1" "present ⇒ tell" "$v" FIRE; else row "$1" "present ⇒ tell" "$v" CLEAR; fi; }
sig_info()    { local v; v=$(gp "$1"); row "$1" "$2" "$v" INFO; }

sig_eq      ro.kernel.qemu 1
sig_present ro.boot.qemu
sig_match   ro.hardware 'goldfish|ranchu|vbox86|cutf'
sig_match   ro.product.model 'sdk|emulator|Android SDK built'
sig_match   ro.product.name 'sdk|vbox86|sdk_gphone|emu64'
sig_match   ro.product.device 'generic|emu64|vsoc'
sig_match   ro.build.fingerprint 'generic|unknown|sdk_gphone|emu64|test-keys|dev-keys'
sig_match   ro.build.flavor 'sdk_gphone|-userdebug|-eng'
sig_eq      ro.build.tags test-keys
sig_ne      ro.build.type user
sig_eq      ro.secure 0
sig_eq      ro.debuggable 1
sig_match   ro.build.characteristics 'emulator'
sig_eq      ro.hardware.egl emulation
sig_present ro.kernel.qemu.gles
sig_present ro.kernel.android.qemud
sig_present qemu.hw.mainkeys
sig_present qemu.sf.fake_camera
sig_present ro.boot.qemu.gltransport
sig_present vendor.qemu.dev.bootcomplete

# --- coherence / identity (informational context for the fingerprint) ---
sig_info    ro.product.cpu.abilist "ABI list (expect arm64-v8a on AS)"
sig_info    ro.product.manufacturer "MANUFACTURER"
sig_info    ro.product.brand "BRAND"
sig_info    ro.product.board "BOARD codename"
sig_info    ro.board.platform "SoC platform (separate from board)"
sig_info    ro.soc.model "SoC model"
sig_info    ro.build.host "build host (OEM farm vs android-build)"
sig_info    ro.build.user "build user"
sig_info    gsm.version.baseband "baseband (empty/1.0.0.0 on emu)"
sig_info    ro.serialno "serial"

# --- verified-boot props that attestation mirrors (research: detection-adversary) ---
VBS=$(gp ro.boot.verifiedbootstate)
if [ "$VBS" = "green" ]; then row ro.boot.verifiedbootstate "== green (retail locked)" "$VBS" CLEAR
elif [ -z "$VBS" ]; then row ro.boot.verifiedbootstate "absent ⇒ tell" "$VBS" FIRE
else row ro.boot.verifiedbootstate "!= green" "$VBS" FIRE; fi
sig_info ro.boot.flash.locked "flash.locked (retail=1)"
sig_info ro.boot.vbmeta.device_state "device_state (retail=locked)"
sig_info ro.boot.veritymode "veritymode (retail=enforcing)"

# --- Device nodes / sockets / files (single adb round-trip) ---
NODES='/dev/qemu_pipe /dev/goldfish_pipe /dev/goldfish_sync /dev/goldfish_dma /dev/socket/qemud /sys/qemu_trace /vendor/etc/fstab.ranchu /system/bin/qemu-props /dev/gnss0'
NODE_OUT="$("$ADB" shell "for p in $NODES; do if [ -e \"\$p\" ]; then echo \"EXIST \$p\"; else echo \"ABSENT \$p\"; fi; done" 2>/dev/null | tr -d '\r')"
for p in $NODES; do
    if printf '%s\n' "$NODE_OUT" | grep -q "EXIST $p$"; then
        row "$p" "node exists ⇒ tell" "present" FIRE
    else
        row "$p" "node exists ⇒ tell" "absent" CLEAR
    fi
done

# --- /proc/tty/drivers contains 'goldfish' ---
TTY_OUT="$("$ADB" shell cat /proc/tty/drivers 2>/dev/null | tr -d '\r')"
if printf '%s' "$TTY_OUT" | grep -qi goldfish; then row "/proc/tty/drivers" "grep goldfish" "match" FIRE
else row "/proc/tty/drivers" "grep goldfish" "no-match/denied" CLEAR; fi

# --- /proc/cpuinfo: Hardware line + goldfish/ranchu/qemu ---
CPUINFO="$("$ADB" shell cat /proc/cpuinfo 2>/dev/null | tr -d '\r')"
CPU_HW="$(printf '%s\n' "$CPUINFO" | grep -i '^Hardware' | head -1)"
if printf '%s' "$CPUINFO" | grep -Eqi 'goldfish|ranchu|qemu'; then
    row "/proc/cpuinfo" "grep goldfish|ranchu|qemu" "${CPU_HW:-match}" FIRE
else
    row "/proc/cpuinfo" "grep goldfish|ranchu|qemu" "${CPU_HW:-<no Hardware line>}" INFO
fi

# --- Sensors: dumpsys sensorservice → Goldfish / AOSP vendor ---
SENSORS="$("$ADB" shell dumpsys sensorservice 2>/dev/null | tr -d '\r')"
if printf '%s' "$SENSORS" | grep -Eqi 'goldfish|The Android Open Source Project'; then
    SNAME="$(printf '%s\n' "$SENSORS" | grep -Ei 'goldfish' | head -1 | sed 's/^[[:space:]]*//')"
    row "sensors HAL" "dumpsys sensorservice grep Goldfish" "${SNAME:-Goldfish/AOSP}" FIRE
else
    row "sensors HAL" "dumpsys sensorservice grep Goldfish" "no goldfish vendor" CLEAR
fi

# --- Network: 10.0.2.15 SLIRP address ---
IPADDR="$("$ADB" shell ip -o addr 2>/dev/null | tr -d '\r')"
[ -z "$IPADDR" ] && IPADDR="$("$ADB" shell ip addr 2>/dev/null | tr -d '\r')"
if printf '%s' "$IPADDR" | grep -q '10.0.2.15'; then row "network" "ip addr grep 10.0.2.15 (SLIRP)" "10.0.2.15" FIRE
else row "network" "ip addr grep 10.0.2.15 (SLIRP)" "not 10.0.2.15" CLEAR; fi

# --- Device-tree root compatible (best-effort; often SELinux-denied) ---
DT="$("$ADB" shell cat /proc/device-tree/compatible 2>/dev/null | tr -d '\r\000')"
if [ -n "$DT" ]; then
    if printf '%s' "$DT" | grep -Eqi 'dummy-virt|goldfish|ranchu'; then row "/proc/device-tree/compatible" "grep dummy-virt|goldfish" "$DT" FIRE
    else row "/proc/device-tree/compatible" "grep dummy-virt|goldfish" "$DT" INFO; fi
else
    row "/proc/device-tree/compatible" "read (SELinux may deny untrusted_app)" "denied/empty" INFO
fi

TOTAL=$((FIRED + CLEAR + INFO))
step "Signals: 🔴 $FIRED fire · 🟢 $CLEAR clear · ⚪ $INFO info (of $TOTAL probed)"

# ---------------------------------------------------------------------------
# Write the report
# ---------------------------------------------------------------------------
{
    echo "# Baseline — stock Android emulator detection signals"
    echo
    echo "_Generated: $(now_utc) · AVD \`$SELECTED_AVD\`_"
    echo
    echo "Ground-truth run of the AEmulator detection catalogue against a **live stock**"
    echo "Android Studio emulator (goldfish/ranchu). Every 🔴 below is a signal the stock"
    echo "emulator trips — i.e. a signal a real AEmulator build MUST NOT trip. This is the"
    echo "adversary's-eye baseline the \`aemulator-detection\` crate is validated against."
    echo
    echo "## Summary"
    echo
    echo "| | Count |"
    echo "|--|--|"
    echo "| 🔴 Signals FIRING (emulator revealed) | $FIRED |"
    echo "| 🟢 Signals clear | $CLEAR |"
    echo "| ⚪ Informational (identity context) | $INFO |"
    echo "| Total probed | $TOTAL |"
    echo
    echo "## Environment"
    echo
    echo "| Item | Value |"
    echo "|------|-------|"
    echo "| adb | \`$ADB\` |"
    echo "| adb version | $ADB_VERSION |"
    echo "| emulator | \`$EMULATOR\` |"
    echo "| emulator version | $EMU_VERSION |"
    echo "| AVD | \`$SELECTED_AVD\` |"
    echo "| GPU mode | \`$GPU_MODE\` |"
    echo "| Host | $(uname -mrs) |"
    echo
    echo "## Signal-by-signal"
    echo
    echo "| Signal | Probe | Live value | Verdict |"
    echo "|--------|-------|------------|---------|"
    printf '%s' "$ROWS"
    echo
    echo "## Raw qemu/goldfish property surface"
    echo
    echo "Every property whose key or value mentions qemu/goldfish/ranchu (bulk context):"
    echo
    echo '```'
    grep -Ei 'qemu|goldfish|ranchu' "$PROP_DUMP" || echo "(none)"
    echo '```'
    echo
    echo "## Key identity properties (the DeviceProfile must reproduce a *coherent* set)"
    echo
    echo '```'
    for k in ro.product.brand ro.product.name ro.product.device ro.product.model \
             ro.product.manufacturer ro.product.board ro.board.platform ro.hardware \
             ro.build.fingerprint ro.build.type ro.build.tags ro.product.cpu.abilist; do
        printf '%s=%s\n' "$k" "$(gp "$k")"
    done
    echo '```'
    echo
    echo "---"
    echo "_Artifacts: full property dump at \`oracle/downloads/getprop-$SELECTED_AVD.txt\`,"
    echo "emulator log at \`oracle/downloads/emulator-$SELECTED_AVD.log\`._"
} > "$REPORT"

ok "Wrote $REPORT"
log "Cleanup will shut the emulator down (AEMU_KEEP_RUNNING=1 to leave it up)."
