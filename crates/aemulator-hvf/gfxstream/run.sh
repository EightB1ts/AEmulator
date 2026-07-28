#!/usr/bin/env bash
# run.sh — build + run the gfxstream host-renderer probe on Mesa KosmicKrisp.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# Derive the repo root from this script's location — never hardcode an absolute path; the tree
# gets relocated and a stale constant bakes a dead -rpath into the probe.
# <root>/crates/aemulator-hvf/gfxstream -> <root>
ROOT="$(cd "$HERE/../../.." && pwd)"
# paths.sh discovers the ICD with a glob that legitimately matches nothing before the first
# bootstrap; under `pipefail` that empty glob would abort here instead of reaching the check below.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
BREW_LIB="$(brew --prefix 2>/dev/null || echo /opt/homebrew)/lib"

# 1. Compile + link against the host renderer dylib (rpath so it resolves at run time).
cc -g -O0 \
    -o "$HERE/probe" \
    "$HERE/probe.c" \
    -L"$TP_GFXSTREAM_BUILD_HOST" -lgfxstream_backend \
    -Wl,-rpath,"$TP_GFXSTREAM_BUILD_HOST"

# 2. Ad-hoc codesign (touching the GPU under HVF-adjacent frameworks is happier signed).
codesign -s - --force "$HERE/probe" 2>/dev/null || true

# 3. Host Vulkan driver: Mesa KosmicKrisp (Metal). MoltenVK is gone; macOS 26+ required.
[ -f "$TP_KOSMICKRISP_ICD" ] || {
  echo "no KosmicKrisp ICD at $TP_KOSMICKRISP_ICD - run third_party/bootstrap.sh mesa"; exit 1; }
#    The Vulkan LOADER must still be staged here: VulkanDispatch::getPossibleLoaderPaths()
#    restricts the libvulkan.dylib search to <programDir>/lib64/vulkan (and
#    <launcherDir>/lib64/vulkan) whenever ANDROID_EMU_VK_ICD is non-empty. So we build that
#    directory next to ./probe and symlink the Homebrew loader in.
VKDIR="$HERE/lib64/vulkan"
mkdir -p "$VKDIR"
ln -sf "$BREW_LIB/libvulkan.dylib" "$VKDIR/libvulkan.dylib"
# Staged manifest = the fallback path gfxstream uses when no VK_DRIVER_FILES is exported
# (i.e. what a shipped bundle would do). Re-copied every run, so it cannot go stale.
cp -f "$TP_KOSMICKRISP_ICD" "$VKDIR/$TP_KOSMICKRISP_ICD_BASENAME"

# 4. Authoritative driver selection: gfxstream now explicitly honors a caller-supplied
#    VK_DRIVER_FILES, and this is the same manifest kosmickrisp/run-gate.sh drives, so the
#    probe and the gate provably load the same driver binary. ANDROID_EMUGL_VERBOSE=1
#    surfaces the chosen physical device in the log.
export VK_DRIVER_FILES="$TP_KOSMICKRISP_ICD"
export ANDROID_EMU_VK_ICD=kosmickrisp
export ANDROID_EMUGL_VERBOSE=1

echo "== env =="
echo "ANDROID_EMU_VK_ICD=$ANDROID_EMU_VK_ICD"
echo "loader:    $(ls -l "$VKDIR/libvulkan.dylib" | sed 's/.* -> //')"
echo "icd:       $VK_DRIVER_FILES"
echo "== run =="
exec "$HERE/probe"
