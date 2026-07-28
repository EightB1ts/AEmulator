#!/usr/bin/env bash
# run-android64.sh — Phase 4 pass-9 harness: boot the **64-bit-only** Cuttlefish image
# (aosp_cf_arm64_only_phone, branch aosp-android-latest-release) under HVF.
#
#   ./run-android64.sh                 # headless: stream the guest console (default hermetic build)
#   ./run-android64.sh --gpu3d         # + gfxstream host renderer (GPU is always compiled in)
#   ./run-android64.sh --display       # + open the window
#   SWRENDER=1 ./run-android64.sh --display   # guest-CPU SwiftShader/ANGLE display path
#
# WHY A SEPARATE IMAGE (the pass-5..8 wall): the stock aosp_cf_arm64_phone target is DUAL-ABI
# (zygote64_32). Apple Silicon (M2) has NO AArch32 execution state, so its 32-bit HAL SERVICE
# binaries — chiefly /vendor/bin/hw/android.hardware.audio.service (32-bit ARM) — cannot exec, and
# AudioService's block on media.audio_policy gets system_server Watchdog-killed. There is no clean
# host-side fix on a dual-ABI image. The cure is the 64-bit-ONLY target, which ships NO 32-bit
# userspace at all (verified: empty /system/lib + /vendor/lib, no app_process32, no 32-bit linker;
# the audio HAL is now the arm64 android.hardware.audio.service-aidl.example inside the
# com.android.hardware.audio APEX). Images live under testdata/cf64/ (gitignored) — see
# testdata/cf64/SOURCE.txt.
#
# Everything else (composite GPT disk, vbmeta VERIFICATION_DISABLED, /data-unencrypted, secure_env on
# hvc11 + the 12 HVC channels) is IDENTICAL to run-android.sh; only --cf-dir changes. The Rust binary
# builds the single GPT composite /dev/vda, patches top vbmeta, grows userdata, and fabricates
# metadata/misc exactly as before.
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
CF="$HERE/testdata/cf64"
SUPER="$CF/img/super.raw.img"

# virtio-net now uses a real libslirp user-mode NAT backend (guest 10.0.2.15/24, gw 10.0.2.2, DNS
# 10.0.2.3, DHCP) — compiled in unconditionally (needs `brew install libslirp`). Host
# port-forwards default to TCP 5555->5555; extend with AEMU_HOSTFWD="tcp:HOST:GUEST,udp:H:G,...".
WANT_GFX=0
GPU3D=0
for a in "$@"; do [ "$a" = "--gpu3d" ] && { GPU3D=1; WANT_GFX=1; }; done

# gfxstream REAL-GPU path (gfxstream_vk over our virtio-gpu ASG ring → KosmicKrisp → Metal):
#   gralloc=minigbm  egl=angle  vulkan=ranchu  hwcomposer=ranchu  gltransport=virtio-gpu-asg
# WHY egl=angle (not egl=emulation): our host gfxstream dylib is a **GuestVulkanOnly** build (VK-only
# via KosmicKrisp — macOS has NO host OpenGL-ES driver, so host GLES emulation is disabled and any GLES
# client aborts the host renderer: "getEmulationGl: GL/EGL emulation not enabled"). The fix for a
# VulkanOnly host is guest-side ANGLE: `egl=angle` makes the guest's ANGLE translate GLES→Vulkan
# IN THE GUEST, so ONLY Vulkan crosses the wire → vulkan=ranchu (libvulkan_ranchu = gfxstream_vk) →
# our virtio-gpu ASG ring → host KosmicKrisp → Metal. `vulkan=ranchu` (NOT pastel/SwiftShader) is what
# selects the real host-GPU driver. hwcomposer display_finder_mode=drm + mode=client select our
# virtio-gpu KMS scanout (no vsock config server, pass 6). Needs the host-visible blob memory + ASG
# (VIRTIO_GPU_F_RESOURCE_BLOB + BAR2 shmem + hv_vm_map blob maps) this build now provides.
GPU_PROPS="${GPU_PROPS-androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=ranchu androidboot.hardware.gltransport=virtio-gpu-asg androidboot.hardware.gltransport.drawFlushInterval=800 androidboot.opengles.version=196609}"
SWRENDER_PROPS="androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=pastel androidboot.opengles.version=196609"
if [ "${SWRENDER:-0}" = "1" ]; then GPU_PROPS="$SWRENDER_PROPS"; GPU3D=1; fi

# Image prep (idempotent) — SAME scripts as run-android.sh, pointed at the cf64 super.raw.img.
#  · prep-super.sh: camera-APEX de-dup (no-op on 64-bit-only — single APEX) + 32-bit boringssl
#    self-test disable (inert here; abilist32 is empty so they never fire — commenting is harmless).
#  · prep-data-unencrypted.sh: drop /data FBE/metadata-encryption flags + lay a real empty ext4.
#  · prep-disable-32bit.sh: neuter any 32-bit zygote_secondary/drm/media (defensive; ro.zygote=
#    zygote64 means the secondary is never imported, so this mostly no-ops here).
if [ "${SKIP_PREP_SUPER:-0}" != "1" ]; then
  echo "== prep super.raw.img (apex de-dup + 32-bit self-test disable) =="
  "$HERE/prep-super.sh" "$SUPER"
fi
if [ "${SKIP_PREP_DATA:-0}" != "1" ]; then
  echo "== prep /data unencrypted =="
  "$HERE/prep-data-unencrypted.sh" "$SUPER"
fi
if [ "${SKIP_PREP_32BIT:-0}" != "1" ]; then
  echo "== prep disable 32-bit services (defensive; mostly no-op on 64-bit-only) =="
  "$HERE/prep-disable-32bit.sh" "$SUPER"
fi

# Duplicate-APEX prune (idempotent). This image ships 3 vendor APEX modules with multiple variant
# files declaring the SAME module name → apexd-bootstrap fatal (reboot,bootstrap-apexd-failed). The
# ro.boot.vendor.apex.<module> selection property is too long to survive the bootconfig→property
# import, so we prune host-side: rename the unwanted variant's `.apex`→`.apeX` in vendor_a EROFS
# (prep-super.sh's proven camera-dedup technique). See prep-apex-dedup.sh. Skip with SKIP_PREP_APEX=1.
if [ "${SKIP_PREP_APEX:-0}" != "1" ]; then
  echo "== prep apex dedup (prune duplicate-module vendor APEX variants) =="
  "$HERE/prep-apex-dedup.sh" "$SUPER"
fi

# Neutralize the crashing vsock-dependent Cuttlefish HALs (light/sensors/threadnetwork). Without a
# host vsock backend they abort + crash-loop forever; while DECLARED in VINTF they block system_server
# (LightsService in startBootstrapServices -> Watchdog -> boot_completed never fires) and, even
# undeclared, their respawn churn starves system_server at phase 500. This VINTF-undeclares them +
# neuters the sensors init service + disables the light/threadnetwork APEXes. See pass 10. Skip with
# SKIP_PREP_VSOCK_HALS=1.
if [ "${SKIP_PREP_VSOCK_HALS:-0}" != "1" ]; then
  echo "== prep disable vsock HALs (light/sensors/threadnetwork undeclare + neuter) =="
  "$HERE/prep-disable-vsock-hals.sh" "$SUPER"
fi

# Phase 5 (stealth): in-place de-brand of the uncompressed odm/system_ext build.prop identity
# (Cuttlefish generic/vsoc/aosp_cf → Xiaomi/fuxi). Byte-length-preserving, idempotent, no repack.
# The DT root compatible/model + serialno + verifiedbootstate=green are emitted by the VMM itself
# (src/bin/aemulator-hvf.rs). Skip with SKIP_PREP_DEBRAND=1.
if [ "${SKIP_PREP_DEBRAND:-0}" != "1" ]; then
  echo "== prep de-brand (odm/system_ext build.prop identity → Xiaomi/fuxi) =="
  "$HERE/prep-debrand.sh" "$SUPER"
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

echo "== run-android (--cf-dir $CF) =="
if [ "$GPU3D" = "1" ]; then
  exec "$BIN" run-android "$@" --cf-dir "$CF" --extra-cmdline "$GPU_PROPS"
else
  exec "$BIN" run-android "$@" --cf-dir "$CF"
fi
