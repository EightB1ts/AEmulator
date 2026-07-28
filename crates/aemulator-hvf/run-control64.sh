#!/usr/bin/env bash
# run-control64.sh — Phase 6: boot the 64-bit-only Cuttlefish image under HVF and serve the
# host-driven CONTROL PLANE (framebuffer capture + virtio-input injection) over a Unix socket +
# stdin REPL. No adb, no in-guest accessibility service — all host-side.
#
#   ./run-control64.sh                         # boot + serve control (socket /tmp/aemulator-hvf-control.sock)
#   ./run-control64.sh --socket /tmp/x.sock    # custom socket
#   ./run-control64.sh --repl                  # + interactive stdin REPL
#   ./run-control64.sh --script cmds.txt       # run a command file at startup, then keep serving
#   SWRENDER=1 ./run-control64.sh              # guest-CPU SwiftShader/ANGLE render into the scanout
#
# This is IDENTICAL image prep + build + sign to run-android64.sh; only the final invocation changes
# (`run-android --control` instead of streaming the console). Drive it with the one-shot client:
#   ./target/debug/aemulator-hvf tap 500 400
#   ./target/debug/aemulator-hvf screenshot /tmp/shot.png
# or wrap the socket with a thin MCP server (see PHASE6-STATUS.md § MCP-wrap seam).
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

# virtio-net always uses the real libslirp user-mode NAT backend (compiled in unconditionally)
# (needs `brew install libslirp`). Default hostfwd TCP 5555->5555; extend via AEMU_HOSTFWD.
WANT_GFX=0
GPU3D=0
for a in "$@"; do [ "$a" = "--gpu3d" ] && { GPU3D=1; WANT_GFX=1; }; done

GPU_PROPS="${GPU_PROPS-androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=ranchu androidboot.hardware.gltransport=virtio-gpu-asg androidboot.hardware.gltransport.drawFlushInterval=800 androidboot.opengles.version=196609}"
SWRENDER_PROPS="androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=pastel androidboot.opengles.version=196609"
if [ "${SWRENDER:-0}" = "1" ]; then GPU_PROPS="$SWRENDER_PROPS"; GPU3D=1; fi

# Idempotent image prep — SAME scripts as run-android64.sh (skip each with its SKIP_* env).
[ "${SKIP_PREP_SUPER:-0}" != "1" ] && { echo "== prep super.raw.img =="; "$HERE/prep-super.sh" "$SUPER"; }
[ "${SKIP_PREP_DATA:-0}" != "1" ] && { echo "== prep /data unencrypted =="; "$HERE/prep-data-unencrypted.sh" "$SUPER"; }
# NOT prep-disable-32bit-dualabi.sh — it is for the dual-ABI `cf` image only. Applying it here
# disabled mediaserver (arm64 and healthy on this image), which cost us media.player +
# media.resource_manager and a ~1 Hz lazy-start retry storm. See run-android64.sh's header.
[ "${SKIP_PREP_APEX:-0}" != "1" ] && { echo "== prep apex dedup =="; "$HERE/prep-apex-dedup.sh" "$SUPER"; }
# PREP_VSOCK_HALS_ARGS forwards per-HAL flags (--revert=/--probe=) so a re-enable survives a re-run;
# see run-android64.sh for why sensors+light are on by default and threadnetwork is not.
# shellcheck disable=SC2086 # deliberate word-splitting: the var carries multiple flags
[ "${SKIP_PREP_VSOCK_HALS:-0}" != "1" ] && { echo "== prep vsock-HAL enable/disable =="; "$HERE/prep-disable-vsock-hals.sh" "$SUPER" ${PREP_VSOCK_HALS_ARGS-"--revert=sensors,light"}; }
[ "${SKIP_PREP_DEBRAND:-0}" != "1" ] && { echo "== prep de-brand identity =="; "$HERE/prep-debrand.sh" "$SUPER"; }

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

echo "== run-android --control (--cf-dir $CF) =="
if [ "$GPU3D" = "1" ]; then
  exec "$BIN" run-android --control "$@" --cf-dir "$CF" --extra-cmdline "$GPU_PROPS"
else
  exec "$BIN" run-android --control "$@" --cf-dir "$CF"
fi
