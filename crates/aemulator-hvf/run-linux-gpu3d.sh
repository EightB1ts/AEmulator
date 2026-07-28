#!/usr/bin/env bash
# run-linux-gpu3d.sh — Phase 3d integration harness.
#
# Boots the Alpine aarch64 guest under HVF with the virtio-gpu device UPGRADED to a
# VIRTIO_GPU_F_VIRGL 3D device whose control stream forwards into the gfxstream host renderer
# (libgfxstream_backend.dylib, KosmicKrisp/Metal). This is the standard build + `--gpu3d`.
#
#   ./run-linux-gpu3d.sh                 # headless: capture guest dmesg for the VIRGL/capset probe
#   ./run-linux-gpu3d.sh --display       # + open the window (2D scanout / present coupling)
#
# What this proves (honest scope): a REAL guest kernel enumerates our virtio-gpu device, negotiates
# VIRTIO_GPU_F_VIRGL, and (via the DRM virtio-gpu driver) queries the host capset through
# stream_renderer_get_cap_set + creates a render node. It does NOT run a 3D app: a general-purpose
# Linux userspace ships Mesa's *virgl* Vulkan/GL driver, not Mesa's *gfxstream_vk* — a different host
# protocol than our gfxstream renderer. The end-to-end 3D-app-on-screen proof is deferred to Phase 4
# (Android ships gfxstream_vk natively). See gfxstream/PHASE3-STATUS.md.
#
# Staging: with ANDROID_EMU_VK_ICD non-empty, gfxstream restricts the Vulkan LOADER search to
# <programDir>/lib64/vulkan/, so we build that dir next to the built binary (same as gfxstream/run.sh).
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
TD="$HERE/testdata"

echo "== build =="
cargo build --manifest-path "$MANIFEST" --bin aemulator-hvf

echo "== ad-hoc codesign with hypervisor entitlement =="
codesign --sign - --entitlements "$ENTITLEMENTS" --force "$BIN"

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

[ -f "$TD/scratch.img" ] || dd if=/dev/zero of="$TD/scratch.img" bs=1m count=64 2>/dev/null

# Authoritative selection: same manifest kosmickrisp/run-gate.sh drives, so the emulator and
# the gate provably load the same driver binary. Takes strict precedence in the loader.
export VK_DRIVER_FILES="$TP_KOSMICKRISP_ICD"
export ANDROID_EMU_VK_ICD=kosmickrisp
export ANDROID_EMUGL_VERBOSE=1

echo "== run (--gpu3d) =="
exec "$BIN" run-linux \
  --kernel "$TD/Image" \
  --initrd "$TD/initramfs-virt" \
  --disk   "$TD/scratch.img" \
  --mem-mb 1024 \
  --cpus 1 \
  --gpu3d \
  "$@"
