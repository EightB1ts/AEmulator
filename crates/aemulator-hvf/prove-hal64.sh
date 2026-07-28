#!/usr/bin/env bash
# prove-hal64.sh — end-to-end PROOF that the host-side guest-HAL backends work.
#
# The project used to make the Cuttlefish guest boot by binary-patching its image to DISABLE every HAL
# the host could not serve (sensors, lights, threadnetwork) — and, by accident, `mediaserver`. That is
# the wrong trade twice over: the device is functionally incomplete, and a phone reporting zero sensors
# is a stronger virtualization tell than any build-prop string. `aemulator-hal` implements the real host
# protocols instead. This script boots the guest with them live and asserts, from the guest's own
# logcat, both that the old failures are gone and that each backend actually answered.
#
# It is a REGRESSION GATE, not a smoke test: every check below corresponds to a specific, previously
# observed failure, with the guest-side error string quoted so a future breakage is self-explanatory.
#
#   ./prove-hal64.sh                 # boot, watch, assert, report
#   TIMEOUT=300 ./prove-hal64.sh     # allow longer for a cold/first boot
#   LOG=/tmp/mine.log ./prove-hal64.sh   # reuse an existing boot log instead of booting
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG="${LOG:-/tmp/aemulator-prove-hal.log}"
TIMEOUT="${TIMEOUT:-240}"
# The HALs whose host backends have landed, so this boot re-declares them in VINTF. Keep in step with
# machine.rs's PORT_ROLES wiring: re-declaring an interface with no backend is what used to get
# system_server Watchdog-killed.
PREP_ARGS="${PREP_VSOCK_HALS_ARGS:---revert=sensors,light}"

if [ -t 1 ]; then C_G=$'\033[32m'; C_R=$'\033[31m'; C_Y=$'\033[33m'; C_B=$'\033[1m'; C_0=$'\033[0m'
else C_G=; C_R=; C_Y=; C_B=; C_0=; fi
PASS=0; FAIL=0; WARN=0
ok()   { printf '%s  PASS%s %s\n' "$C_G" "$C_0" "$*"; PASS=$((PASS+1)); }
bad()  { printf '%s  FAIL%s %s\n' "$C_R" "$C_0" "$*"; FAIL=$((FAIL+1)); }
warn() { printf '%s  WARN%s %s\n' "$C_Y" "$C_0" "$*"; WARN=$((WARN+1)); }
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }

# --- boot, unless the caller supplied a log to inspect -----------------------------------------------
if [ -n "${LOG_PROVIDED:-}" ] || { [ -n "${LOG:-}" ] && [ "${REUSE_LOG:-0}" = "1" ]; }; then
  step "reusing existing boot log $LOG"
else
  step "booting (SWRENDER=1, prep: $PREP_ARGS, up to ${TIMEOUT}s)"
  rm -f "$LOG"
  # shellcheck disable=SC2086 # deliberate word-splitting of the prep flag list
  PREP_VSOCK_HALS_ARGS="$PREP_ARGS" SWRENDER=1 "$HERE/run-android64.sh" >"$LOG" 2>&1 &
  BOOT_PID=$!
  cleanup() {
    kill "$BOOT_PID" 2>/dev/null || true
    pkill -P "$BOOT_PID" 2>/dev/null || true
    pkill -f 'target/debug/aemulator-hvf run-android' 2>/dev/null || true
  }
  trap cleanup EXIT

  # Wait until the guest is far enough along to have exercised every HAL, or until the cap. Boot phase
  # 1000 is PHASE_BOOT_COMPLETED; the sensors marker confirms the pump actually started streaming.
  waited=0
  while [ "$waited" -lt "$TIMEOUT" ]; do
    if grep -q "OnBootPhase_1000_" "$LOG" 2>/dev/null; then
      echo "  guest reached boot phase 1000 after ${waited}s"
      # Give the late-boot HALs (NFC settings probe, BT adapter enable) a moment to run.
      sleep 20
      break
    fi
    if grep -qE "InitFatalReboot|HV_DENIED|no KosmicKrisp" "$LOG" 2>/dev/null; then
      echo "  fatal boot failure detected after ${waited}s"
      break
    fi
    sleep 5
    waited=$((waited+5))
  done
  [ "$waited" -ge "$TIMEOUT" ] && warn "boot did not reach phase 1000 within ${TIMEOUT}s — checks below may be incomplete"
fi

[ -s "$LOG" ] || { bad "boot log $LOG is empty"; exit 1; }

# `absent <label> <pattern>` — the pattern MUST NOT appear (a fixed regression).
absent() {
  local label="$1" pat="$2" n
  n=$(grep -cF "$pat" "$LOG" 2>/dev/null || true); n=${n:-0}
  if [ "$n" -eq 0 ]; then ok "$label"; else bad "$label — still present ${n}x: \"$pat\""; fi
}
# `present <label> <pattern>` — the pattern MUST appear (a backend actually answered).
present() {
  local label="$1" pat="$2" n
  n=$(grep -cE "$pat" "$LOG" 2>/dev/null || true); n=${n:-0}
  if [ "$n" -gt 0 ]; then ok "$label (${n}x)"; else bad "$label — never seen: /$pat/"; fi
}

step "regressions that must stay fixed"
# prep-disable-32bit-dualabi.sh was wrongly applied to this 64-bit-only image and disabled mediaserver
# (arm64 and healthy here), so media.player + media.resource_manager never registered and four
# processes retried the lazy AIDL start ~1/s forever.
absent "mediaserver runs: no media.resource_manager lazy-start storm" \
       "Could not find 'aidl/media.resource_manager'"
absent "mediaserver runs: no media.player lazy-start storm" \
       "Could not find 'aidl/media.player'"
absent "no client blocks waiting on media.resource_manager" \
       "Waited one second for media.resource_manager"
# The fleet only had hvc0..hvc11, so the NFC HAL's /dev/hvc12 did not exist and it crash-looped.
absent "NFC opens /dev/hvc12 (fleet is 20 ports, not 12)" \
       "Failed to open virtio-console device"
# The sensors sub-HAL opens hvc18 (control) + hvc19 (data).
absent "sensors sub-HAL connects to hvc18/hvc19" \
       "Could not connect to sensors control"
# The threadnetwork HAL asserts ro.boot.openthread_node_id > 0, which reached the guest only after the
# bootconfig duplicate-key bug was fixed. That check is now unconditional: the property is emitted
# whether or not the HAL is enabled, so this must never regress even while the HAL stays off (below).
absent "threadnetwork gets its node id" "Check failed: node_id > 0"
# A duplicated key makes the kernel discard the WHOLE bootconfig, so anything bootconfig-only silently
# never arrived. This was true on every boot until the launch keys were deduped against the vendor blob.
absent "kernel accepts the bootconfig blob" "Failed to parse bootconfig"

step "host backends actually answered"
present "20 HVC channels created" "virtio-pci consoles: 20 HVC channels"
present "sensors: guest enabled sensors and the pump is streaming" "\[hal:sensors\] guest enabled [0-9]+ sensor"
present "sensors: ISensors/default resolved from the VINTF manifest" "Found android.hardware.sensors.ISensors/default in device VINTF manifest"
present "sensors: SensorService enumerated the HAL's sensors" "SensorService: (Goldfish|nuSensorService starting)"
present "NFC: INfc/default resolved from the VINTF manifest" "Found android.hardware.nfc.INfc/default in device VINTF manifest"
# The guest's own NCI decoder logging our replies is the strongest possible evidence: it parsed them.
present "NFC: guest decoded our CORE_RESET_RSP" "mt: Response, payload: \[0, 1, 0\]"
present "NFC: guest decoded our CORE_RESET_NTF (NCI 2.0 => version 32)" "mt: Notification, payload: \[0, 9, 0, 0, 32"
# The guest's own HCI parser accepting our Reset reply is the strongest evidence the H4 path works.
present "Bluetooth: guest decoded our HCI_Reset reply" "ResetComplete \{ status: Success \}"
present "Bluetooth: HAL initialization completed" "HCI HAL initialization completed"
# The real end-to-end result: the adapter state machine walks all the way to ON. Asserting the state
# rather than the absence of a warning matters, because the guest logs
# `waitForInitialization: Running in degraded performance mode due to slow hardware` a few
# milliseconds BEFORE `HCI HAL initialization completed` — it is a timing note about the first
# initialize round-trip, not a failure, and it is emitted even on a fully working adapter.
present "Bluetooth: adapter reached BLE_ON" "BluetoothAdapterState: BLE_ON : State entered"
present "Bluetooth: adapter reached ON" "BluetoothAdapterState: ON : State entered"

step "framing and protocol coverage"
absent "no HAL port lost framing" "framing desync"
# A 1-byte UNKNOWN_COMMAND refusal is fatal for any command the Android stack parses into a typed
# view — it aborts the whole Bluetooth process. Any opcode reaching the fallback is a latent abort.
absent "Bluetooth: every opcode the guest issued is modelled" "[hal:bluetooth] unmodelled opcode"
absent "Bluetooth: stack never aborted on a malformed reply" "Complete view is invalid"
absent "Bluetooth: no native crash" "Native crash"

step "vsock: RIL and lights"
present "vsock device present with host services" "virtio-pci vsock: guest CID"
# The whole point of the modem: rild stops crash-looping, so init stops declaring the device unhealthy
# and wiping every server-configurable flag.
absent "RIL: no missing-vsock-port failure" "must be an integer vsock port"
absent "RIL: no crash-loop flag reset" "updatable crashing detected"
present "modem: RIL connected to the host modem" "\[hal:modem\] RIL connected"
absent "modem: every AT command the RIL issued is handled" "[hal:modem] unhandled AT command"
# The lights HAL was disabled because it ABORTED on startup without a vsock port. What matters is that
# it now starts, registers, and stays up; it opens its vsock connection lazily on the first
# setLightState, which a headless boot with no notifications may never trigger.
present "lights: HAL started" "started service 'vendor.light-cuttlefish'"
present "lights: ILights/default resolved from the VINTF manifest" "Found android.hardware.light.ILights/default in device VINTF manifest"

step "still open (tracked)"
# threadnetwork: `ro.boot.openthread_node_id` was only ONE of two blockers. Its bundled `ot-rcp` uses
# OpenThread's *simulation* platform, which opens a multicast socket on the interface named by its `-L`
# argument (`-Leth1` here) to form a simulated radio medium — and this guest has only eth0, so
# `utilsInitSocket() at simul_utils.c:370: Failure` kills ot-rcp, the HAL exits 5, and after four
# respawns init resets every server-configurable flag. Enabling it therefore trades a fixed bug for a
# live one, so it stays disabled until the guest has a second NIC. Re-enable with
# `PREP_VSOCK_HALS_ARGS="--revert=sensors,light,threadnetwork"` once eth1 exists.
for pair in \
  "modem: SIM READY reported to the framework|EVENT_ICC_CHANGED: SIM absent" \
  "modem: every response format the RIL parses|RadioError::INTERNAL_ERR" ; do
  label="${pair%%|*}"; pat="${pair#*|}"
  n=$(grep -cF "$pat" "$LOG" 2>/dev/null || true); n=${n:-0}
  if [ "$n" -eq 0 ]; then ok "$label — resolved"; else warn "$label — ${n}x \"$pat\""; fi
done

step "boot reached"
present "system_server completed boot phase 1000" "OnBootPhase_1000_"

step "SUMMARY"
printf '  %s%d pass%s  %s%d fail%s  %s%d warn%s   (log: %s)\n' \
  "$C_G" "$PASS" "$C_0" "$C_R" "$FAIL" "$C_0" "$C_Y" "$WARN" "$C_0" "$LOG"
[ "$FAIL" -eq 0 ] || exit 1
