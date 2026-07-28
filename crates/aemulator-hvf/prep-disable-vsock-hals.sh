#!/usr/bin/env bash
# prep-disable-vsock-hals.sh — enable/disable the Cuttlefish HALs that need a host-side backend.
#
# ORIGINAL PURPOSE (pass-10): neutralize three HALs that hard-depend on a host backend this VMM did
# not provide, so each aborted at startup, init respawned it forever, and system_server either blocked
# on `waitForDeclaredService` until the Watchdog killed it or was starved by the respawn churn:
#       vendor.light-cuttlefish      android.hardware.light  ILights/default   (APEX com.google.cf.light)
#       vendor.sensors-hal-multihal  android.hardware.sensors ISensors/default (/vendor/bin/hw)
#       vendor.threadnetwork_hal     android.hardware.threadnetwork IThreadChip/chip0 (APEX)
#
# WHAT WE NOW KNOW (and why this script grew a `--revert`): the script's name is a misnomer for two of
# the three. Only **light** actually needs vsock (its Rust HAL reads `ro.boot.vsock_lights_port`).
#   · sensors is NOT vsock — it opens two virtio-console ports, /dev/hvc18 (control) and /dev/hvc19
#     (data), and speaks the goldfish ASCII sensor protocol. It failed because the VMM only created
#     hvc0..hvc11, so those nodes did not exist.
#   · threadnetwork needs NO host backend whatsoever — it forks its own bundled `ot-rcp` over a pty
#     and speaks Spinel to it inside the guest. It aborted purely because `ro.boot.openthread_node_id`
#     was unset and it asserts `node_id > 0`. Setting that one bootconfig property fixes it outright.
#
# Disabling HALs is also the wrong long-run trade: a device reporting zero sensors is a stronger
# virtualization tell than any build-prop string. So each HAL here is re-enabled as its host backend
# lands, via `--revert`.
#
# USAGE
#   ./prep-disable-vsock-hals.sh <super.raw.img>                     # disable all three (default)
#   ./prep-disable-vsock-hals.sh <super.raw.img> --revert=threadnetwork
#   ./prep-disable-vsock-hals.sh <super.raw.img> --revert=sensors,light
#   ./prep-disable-vsock-hals.sh <super.raw.img> --only=light         # disable just one
#   ./prep-disable-vsock-hals.sh <super.raw.img> --probe=sensors      # run the HAL, leave it undeclared
#
# `--probe=<hal>` is the diagnostic half-step, and it exists because of a chicken-and-egg problem:
# to implement a HAL's protocol you must first observe its wire framing, but the HAL only writes to its
# port if it is running, and a fully-enabled HAL blocks system_server. So `--probe` starts the HAL
# *binary* (init class / APEX dirent) while leaving its VINTF <name> mangled. The HAL then opens its
# port and sends its first request — which the VMM hex-dumps — but system_server never waits on it, so
# nothing gets Watchdog-killed. Expect respawn churn during a probe boot; that is the price of the
# observation and is why it is not a normal mode.
#
# ORDERING RULE — the one that bites. A HAL's VINTF re-declaration and its backend must land in the
# SAME boot. Re-declaring an interface whose backend does not exist yet puts system_server back into
# `waitForDeclaredService` until the Watchdog kills it: that is the original failure this script was
# written to escape. `--revert` therefore takes explicit HAL names; there is deliberately no
# `--revert-all`.
#
# MECHANISM (unchanged; all edits are same-length, in-place, and idempotent — verity is disabled so
# there is no hashtree, and EROFS file-data edits land far from block-0 superblocks so no checksum
# recompute is needed):
#   1. VINTF (un)declare — corrupt/restore the last character of each HAL's <name>, so
#      waitForDeclaredService either returns null immediately or resolves properly.
#          android.hardware.light         <-> android.hardware.lighX
#          android.hardware.sensors       <-> android.hardware.sensorX
#          android.hardware.threadnetwork <-> android.hardware.threadnetworX
#   2. sensors is a plain /vendor init service: flip its `class hal` <-> `class hxl` (a class no
#      `class_start` ever names), so the binary is never launched at all -> zero crash churn.
#   3. light + threadnetwork are APEX HALs: flip the /vendor/apex dirent `.apex` <-> `.apeX` (apexd's
#      suffix scan skips it). The in-APEX .rc and VINTF fragment vanish with the APEX, so the service
#      is never even defined. apexd tolerates the absent optional module.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf64/img/super.raw.img}"
REVERT=""
ONLY=""
PROBE=""
# Each flag ACCUMULATES rather than overwrites, so `--revert=a --revert=b` and `--revert=a,b` are
# equivalent. (An earlier version assigned instead of appending, which silently dropped every value but
# the last — `--revert=threadnetwork --revert=sensors` re-disabled threadnetwork.)
append() { if [ -z "$1" ]; then printf '%s' "$2"; else printf '%s,%s' "$1" "$2"; fi; }
for a in "$@"; do
  case "$a" in
    --revert=*) REVERT="$(append "$REVERT" "${a#--revert=}")" ;;
    --revert)   echo "error: --revert needs HAL names, e.g. --revert=sensors (see the ORDERING RULE" \
                     "in this script's header)" >&2; exit 2 ;;
    --only=*)   ONLY="$(append "$ONLY" "${a#--only=}")" ;;
    --probe=*)  PROBE="$(append "$PROBE" "${a#--probe=}")" ;;
  esac
done

python3 - "$SUPER" "$REVERT" "$ONLY" "$PROBE" <<'PY'
import os, struct, sys
super_path, revert_arg, only_arg, probe_arg = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
revert = {h for h in revert_arg.split(',') if h}
only = {h for h in only_arg.split(',') if h}
probe = {h for h in probe_arg.split(',') if h}
fd = os.open(super_path, os.O_RDWR)
sz = os.lseek(fd, 0, 2)

def find_all(needle, start=0, end=None):
    if end is None: end = sz
    CHUNK = 32 * 1024 * 1024
    offs=[]; pos=start; prev=b''
    while pos < end:
        buf=os.pread(fd, min(CHUNK, end-pos), pos)
        if not buf: break
        hay=prev+buf; s=0
        while True:
            i=hay.find(needle, s)
            if i<0: break
            offs.append(pos-len(prev)+i); s=i+1
        prev=hay[-len(needle):]; pos+=len(buf)
    return offs

def rc_block(base, maxlen=1024):
    """rc-text bytes of the service block at `base`, truncated at the first byte that cannot appear in
    an rc file (the EROFS inode/padding tail). Confines every search to the ONE .rc file."""
    raw=os.pread(fd, maxlen, base); out=bytearray()
    for b in raw:
        if b in (0x0a,0x09) or 0x20<=b<=0x7e: out.append(b)
        else: break
    return bytes(out)

# ---- locate the vendor_a EROFS extent via liblp metadata (mirrors prep-apex-dedup.sh) -------------
RESERVED=4096; GEO=4096; meta_off=RESERVED+GEO*2
hdr=os.pread(fd,256,meta_off); header_size=struct.unpack('<I',hdr[8:12])[0]
def desc(o): return struct.unpack('<III',hdr[o:o+12])
p_off,p_num,p_sz=desc(80); e_off,e_num,e_sz=desc(92); tables_off=meta_off+header_size
parts={}
for i in range(p_num):
    b=os.pread(fd,p_sz,tables_off+p_off+i*p_sz); nm=b[0:36].split(b'\0')[0].decode()
    _,fe,ne,_=struct.unpack('<IIII',b[36:52]); parts[nm]=(fe,ne)
fe,ne=parts['vendor_a']
b=os.pread(fd,e_sz,tables_off+e_off+fe*e_sz); ns,tt,td,_=struct.unpack('<QIQI',b[0:24])
VENDOR_FS=td*512; VENDOR_LEN=ns*512

# ---- the per-HAL edit table ------------------------------------------------------------------------
# Each entry: (enabled_bytes, disabled_bytes) pairs. Both members of a pair are ALWAYS the same length.
HAL_EDITS = {
    'light': {
        'vintf': (b'<name>android.hardware.light</name>', b'<name>android.hardware.lighX</name>'),
        'apex':  (b'com.google.cf.light.apex', b'com.google.cf.light.apeX'),
        'needs': 'a host vsock service on ro.boot.vsock_lights_port',
    },
    'sensors': {
        'vintf': (b'<name>android.hardware.sensors</name>', b'<name>android.hardware.sensorX</name>'),
        'rc':    (b'service vendor.sensors-hal-multihal /vendor/bin/hw/android.hardware.sensors-service.multihal',
                  b'\n    class hal\n', b'\n    class hxl\n', len(b'\n    class ')),
        'needs': 'virtio-console ports hvc18 + hvc19 and the goldfish ASCII sensor protocol',
    },
    'threadnetwork': {
        'vintf': (b'<name>android.hardware.threadnetwork</name>',
                  b'<name>android.hardware.threadnetworX</name>'),
        'apex':  (b'com.android.hardware.threadnetwork.apex', b'com.android.hardware.threadnetwork.apeX'),
        'needs': 'nothing host-side — only bootconfig androidboot.openthread_node_id=1',
    },
}

unknown = (revert | only | probe) - set(HAL_EDITS)
if unknown:
    print("error: unknown HAL name(s): %s (known: %s)" % (', '.join(sorted(unknown)),
          ', '.join(sorted(HAL_EDITS))), file=sys.stderr)
    sys.exit(2)
both = revert & probe
if both:
    print("error: %s given to both --revert and --probe; --revert already implies a running HAL"
          % ', '.join(sorted(both)), file=sys.stderr)
    sys.exit(2)

targets = only if only else set(HAL_EDITS)
targets |= probe   # a probed HAL is always processed, even alongside --only
total = 0

def swap(pair, label, start=0, end=None, enable=False):
    """Rewrite only the differing bytes of `pair` in whichever direction `enable` asks for."""
    global total
    enabled, disabled = pair
    assert len(enabled) == len(disabled), "edits must preserve length"
    src, dst = (disabled, enabled) if enable else (enabled, disabled)
    diff = [k for k in range(len(src)) if src[k] != dst[k]]
    offs = find_all(src, start, end)
    if offs:
        for base in offs:
            for k in diff:
                os.pwrite(fd, dst[k:k+1], base+k)
            total += 1
        print("  %-28s %s (%d site(s))" % (label, "ENABLED" if enable else "disabled", len(offs)))
    elif find_all(dst, start, end):
        print("  %-28s already %s (no-op)" % (label, "enabled" if enable else "disabled"))
    else:
        print("  WARN: %s: found neither form" % label, file=sys.stderr)

for hal in sorted(targets):
    spec = HAL_EDITS[hal]
    probing = hal in probe
    enable = hal in revert or probing
    mode = "PROBE  " if probing else ("ENABLE " if enable else "disable")
    print("%s %s   [needs: %s]" % (mode, hal, spec['needs']))
    # Probing runs the binary but leaves the interface UNDECLARED, so system_server never blocks on it.
    swap(spec['vintf'], "%s VINTF" % hal, enable=False if probing else enable)
    if 'apex' in spec:
        # Bounded to vendor_a's EROFS extent so a dirent edit can never land in a block-0 superblock.
        swap(spec['apex'], "%s APEX dirent" % hal, VENDOR_FS, VENDOR_FS+VENDOR_LEN, enable=enable)
    if 'rc' in spec:
        decl, cls_enabled, cls_disabled, word_off = spec['rc']
        src, dst = (cls_disabled, cls_enabled) if enable else (cls_enabled, cls_disabled)
        done = False
        for base in find_all(decl):
            blk = rc_block(base)
            ci = blk.find(src)
            if ci >= 0:
                os.pwrite(fd, dst[word_off:], base+ci+word_off)
                total += 1
                print("  %-28s %s" % ("%s init class" % hal, "ENABLED" if enable else "disabled"))
                done = True
            elif blk.find(dst) >= 0:
                print("  %-28s already %s (no-op)" % ("%s init class" % hal,
                      "enabled" if enable else "disabled"))
                done = True
        if not done:
            print("  WARN: %s init class: decl not found" % hal, file=sys.stderr)

os.fsync(fd); os.close(fd)
print("prep-disable-vsock-hals: %d edit(s) this run (reverted: %s)"
      % (total, ', '.join(sorted(revert)) or 'none'))
PY
