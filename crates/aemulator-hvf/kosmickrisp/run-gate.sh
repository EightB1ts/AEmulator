#!/usr/bin/env bash
# run-gate.sh — prove the host Vulkan driver can do the three things gfxstream actually stands on,
# and (--dump-identity) record what that driver reports about itself.
#
# `vulkaninfo` only proves the loader can enumerate a driver. This proves:
#   1. vkCreateDevice with VK_EXT_external_memory_metal enabled.
#   2. An external-memory VkImage exported as an MTLHeap. gfxstream's ColorBuffer path uses exactly
#      VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT, and KosmicKrisp supports ONLY that handle type
#      (kk_GetMemoryMetalHandleEXT asserts it) — so this is the single load-bearing capability.
#   3. Real queue submission reaching the GPU (clear + fence wait), i.e. Metal encoding works.
#
# Run this before blaming gfxstream for a graphics failure: it isolates driver from renderer.
#
#   ./run-gate.sh                    # the capability gate (default, unchanged)
#   ./run-gate.sh --dump-identity    # refresh gfxstream/evidence/host-vulkan-identity.json
#   ./run-gate.sh --dump-identity -  # ...to stdout instead, for an ad-hoc diff
#
# --dump-identity captures the Phase-0.6 identity ground truth: the full VkPhysicalDeviceProperties2
# chain, every feature bool, the sorted extension list, the BC1..BC7 format properties and the first
# 32 bytes of vkGetPipelineCacheData. Every GPU-identity change diffs against that file, so the write
# goes through a temp file and only replaces the golden on success — a driver that fails halfway must
# not leave a truncated baseline behind.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

MODE=gate
OUT="$ROOT/crates/aemulator-hvf/gfxstream/evidence/host-vulkan-identity.json"
if [ "${1:-}" = "--dump-identity" ]; then
  MODE=dump; shift
  # Spelled as a full `if` rather than `[ … ] && OUT=…`: under `set -e` that form's exit status when
  # the test is false is a trap, not a convenience.
  if [ -n "${1:-}" ]; then OUT="$1"; fi
elif [ -n "${1:-}" ]; then
  echo "usage: $(basename "$0") [--dump-identity [outfile|-]]"; exit 2
fi

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
if [ "$MODE" = gate ]; then
  echo "ICD: $ICD"
  VK_DRIVER_FILES="$ICD" exec "$BIN"
fi

# Dump mode: stdout belongs to the JSON and nothing else. Every progress line goes to stderr, so
# `--dump-identity -` can be piped straight into a diff — one stray "ICD:" line would corrupt it.
echo "ICD: $ICD" >&2

if [ "$OUT" = - ]; then
  VK_DRIVER_FILES="$ICD" exec "$BIN" --dump-identity
fi

mkdir -p "$(dirname "$OUT")"
TMP="$OUT.tmp.$$"
trap 'rm -f "$TMP"' EXIT
VK_DRIVER_FILES="$ICD" "$BIN" --dump-identity >"$TMP"
mv "$TMP" "$OUT"
trap - EXIT
echo "identity: $OUT" >&2
# The fields Phase 4 exists to change, echoed so a run is self-verifying without opening the file.
# The AgreesWithProperties flags read true today — the pipeline-cache header still carries the real
# host vendorID/deviceID/pipelineCacheUUID. Phase 4.9 is what turns them false.
# Anchored: the `why` prose in the same object also mentions AgreesWithProperties.
grep -E '^ +"(deviceName|driverName|vendorIDHex|deviceIDHex|pipelineCacheUUID|[a-zA-Z]+AgreesWithProperties)": ' "$OUT" \
  | sed 's/^ *//; s/,$//; s/^/  /' >&2
