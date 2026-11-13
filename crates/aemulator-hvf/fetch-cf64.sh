#!/usr/bin/env bash
# fetch-cf64.sh — download and prepare the base Android image Studio boots (testdata/cf64).
#
#   crates/aemulator-hvf/fetch-cf64.sh           # download + prepare whatever is missing
#   crates/aemulator-hvf/fetch-cf64.sh --check   # exit 0 if the image is ready, 1 if not
#
# What it does, in order (each stage is recorded under <cf>/.aemu-setup/ so an interrupted run
# resumes where it stopped instead of starting over):
#   1. download   the 64-bit-only Cuttlefish image zip from ci.android.com (~1.1 GB, md5-checked)
#   2. unzip      into <cf>/img
#   3. unsparse   img/super.img -> img/super.raw.img          (simg2img.py — the host-package
#                 simg2img is a Linux binary)
#   4. extract    kernel, initramfs.cf.gz, bootconfig.txt     (extract_cf.py)
#   5. prep       the same image preps run-control64.sh applies, in the same order
#
# The pinned build is the one testdata/cf64/SOURCE.txt describes (and why it is the 64-bit-only
# target). Its old download recipe — androidbuildinternal.googleapis.com/.../artifacts/<a>/url —
# now returns 404; ci.android.com's /raw/ path redirects to the same signed GCS object.
#
# Needs ~8 GB free. Studio clones this directory per VM with APFS clonefile, so instances cost
# almost nothing on top. Studio does not re-run preps on existing instances: after changing a prep,
# delete and re-create the instance.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Studio only looks in testdata/cf64; the override exists to rehearse this script somewhere else.
CF="${AEMU_CF64_DIR:-$HERE/testdata/cf64}"
STATE="$CF/.aemu-setup"

BID=15885347
TARGET=aosp_cf_arm64_only_phone-userdebug
ZIP_NAME="aosp_cf_arm64_only_phone-img-$BID.zip"
ZIP_SIZE=1101172766
ZIP_MD5=ec4e9ebac9c853dddec12cf60d5799a4
ZIP_URL="https://ci.android.com/builds/submitted/$BID/$TARGET/latest/raw/$ZIP_NAME"
ZIP="$CF/$ZIP_NAME"
SUPER="$CF/img/super.raw.img"
NEED_FREE_GB=8

if [ -t 1 ]; then C_G=$'\033[32m'; C_R=$'\033[31m'; C_B=$'\033[1m'; C_0=$'\033[0m'; else C_G=; C_R=; C_B=; C_0=; fi
step() { printf '%s  ->%s %s\n' "$C_B" "$C_0" "$*"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
die()  { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

# Ready = this script finished, or (for a cf64 prepared by hand before this script existed) every
# file the VMM loads is present and no run of this script is half done.
is_ready() {
    [ -f "$STATE/complete" ] && return 0
    [ -d "$STATE" ] && return 1
    local f
    for f in kernel initramfs.cf.gz bootconfig.txt img/super.raw.img img/boot.img img/init_boot.img \
             img/vendor_boot.img img/vbmeta.img img/vbmeta_system.img img/userdata.img; do
        [ -f "$CF/$f" ] || return 1
    done
    return 0
}

if [ "${1:-}" = "--check" ]; then is_ready; exit $?; fi
if is_ready; then ok "Android image ready: $CF"; exit 0; fi

mkdir -p "$STATE" "$CF/img"
done_stage() { [ -f "$STATE/$1" ]; }
mark()       { touch "$STATE/$1"; }

# 1. download ------------------------------------------------------------------------------------
if ! done_stage downloaded; then
    # A zip that is already here but not recorded (a run stopped between saving and recording it,
    # or one copied in by hand) goes through the same size + checksum check as a fresh download.
    if [ -f "$ZIP" ]; then mv -f "$ZIP" "$ZIP.part"; fi
    if [ "$(stat -f%z "$ZIP.part" 2>/dev/null || echo 0)" != "$ZIP_SIZE" ]; then
        _free_kb="$(df -k "$CF" | awk 'NR==2 {print $4}')"
        [ "$_free_kb" -ge $((NEED_FREE_GB * 1024 * 1024)) ] ||
            die "only $((_free_kb / 1024 / 1024)) GB free on $(df -h "$CF" | awk 'NR==2 {print $9}'); the Android image needs ~${NEED_FREE_GB} GB"
        step "downloading Android image build $BID (~1.1 GB, resumable — re-run to continue)"
        curl -fL --retry 5 --retry-delay 3 --progress-bar -C - -o "$ZIP.part" "$ZIP_URL" ||
            die "download failed — re-run to resume"
    fi
    [ "$(stat -f%z "$ZIP.part")" = "$ZIP_SIZE" ] || die "downloaded size is wrong — re-run to resume"
    step "verifying checksum"
    if [ "$(md5 -q "$ZIP.part")" != "$ZIP_MD5" ]; then
        rm -f "$ZIP.part"
        die "checksum mismatch (deleted the download) — re-run to fetch it again"
    fi
    mv "$ZIP.part" "$ZIP"
    mark downloaded
    ok "downloaded $ZIP_NAME"
fi

# 2. unzip ---------------------------------------------------------------------------------------
if ! done_stage unzipped; then
    step "unzipping into img/"
    unzip -o -q "$ZIP" -d "$CF/img" || die "unzip failed"
    mark unzipped
    ok "unzipped"
fi

# 3. unsparse super.img --------------------------------------------------------------------------
if ! done_stage unsparsed; then
    step "converting img/super.img to a raw image"
    rm -f "$SUPER.tmp"
    python3 "$HERE/simg2img.py" "$CF/img/super.img" "$SUPER.tmp" || die "simg2img.py failed"
    mv "$SUPER.tmp" "$SUPER"
    mark unsparsed
    ok "img/super.raw.img"
fi

# 4. kernel / initramfs / bootconfig -------------------------------------------------------------
if ! done_stage extracted; then
    step "extracting kernel, initramfs and bootconfig"
    python3 "$HERE/extract_cf.py" "$CF" || die "extract_cf.py failed"
    mark extracted
    ok "kernel, initramfs.cf.gz, bootconfig.txt"
fi

# 5. image preps — keep in step with run-control64.sh -------------------------------------------
if ! done_stage prepped; then
    step "applying image preps"
    "$HERE/prep-super.sh" "$SUPER"
    "$HERE/prep-data-unencrypted.sh" "$SUPER"
    "$HERE/prep-apex-dedup.sh" "$SUPER"
    "$HERE/prep-disable-vsock-hals.sh" "$SUPER" --revert=sensors,light
    "$HERE/prep-debrand.sh" "$SUPER"
    mark prepped
    ok "image preps applied"
fi

rm -f "$ZIP"
mark complete
ok "Android image ready: $CF"
