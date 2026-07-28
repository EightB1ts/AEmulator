#!/usr/bin/env bash
# prep-apex-dedup.sh — resolve the DUPLICATE-MODULE apexd-bootstrap wall on the 64-bit-only image.
#
# The aosp_cf_arm64_only_phone (android16) image ships THREE vendor APEX modules with multiple
# variant files that each declare the SAME module name — apexd-bootstrap treats a duplicate module
# name as fatal (`reboot,bootstrap-apexd-failed`):
#     com.android.hardware.gatekeeper   : .nonsecure.apex + .cf_remote.apex
#     com.android.hardware.keymint      : .rust_nonsecure.apex + .rust_cf_remote.apex
#                                                             + .rust_cf_guest_trusty_nonsecure.apex
#     com.google.emulated.camera.provider.hal : .apex + .v4l2.apex
# (module names confirmed by reading each apex_manifest.pb).
#
# Cuttlefish's assemble_cvd normally disambiguates with a `ro.boot.vendor.apex.<module>=<file>`
# property, but that property name (e.g. ro.boot.vendor.apex.com.android.hardware.gatekeeper, 51 chars)
# exceeds the bootconfig→property import limit and is silently dropped — apexd never sees it (verified:
# boot still aborts on gatekeeper with the selection bootconfig set). So we PRUNE host-side, exactly
# like prep-super.sh prunes the dual-ABI image's duplicate camera APEX: rename the UNWANTED variant's
# `.apex` extension to `.apeX` in the vendor_a EROFS /vendor/apex directory name-area. apexd's
# ".apex"-suffix directory scan then skips it, leaving exactly ONE file per module. Same-length 1-byte
# edit; AVB/dm-verity is disabled so there's no hashtree to fix; the edit recomputes the EROFS
# superblock crc32c ONLY if it happens to land in vendor_a block 0 (it doesn't — /vendor/apex dirents
# live far from the superblock block).
#
# Variant kept per module = the nonsecure/software one (our secure_env is software, no trusty TEE) +
# the default (non-v4l2) camera. Idempotent: re-running detects the already-renamed `.apeX` and no-ops.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf64/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, struct, sys
super_path = sys.argv[1]

# Files to prune (rename .apex -> .apeX). Keep the nonsecure/default variant of each duplicate module.
PRUNE = [
    b'com.android.hardware.gatekeeper.cf_remote.apex',
    b'com.android.hardware.keymint.rust_cf_remote.apex',
    b'com.android.hardware.keymint.rust_cf_guest_trusty_nonsecure.apex',
    b'com.google.emulated.camera.provider.hal.v4l2.apex',
    # graphics.composer: keep .ranchu (the Cuttlefish emulated HWC our display path selects with
    # androidboot.hardware.hwcomposer=ranchu), prune .drm_hwcomposer.
    b'com.android.hardware.graphics.composer.drm_hwcomposer.apex',
]

POLY = 0x82F63B78
_tbl = []
for n in range(256):
    c = n
    for _ in range(8):
        c = (c >> 1) ^ POLY if (c & 1) else (c >> 1)
    _tbl.append(c)
def crc32c(buf, crc=0xFFFFFFFF):
    for b in buf:
        crc = (crc >> 8) ^ _tbl[(crc ^ b) & 0xFF]
    return crc & 0xFFFFFFFF

# Locate vendor_a EROFS extent via liblp metadata (mirrors prep-super.sh).
RESERVED = 4096; GEO = 4096
fd = os.open(super_path, os.O_RDWR)
def pread(off, n): return os.pread(fd, n, off)
geo = pread(RESERVED, 4096)
assert struct.unpack('<I', geo[0:4])[0] == 0x616c4467, "bad LP geometry magic"
meta_off = RESERVED + GEO * 2
hdr = pread(meta_off, 256)
assert struct.unpack('<I', hdr[0:4])[0] == 0x414c5030, "bad LP header magic"
header_size = struct.unpack('<I', hdr[8:12])[0]
def desc(o): return struct.unpack('<III', hdr[o:o+12])
p_off, p_num, p_sz = desc(80)
e_off, e_num, e_sz = desc(92)
tables_off = meta_off + header_size
parts = {}
for i in range(p_num):
    b = pread(tables_off + p_off + i * p_sz, p_sz)
    name = b[0:36].split(b'\0')[0].decode()
    _, first_extent, num_extents, _ = struct.unpack('<IIII', b[36:52])
    parts[name] = (first_extent, num_extents)
fe, ne = parts['vendor_a']
assert ne == 1, "vendor_a fragmented"
b = pread(tables_off + e_off + fe * e_sz, e_sz)
ns, tt, td, _ = struct.unpack('<QIQI', b[0:24])
FS = td * 512; FS_LEN = ns * 512

# EROFS superblock: does the fs carry a superblock crc32c (feature_compat bit0)?
block0 = pread(FS, 4096)
assert struct.unpack('<I', block0[1024:1028])[0] == 0xE0F5E1E2, "vendor_a not EROFS"
SB_CHKSUM = bool(struct.unpack('<I', block0[1032:1036])[0] & 0x1)

def find_one(needle):
    CHUNK = 16 * 1024 * 1024
    p = FS
    while p < FS + FS_LEN:
        n = min(CHUNK, FS + FS_LEN - p)
        buf = pread(p, n + len(needle))
        j = buf.find(needle)
        if 0 <= j < n:
            return p + j
        p += n
    return -1

patched = 0; already = 0; missing = 0
for name in PRUNE:
    patched_name = name[:-1] + b'X'          # ".apex" -> ".apeX"
    off = find_one(name)
    if off >= 0:
        # rename the final 'x' -> 'X'
        x_off = off + len(name) - 1
        # recompute superblock csum only if this byte is inside vendor_a block 0
        if FS <= x_off < FS + 4096 and SB_CHKSUM:
            blk = bytearray(pread(FS, 4096))
            blk[x_off - FS] = ord('X')
            region = bytearray(blk[1024:4096]); region[4:8] = b'\0\0\0\0'
            struct.pack_into('<I', blk, 1024 + 4, crc32c(bytes(region)))
            os.pwrite(fd, bytes(blk), FS)
            print("pruned %s (block 0; sb_csum recomputed)" % name.decode())
        else:
            os.pwrite(fd, b'X', x_off)
            print("pruned %s -> .apeX @ byte %d" % (name.decode(), x_off))
        patched += 1
    elif find_one(patched_name) >= 0:
        already += 1
        print("already pruned: %s" % name.decode())
    else:
        missing += 1
        print("WARN: %s not found in vendor_a (image layout changed?)" % name.decode(), file=sys.stderr)

os.fsync(fd); os.close(fd)
print("apex-dedup: pruned=%d already=%d missing=%d" % (patched, already, missing))
PY
