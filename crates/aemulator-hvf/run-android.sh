#!/usr/bin/env bash
# run-android.sh — Phase 4 pass-1 harness: boot the Cuttlefish (aosp_cf_arm64) images under HVF.
#
#   ./run-android.sh                 # headless: stream the guest console (default hermetic build)
#   ./run-android.sh --gpu3d         # + gfxstream host renderer (GPU is always compiled in)
#   ./run-android.sh --display       # + open the window
#   ./run-android.sh --first-stage-console   # pause first-stage init in an interactive debug shell
#
# HONEST SCOPE (pass 1): this boots the real GKI android14 kernel + first-stage init far enough to
# DIAGNOSE the boot. It does NOT reach a launcher. See PHASE4-STATUS.md for the exact stall
# (first_stage_mount by-name partition discovery — the raw virtio-blk disks carry no GPT labels).
#
# Images live under testdata/cf/ (gitignored) — acquired per testdata/cf/SOURCE.txt.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# <root>/crates/aemulator-hvf -> <root>. Out-of-tree paths come from third_party/paths.sh only.
ROOT="$(cd "$HERE/../.." && pwd)"
# paths.sh discovers the ICD with a glob that legitimately matches nothing before the first
# bootstrap; under `pipefail` that empty glob would abort here instead of reaching the check below.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
MANIFEST="$HERE/Cargo.toml"
ENTITLEMENTS="$HERE/hv.entitlements"
BIN="$HERE/target/debug/aemulator-hvf"
BREW_LIB="/opt/homebrew/lib"

# GPU + networking are compiled in unconditionally (no cargo features). WANT_GFX only decides
# whether to stage the KosmicKrisp ICD for a --gpu3d run.
WANT_GFX=0
GPU3D=0
for a in "$@"; do [ "$a" = "--gpu3d" ] && { GPU3D=1; WANT_GFX=1; }; done

# When --gpu3d is on, steer the guest's gfxstream userspace onto OUR virtio-gpu transport (not the
# QEMU goldfish_pipe we don't present). `ro.boot.hardware.gltransport=virtio-gpu-asg` selects the
# Address-Space-Graphics transport over virtio-gpu (host-visible ring), and the gralloc/hwcomposer/
# egl/vulkan selectors point the HALs at the gfxstream (minigbm + ranchu) implementations. These are
# appended to any user --extra-cmdline. Override with GPU_PROPS= to experiment.
GPU_PROPS="${GPU_PROPS-androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=ranchu androidboot.hardware.gltransport=virtio-gpu-asg androidboot.hardware.gltransport.drawFlushInterval=800 androidboot.opengles.version=196609}"

# --- Guest-CPU (SwiftShader) display path (default hermetic build, no host GPU/gfxstream) ---------
# The gfxstream ASG transport (above) still needs a host-visible virtio-gpu ring we don't yet map.
# The tractable path that renders REAL Android pixels TODAY: ANGLE-over-SwiftShader does GLES/Vulkan
# entirely on the guest CPU, and the composer3-ranchu HWC3 (in `mode=client`, `display_finder_mode=
# drm`) discovers OUR virtio-gpu KMS display (1024x768) and client-composites to it — no host GPU,
# no gfxstream, no ASG. Select with `SWRENDER=1 ./run-android.sh --display` (no --gpu3d needed).
SWRENDER_PROPS="androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=pastel androidboot.opengles.version=196609"
if [ "${SWRENDER:-0}" = "1" ]; then GPU_PROPS="$SWRENDER_PROPS"; GPU3D=1; fi

# Image prep (idempotent): prune the duplicate camera-HAL APEX so apexd-bootstrap passes, and
# disable the 32-bit BoringSSL self-tests (Apple Silicon has no AArch32) so second-stage init gets
# past early-init. See prep-super.sh. Skip with SKIP_PREP_SUPER=1.
if [ "${SKIP_PREP_SUPER:-0}" != "1" ]; then
  echo "== prep super.raw.img (apex de-dup + 32-bit self-test disable) =="
  "$HERE/prep-super.sh"
fi

# /data crypto-wall bypass (idempotent): neutralize the metadata-encryption + FBE fs_mgr flags on the
# vendor_a /data fstab entry so /data mounts as plain formattable ext4 (our software secure_env has no
# real KeyMint key ops yet). See prep-data-unencrypted.sh. Skip with SKIP_PREP_DATA=1.
if [ "${SKIP_PREP_DATA:-0}" != "1" ]; then
  echo "== prep /data unencrypted (drop fileencryption/keydirectory/checkpoint) =="
  "$HERE/prep-data-unencrypted.sh"
fi

# 32-bit-service loop bypass (idempotent): Apple Silicon has no AArch32, so the dual-ABI image's
# 32-bit services (app_process32/drmserver/mediaserver) Exec-format-error loop. The 32-bit
# `zygote_secondary`'s `onrestart restart zygote` kills the primary zygote's process group — and the
# freshly-forked system_server with it — every ~5s, so boot never completes. Neuter them. See
# prep-disable-32bit-dualabi.sh. Skip with SKIP_PREP_32BIT=1.
#
# The `-dualabi` suffix is load-bearing: this script is correct ONLY for this dual-ABI `cf` image. It
# must never be applied to the 64-bit-only `cf64` image, where those same binaries are arm64 and run
# fine — disabling mediaserver there costs you media.player + media.resource_manager. See the header
# of run-android64.sh.
if [ "${SKIP_PREP_32BIT:-0}" != "1" ]; then
  echo "== prep disable 32-bit services (zygote_secondary loop-breaker + drm/media) =="
  "$HERE/prep-disable-32bit-dualabi.sh"
fi

# NOTE (pass 8): the 32-bit audio-HAL Watchdog wall is NOT bypassable host-side on this dual-ABI
# image — see prep-audio-stub.sh's header and PHASE4-STATUS.md § Pass 8. prep-audio-stub.sh is kept
# only as a documented dead-end (undeclaring the HAL trades the 66s block for an AudioFlinger
# SIGSEGV — still Watchdog). It is DEFAULT-OFF; opt in with PREP_AUDIO_STUB=1 only to reproduce the
# dead-end. The real fix needs a 64-bit vendor audio service (all-64-bit aosp_cf_arm64_only_phone
# target — not built on public AOSP CI) or defeating Treble linker isolation (not a clean host edit).
if [ "${PREP_AUDIO_STUB:-0}" = "1" ]; then
  echo "== prep audio stub (DEAD-END repro: undeclare audio HAL -> AudioFlinger SIGSEGV) =="
  "$HERE/prep-audio-stub.sh"
fi

echo "== build =="
cargo build --manifest-path "$MANIFEST" --bin aemulator-hvf

echo "== ad-hoc codesign with hypervisor entitlement =="
codesign --sign - --entitlements "$ENTITLEMENTS" --force "$BIN"

if [ "$WANT_GFX" = "1" ]; then
  # Host Vulkan driver: Mesa KosmicKrisp (Metal). MoltenVK is gone; macOS 26+ required.
  [ -f "$TP_KOSMICKRISP_ICD" ] || {
    echo "no KosmicKrisp ICD at $TP_KOSMICKRISP_ICD - run third_party/bootstrap.sh mesa"; exit 1; }
  # The Vulkan LOADER must still be staged here: VulkanDispatch::getPossibleLoaderPaths()
  # restricts the libvulkan.dylib search to <programDir>/lib64/vulkan whenever
  # ANDROID_EMU_VK_ICD is non-empty.
  VKDIR="$(dirname "$BIN")/lib64/vulkan"
  mkdir -p "$VKDIR"
  ln -sf "$BREW_LIB/libvulkan.dylib" "$VKDIR/libvulkan.dylib"
  # Staged manifest = the fallback path gfxstream uses when no VK_DRIVER_FILES is exported
  # (i.e. what a shipped bundle would do). Re-copied every run, so it cannot go stale.
  cp -f "$TP_KOSMICKRISP_ICD" "$VKDIR/$TP_KOSMICKRISP_ICD_BASENAME"
  # Authoritative selection: same manifest kosmickrisp/run-gate.sh drives, so the emulator and
  # the gate provably load the same driver binary. Takes strict precedence in the loader.
  export VK_DRIVER_FILES="$TP_KOSMICKRISP_ICD"
  export ANDROID_EMU_VK_ICD=kosmickrisp
fi

echo "== run-android =="
if [ "$GPU3D" = "1" ]; then
  # Fold the graphics props into a single --extra-cmdline (the bin appends it to the kernel cmdline).
  exec "$BIN" run-android "$@" --extra-cmdline "$GPU_PROPS"
else
  exec "$BIN" run-android "$@"
fi
