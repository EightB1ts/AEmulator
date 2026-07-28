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
#
# NOT run here: prep-disable-32bit-dualabi.sh. It was applied to this image for a long time on the
# theory that it was a defensive no-op, and that was wrong. Its step 4 unconditionally rewrites
# `service media /system/bin/mediaserver`'s `class main` -> `class hvfx` (a class no `class_start`
# ever names). On the dual-ABI `cf` image that is correct — mediaserver is 32-bit there and cannot
# exec on Apple Silicon. On THIS 64-bit-only image mediaserver is arm64 and runs fine, so all the
# patch achieved was to stop it starting, which means `media.player` and `media.resource_manager`
# never register. system_server, mediaprovider_app and platform_app then retry the lazy AIDL
# start ~1/s forever: `init: Could not find 'aidl/media.resource_manager'` +
# `libbinder.ServiceManagerCppClient: Waited one second for media.resource_manager`. The script's
# own header says it targets `testdata/cf/img/super.raw.img`; it should never have been pointed at
# cf64. (The zygote_secondary/drmserver edits genuinely were inert here: ro.zygote=zygote64 imports
# init.zygote64.rc, so the patched declaration is never read.)
if [ "${SKIP_PREP_SUPER:-0}" != "1" ]; then
  echo "== prep super.raw.img (apex de-dup + 32-bit self-test disable) =="
  "$HERE/prep-super.sh" "$SUPER"
fi
if [ "${SKIP_PREP_DATA:-0}" != "1" ]; then
  echo "== prep /data unencrypted =="
  "$HERE/prep-data-unencrypted.sh" "$SUPER"
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

# Per-HAL enable/disable for the three HALs that used to be neutered wholesale (light, sensors,
# threadnetwork). Historically all three were disabled because each aborted and crash-looped without a
# host backend — and while DECLARED in VINTF they blocked system_server (LightsService in
# startBootstrapServices -> Watchdog -> boot_completed never fires). `aemulator-hal` now provides the
# backends, so they are re-enabled per HAL instead. Skip the whole step with SKIP_PREP_VSOCK_HALS=1.
#
# PREP_VSOCK_HALS_ARGS forwards per-HAL flags so an enable survives a re-run of this script, e.g.
#   PREP_VSOCK_HALS_ARGS="--revert=sensors,light" ./run-android64.sh     # the default
#   PREP_VSOCK_HALS_ARGS="--revert=sensors --probe=light" ./run-android64.sh
# sensors and light are reverted by default: both have host backends now (aemulator-hal).
# threadnetwork is NOT, and the reason is worth knowing. Its `ro.boot.openthread_node_id` blocker is
# fixed (the VMM emits it), but its bundled `ot-rcp` uses OpenThread's simulation platform, which opens
# a multicast socket on the interface its `-L` argument names (`-Leth1`) — and this guest has only eth0.
# So `utilsInitSocket() at simul_utils.c:370: Failure` kills ot-rcp, the HAL exits 5, and four respawns
# make init reset every server-configurable flag. Add `--revert=threadnetwork` once a second guest NIC
# exists.
if [ "${SKIP_PREP_VSOCK_HALS:-0}" != "1" ]; then
  echo "== prep vsock-HAL enable/disable =="
  # shellcheck disable=SC2086 # deliberate word-splitting: the var carries multiple flags
  "$HERE/prep-disable-vsock-hals.sh" "$SUPER" ${PREP_VSOCK_HALS_ARGS-"--revert=sensors,light"}
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
