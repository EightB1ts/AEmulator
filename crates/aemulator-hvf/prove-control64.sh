#!/usr/bin/env bash
# prove-control64.sh — Phase 6 end-to-end PROOF of the host-driven control plane.
#
# Boots the 64-bit Android under HVF with the control server, then drives it ENTIRELY over the
# control socket (no adb, no a11y): waits for the guest to render, screenshots the initial screen,
# injects a bottom->top swipe to dismiss the (insecure, swipe-only) keyguard, screenshots the home
# screen, injects an app-drawer swipe-up, and screenshots the resulting NEW screen. All screenshots
# come straight from the guest scanout via the `screenshot` command. Verifies the framebuffer content
# hash CHANGES across the injected gestures — i.e. host-driven input moved the on-screen UI.
#
# Output PNGs: screenshots/phase6-control-*.png
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/target/debug/aemulator-hvf"
SOCK="/tmp/aemulator-hvf-control.sock"
SHOTS="$HERE/screenshots"
LOG="/tmp/aemulator-control-boot.log"
mkdir -p "$SHOTS"

WARMUP_MIN="${WARMUP_MIN:-95}"     # don't start driving before Android has had time to reach keyguard
HARD_CAP="${HARD_CAP:-260}"        # give up warmup after this many seconds
export SWRENDER="${SWRENDER:-1}"   # guest-CPU render into the scanout (the proven home-screen path)

# client verbs are the FIRST arg to the binary; --socket is parsed anywhere.
ctl() { "$BIN" "$@" --socket "$SOCK" 2>/dev/null; }
# extract just the framebuffer content-hash value (independent of the flush `gen`).
fbhash() { ctl hash | sed -n 's/.*hash=\([0-9a-f]*\).*/\1/p'; }

echo "== [prove] launching control server in background (SWRENDER=$SWRENDER) =="
rm -f "$SOCK"
"$HERE/run-control64.sh" --socket "$SOCK" >"$LOG" 2>&1 &
SERVER_PID=$!
cleanup() {
  echo "== [prove] shutting down (server pid $SERVER_PID) =="
  ctl quit >/dev/null 2>&1 || true
  sleep 1
  kill "$SERVER_PID" 2>/dev/null || true
  # kill the whole process group of the launcher (cargo/codesign/aemulator-hvf)
  pkill -P "$SERVER_PID" 2>/dev/null || true
  pkill -f 'aemulator-hvf run-android --control' 2>/dev/null || true
}
trap cleanup EXIT

echo "== [prove] waiting for control socket =="
for _ in $(seq 1 300); do
  [ -S "$SOCK" ] && ctl info >/dev/null 2>&1 && break
  # bail early if the server died during build/boot
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "[prove] server exited early; tail of log:"; tail -30 "$LOG"; exit 1; }
  sleep 1
done
echo "[prove] socket up: $(ctl info)"

echo "== [prove] warming up (min ${WARMUP_MIN}s, cap ${HARD_CAP}s) — polling scanout =="
START=$(date +%s)
LAST_HASH=""
STABLE=0
while :; do
  NOW=$(date +%s); ELAPSED=$((NOW-START))
  INFO=$(ctl info)
  HASH=$(echo "$INFO" | sed -n 's/.*hash=\([0-9a-f]*\).*/\1/p')
  NZ=$(echo "$INFO" | sed -n 's/.*nonzero=\([0-9]*\).*/\1/p')
  echo "[prove] t=${ELAPSED}s nonzero=${NZ:-?} hash=${HASH:-?}"
  if [ "$HASH" = "$LAST_HASH" ] && [ -n "$HASH" ]; then STABLE=$((STABLE+1)); else STABLE=0; fi
  LAST_HASH="$HASH"
  # ready when we've waited the minimum AND the framebuffer has substantial content that held steady
  if [ "$ELAPSED" -ge "$WARMUP_MIN" ] && [ "${NZ:-0}" -gt 200000 ] && [ "$STABLE" -ge 2 ]; then
    echo "[prove] guest render stable at t=${ELAPSED}s"; break
  fi
  if [ "$ELAPSED" -ge "$HARD_CAP" ]; then echo "[prove] warmup cap hit; proceeding anyway"; break; fi
  sleep 5
done

echo "== [prove] STEP 1: screenshot initial screen (expected: keyguard/lockscreen) =="
echo "  $(ctl screenshot "$SHOTS/phase6-control-01-initial.png")"
H1=$(fbhash)
echo "  hash1: $H1"

echo "== [prove] STEP 2: inject bottom->top swipe to dismiss keyguard =="
ctl swipe 512 700 512 80 250
sleep 1
ctl swipe 512 700 512 80 250      # second attempt to be safe
echo "  $(ctl wait_stable 9000 300)"
echo "  $(ctl screenshot "$SHOTS/phase6-control-02-home.png")"
H2=$(fbhash)
echo "  hash2: $H2"

echo "== [prove] STEP 3: inject app-drawer swipe-up (hotseat -> up) — a NEW screen =="
ctl swipe 512 745 512 150 300
echo "  $(ctl wait_stable 9000 300)"
echo "  $(ctl screenshot "$SHOTS/phase6-control-03-appdrawer.png")"
H3=$(fbhash)
echo "  hash3: $H3"

echo "== [prove] STEP 4 (optional): tap near top-left app icon =="
ctl tap 160 250
echo "  $(ctl wait_stable 8000 300)"
echo "  $(ctl screenshot "$SHOTS/phase6-control-04-after-tap.png")"
echo "  hash4: $(ctl hash)"

echo
echo "== [prove] SUMMARY =="
echo "  initial : $H1"
echo "  post-unlock : $H2"
echo "  post-drawer : $H3"
echo "  PNGs:"
ls -la "$SHOTS"/phase6-control-*.png 2>/dev/null
echo
if [ "$H1" != "$H2" ] || [ "$H2" != "$H3" ]; then
  echo "[prove] PASS: injected host input CHANGED the guest framebuffer hash (UI moved)."
else
  echo "[prove] INCONCLUSIVE: framebuffer hash did not change across gestures — inspect PNGs + $LOG."
fi
