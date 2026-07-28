#!/usr/bin/env bash
# prep-disable-vsock-hals.sh — neutralize the crashing vsock-dependent Cuttlefish HALs so
# system_server does not block/starve on them and boot reaches the launcher.
#
# ROOT CAUSE (pass-10 diagnosis, verified from logcat on hvc2 in the SWRENDER --display boot):
#   The 64-bit-only image ships three Cuttlefish HALs that hard-depend on a HOST cuttlefish backend
#   reachable over virtio-vsock. aemulator-hvf presents NO virtio-vsock device (and there is no host
#   sensors/config daemon), so each HAL aborts at startup and init respawns it forever:
#       vendor.light-cuttlefish      android.hardware.light  ILights/default  (APEX com.google.cf.light)
#       vendor.sensors-hal-multihal  android.hardware.sensors ISensors/default (vendor /vendor/bin/hw;
#                                      impl @2.1-impl.cuttlefish: "Could not connect to sensors control")
#       vendor.threadnetwork_hal     android.hardware.threadnetwork IThreadChip/chip0
#                                      ("Check failed: node_id > 0 (node_id=0)" — needs a vsock CID)
#   (a) Because each interface is DECLARED in the vendor VINTF manifest, system_server framework
#       services (LightsService in startBootstrapServices; SensorService later) call
#       ServiceManager.waitForDeclaredService(...) and BLOCK on the never-registered HAL until the
#       Watchdog kills system_server — boot_completed never fires. (b) Even once undeclared, the init
#       services still fork+exec+ART-abort every ~5s, firing `sys.init.updatable_crashing` health
#       checks and flooding the audit log, which STARVES system_server so it crawls at phase 500.
#
# FIX (three surgical, idempotent, same-length host edits to super.raw.img; verity disabled so no
# hashtree; EROFS file-data edits land far from block-0 superblocks so no csum recompute — the
# prep-super.sh / prep-apex-dedup.sh techniques):
#   1. VINTF undeclare — corrupt each HAL's <name> in the vendor manifest so waitForDeclaredService
#      returns null immediately (LightsService / SensorService tolerate an absent HAL and boot fine):
#          <name>android.hardware.light</name>          -> ...lighX
#          <name>android.hardware.sensors</name>        -> ...sensorX
#          <name>android.hardware.threadnetwork</name>  -> ...threadnetworX
#   2. sensors init service (vendor_a /vendor/etc/init rc) `class hal` -> `class hxl` (a class no
#      `class_start` ever names) so the sensors HAL binary is never launched -> zero crash churn.
#   3. light + threadnetwork are APEX HALs; disable each APEX by renaming its /vendor/apex dirent
#      `.apex` -> `.apeX` (apexd's suffix scan skips it). Their in-APEX .rc vanishes with the APEX, so
#      the service is never even defined -> zero crash churn, and their VINTF fragment (inside the
#      APEX) auto-undeclares too. apexd tolerates the absent optional module (no bootstrap failure).
#
# Idempotent + repeatable: re-running detects already-patched bytes and no-ops.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf64/img/super.raw.img}"

python3 - "$SUPER" <<'PY'
import os, struct, sys
super_path = sys.argv[1]
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

# ---- 1. VINTF undeclare (whole-super search) -------------------------------------------------------
HALS = [
    (b'<name>android.hardware.light</name>',         b'<name>android.hardware.lighX</name>'),
    (b'<name>android.hardware.sensors</name>',       b'<name>android.hardware.sensorX</name>'),
    (b'<name>android.hardware.threadnetwork</name>', b'<name>android.hardware.threadnetworX</name>'),
]
n1=0
for orig, patched in HALS:
    fi=[k for k in range(len(orig)) if orig[k]!=patched[k]][0]
    offs=find_all(orig)
    if offs:
        for base in offs:
            os.pwrite(fd, patched[fi:fi+1], base+fi); n1+=1
        print("VINTF undeclared %s (%d)" % (orig.decode(), len(offs)))
    elif find_all(patched):
        print("VINTF already undeclared: %s" % orig.decode())
    else:
        print("WARN: %s not found" % orig.decode(), file=sys.stderr)

# ---- 2. sensors init rc: class hal -> hxl (bounded to the sensors service block) -------------------
def rc_block(base, maxlen=1024):
    raw=os.pread(fd, maxlen, base); out=bytearray()
    for b in raw:
        if b in (0x0a,0x09) or 0x20<=b<=0x7e: out.append(b)
        else: break
    return bytes(out)
n2=0
sdecl=b'service vendor.sensors-hal-multihal /vendor/bin/hw/android.hardware.sensors-service.multihal'
for base in find_all(sdecl):
    blk=rc_block(base)
    ci=blk.find(b'\n    class hal\n')
    if ci>=0:
        os.pwrite(fd, b'hxl', base+ci+len(b'\n    class ')); n2+=1
        print("sensors rc: class hal -> hxl @ %d" % (base+ci))
    elif blk.find(b'\n    class hxl\n')>=0:
        print("sensors rc: class already hxl")
    else:
        print("WARN: sensors 'class hal' not found in block", file=sys.stderr)

# ---- 3. disable light + threadnetwork APEXes (rename /vendor/apex dirent .apex -> .apeX) -----------
# Locate vendor_a EROFS extent via liblp metadata (mirrors prep-apex-dedup.sh).
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
FS=td*512; FS_LEN=ns*512
n3=0
for apex in [b'com.google.cf.light.apex', b'com.android.hardware.threadnetwork.apex']:
    patched=apex[:-1]+b'X'
    offs=[o for o in find_all(apex, FS, FS+FS_LEN)]
    if offs:
        for off in offs:
            xo=off+len(apex)-1
            if FS<=xo<FS+4096:  # skip if it would touch block-0 superblock (it never does here)
                print("WARN: %s dirent in block0 — skipped" % apex.decode(), file=sys.stderr); continue
            os.pwrite(fd, b'X', xo); n3+=1
        print("APEX disabled %s -> .apeX (%d)" % (apex.decode(), len(offs)))
    elif [o for o in find_all(patched, FS, FS+FS_LEN)]:
        print("APEX already disabled: %s" % apex.decode())
    else:
        print("WARN: %s not found in vendor_a" % apex.decode(), file=sys.stderr)

os.fsync(fd); os.close(fd)
print("prep-disable-vsock-hals: VINTF=%d sensors_rc=%d apex=%d edit(s) this run" % (n1,n2,n3))
PY
