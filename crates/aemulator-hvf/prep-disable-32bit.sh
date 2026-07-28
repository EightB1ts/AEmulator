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
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, sys

super_path = sys.argv[1]
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

def rename_class(decl_bytes, label):
    """In the single-service rc block that starts with `decl_bytes`, rename `class main`->`class hvfx`.
    Bounded to the block via rc_block so it can't walk into the next inode's `class main`."""
    global changed
    bases = find_all(decl_bytes)
    if not bases:
        print("WARN: %r not found" % decl_bytes, file=sys.stderr)
        return
    for base in bases:
        block = rc_block(base)
        ci = block.find(b'\n    class main\n')
        if ci >= 0:
            main_off = base + ci + len(b'\n    class ')
            os.pwrite(fd, b'hvfx', main_off)
            changed += 1
            print("%s: class main -> class hvfx @ %d" % (label, main_off))
        elif block.find(b'\n    class hvfx\n') >= 0:
            print("%s: class already hvfx (no-op)" % label)
        else:
            print("WARN: %s class main not found in block" % label, file=sys.stderr)

# ---- 1 + 2. zygote_secondary: rename class, neuter onrestart-restart-zygote ------------------------
zbases = find_all(b'service zygote_secondary /system/bin/app_process32')
if not zbases:
    print("WARN: zygote_secondary decl not found", file=sys.stderr)
for base in zbases:
    block = rc_block(base)
    # 1. class main -> hvfx
    ci = block.find(b'\n    class main\n')
    if ci >= 0:
        main_off = base + ci + len(b'\n    class ')
        os.pwrite(fd, b'hvfx', main_off)
        changed += 1
        print("zygote_secondary: class main -> class hvfx @ %d" % main_off)
    elif block.find(b'\n    class hvfx\n') >= 0:
        print("zygote_secondary: class already hvfx (no-op)")
    else:
        print("WARN: zygote_secondary class main not found", file=sys.stderr)
    # 2. onrestart restart zygote -> commented
    oi = block.find(b'\n    onrestart restart zygote\n')
    if oi >= 0:
        o_off = base + oi + len(b'\n    ')
        os.pwrite(fd, b'#', o_off)
        changed += 1
        print("zygote_secondary: onrestart restart zygote -> commented @ %d" % o_off)
    elif block.find(b'\n    #nrestart restart zygote\n') >= 0:
        print("zygote_secondary: onrestart already commented (no-op)")
    else:
        print("WARN: zygote_secondary onrestart line not found", file=sys.stderr)

# ---- 3. drmserver: comment its `start drm` property triggers ---------------------------------------
# Match only the exact `\n    start drm\n` line (never `start drmserver`/`start drm.foo`).
for o in find_all(b'\n    start drm\n'):
    s_off = o + len(b'\n    ')
    os.pwrite(fd, b'#tart', s_off)
    changed += 1
    print("drmserver: 'start drm' -> commented @ %d" % s_off)
n_done = len(find_all(b'\n    #tart drm\n'))
if n_done:
    print("drmserver: %d 'start drm' line(s) commented total" % n_done)

# ---- 4. mediaserver: rename class so class_start main never launches it ----------------------------
rename_class(b'service media /system/bin/mediaserver', "mediaserver")

os.fsync(fd)
os.close(fd)
print("prep-disable-32bit: %d byte-edit(s) applied this run" % changed)
PY
