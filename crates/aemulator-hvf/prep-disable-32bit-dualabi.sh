#!/usr/bin/env bash
# prep-disable-32bit.sh — break the system_server boot loop caused by 32-bit-only services.
#
# ROOT CAUSE (pass-7 diagnosis, verified from logcat on hvc2):
#   The aosp_cf_arm64 image is a DUAL-ABI (zygote64_32) build. Apple Silicon (M2) has NO AArch32
#   execution state, so every 32-bit binary yields `cannot execv(...): Exec format error` and the
#   service exits status 127. Four such services loop:
#       zygote_secondary  /system/bin/app_process32                       (THE boot blocker)
#       drm               /system/bin/drmserver
#       media             /system/bin/mediaserver
#       vendor.audio-hal  /vendor/bin/hw/android.hardware.audio.service
#
#   The fatal one is `zygote_secondary`: its `.rc` carries `onrestart restart zygote`, so every time
#   the 32-bit secondary zygote exits 127 (~every 5s) init runs that directive and sends **signal 9
#   to the PRIMARY zygote's process group** — which includes the freshly-forked `system_server`. So
#   the 64-bit zygote fully preloads (17308 classes), forks system_server ("System server process
#   NNNN has been created" → ART "Using CollectorTypeCC GC"), and ~5s later the whole group is killed
#   by the secondary's onrestart. system_server therefore restart-loops forever and never reaches
#   boot_completed. (Observed 43x in the pass-6 log: fork → GC → group-kill → repeat.)
#
# FIX (surgical, same-length EROFS file-data edits — the prep-super.sh technique; verity disabled, and
# these bytes live in system_a file-data far from any EROFS block-0 superblock, so no csum recompute):
#   1. zygote_secondary `class main`  -> `class hvfx`   (a class no `class_start` ever names → the
#      secondary is never started at boot, so it never 127-loops, so its onrestart never fires).
#   2. zygote_secondary `onrestart restart zygote` -> `#nrestart restart zygote`  (belt-and-suspenders:
#      even if something else `start`s the secondary — e.g. netd's onrestart — its death can no longer
#      kill the primary zygote / system_server).
#   3. drmserver: comment its two `start drm` property-trigger lines (`start`->`#tart`). drmserver is
#      declared `disabled`, so those triggers are its ONLY start path; commenting them stops the loop.
#   4. mediaserver `class main` -> `class hvfx`  (stop the class_start autostart loop).
#
# vendor.audio-hal is intentionally left alone (it has several `start vendor.audio-hal` triggers across
# audioserver.rc; disabling it cleanly needs more edits and it does NOT block boot_completed — it just
# times out as an absent HAL). Add it here later only if a boot log shows system_server blocking on it.
#
# NOTE on block bounding: each .rc file is a separate EROFS inode; the file's text is followed by
# binary inode/padding (NOT a clean `\nservice `). So a service block is bounded by the first NUL/
# non-rc byte after the decl, never by scanning for the next `\nservice ` (which would bleed across the
# file boundary into the NEXT rc file — the mtpd-clobber bug). `_rc_block()` truncates at the first
# byte that can't appear in rc text.
#
# Idempotent + repeatable: re-running detects already-patched bytes and no-ops.
#
# ---------------------------------------------------------------------------------------------------
# SCOPE — THIS SCRIPT IS FOR THE DUAL-ABI `cf` IMAGE ONLY (hence the `-dualabi` name).
#
# It was for a long time also run against the 64-bit-only `cf64` image on the theory that it was a
# harmless no-op there. It is not. Step 4 unconditionally disables `mediaserver`, which on cf64 is a
# perfectly good arm64 binary; the result was that `media.player` and `media.resource_manager` never
# registered and four processes retried the lazy AIDL start roughly once a second, forever. cf64's
# run scripts no longer call this.
#
# `--revert` undoes all four edits, for cleaning up an image this was wrongly applied to. It is
# targeted and verifiable (afterwards, zero `class hvfx` should remain), which makes it preferable to
# regenerating the 8.6 GB super.raw.img from super.img and replaying every other prep script.
#
#   ./prep-disable-32bit-dualabi.sh <super.raw.img> --revert
# ---------------------------------------------------------------------------------------------------
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf/img/super.raw.img}"
MODE="apply"
for a in "$@"; do [ "$a" = "--revert" ] && MODE="revert"; done

python3 - "$SUPER" "$MODE" <<'PY'
import os, sys

super_path = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "apply"
fd = os.open(super_path, os.O_RDWR)
sz = os.lseek(fd, 0, 2)

def find_all(needle, start=0, end=None):
    """Byte offsets of every occurrence of `needle` (streamed, low-memory)."""
    if end is None:
        end = sz
    CHUNK = 32 * 1024 * 1024
    offs = []
    pos = start
    prev = b''
    while pos < end:
        buf = os.pread(fd, min(CHUNK, end - pos), pos)
        if not buf:
            break
        hay = prev + buf
        s = 0
        while True:
            i = hay.find(needle, s)
            if i < 0:
                break
            offs.append(pos - len(prev) + i)
            s = i + 1
        prev = hay[-len(needle):]
        pos += len(buf)
    return offs

def read_at(off, n):
    return os.pread(fd, n, off)

def rc_block(base, maxlen=1024):
    """The rc-text bytes of the service block starting at `base`, truncated at the first byte that
    cannot appear in an rc file (the EROFS inode/padding tail that follows every file's data). This
    confines every search to the ONE .rc file and never bleeds into the next inode."""
    raw = read_at(base, maxlen)
    out = bytearray()
    for b in raw:
        # printable ASCII + \n + \t only; anything else = end-of-file-data padding.
        if b == 0x0a or b == 0x09 or 0x20 <= b <= 0x7e:
            out.append(b)
        else:
            break
    return bytes(out)

changed = 0
REVERT = (mode == "revert")

def swap_in_block(decl_bytes, pristine_line, patched_line, word_off, label):
    """Swap one line inside the single-service rc block that starts with `decl_bytes`.

    Applying rewrites `pristine_line` -> `patched_line`; reverting does the reverse. Both lines must be
    the same length (an EROFS file-data edit cannot change a file's size). `word_off` is the byte
    offset within the line at which the two differ, and only those differing bytes are written.

    Bounded to the block via rc_block so it can never walk into the next inode's identical line."""
    global changed
    src, dst = (patched_line, pristine_line) if REVERT else (pristine_line, patched_line)
    assert len(src) == len(dst), "rc edits must preserve length"
    bases = find_all(decl_bytes)
    if not bases:
        print("WARN: %r not found" % decl_bytes, file=sys.stderr)
        return
    for base in bases:
        block = rc_block(base)
        ci = block.find(src)
        if ci >= 0:
            off = base + ci + word_off
            os.pwrite(fd, dst[word_off:], off)
            changed += 1
            print("%s: %r -> %r @ %d" % (label, src.strip(), dst.strip(), off))
        elif block.find(dst) >= 0:
            print("%s: already %s (no-op)" % (label, "reverted" if REVERT else "patched"))
        else:
            print("WARN: %s: neither %r nor %r in block" % (label, src.strip(), dst.strip()),
                  file=sys.stderr)

ZYGOTE2 = b'service zygote_secondary /system/bin/app_process32'
CLASS_PRISTINE = b'\n    class main\n'
CLASS_PATCHED  = b'\n    class hvfx\n'
CLASS_WORD_OFF = len(b'\n    class ')

# ---- 1. zygote_secondary: never start it (its 127-exit loop kills the primary zygote's group) ------
swap_in_block(ZYGOTE2, CLASS_PRISTINE, CLASS_PATCHED, CLASS_WORD_OFF, "zygote_secondary class")

# ---- 2. zygote_secondary: neuter `onrestart restart zygote` (belt and braces for step 1) ----------
swap_in_block(ZYGOTE2,
              b'\n    onrestart restart zygote\n',
              b'\n    #nrestart restart zygote\n',
              len(b'\n    '),
              "zygote_secondary onrestart")

# ---- 3. drmserver: comment its `start drm` property triggers ---------------------------------------
# Match only the exact `\n    start drm\n` line (never `start drmserver`/`start drm.foo`). These live
# in property-trigger blocks rather than a service block, so they are matched whole-image rather than
# bounded to a decl — the exact-line match is what keeps that safe.
DRM_PRISTINE = b'\n    start drm\n'
DRM_PATCHED = b'\n    #tart drm\n'
drm_src, drm_dst = (DRM_PATCHED, DRM_PRISTINE) if REVERT else (DRM_PRISTINE, DRM_PATCHED)
for o in find_all(drm_src):
    s_off = o + len(b'\n    ')
    os.pwrite(fd, drm_dst[len(b'\n    '):], s_off)
    changed += 1
    print("drmserver: %r -> %r @ %d" % (drm_src.strip(), drm_dst.strip(), s_off))
n_done = len(find_all(drm_dst))
if n_done:
    print("drmserver: %d line(s) now %r" % (n_done, drm_dst.strip()))

# ---- 4. mediaserver: rename class so class_start main never launches it ----------------------------
# THE ONE THAT MUST NEVER TOUCH cf64 — see the SCOPE block at the top of this file.
swap_in_block(b'service media /system/bin/mediaserver',
              CLASS_PRISTINE, CLASS_PATCHED, CLASS_WORD_OFF, "mediaserver class")

os.fsync(fd)

# Reverting is only meaningful if it is complete, so verify rather than trust: after a revert no
# `class hvfx` may remain anywhere in the image.
if REVERT:
    left = find_all(b'\n    class hvfx\n')
    if left:
        print("ERROR: %d 'class hvfx' line(s) still present at %s — revert incomplete"
              % (len(left), left), file=sys.stderr)
        os.close(fd)
        sys.exit(1)
    print("verified: no 'class hvfx' remains in the image")

os.close(fd)
print("prep-disable-32bit-dualabi: %d byte-edit(s) %s this run"
      % (changed, "reverted" if REVERT else "applied"))
PY
