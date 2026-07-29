#!/usr/bin/env bash
# prep-adb-keys.sh — authorise THIS host's adb key inside the guest image, so `adb connect` just
# works instead of needing a console dance.
#
# WHY THIS EXISTS. Every in-guest measurement worth having — `cmd gpu vkjson`, `pm install`,
# `logcat`, screenshots, driving a real app — is either impossible or miserable over the serial
# console. The console ECHOES and WRAPS, so `$(…)` gets mangled, and pipeline output is frequently
# swallowed entirely; a past session drew a wrong conclusion from a grep whose output never arrived.
# `adb` has none of those problems. The one thing that stands between us and it is the RSA
# authorisation handshake, whose answer lives in a file on /data that nothing host-side writes.
#
# WHERE THE KEYS LIVE, and why it is `userdata.img` and not `super.raw.img`:
#   /data/misc/adb/adb_keys           <- one authorised public key per line, "<base64> <comment>"
#   /data/misc/adb/adb_temp_keys.xml  <- system_server's OWN store, and the durable one (see below)
#
#   `/data` is the userdata partition, i.e. the plain ext4 `userdata.img` that
#   prep-data-unencrypted.sh formats (that script is what makes /data plain ext4 at all — with the
#   stock metadata-encryption + FBE fstab flags there would be no host-readable filesystem here to
#   write into, and this script could not exist). So unlike every other prep-*.sh in this directory,
#   this one does NOT byte-patch an EROFS partition inside super.raw.img: it is an ordinary
#   debugfs write into an ordinary ext4 image.
#
#   `/data/misc/adb` already exists on any image that has booted once — Android init creates it as
#   `system:shell 02750 u:object_r:adb_keys_file:s0`. On a freshly formatted userdata it does not,
#   so this script creates `/misc` and `/misc/adb` itself with exactly those attributes (init's own
#   `mkdir` is idempotent and does not clear a directory that already has contents, so pre-creating
#   is safe and survives the next boot).
#
# !! THE PROVISIONING DOES NOT SURVIVE A BOOT ON THIS IMAGE. Measured twice, and stated up front so
# nobody builds on it:
#
#   attempt 1 — write `adb_keys` only          -> after the next boot debugfs lists the name with
#                                                 **inode 0**, i.e. UNLINKED, while the rest of /data
#                                                 (installed apps included) is intact.
#   attempt 2 — write `adb_keys` AND the XML   -> after the next boot `/data/misc/adb/` is **empty**.
#                                                 Both files gone. No `adb root` was issued in that
#                                                 cycle, so the clear happened during boot.
#
# The platform owns that directory. `AdbService: setAdbEnabled(true), mIsAdbUsbEnabled=true` appears
# in every boot log of this image, so `AdbDebuggingManager` is live and it treats
# `adb_temp_keys.xml` as the source of truth for `adb_keys`. Exactly which of its paths does the
# delete is NOT established here and this comment will not guess: what is established is that
# host-side pre-provisioning of adb authorisation is not durable on this image.
#
# WHY THE SCRIPT IS STILL WORTH HAVING, and why that is not a rationalisation:
#   * `--verify` is the only way to read the guest's authorisation state without booting it, and it
#     reports BOTH files plus a warning when they disagree. That is how the above was measured.
#   * The write is correct and idempotent for the window before system_server starts, and for any
#     image whose AdbDebuggingManager does not clear the store — which is the case this exists for.
#   * It documents, in the one place someone will look, that **the key is not what makes adb work
#     here** (see below) — which is worth more than the write itself.
#
# It writes both files, for the two different readers:
#   * `adb_keys`          — what **adbd** reads.
#   * `adb_temp_keys.xml` — what **system_server** reads. `lastConnection` is stamped with the current
#     host time in epoch-millis because `AdbKeyStore.filterOutOldKeys()` drops entries older than
#     `Settings.Global.ADB_ALLOWED_CONNECTION_TIME` (7 days by default). A timestamp in the guest's
#     future is safe — the comparison is `lastConnection < now - allowed`, so only *old* loses.
#
# THE LABEL IS NOT DECORATION. sepolicy grants `allow adbd adb_keys_file:file r_file_perms` — and
# nothing else. A file written into that directory with the inherited-looking-but-wrong
# `system_data_file` label is UNREADABLE to adbd under enforcing SELinux, and the symptom is not a
# permission error, it is `adb devices` sitting at `unauthorized` forever. The label is therefore
# copied off the containing directory when it exists (so we track the image rather than a constant),
# and falls back to the literal `u:object_r:adb_keys_file:s0`. Same for uid/gid: system:shell, 0640.
#
# WHAT ACTUALLY MAKES adb WORK HERE — read this before debugging an `unauthorized` device.
# Not this script. `ro.adb.secure=0` on this Cuttlefish image, so **adbd does not require
# authorisation at all**. Measured with `/data/misc/adb` completely empty: `adb connect
# 127.0.0.1:5555` came up `device` (not `unauthorized`) immediately, `adb install` pushed a 335 MB
# APK in 29 s, `uiautomator dump` worked, and `adb root` worked (`ro.debuggable=1`). The three things
# that are genuinely load-bearing, none of them in this file:
#   1. `ro.adb.secure=0`  — no auth handshake. If an image sets it to 1, THEN this script matters, and
#                           then its non-durability above becomes a real problem to solve.
#   2. TCP 5555, forwarded host->guest by the VMM's slirp NAT (`net/slirp.rs`, default 5555->5555).
#   3. `machine.rs::net_autoconfig_script`, which does `setprop service.adb.tcp.port 5555; start adbd`
#      on a timer during boot. If `adb connect` is REFUSED, the fault is here or in (2) — not the key.
#
# AND THE TRAP THAT COSTS THE MOST TIME: the guest boots ASLEEP with the keyguard up. adb connects
# fine, `am start` succeeds, `dumpsys` shows your activity resumed — and every `screencap` is the lock
# screen, because the app resumed behind it (`callingUidProcState: TOP_SLEEPING`). Two commands fix
# it and they are not optional for any visual measurement:
#     adb shell input keyevent KEYCODE_WAKEUP && adb shell wm dismiss-keyguard
#     adb shell svc power stayon true          # keeps it awake for the rest of the session
#
# The `adb_temp_keys.xml` schema is AOSP-internal and versioned. If a platform changes it, this
# script's XML is simply ignored — no worse than the measured status quo. `--verify` reports what is
# actually in both files, so any such degradation is visible rather than silent.
#
# APPEND, NEVER CLOBBER: an existing adb_keys may hold keys from other hosts (or a previous user of
# this image). Our line is appended if absent and the file is left untouched if present, so a
# re-run is a true no-op and no other host loses access.
#
# USAGE
#   ./prep-adb-keys.sh                                   # userdata.img next to testdata/cf64/img
#   ./prep-adb-keys.sh <userdata.img>                    # explicit image
#   ./prep-adb-keys.sh <userdata.img> --key=<adbkey.pub>  # explicit public key
#   ./prep-adb-keys.sh <userdata.img> --verify           # report what is authorised, write nothing
#   AEMU_ADB_PUBKEY=<path> ./prep-adb-keys.sh            # same as --key=
#
# The image may also be a cf-dir or an img dir; both are resolved to `<dir>/userdata.img`.
#
# HOST DEPS: python3 (stdlib only) + e2fsprogs (`brew install e2fsprogs`) for debugfs — the same
# dependency prep-vulkan-icd.sh and prep-data-unencrypted.sh already take.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

IMG=""
KEY="${AEMU_ADB_PUBKEY-}"
MODE="install"
for a in "$@"; do
  case "$a" in
    --key=*)  KEY="${a#--key=}" ;;
    --key)    echo "error: --key needs a path, e.g. --key=$HOME/.android/adbkey.pub" >&2; exit 2 ;;
    --verify) MODE="verify" ;;
    --*)      echo "error: unknown flag $a (want --key=<pub>, --verify)" >&2; exit 2 ;;
    *)        IMG="$a" ;;
  esac
done

# Accept a cf-dir, its img/ subdir, or the image itself — the sibling scripts take a super.raw.img
# path, and typing the wrong one of the three is the obvious mistake to absorb rather than punish.
if [ -z "$IMG" ]; then
  IMG="$HERE/testdata/cf64/img/userdata.img"
elif [ -d "$IMG" ]; then
  if   [ -f "$IMG/userdata.img" ];     then IMG="$IMG/userdata.img"
  elif [ -f "$IMG/img/userdata.img" ]; then IMG="$IMG/img/userdata.img"
  else echo "ERROR: no userdata.img under $IMG" >&2; exit 3
  fi
fi
[ -f "$IMG" ] || { echo "ERROR: no userdata image at $IMG" >&2; exit 3; }

: "${KEY:=$HOME/.android/adbkey.pub}"
if [ "$MODE" = "install" ] && [ ! -f "$KEY" ]; then
  echo "ERROR: no adb public key at $KEY" >&2
  echo "       adb mints one on its first run; generate it with:  adb keygen $HOME/.android/adbkey" >&2
  echo "       (or point --key= / AEMU_ADB_PUBKEY at an existing .pub)" >&2
  exit 3
fi

# e2fsprogs is keg-only under Homebrew, so fall back to its Cellar sbin (same as the siblings).
E2SBIN=/opt/homebrew/opt/e2fsprogs/sbin
DEBUGFS="$(command -v debugfs || echo $E2SBIN/debugfs)"
[ -x "$DEBUGFS" ] || { echo "ERROR: $DEBUGFS not found (install e2fsprogs: brew install e2fsprogs)" >&2; exit 3; }

# A VMM holding this image would have the ext4 mounted-dirty under us; debugfs would read a
# half-written filesystem and our write would race the guest's. Refuse rather than corrupt /data.
if pgrep -f "aemulator-hvf (control|run-android)" >/dev/null 2>&1; then
  echo "ERROR: a VMM is running and holds $IMG. Stop it first:" >&2
  echo "         pkill -f \"aemulator-hvf control\"" >&2
  exit 3
fi

python3 - "$IMG" "$KEY" "$MODE" "$DEBUGFS" <<'PY'
import os, re, subprocess, sys, tempfile, time

img, key_path, mode, DEBUGFS = sys.argv[1:5]

ADB_DIR   = '/misc/adb'                      # /data/misc/adb — this image IS /data
KEYS      = ADB_DIR + '/adb_keys'
XML       = ADB_DIR + '/adb_temp_keys.xml'   # system_server's store; the durable one
DIR_LABEL = b'u:object_r:adb_keys_file:s0\x00'
DIR_MODE, DIR_UID, DIR_GID = 0o2750, 1000, 2000     # system:shell, as init creates it
KEY_MODE, KEY_UID, KEY_GID = 0o0640, 1000, 2000
XML_MODE, XML_UID, XML_GID = 0o0600, 1000, 1000     # system:system, as AtomicFile creates it

def die(msg, *extra):
    print("ERROR: %s" % msg, file=sys.stderr)
    for e in extra:
        print("       %s" % e, file=sys.stderr)
    sys.exit(4)

with open(img, 'rb') as f:
    f.seek(0x438)
    if f.read(2) != b'\x53\xef':
        die("%s carries no ext4 superblock — /data has never been formatted." % img,
            "Run prep-data-unencrypted.sh first; it is what makes /data a plain ext4 at all.")

def dbg(script, write=False):
    cmd = [DEBUGFS] + (['-w'] if write else []) + ['-f', '/dev/stdin', img]
    p = subprocess.run(cmd, input=script, capture_output=True, text=True)
    return p.stdout + p.stderr

def exists(path):
    # debugfs prints "File not found by ea_get" style errors to stderr and nothing useful to
    # stdout; `stat` is the reliable probe because a hit always contains an "Inode:" line.
    return 'Inode:' in dbg("stat %s\nquit\n" % path)

tmp = tempfile.mkdtemp(prefix='prep-adb-keys.')
try:
    # ---- 1. the authorised-keys directory ------------------------------------------------------
    # Present on any image that has booted once (init mkdir's it). Created here with init's exact
    # attributes otherwise, so a freshly formatted /data is authorised on its FIRST boot.
    label = DIR_LABEL
    if exists(ADB_DIR):
        got = os.path.join(tmp, 'dirlabel.bin')
        dbg("ea_get -f %s %s security.selinux\nquit\n" % (got, ADB_DIR))
        if os.path.exists(got) and os.path.getsize(got):
            label = open(got, 'rb').read()
        print("%s exists (label %s)" % (ADB_DIR, label.rstrip(b'\x00').decode('utf-8', 'replace')))
    else:
        if mode == 'verify':
            die("%s does not exist in %s and --verify writes nothing." % (ADB_DIR, img),
                "Re-run without --verify to create it.")
        for d, m in (('/misc', 0o1771), (ADB_DIR, DIR_MODE)):
            if exists(d):
                continue
            dbg("mkdir %s\nquit\n" % d, write=True)
            if not exists(d):
                die("could not create %s inside %s" % (d, img))
            # /data/misc is system_data_file; only /data/misc/adb is adb_keys_file.
            lbl = DIR_LABEL if d == ADB_DIR else b'u:object_r:system_data_file:s0\x00'
            lf = os.path.join(tmp, 'newlabel.bin')
            with open(lf, 'wb') as f:
                f.write(lbl)
            dbg("sif %s mode 0%o\nsif %s uid %d\nsif %s gid %d\n"
                "ea_set -f %s %s security.selinux\nquit\n"
                % (d, m, d, DIR_UID, d, DIR_GID, lf, d), write=True)
            print("created %s (mode 0%o, %d:%d, %s)"
                  % (d, m, DIR_UID, DIR_GID, lbl.rstrip(b'\x00').decode()))

    # ---- 2. what is authorised right now? ------------------------------------------------------
    cur = os.path.join(tmp, 'adb_keys.cur')
    lines = []
    if exists(KEYS):
        dbg("dump %s %s\nquit\n" % (KEYS, cur))
        if os.path.exists(cur):
            lines = [l for l in open(cur, 'rb').read().decode('utf-8', 'replace').splitlines()
                     if l.strip()]
    for l in lines:
        # A key line is "<base64> <comment>"; the comment is the useful half to a human.
        parts = l.split(None, 1)
        print("adb_keys       : %s (%s…)" % (parts[1] if len(parts) > 1 else '<no comment>', parts[0][:24]))
    if not lines:
        print("adb_keys       : (none)")

    # system_server's own store, separately — the two disagreeing is the interesting state, and it
    # is what tells you whether an authorisation will survive the next boot.
    xml_raw = ''
    if exists(XML):
        xf = os.path.join(tmp, 'adb_temp_keys.xml')
        dbg("dump %s %s\nquit\n" % (XML, xf))
        if os.path.exists(xf):
            xml_raw = open(xf, 'rb').read().decode('utf-8', 'replace')
    xml_keys = re.findall(r'key="([^"]*)"', xml_raw)
    for k in xml_keys:
        print("adb_temp_keys  : %s…" % k[:24])
    if not xml_keys:
        print("adb_temp_keys  : (none)%s" % ("" if exists(XML) else "  [file absent]"))
    if lines and not xml_keys:
        print("WARNING: adb_keys holds %d key(s) but system_server's adb_temp_keys.xml is empty. "
              "AdbDebuggingManager regenerates adb_keys from that store and UNLINKS it when the "
              "store is empty, so this authorisation will NOT survive the next boot. Re-run this "
              "script without --verify to write both." % len(lines))

    if mode == 'verify':
        print("prep-adb-keys: %d key(s) in adb_keys, %d in adb_temp_keys.xml"
              % (len(lines), len(xml_keys)))
        sys.exit(0)

    # ---- 3. append ours if it is not already there ----------------------------------------------
    raw = open(key_path).read().strip()
    if not raw:
        die("%s is empty" % key_path)
    if len(raw.splitlines()) != 1:
        die("%s holds %d lines; an adb public key is exactly one."
            % (key_path, len(raw.splitlines())))
    b64 = raw.split(None, 1)[0]
    if len(b64) < 64:
        die("%s does not look like an adb public key (first field is %d chars)" % (key_path, len(b64)))
    # Match on the KEY MATERIAL, not the whole line: adb rewrites the trailing
    # "<user>@<host>" comment (it embeds the current hostname), so comparing full lines would
    # append a duplicate of the same key every time the machine is renamed.
    in_keys = any(l.split(None, 1)[0] == b64 for l in lines)
    in_xml  = any(k.split(None, 1)[0] == b64 for k in xml_keys)
    if in_keys and in_xml:
        print("prep-adb-keys: this host's key is already in both stores — no-op")
        sys.exit(0)

    # A shared helper: debugfs `write` refuses to overwrite an existing name, so an update is
    # rm-then-write. Attributes are re-applied every time because a fresh inode does not inherit
    # them, and the SELinux label in particular is the difference between "authorised" and
    # "unauthorized forever" (see the header).
    def install(path, src, mode_, uid_, gid_, lbl):
        name = path.rsplit('/', 1)[1]
        lf = os.path.join(tmp, name + '.label')
        with open(lf, 'wb') as f:
            f.write(lbl)
        script = ("rm %s\n" % path) if exists(path) else ""
        script += ("cd %s\n"
                   "write %s %s\n"
                   "sif %s mode 0%o\n"
                   "sif %s uid %d\n"
                   "sif %s gid %d\n"
                   "ea_set -f %s %s security.selinux\n"
                   "quit\n" % (ADB_DIR, os.path.abspath(src), name,
                               name, mode_, name, uid_, name, gid_, lf, name))
        dbg(script, write=True)
        back = os.path.join(tmp, name + '.back')
        dbg("dump %s %s\nquit\n" % (path, back))
        if not os.path.exists(back):
            die("%s did not appear in %s after the write" % (path, img))
        if open(back, 'rb').read() != open(src, 'rb').read():
            die("round-trip mismatch: %s is not the file we wrote" % path)
        st = dbg("stat %s\nquit\n" % path)
        if 'adb_keys_file' not in st:
            die("security.selinux did not stick on %s. adbd cannot read an adb_keys_file-less file "
                "under enforcing SELinux, and the symptom is not an error — it is `adb devices` "
                "stuck at `unauthorized` forever:\n%s" % (path, st))

    # ---- 3a. adb_keys: what adbd itself reads ---------------------------------------------------
    new = os.path.join(tmp, 'adb_keys')
    with open(new, 'w') as f:
        f.write(''.join(l.rstrip('\n') + '\n' for l in lines))
        if not in_keys:
            f.write(raw + '\n')
    install(KEYS, new, KEY_MODE, KEY_UID, KEY_GID, label)

    # ---- 3b. adb_temp_keys.xml: what makes it survive the next boot ------------------------------
    # Regenerated from the union of what was already there and our key, so another host's
    # authorisation is preserved exactly as in adb_keys. Schema is AdbDebuggingManager.AdbKeyStore's:
    # a <keyStore> of <adbKey key="…" lastConnection="<epoch millis>"/>.
    now_ms = int(time.time() * 1000)
    keep = [k for k in xml_keys if k.split(None, 1)[0] != b64] + [raw]
    xml_new = os.path.join(tmp, 'adb_temp_keys.xml')
    with open(xml_new, 'w') as f:
        f.write("<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n")
        f.write('<keyStore version="1">\n')
        for k in keep:
            f.write('<adbKey key="%s" lastConnection="%d" />\n'
                    % (k.replace('&', '&amp;').replace('"', '&quot;').replace('<', '&lt;'), now_ms))
        f.write('</keyStore>\n')
    install(XML, xml_new, XML_MODE, XML_UID, XML_GID, label)

    who = raw.split(None, 1)[1] if len(raw.split(None, 1)) > 1 else os.path.basename(key_path)
    print("prep-adb-keys: authorised %s" % who)
    print("  %s          %d key(s), mode 0%o, %d:%d, %s"
          % (KEYS, len(lines) + (0 if in_keys else 1), KEY_MODE, KEY_UID, KEY_GID,
             label.rstrip(b'\x00').decode('utf-8', 'replace')))
    print("  %s %d key(s), mode 0%o, %d:%d  (lastConnection=%d)"
          % (XML, len(keep), XML_MODE, XML_UID, XML_GID, now_ms))
finally:
    import shutil
    shutil.rmtree(tmp, ignore_errors=True)
PY

cat <<'EOF'

Next: boot, then reach adbd over the slirp port-forward (host 5555 -> guest 5555). The VMM's
net-autoconfig thread sets service.adb.tcp.port and starts adbd a few seconds into boot:

    adb connect 127.0.0.1:5555 && adb devices
    adb shell input keyevent KEYCODE_WAKEUP && adb shell wm dismiss-keyguard
    adb shell svc power stayon true

The wake + dismiss-keyguard is NOT optional for anything visual: the guest boots asleep, so an app
launched without it resumes BEHIND the lock screen and every screencap shows the keyguard.

NOTE, measured on this image: the platform clears /data/misc/adb at boot, so the authorisation above
does not persist — and it does not need to, because ro.adb.secure=0 means adbd never asks. If a
device comes up `unauthorized` rather than `device`, that image is NOT this one; see the header.
EOF
