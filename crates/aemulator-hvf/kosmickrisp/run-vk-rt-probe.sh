#!/usr/bin/env bash
# run-vk-rt-probe.sh — push vk-rt-probe into a booted guest and run it.
#
# Requires adb to be working (crates/aemulator-hvf/prep-adb-keys.sh provisions
# the key into the image). Output goes to stdout and to a log file, because the
# probe's value is its stage-by-stage trace and that trace is the artifact.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ADB="${ADB:-$HOME/Library/Android/sdk/platform-tools/adb}"
LOG="${LOG:-$HERE/guest-run.log}"

[ -x "$HERE/vk-rt-probe" ] || { echo "build-vk-rt-probe.sh first" >&2; exit 1; }
[ -x "$ADB" ] || { echo "no adb at $ADB" >&2; exit 1; }

"$ADB" wait-for-device
"$ADB" push "$HERE/vk-rt-probe" /data/local/tmp/vk-rt-probe >/dev/null
"$ADB" shell chmod 755 /data/local/tmp/vk-rt-probe

echo "==> running vk-rt-probe in the guest (log: $LOG)"
# The probe's own exit status is what matters, so it is captured rather than
# swallowed by the pipe to tee. `adb shell` does not forward exit status on all
# versions, so the verdict line is the authority; both are recorded.
set +e
"$ADB" shell "/data/local/tmp/vk-rt-probe $*; echo GUEST_EXIT=\$?" 2>&1 | tee "$LOG"
set -e

echo
echo "==> verdict"
grep -E "^VK_RT_PROBE:|^GUEST_EXIT=" "$LOG" || echo "no verdict line — the probe died before printing one; read $LOG"
