#!/usr/bin/env bash
# run-display-selftest.sh — Phase 3b proof harness.
#
#   ./run-display-selftest.sh            # pure-Metal CAMetalLayer presenter proof (no Vulkan)
#   ./run-display-selftest.sh --gfxstream [--gles]   # + stream_renderer_init + setup_window
#
# The gfxstream mode stages the KosmicKrisp loader/ICD exactly like gfxstream/run.sh: with
# ANDROID_EMU_VK_ICD non-empty, gfxstream restricts the Vulkan LOADER search to <programDir>/lib64/vulkan/,
# so we build that dir next to the built binary.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# <root>/crates/aemulator-hvf -> <root>. Out-of-tree paths come from third_party/paths.sh only.
ROOT="$(cd "$HERE/../.." && pwd)"
# paths.sh discovers the ICD with a glob that legitimately matches nothing before the first
# bootstrap; under `pipefail` that empty glob would abort here instead of reaching the check below.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
BIN="$HERE/target/debug/aemu-display-selftest"
BREW_LIB="/opt/homebrew/lib"
OUT="${OUT:-/tmp/aemu-3b-selftest.png}"

cargo build --manifest-path "$HERE/Cargo.toml"

# Hypervisor entitlement is harmless here (the selftest doesn't use HVF); ad-hoc sign for the GPU.
codesign --sign - --entitlements "$HERE/hv.entitlements" --force "$BIN" 2>/dev/null || true

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
export ANDROID_EMUGL_VERBOSE=1

echo "== run: $BIN $* (out=$OUT) =="
exec "$BIN" --out "$OUT" "$@"
