#!/usr/bin/env bash
# prep-vulkan-icd.sh — swap the GUEST Vulkan ICD (`vulkan.ranchu.so`) inside the Cuttlefish image.
#
# WHY: the guest-visible Vulkan extension set is not decided by the host renderer. It is decided by a
# hardcoded allow-list in the guest ICD — `allowedExtensionNames` in mesa's
# src/gfxstream/guest/vulkan_enc/ResourceTracker.cpp — which is intersected with whatever the host
# advertises before Android ever sees a name. Widening it means shipping a REBUILT
# libvulkan_gfxstream.so into the guest. Host-side work (Phase A) was worth +2 extensions; this is
# worth ~+42.
#
# WHERE THE ICD LIVES (verified with erofs_extract.py, do not guess):
#   /vendor/apex/com.google.cf.vulkan.apex        <- a real ZIP, not a flattened directory
#     └─ apex_payload.img  (STORED at zip offset 4096, 37,253,120 B, ext4 + AVB hashtree + AVBf footer)
#          └─ /lib64/hw/vulkan.ranchu.so          <- 5,218,752 B, uid/gid 1000, 0644,
#                                                    security.selinux=u:object_r:same_process_hal_file:s0
#   The .apex file itself is EROFS **FLAT_PLAIN** (contiguous, uncompressed) inside the `vendor_a`
#   logical partition, so its bytes can be rewritten in place exactly like every other prep-*.sh edit.
#
# WHY THIS IS SAFE — the four integrity structures, and why none of them bite:
#   1. Partition dm-verity: OFF. run-android64.sh boots with vbmeta VERIFICATION_DISABLED and
#      androidboot.veritymode=disabled, so `vendor_a` has no enforced hashtree (that is what lets
#      prep-super.sh / prep-apex-dedup.sh byte-patch this partition at all).
#   2. APEX payload dm-verity: NOT USED for pre-installed APEXes. Boot evidence: every one of the 85
#      activated APEXes mounts on a bare loop device —
#        `EXT4-fs (loop23): mounted filesystem 7d1522e1-… ro without journal`
#      — and the only device-mapper devices in the whole boot are dm-0..dm-8, all created by
#      libfs_mgr for the liblp logical partitions. There is no `dm-verity` target anywhere, so the
#      hashtree's root digest is never checked against the data. We therefore leave the hashtree,
#      the vbmeta struct and the AVBf footer byte-identical and untouched; apexd's signature-only
#      `VerifyApexVerity` still passes.
#   3. APEX container APK v2/v3 signature: present (APK Sig Block 42 @ 37,384,192 with pair ids
#      0x7109871a/v2, 0xf05368c0/v3, 0x42726577/verity-padding) but NOT enforced on this path. apexd
#      contains no APK-signature-scheme code at all. system_server scans pre-installed APEXes through
#      ScanPackageUtils.collectCertificatesLI(..., skipVerify=scanSystemPartition), which for a
#      /vendor/apex module calls ApkSignatureVerifier.unsafeGetCertsWithoutVerification() — that reads
#      the certificate chain out of the signing block but never re-digests the file contents. The
#      signing block itself is left byte-identical here, so the cert chain still parses.
#      (The one place the container signature IS enforced — PackageSessionVerifier.checkApexSignature,
#      "APK container signature of APEX package … is not compatible" — only runs for staged
#      PackageInstaller sessions, which we never create.)
#   4. ZIP CRC-32 of apex_payload.img: nobody checks it (apexd only reads the entry's offset/length and
#      loop-mounts it), but we recompute and rewrite it anyway — in the local file header AND in the
#      central directory — so `unzip -t` on the extracted APEX stays clean and a corrupted write is
#      caught by tooling instead of by the guest.
#
# WHY NOT the obvious alternative (shadow the ICD at /vendor/lib64/hw/vulkan.ranchu.so): it cannot
# work, for two independent reasons.
#   a) libvulkan never looks there. `/apex/com.google.cf.vulkan/etc/com.google.cf.vulkan.rc` runs
#      `on init: setprop ro.vulkan.apex com.google.cf.vulkan`, and /system/lib64/libvulkan.so's
#      LoadBuiltinDriver logs "Loading builtin Vulkan driver from APEX: ro.vulkan.apex=%s" and then
#      resolves the driver ONLY inside that APEX's exported linker namespace (the payload ships
#      etc/linker.config.pb = `10 01`, i.e. visible=true). The sphal namespace — the one that would
#      reach /vendor/lib64/hw — is the *else* branch, taken only when ro.vulkan.apex is empty; and
#      `ro.` properties are write-once, so it cannot be pre-empted from the kernel cmdline
#      (androidboot.X becomes ro.boot.X, never ro.X).
#   b) There is no vulkan.*.so in /vendor/lib64/hw to overwrite (it holds exactly 5 files), and
#      vendor_a is EROFS: adding a file needs a full partition repack, which is precisely the
#      "heavier, boot-risky repack pass" the rest of this directory exists to avoid.
#
# SIZE BUDGET: the payload ext4 is 9021 blocks x 4 KiB with 8 free, so the replacement ICD must fit in
# (free blocks + the blocks freed by deleting the old ICD) = 1280 blocks = 5,242,880 B. The script
# computes this from the image and refuses to write if the new ICD does not fit. `--drop-pastel` also
# deletes /lib64/hw/vulkan.pastel.so (28,763,400 B, the SwiftShader driver) to buy ~28 MB more — that
# breaks `SWRENDER=1 ./run-android64.sh`, so it is opt-in and never the default.
#
# IDEMPOTENT: the ICD currently inside the payload is SHA-256'd and compared with the source; an
# already-swapped image is a no-op. The pristine ICD is saved next to the image as
# `vulkan.ranchu.so.orig` on the first run, so `--revert` restores the stock driver exactly.
# `--revert` does NOT undo `--drop-pastel`: deleting vulkan.pastel.so is one-way, and getting it back
# means re-deriving super.raw.img from super.img and replaying every prep-*.sh.
#
# USAGE
#   ./prep-vulkan-icd.sh <super.raw.img> --icd=<path/to/libvulkan_gfxstream.so>
#   ./prep-vulkan-icd.sh <super.raw.img> --icd=<...> --drop-pastel   # free 28 MB, kills SWRENDER=1
#   ./prep-vulkan-icd.sh <super.raw.img> --revert                    # restore vulkan.ranchu.so.orig
#   ./prep-vulkan-icd.sh <super.raw.img> --verify                    # report what is installed, no write
#   AEMU_VULKAN_ICD=<path> ./prep-vulkan-icd.sh <super.raw.img>      # same as --icd=
#
# HOST DEPS: python3 (stdlib only) + e2fsprogs (`brew install e2fsprogs`) for debugfs/fsck.ext4 — the
# same dependency prep-data-unencrypted.sh already takes for mke2fs.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf64/img/super.raw.img}"
ICD="${AEMU_VULKAN_ICD-}"
MODE="install"
DROP_PASTEL=0
FORCE=0
for a in "$@"; do
  case "$a" in
    --icd=*)       ICD="${a#--icd=}" ;;
    --icd)         echo "error: --icd needs a path, e.g. --icd=/path/to/libvulkan_gfxstream.so" >&2
                   exit 2 ;;
    --revert)      MODE="revert" ;;
    --verify)      MODE="verify" ;;
    --drop-pastel) DROP_PASTEL=1 ;;
    --force)       FORCE=1 ;;
  esac
done

# e2fsprogs is keg-only under Homebrew, so fall back to its Cellar sbin (same as prep-data-unencrypted.sh).
E2SBIN=/opt/homebrew/opt/e2fsprogs/sbin
DEBUGFS="$(command -v debugfs || echo $E2SBIN/debugfs)"
FSCK="$(command -v fsck.ext4 || echo $E2SBIN/fsck.ext4)"
for t in "$DEBUGFS" "$FSCK"; do
  [ -x "$t" ] || { echo "ERROR: $t not found (install e2fsprogs: brew install e2fsprogs)" >&2; exit 3; }
done
[ -f "$SUPER" ] || { echo "ERROR: no super image at $SUPER" >&2; exit 3; }

if [ "$MODE" = "install" ] && [ -z "$ICD" ]; then
  echo "ERROR: no ICD given. Pass --icd=<path to the rebuilt libvulkan_gfxstream.so> (or set" >&2
  echo "       AEMU_VULKAN_ICD). Use --revert to restore the stock driver, --verify to inspect." >&2
  exit 2
fi

python3 - "$SUPER" "$ICD" "$MODE" "$DEBUGFS" "$FSCK" "$DROP_PASTEL" "$FORCE" <<'PY'
import hashlib, os, shutil, struct, subprocess, sys, tempfile, zlib

super_path, icd_path, mode, DEBUGFS, FSCK, drop_pastel, force = (
    sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5],
    sys.argv[6] == '1', sys.argv[7] == '1')

APEX_PATH   = '/apex/com.google.cf.vulkan.apex'      # inside the vendor_a EROFS
ICD_IN_APEX = '/lib64/hw/vulkan.ranchu.so'           # inside the APEX ext4 payload
PASTEL      = '/lib64/hw/vulkan.pastel.so'
PAYLOAD     = 'apex_payload.img'                     # the STORED zip entry we rewrite

def die(msg):
    print("ERROR: %s" % msg, file=sys.stderr)
    sys.exit(4)

# ---- 1. locate the vendor_a EROFS extent via liblp metadata (mirrors prep-apex-dedup.sh) ----------
RESERVED = 4096; GEO = 4096
fd = os.open(super_path, os.O_RDWR)
def pread(off, n): return os.pread(fd, n, off)
geo = pread(RESERVED, 4096)
if struct.unpack('<I', geo[0:4])[0] != 0x616c4467: die("bad LP geometry magic in %s" % super_path)
meta_off = RESERVED + GEO * 2
hdr = pread(meta_off, 256)
if struct.unpack('<I', hdr[0:4])[0] != 0x414c5030: die("bad LP header magic")
header_size = struct.unpack('<I', hdr[8:12])[0]
def desc(o): return struct.unpack('<III', hdr[o:o+12])
p_off, p_num, p_sz = desc(80)
e_off, e_num, e_sz = desc(92)
tables_off = meta_off + header_size
parts = {}
for i in range(p_num):
    b = pread(tables_off + p_off + i * p_sz, p_sz)
    nm = b[0:36].split(b'\0')[0].decode()
    _, fe, ne, _ = struct.unpack('<IIII', b[36:52])
    parts[nm] = (fe, ne)
if 'vendor_a' not in parts: die("no vendor_a in super (%s)" % sorted(parts))
fe, ne = parts['vendor_a']
if ne != 1: die("vendor_a is fragmented (%d extents) — extend this script" % ne)
b = pread(tables_off + e_off + fe * e_sz, e_sz)
ns, tt, td, _ = struct.unpack('<QIQI', b[0:24])
VFS, VLEN = td * 512, ns * 512

# ---- 2. walk the vendor_a EROFS to the .apex inode; require FLAT_PLAIN (contiguous, uncompressed) --
sb = pread(VFS + 1024, 128)
if struct.unpack('<I', sb[0:4])[0] != 0xE0F5E1E2: die("vendor_a is not EROFS")
BLKSZ = 1 << sb[12]
ROOT_NID = struct.unpack('<H', sb[14:16])[0]
META = struct.unpack('<I', sb[40:44])[0]

def inode(nid):
    io = META * BLKSZ + nid * 32
    raw = pread(VFS + io, 64)
    fmt = struct.unpack('<H', raw[0:2])[0]
    extended = (fmt & 1) == 1
    dl = (fmt >> 1) & 7
    xattr_icount = struct.unpack('<H', raw[2:4])[0]
    mode = struct.unpack('<H', raw[4:6])[0]
    size = struct.unpack('<Q', raw[8:16])[0] if extended else struct.unpack('<I', raw[8:12])[0]
    raw_blk = struct.unpack('<I', raw[16:20])[0]
    isz = 64 if extended else 32
    xsz = (12 + (xattr_icount - 1) * 4) if xattr_icount else 0
    return dict(nid=nid, dl=dl, mode=mode, size=size, raw_blk=raw_blk, inline_off=io + isz + xsz)

def read_ino(ino, n):
    n = min(n, ino['size'])
    if ino['dl'] == 0:
        return pread(VFS + ino['raw_blk'] * BLKSZ, n)
    if ino['dl'] == 2:
        full = ino['size'] // BLKSZ * BLKSZ
        if n <= full: return pread(VFS + ino['raw_blk'] * BLKSZ, n)
        out = pread(VFS + ino['raw_blk'] * BLKSZ, full) if full else b''
        return (out + pread(VFS + ino['inline_off'], ino['size'] - full))[:n]
    die("EROFS inode nid=%d is compressed (datalayout %d) — cannot patch in place" % (ino['nid'], ino['dl']))

def listdir(ino):
    data = read_ino(ino, ino['size'])
    out = []
    for blk in range((ino['size'] + BLKSZ - 1) // BLKSZ):
        block = data[blk * BLKSZ:(blk + 1) * BLKSZ]
        if len(block) < 12: break
        count = struct.unpack('<H', block[8:10])[0] // 12
        for k in range(count):
            de = block[k * 12:(k + 1) * 12]
            nid = struct.unpack('<Q', de[0:8])[0]
            noff = struct.unpack('<H', de[8:10])[0]
            end = struct.unpack('<H', block[(k+1)*12+8:(k+1)*12+10])[0] if k + 1 < count else None
            name = (block[noff:] if end is None else block[noff:end]).split(b'\0')[0]
            if name: out.append((name.decode('utf-8', 'replace'), nid))
    return out

cur = inode(ROOT_NID)
for comp in [c for c in APEX_PATH.strip('/').split('/') if c]:
    nxt = next((nid for nm, nid in listdir(cur) if nm == comp), None)
    if nxt is None: die("%s not found in vendor_a (image layout changed?)" % APEX_PATH)
    cur = inode(nxt)
if cur['dl'] != 0:
    die("%s is EROFS datalayout %d, expected FLAT_PLAIN(0) — in-place rewrite is not possible"
        % (APEX_PATH, cur['dl']))
APEX_OFF, APEX_LEN = VFS + cur['raw_blk'] * BLKSZ, cur['size']
if pread(APEX_OFF, 4) != b'PK\x03\x04': die("%s does not start with a ZIP local header" % APEX_PATH)

# ---- 3. find the STORED apex_payload.img entry + both CRC-32 fields (local header + central dir) ---
tail = pread(APEX_OFF + max(0, APEX_LEN - 70000), min(70000, APEX_LEN))
e = tail.rfind(b'PK\x05\x06')
if e < 0: die("no ZIP end-of-central-directory in %s" % APEX_PATH)
eocd = APEX_LEN - len(tail) + e
n_ent, cd_sz, cd_off = struct.unpack('<HII', pread(APEX_OFF + eocd + 10, 10))
o, ent = cd_off, None
for _ in range(n_ent):
    cdh = pread(APEX_OFF + o, 46)
    if cdh[0:4] != b'PK\x01\x02': die("corrupt central directory at %d" % o)
    (method, crc, csz, usz, nlen, elen, clen, lfh) = struct.unpack('<H4xIIIHHH8xI', cdh[10:46])
    name = pread(APEX_OFF + o + 46, nlen).decode('utf-8', 'replace')
    if name == PAYLOAD:
        ent = dict(cd=o, method=method, crc=crc, csz=csz, usz=usz, lfh=lfh)
        break
    o += 46 + nlen + elen + clen
if ent is None: die("%s has no %s entry" % (APEX_PATH, PAYLOAD))
if ent['method'] != 0: die("%s is DEFLATEd (method %d); apexd requires STORED" % (PAYLOAD, ent['method']))
lfh = pread(APEX_OFF + ent['lfh'], 30)
lnlen, lelen = struct.unpack('<HH', lfh[26:30])
PAY_OFF = ent['lfh'] + 30 + lnlen + lelen          # byte offset of the payload WITHIN the .apex
PAY_LEN = ent['csz']
if PAY_OFF % 4096: die("%s is not 4 KiB aligned inside the APEX (%d)" % (PAYLOAD, PAY_OFF))
if PAY_OFF + PAY_LEN > APEX_LEN: die("%s runs past the end of the APEX" % PAYLOAD)
LFH_CRC_ABS = APEX_OFF + ent['lfh'] + 14
CD_CRC_ABS  = APEX_OFF + ent['cd'] + 16

print("vendor_a EROFS @ %d; %s @ %d (%d B, FLAT_PLAIN); %s @ +%d (%d B, STORED)"
      % (VFS, APEX_PATH, APEX_OFF, APEX_LEN, PAYLOAD, PAY_OFF, PAY_LEN))

# ---- 4. pull the payload out to a scratch file --------------------------------------------------
tmp = tempfile.mkdtemp(prefix='prep-vulkan-icd.')
img = os.path.join(tmp, PAYLOAD)
try:
    with open(img, 'wb') as f:
        done = 0
        while done < PAY_LEN:
            chunk = pread(APEX_OFF + PAY_OFF + done, min(8 << 20, PAY_LEN - done))
            if not chunk: die("short read of %s" % PAYLOAD)
            f.write(chunk); done += len(chunk)
    if struct.unpack('<H', open(img, 'rb').read(0x43a)[0x438:0x43a])[0] != 0xEF53:
        die("%s is not ext4" % PAYLOAD)
    with open(img, 'rb') as f:
        f.seek(-64, os.SEEK_END)
        if f.read(4) != b'AVBf': die("%s has no AVB footer — refusing to touch it" % PAYLOAD)

    def dbg(script, write=False):
        cmd = [DEBUGFS] + (['-w'] if write else []) + ['-f', '/dev/stdin', img]
        p = subprocess.run(cmd, input=script, capture_output=True, text=True)
        return p.stdout + p.stderr

    def dump(path, out):
        dbg("dump %s %s\nquit\n" % (path, out))
        return os.path.exists(out) and os.path.getsize(out) > 0

    def sha(p):
        h = hashlib.sha256()
        with open(p, 'rb') as f:
            for b in iter(lambda: f.read(1 << 20), b''): h.update(b)
        return h.hexdigest()

    # ---- 5. what is installed right now? --------------------------------------------------------
    cur_icd = os.path.join(tmp, 'current.so')
    if not dump(ICD_IN_APEX, cur_icd):
        die("%s is missing from %s — image layout changed?" % (ICD_IN_APEX, PAYLOAD))
    cur_sha, cur_len = sha(cur_icd), os.path.getsize(cur_icd)
    backup = os.path.join(os.path.dirname(os.path.abspath(super_path)), 'vulkan.ranchu.so.orig')
    print("installed ICD: %d bytes  sha256=%s" % (cur_len, cur_sha))
    print("stock backup : %s" % (backup if os.path.exists(backup) else "(none yet)"))

    if mode == 'verify':
        st = dbg("stat %s\nquit\n" % ICD_IN_APEX)
        print(st.strip())
        sys.exit(0)

    if mode == 'revert':
        if not os.path.exists(backup):
            die("no %s — nothing to revert to (this image was never patched by this script)" % backup)
        src = backup
    else:
        src = icd_path
    if not os.path.isfile(src): die("no ICD at %s" % src)

    # ---- 6. sanity-check the replacement: arm64 ET_DYN, SONAME + DT_NEEDED --------------------
    blob = open(src, 'rb').read()
    if blob[:4] != b'\x7fELF' or blob[4] != 2 or blob[5] != 1:
        die("%s is not a little-endian 64-bit ELF" % src)
    e_type, e_machine = struct.unpack('<HH', blob[16:20])
    if e_machine != 0xB7: die("%s is e_machine %#x, expected 0xb7 (AArch64)" % (src, e_machine))
    if e_type != 3: die("%s is e_type %d, expected 3 (ET_DYN / shared object)" % (src, e_type))
    e_phoff, = struct.unpack('<Q', blob[32:40])
    e_phentsize, e_phnum = struct.unpack('<HH', blob[54:58])
    loads, dyn = [], None
    for i in range(e_phnum):
        ph = blob[e_phoff + i * e_phentsize:e_phoff + (i + 1) * e_phentsize]
        p_type, = struct.unpack('<I', ph[0:4])
        p_offset, p_vaddr, _, p_filesz = struct.unpack('<QQQQ', ph[8:40])
        if p_type == 1: loads.append((p_vaddr, p_filesz, p_offset))
        if p_type == 2: dyn = (p_offset, p_filesz)
    if dyn is None: die("%s has no PT_DYNAMIC" % src)
    def v2o(v):
        for va, sz, off in loads:
            if va <= v < va + sz: return off + (v - va)
        die("%s: unmappable vaddr %#x" % (src, v))
    tags, o2 = [], dyn[0]
    while o2 < dyn[0] + dyn[1]:
        t, v = struct.unpack('<qQ', blob[o2:o2 + 16]); o2 += 16
        if t == 0: break
        tags.append((t, v))
    strtab = next((v for t, v in tags if t == 5), None)
    if strtab is None: die("%s has no DT_STRTAB" % src)
    sbase = v2o(strtab)
    def dstr(off): return blob[sbase + off:blob.index(b'\0', sbase + off)].decode()
    soname = next((dstr(v) for t, v in tags if t == 14), None)
    needed = [dstr(v) for t, v in tags if t == 1]
    if soname != 'vulkan.ranchu.so':
        die("%s has SONAME %r; the Android Vulkan loader dlopen()s \"vulkan.ranchu.so\" by basename "
            "inside the APEX namespace, so the SONAME must match exactly. Fix it with:\n"
            "      patchelf --set-soname vulkan.ranchu.so %s" % (src, soname, src))
    bundled = set()
    for line in dbg("ls -l /lib64\nquit\n").splitlines():
        f2 = line.split()
        if f2 and f2[-1].endswith('.so'): bundled.add(f2[-1])
    # Declared in the APEX's apex_manifest.pb `requireNativeLibs` — the only platform libs the APEX
    # linker namespace can reach. Anything else in DT_NEEDED fails dlopen() at runtime.
    platform = {'libapexsupport.so', 'libc.so', 'libdl.so', 'liblog.so', 'libm.so',
                'libnativewindow.so', 'libsync.so', 'libvndksupport.so'}
    missing = [n for n in needed if n not in bundled and n not in platform]
    if missing and not force:
        die("%s needs %s, which the com.google.cf.vulkan namespace cannot provide "
            "(bundled: %s; platform: %s). dlopen() would fail at boot. Re-link, or pass --force."
            % (src, ', '.join(missing), ', '.join(sorted(bundled)), ', '.join(sorted(platform))))
    print("replacement  : %d bytes  sha256=%s  soname=%s  needed=%s"
          % (len(blob), sha(src), soname, ' '.join(needed)))

    # ---- 7. idempotency ------------------------------------------------------------------------
    if sha(src) == cur_sha:
        print("prep-vulkan-icd: already installed (identical sha256) — no-op")
        sys.exit(0)

    # ---- 8. capture the pristine ICD before the first overwrite --------------------------------
    if not os.path.exists(backup):
        shutil.copyfile(cur_icd, backup)
        print("saved stock ICD -> %s" % backup)

    # ---- 9. swap the file inside the ext4 payload ----------------------------------------------
    sel = os.path.join(tmp, 'selabel.bin')
    dbg("ea_get -f %s %s security.selinux\nquit\n" % (sel, ICD_IN_APEX))
    if not os.path.exists(sel) or os.path.getsize(sel) == 0:
        die("could not read security.selinux off %s" % ICD_IN_APEX)
    uid = gid = 1000; imode = 0o100644
    for line in dbg("stat %s\nquit\n" % ICD_IN_APEX).splitlines():
        if line.startswith('User:'):
            f2 = line.split(); uid, gid = int(f2[1]), int(f2[3])

    def free_bytes():
        # `dumpe2fs -h` is not always installed under the same name; debugfs's own `show_super_stats`
        # reports the same counters and is always there.
        for line in dbg("show_super_stats -h\nquit\n").splitlines():
            if 'Free blocks:' in line: return int(line.split(':')[1].strip().split()[0]) * 4096
        die("could not read the payload's free-block count")

    script = "rm %s\n" % ICD_IN_APEX
    if drop_pastel:
        script += "rm %s\n" % PASTEL
        print("WARNING: --drop-pastel deletes %s. `SWRENDER=1 ./run-android64.sh` sets "
              "androidboot.hardware.vulkan=pastel and will lose its driver. This is NOT undone by "
              "--revert; recovering it means re-deriving super.raw.img from super.img." % PASTEL,
              file=sys.stderr)
    script += "quit\n"
    dbg(script, write=True)
    avail = free_bytes()
    print("free in payload after removing the old ICD%s: %d bytes"
          % (" + vulkan.pastel.so" if drop_pastel else "", avail))
    if len(blob) > avail:
        die("the replacement ICD is %d bytes but only %d are free in the APEX payload. The payload "
            "is a fixed-size ext4 (its length is pinned by the surrounding APEX/EROFS layout), so it "
            "cannot grow. Build with -Dbuildtype=release -Db_ndebug=true and strip, or re-run with "
            "--drop-pastel (frees ~28 MB by deleting the SwiftShader driver — breaks SWRENDER=1)."
            % (len(blob), avail))

    dbg("cd /lib64/hw\n"
        "write %s vulkan.ranchu.so\n"
        "sif vulkan.ranchu.so mode 0%o\n"
        "sif vulkan.ranchu.so uid %d\n"
        "sif vulkan.ranchu.so gid %d\n"
        "sif vulkan.ranchu.so atime 0\n"
        "sif vulkan.ranchu.so mtime 0\n"
        "sif vulkan.ranchu.so ctime 0\n"
        "sif vulkan.ranchu.so crtime 0\n"
        "ea_set -f %s vulkan.ranchu.so security.selinux\n"
        "quit\n" % (os.path.abspath(src), imode, uid, gid, sel), write=True)

    # ---- 10. verify the ext4 before it goes anywhere near the image -----------------------------
    back = os.path.join(tmp, 'roundtrip.so')
    if not dump(ICD_IN_APEX, back): die("the new %s did not land in the payload" % ICD_IN_APEX)
    if sha(back) != sha(src): die("round-trip mismatch: the payload's ICD is not the file we wrote")
    st = dbg("stat %s\nquit\n" % ICD_IN_APEX)
    if 'same_process_hal_file' not in st:
        die("security.selinux did not stick on %s:\n%s" % (ICD_IN_APEX, st))
    p = subprocess.run([FSCK, '-fn', img], capture_output=True, text=True)
    if p.returncode != 0:
        die("fsck.ext4 rejects the patched payload (rc=%d):\n%s%s" % (p.returncode, p.stdout, p.stderr))
    if os.path.getsize(img) != PAY_LEN: die("payload changed length (%d != %d)" % (os.path.getsize(img), PAY_LEN))
    with open(img, 'rb') as f:
        f.seek(-64, os.SEEK_END)
        if f.read(4) != b'AVBf': die("AVB footer lost")

    # ---- 11. write the payload back + fix the ZIP CRC-32 in BOTH headers -----------------------
    crc = 0
    with open(img, 'rb') as f:
        off = 0
        while True:
            b2 = f.read(8 << 20)
            if not b2: break
            crc = zlib.crc32(b2, crc)
            os.pwrite(fd, b2, APEX_OFF + PAY_OFF + off); off += len(b2)
    crc &= 0xFFFFFFFF
    os.pwrite(fd, struct.pack('<I', crc), LFH_CRC_ABS)
    os.pwrite(fd, struct.pack('<I', crc), CD_CRC_ABS)
    os.fsync(fd)
    print("rewrote %s in place (%d bytes) and set its ZIP CRC-32 -> %#010x (local hdr @%d, central "
          "dir @%d)" % (PAYLOAD, PAY_LEN, crc, LFH_CRC_ABS, CD_CRC_ABS))
    print("prep-vulkan-icd: %s -> /apex/com.google.cf.vulkan%s (%d bytes)"
          % ("stock ICD restored" if mode == 'revert' else os.path.basename(src),
             ICD_IN_APEX, len(blob)))
finally:
    os.close(fd)
    shutil.rmtree(tmp, ignore_errors=True)
PY
