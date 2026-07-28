#!/usr/bin/env bash
# run-gate.sh — prove the host Vulkan driver can do the three things gfxstream actually stands on.
#
# `vulkaninfo` only proves the loader can enumerate a driver. This proves:
#   1. vkCreateDevice with VK_EXT_external_memory_metal enabled.
#   2. An external-memory VkImage exported as an MTLHeap. gfxstream's ColorBuffer path uses exactly
#      VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT, and KosmicKrisp supports ONLY that handle type
#      (kk_GetMemoryMetalHandleEXT asserts it) — so this is the single load-bearing capability.
#   3. Real queue submission reaching the GPU (clear + fence wait), i.e. Metal encoding works.
#
# Run this before blaming gfxstream for a graphics failure: it isolates driver from renderer.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
# paths.sh discovers the ICD with a glob that legitimately matches nothing before the first
# bootstrap; under `pipefail` that empty glob would abort here instead of reaching the check below.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
ICD="${VK_DRIVER_FILES:-$TP_KOSMICKRISP_ICD}"
[ -f "$ICD" ] || { echo "no ICD manifest at $ICD — run third_party/bootstrap.sh mesa first"; exit 1; }
BIN="$HERE/kk-gate"
if [ ! -x "$BIN" ] || [ "$HERE/kk-gate.c" -nt "$BIN" ]; then
  clang -O1 -o "$BIN" "$HERE/kk-gate.c" \
    -I"$(brew --prefix vulkan-headers)/include" -L/opt/homebrew/lib -lvulkan -Wl,-rpath,/opt/homebrew/lib
fi
echo "ICD: $ICD"
VK_DRIVER_FILES="$ICD" exec "$BIN"
