#!/usr/bin/env bash
# prep-data-unencrypted.sh — mount /data as PLAIN ext4 so boot clears the vold crypto wall.
#
# Problem (pass 4 stall): the Cuttlefish `/data` fstab entry is metadata-encrypted (dm-default-key)
# + file-based-encrypted (FBE):
#   /dev/block/by-name/userdata /data ext4 ... latemount,wait,check,quota,formattable,\
#       keydirectory=/metadata/vold/metadata_encryption,fileencryption=aes-256-xts:aes-256-cts,checkpoint=block
# Establishing those keys needs REAL KeyMint key ops (generateKey/importKey + keyblob + AES/HKDF) that
# our minimal software secure_env does not implement, so:
#   vold: read_key failed in mountFstab   ->   reboot: recovery (init_user0_failed)
#
# Fast-path fix (this pass): neutralize the three crypto fs_mgr flags so fs_mgr/vold treat /data as a
# plain formattable ext4 partition. We corrupt the FIRST letter of each flag keyword
# (keydirectory / fileencryption / checkpoint -> xeydirectory / xileencryption / xheckpoint). fs_mgr
# parses each as an *unknown flag* (LWARNING "unknown flag", then continues), so metadata-encryption +
# FBE + fs-checkpointing are all dropped, while `latemount,wait,check,quota,formattable` survive. On
# first boot the mount of the empty userdata image fails -> `formattable` makes fs_mgr mkfs a fresh
# ext4 -> /data mounts RW -> init_user0 sets up an UNENCRYPTED user 0 -> boot advances past vold.
#
# The /data line lives (4 slot variants: ext4.cts/ext4.hctr2/f2fs.cts/f2fs.hctr2) inside the
# **vendor_a EROFS** logical partition of super.raw.img; `androidboot.fstab_suffix=cf.ext4.cts` selects
# the ext4.cts one, and second-stage vold reads the SAME vendor copy via fs_mgr_read_fstab_default.
# The edit is SAME-LENGTH (1 byte per keyword) and lands in EROFS file-data blocks far from vendor_a's
# block-0 superblock, so there is NO EROFS superblock crc to recompute (same reasoning as prep-super.sh
# step 2). Edits are scoped to the `/dev/block/by-name/userdata /data` lines only, so the other 8
# `keydirectory=` / 12 `checkpoint=` occurrences elsewhere in super are untouched.
#
# Idempotent + repeatable: re-running detects the already-patched keyword and no-ops.
# Real FBE / metadata-encryption via a full KeyMint TA is a later hardening pass.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, sys

super_path = sys.argv[1]
fd = os.open(super_path, os.O_RDWR)
sz = os.lseek(fd, 0, 2)

LINE_MARK = b'/dev/block/by-name/userdata /data'
# (keyword to neutralize, replacement first byte). Corrupting the leading char is enough for fs_mgr to
# fail the keyword match and skip the flag; the trailing `=<value>` rides along as an ignored token.
KEYS = [(b'keydirectory=',  b'x'),
        (b'fileencryption=', b'x'),
        (b'checkpoint=',     b'x')]

# Scan the whole image for the /data line marker (there are 4 — one per fstab slot variant).
CHUNK = 32 * 1024 * 1024
offs = []
pos = 0; prev = b''
while pos < sz:
    buf = os.pread(fd, CHUNK, pos)
    if not buf:
        break
    hay = prev + buf; s = 0
    while True:
        i = hay.find(LINE_MARK, s)
        if i < 0:
            break
        offs.append(pos - len(prev) + i); s = i + 1
    prev = hay[-len(LINE_MARK):]
    pos += len(buf)

if not offs:
    print("ERROR: /data fstab line not found in super (image layout changed?)", file=sys.stderr)
    os.close(fd); sys.exit(2)

patched_lines = 0; already_lines = 0
for lo in offs:
    line = os.pread(fd, 400, lo).split(b'\n', 1)[0]   # the whole fstab /data line
    changed = False; already = False
    for kw, repl in KEYS:
        k = line.find(kw)
        if k >= 0:
            os.pwrite(fd, repl, lo + k)                # same-length single-byte corruption
            changed = True
        else:
            # already neutralized? (leading char replaced -> keyword-minus-first-char present)
            if line.find(kw[1:]) >= 0 and line.find(b'x' + kw[1:]) >= 0:
                already = True
    if changed:
        patched_lines += 1
        print("patched /data crypto flags @ byte %d" % lo)
    elif already:
        already_lines += 1

os.fsync(fd); os.close(fd)
print("/data unencrypt: patched=%d already=%d line(s) (of %d /data fstab copies)"
      % (patched_lines, already_lines, len(offs)))
PY

# ---- Step 2: pre-format userdata.img as a valid empty ext4 ----------------------------------------
# With the crypto flags gone, /data is a plain `formattable` ext4 entry — BUT init's `mount_all --late`
# path does NOT auto-mkfs a wiped partition (it returns FS_MGR_MNTALL_FAIL / "Invalid ext4 superblock
# on userdata" -> "queue_fs_event() failed: Invalid code: -1"), so /data never mounts and stays RO
# ("init_user0_failed", "/data: Read-only file system"). Fix: lay down a real empty ext4 host-side so
# the very first mount succeeds. Idempotent — skips if userdata already carries an ext4 superblock.
#
# `orphan_file` (default in e2fsprogs >= 1.47) is disabled: it is a newer on-disk feature and keeping
# the feature set conservative avoids an "unsupported feature" mount refusal on the guest GKI kernel.
UD="$(dirname "$SUPER")/userdata.img"
UD_BYTES=$((6 * 1024 * 1024 * 1024))     # match the harness ensure_size() 6 GiB /data
MKE2FS="$(command -v mke2fs || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)"

has_ext4() {
  # ext4 magic 0xEF53 (LE) lives at byte 0x438
  python3 - "$UD" <<'PY'
import os,sys
p=sys.argv[1]
try:
    fd=os.open(p,os.O_RDONLY); m=os.pread(fd,2,0x438); os.close(fd)
    sys.exit(0 if m==b'\x53\xef' else 1)
except FileNotFoundError:
    sys.exit(1)
PY
}

if has_ext4; then
  echo "userdata.img already carries an ext4 superblock — leaving /data intact (no reformat)"
else
  if [ ! -x "$MKE2FS" ]; then
    echo "ERROR: mke2fs not found (install e2fsprogs: brew install e2fsprogs)" >&2
    exit 3
  fi
  # Grow to the full /data size FIRST so the fs spans the whole partition (never shrink existing data).
  python3 - "$UD" "$UD_BYTES" <<'PY'
import os,sys
p,sz=sys.argv[1],int(sys.argv[2])
if not os.path.exists(p): open(p,'wb').close()
cur=os.path.getsize(p)
if cur<sz:
    with open(p,'r+b') as f: f.truncate(sz)
    print("grew userdata.img -> %d MiB" % (sz//(1024*1024)))
PY
  echo "formatting userdata.img as ext4 (empty /data) via $MKE2FS ..."
  "$MKE2FS" -F -q -t ext4 -b 4096 -O ^orphan_file -m 0 -L data "$UD"
  echo "userdata.img formatted ext4 (6 GiB, empty /data)"
fi

