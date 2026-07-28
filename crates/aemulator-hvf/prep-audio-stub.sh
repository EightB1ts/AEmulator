#!/usr/bin/env bash
# prep-audio-stub.sh — DEAD-END (pass 8, kept for the record; DEFAULT-OFF in run-android.sh).
#
# GOAL was to clear the 32-bit audio-HAL Watchdog wall so system_server survives. It does NOT.
# VERDICT (proven this pass): undeclaring the audio HAL does not help — it merely trades the 66s
# onFirstRef block for an AudioFlinger SIGSEGV, because AudioFlinger's ctor dereferences the (now
# null) devices-factory. Either way media.audio_policy never publishes and the Watchdog SIGKILLs
# system_server at ~66-67s. Ground truth (erofs-utils extract of vendor_a):
#   * /vendor/bin/hw/android.hardware.audio.service is 32-bit ARM (Exec-format-errors on M2).
#   * 64-bit passthrough impls DO exist (android.hardware.audio@7.1-impl.ranchu.so,
#     audio.primary.default.so, android.hardware.audio.effect@7.0-impl.so) BUT Treble linker
#     namespaces bar the *system* audioserver from dlopen'ing /vendor/lib64 in-process, and there
#     is NO 64-bit *vendor* audio-service host binary anywhere in the image to load them.
#   * The all-64-bit fix image (aosp_cf_arm64_only_phone) is not built on public AOSP CI
#     (aosp-android14-gsi / -android15-gsi / -main all return no builds).
# So there is no clean host-side edit that publishes media.audio_policy on THIS dual-ABI image.
# See PHASE4-STATUS.md § Pass 8. Run only via PREP_AUDIO_STUB=1 to reproduce the dead-end.
#
# ---- original rationale (why undeclaring *seemed* like it would work) ----
# ROOT CAUSE (pass-7 diagnosis, verified from logcat on hvc2):
#   The aosp_cf_arm64 image is DUAL-ABI. Apple Silicon has NO AArch32, so the vendor audio HAL
#   binary `/vendor/bin/hw/android.hardware.audio.service` (32-bit) Exec-format-errors and never
#   registers. Its interface is DECLARED in a plaintext vendor VINTF fragment as the HIDL service
#   `android.hardware.audio@7.1::IDevicesFactory/default`. So when audioserver's
#   AudioPolicyService::onFirstRef() -> AudioFlinger loadHwModule() -> DevicesFactoryHal probes the
#   audio HAL, hwservicemanager sees it DECLARED-but-not-registered and tries to LAZY-START it
#   (init execs the 32-bit binary -> Exec format error, forever), so the probe blocks. onFirstRef
#   never completes, `media.audio_policy` is never published, and system_server's
#   AudioService.<init> (SystemServer.startOtherServices) blocks on the main thread for 66s until
#   the Android Watchdog SIGKILLs system_server (AudioService.java:939). zygote then exits "because
#   system server has terminated" -> a 66s restart loop. Boot never reaches boot_completed.
#     [logcat] AudioPolicyService not published, waiting...            (x43)
#     [logcat] DevicesFactory/default is not registered, trying to start it as a lazy HAL ...  (x15)
#     [logcat] cannot execv('/vendor/bin/hw/android.hardware.audio.service'): Exec format error
#     Watchdog: *** WATCHDOG KILLING SYSTEM PROCESS ... AudioService$Lifecycle.<init>(:939)
#
# FIX (surgical, SAME-LENGTH plaintext EROFS file-data edit — the prep-super.sh technique; verity
# disabled, and the vendor VINTF fragment is uncompressed file-data far from any EROFS block-0
# superblock, so NO csum recompute):
#   Corrupt the audio HAL's VINTF <name> in the vendor manifest fragment:
#       <name>android.hardware.audio</name>  ->  <name>android.hardware.audiX</name>
#   With the interface no longer DECLARED, hwservicemanager no longer lazy-starts the 32-bit binary,
#   and the framework's HIDL audio probe (IDevicesFactory::getService) returns null IMMEDIATELY
#   instead of blocking. AudioFlinger proceeds with no primary audio HAL, AudioPolicyService's
#   onFirstRef() completes, `media.audio_policy` publishes, and AudioService.<init> returns before
#   the Watchdog fires. (Audio playback is a no-op — acceptable to reach the launcher; the real cure
#   is the all-64-bit aosp_cf_arm64_only_phone target.)
#
#   The edit touches ONLY the exact `<name>android.hardware.audio</name>` (the HIDL IDevicesFactory
#   HAL). `android.hardware.audio.effect` and every other HAL are untouched. Well-formed XML is
#   preserved (one name character changes), so the fragment still parses.
#
# Idempotent + repeatable: re-running detects the already-patched name and no-ops.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, sys
super_path = sys.argv[1]
fd = os.open(super_path, os.O_RDWR)
sz = os.lseek(fd, 0, 2)

ORIG    = b'<name>android.hardware.audio</name>'
PATCHED = b'<name>android.hardware.audiX</name>'   # .audiX != .audio -> HAL no longer declared
# byte to flip: the final 'o' of "audio", i.e. index of 'o' in ORIG
FLIP_IDX = ORIG.index(b'audio</name>') + len(b'audi')   # points at 'o'
assert ORIG[FLIP_IDX:FLIP_IDX+1] == b'o'

def find_all(needle):
    offs=[]; pos=0; prev=b''
    while pos < sz:
        buf=os.pread(fd, 64*1024*1024, pos)
        if not buf: break
        hay=prev+buf; s=0
        while True:
            i=hay.find(needle, s)
            if i<0: break
            offs.append(pos-len(prev)+i); s=i+1
        prev=hay[-len(needle):]; pos+=len(buf)
    return offs

patched=0
for base in find_all(ORIG):
    os.pwrite(fd, b'X', base + FLIP_IDX)
    patched += 1
    print("audio HAL VINTF <name> disabled @ byte %d (android.hardware.audio -> .audiX)" % base)

if patched == 0:
    already = len(find_all(PATCHED))
    if already:
        print("audio HAL VINTF <name> already disabled (%d) — no-op" % already)
    else:
        print("WARN: <name>android.hardware.audio</name> not found (image layout changed?)",
              file=sys.stderr)

os.fsync(fd); os.close(fd)
print("prep-audio-stub: %d edit(s) applied this run" % patched)
PY
