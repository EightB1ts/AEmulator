#!/usr/bin/env bash
# prep-super.sh — make the Cuttlefish `super.raw.img` bootable past apexd-bootstrap.
#
# Problem: the raw downloaded `super` ships the `vendor_a` logical partition with TWO APEXes that
# share the module name `com.google.emulated.camera.provider.hal`:
#     /vendor/apex/com.google.emulated.camera.provider.hal.apex
#     /vendor/apex/com.google.emulated.camera.provider.hal.fastscenecycle.apex
# apexd-bootstrap treats a duplicate module name as fatal (reboot,bootstrap-apexd-failed).
# Cuttlefish's assemble_cvd normally prunes the unused camera variant before boot; we do it host-side.
#
# vendor_a is an **EROFS** read-only filesystem (magic 0xE0F5E1E2), so debugfs/ext tooling can't edit
# it and we can't just `rm` the file. Instead we surgically **rename the extension** of the
# fastscenecycle entry in the EROFS directory name-area (…fastscenecycle.apex → …fastscenecycle.apeX)
# — same byte length, so the EROFS dirent metadata (nid/nameoff table) is untouched — which makes
# apexd's ".apex"-suffix directory scan skip it. AVB/dm-verity is disabled, so no hashtree to fix.
# The one integrity structure that DOES cover the edit is the EROFS superblock crc32c (SB_CHKSUM),
# because the /vendor/apex dirent name happens to live inside block 0 (the superblock block); we
# recompute and rewrite it.
#
# Idempotent + repeatable: re-running detects the already-patched name and no-ops. Safe to invoke
# from run-android.sh before every boot.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, struct, sys

super_path = sys.argv[1]

# ---- crc32c (Castagnoli) — matches EROFS erofs_superblock_csum_verify ----
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

# ---- 1. Parse liblp metadata to locate the vendor_a extent inside super ----
RESERVED = 4096
GEO = 4096
fd = os.open(super_path, os.O_RDWR)
def pread(off, n): return os.pread(fd, n, off)

geo = pread(RESERVED, 4096)
assert struct.unpack('<I', geo[0:4])[0] == 0x616c4467, "bad LP geometry magic"
meta_max, slot_count, lbs = struct.unpack('<III', geo[40:52])

meta_off = RESERVED + GEO * 2                       # primary metadata, slot 0
hdr = pread(meta_off, 256)
assert struct.unpack('<I', hdr[0:4])[0] == 0x414c5030, "bad LP header magic"
header_size = struct.unpack('<I', hdr[8:12])[0]
def desc(o): return struct.unpack('<III', hdr[o:o+12])  # offset, num, entry_size
p_off, p_num, p_sz = desc(80)
e_off, e_num, e_sz = desc(92)
tables_off = meta_off + header_size

parts = {}
for i in range(p_num):
    b = pread(tables_off + p_off + i * p_sz, p_sz)
    name = b[0:36].split(b'\0')[0].decode()
    _, first_extent, num_extents, _ = struct.unpack('<IIII', b[36:52])
    parts[name] = (first_extent, num_extents)

def extent(i):
    b = pread(tables_off + e_off + i * e_sz, e_sz)
    num_sectors, target_type, target_data, target_source = struct.unpack('<QIQI', b[0:24])
    return num_sectors, target_type, target_data

first_extent, num_extents = parts['vendor_a']
assert num_extents == 1, "vendor_a is fragmented (%d extents) — extend the script" % num_extents
ns, tt, td = extent(first_extent)
FS = td * 512                                       # vendor_a EROFS start byte in super
FS_LEN = ns * 512
print("vendor_a EROFS @ byte %d (%.1f MB), %d bytes" % (FS, FS / 1e6, FS_LEN))

# ---- 2. Confirm EROFS + read block 0 ----
block0 = bytearray(pread(FS, 4096))
magic = struct.unpack('<I', block0[1024:1028])[0]
assert magic == 0xE0F5E1E2, "vendor_a is not EROFS (magic %#x)" % magic
feat_compat = struct.unpack('<I', block0[1024+8:1024+12])[0]
SB_CHKSUM = bool(feat_compat & 0x1)

# ---- 3. Find the fastscenecycle dirent name and rename its extension ----
TARGET = b'com.google.emulated.camera.provider.hal.fastscenecycle.apex'
PATCHED = b'com.google.emulated.camera.provider.hal.fastscenecycle.apeX'  # .apeX != .apex → apexd skips

# Search block 0 first (that's where THIS image keeps /vendor/apex dirents); fall back to a wider scan.
def find_in(buf, needle):
    i = buf.find(needle)
    return i

pos = find_in(block0, TARGET)
if pos >= 0:
    # edit is inside block 0 → also recompute superblock csum
    block0[pos:pos+len(TARGET)] = PATCHED
    if SB_CHKSUM:
        region = bytearray(block0[1024:4096])
        region[4:8] = b'\0\0\0\0'                    # zero csum field, then compute
        new_csum = crc32c(bytes(region))
        struct.pack_into('<I', block0, 1024+4, new_csum)
        print("recomputed EROFS sb_csum -> %#x" % new_csum)
    os.pwrite(fd, bytes(block0), FS)
    os.fsync(fd)
    print("patched fastscenecycle.apex -> .apeX in block 0; apexd will now skip it")
    os.close(fd)
    sys.exit(0)

# Already patched?
if find_in(block0, PATCHED) >= 0:
    print("already patched (fastscenecycle.apeX present) — no-op")
    os.close(fd)
    sys.exit(0)

# Wider scan across the whole vendor_a extent (in case dirents move in a re-acquired image)
CHUNK = 8 * 1024 * 1024
p = FS
found = -1
while p < FS + FS_LEN:
    n = min(CHUNK, FS + FS_LEN - p)
    buf = pread(p, n + len(TARGET))
    j = buf.find(TARGET)
    if 0 <= j < n:
        found = p + j
        break
    if find_in(buf[:n+len(PATCHED)], PATCHED) >= 0:
        print("already patched (found .apeX in extent) — no-op")
        os.close(fd)
        sys.exit(0)
    p += n
if found < 0:
    # No duplicate camera APEX to prune. The dual-ABI aosp_cf_arm64_phone image ships BOTH
    # com.google.emulated.camera.provider.hal.apex AND …fastscenecycle.apex (same module name →
    # apexd-bootstrap fatal), so this branch patches it. The 64-bit-only aosp_cf_arm64_only_phone
    # image ships only the single camera APEX (verified: 1 occurrence), so there is nothing to
    # de-dup — a clean no-op, NOT an error.
    print("no fastscenecycle.apex duplicate present — nothing to prune (no-op)")
    os.close(fd)
    sys.exit(0)

# Edit outside block 0: name-area only, no superblock csum impact (EROFS has no per-dir-block csum).
os.pwrite(fd, PATCHED, found)
os.fsync(fd)
print("patched fastscenecycle.apex -> .apeX @ byte %d (outside block 0; no csum change)" % found)
os.close(fd)
PY

# ---- Step 2: neutralize the 32-bit BoringSSL self-test's reboot_on_failure ----
# Apple Silicon (M2) has NO AArch32/EL0 execution state, so any 32-bit ARM binary yields
# execv() "Exec format error". This dual-ABI (zygote64_32) Cuttlefish image runs the 32-bit BoringSSL
# self-tests early (system `boringssl_self_test32` + `boringssl_self_test_apex32`, and the vendor
# `boringssl_self_test32_vendor`), each gated by `on ... property:ro.product.cpu.abilist32=*` and
# started with `exec_start`. The 32-bit exec fails and the service's `reboot_on_failure` reboots the
# guest BEFORE servicemanager/keystore2 come up.
#
# The `exec_start` lines live in UNCOMPRESSED rc action blocks in both the system and vendor EROFS
# (verified: found as plaintext), so we comment the 32-bit `exec_start` invocations (leading 'e' ->
# '#' — a 1-byte same-length edit; init reads `#xec_start …` as a comment and never starts the
# service). We match only invocations whose target name contains "32", leaving every 64-bit
# self-test intact. Edits are in file-data blocks, never an EROFS block 0, so no superblock csum
# recompute is needed. This lets the 64-bit daemon layer boot so the NEXT stall is revealed.
#
# NOTE: this does NOT make 32-bit apps runnable (Apple Silicon has no AArch32 at all) — the real cure
# is the arm64-only Cuttlefish target (aosp_cf_arm64_only_phone), which ships no 32-bit userspace.
# This is a boot-advance workaround to expose the 64-bit HAL/secure_env layer.
python3 - "$SUPER" <<'PY'
import os, sys
super_path = sys.argv[1]
fd = os.open(super_path, os.O_RDWR)
sz = os.lseek(fd, 0, 2)

# Match the suffix (no leading char) so BOTH the pristine `exec_start …` and an already-commented
# `#xec_start …` are found — keeps the pass idempotent and its reporting honest on re-runs.
NEEDLE = b'xec_start boringssl_self_test'
CHUNK = 32 * 1024 * 1024
offs = []
pos = 0; prev = b''
while pos < sz:
    buf = os.pread(fd, CHUNK, pos)
    if not buf: break
    hay = prev + buf; s = 0
    while True:
        i = hay.find(NEEDLE, s)
        if i < 0: break
        offs.append(pos - len(prev) + i); s = i + 1
    prev = hay[-len(NEEDLE):]
    pos += len(buf)

patched = 0; already = 0; skipped64 = 0
for o in offs:
    lead_off = o - 1                          # the 'e' of exec_start (or '#' if already commented)
    line = os.pread(fd, 60, lead_off).split(b'\n', 1)[0]
    if b'32' not in line:                     # 64-bit self-tests run fine on AArch64 — leave them
        skipped64 += 1; continue
    lead = os.pread(fd, 1, lead_off)
    if lead == b'#':
        already += 1; continue                # idempotent: already disabled
    if lead != b'e':
        continue
    os.pwrite(fd, b'#', lead_off)             # 'e' -> '#': the whole exec_start line becomes a comment
    patched += 1
    print("disabled 32-bit self-test invocation @ byte %d : %r" % (lead_off, line))

os.fsync(fd)
os.close(fd)
print("32-bit boringssl self-tests disabled (patched=%d already=%d, 64-bit left=%d)"
      % (patched, already, skipped64))
if patched == 0 and already == 0:
    print("WARN: no 32-bit boringssl exec_start found (image layout changed?)", file=sys.stderr)
PY
