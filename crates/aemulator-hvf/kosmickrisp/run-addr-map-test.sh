#!/usr/bin/env bash
# run-addr-map-test.sh — test the VkDeviceAddress -> id<MTLBuffer> resolution.
#
# Two legs, both cheap, neither needing a booted guest:
#
#   1. addr-map-test.c   the unit test for kk_addr_map. It #includes
#                        src/kosmickrisp/vulkan/kk_device_address.c directly, so
#                        it tests the real driver source with no mesa build and
#                        no GPU: edge cases, a 400k-operation model check against
#                        a linear reference implementation, a concurrency leg, and
#                        the publish/lookup cost numbers the header cites.
#                        Also built and run under -fsanitize=thread, without
#                        which the concurrency leg proves very little.
#
#   2. mtl-heap-addr-probe.m
#                        the pure-Metal facts the resolution rests on: that a
#                        heap-placed MTLBuffer's gpuAddress really is
#                        wholeHeapBuffer.gpuAddress + offset, and that coexisting
#                        allocations never share a virtual range. If leg 1 passes
#                        and this fails, the algorithm is fine and the Metal
#                        assumption is not.
#
#   ./run-addr-map-test.sh            # both legs
#   ./run-addr-map-test.sh --quick    # skip TSan and the cost measurement
#
# KK_ADDR_DEBUG=1 in the environment makes the driver itself trace every publish,
# withdraw and failed lookup, which is how to see what a real workload puts in
# the map: KK_ADDR_DEBUG=1 ./run-gate.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
MESA_SRC="$ROOT/third_party/mesa/src"

QUICK=0
if [ "${1:-}" = "--quick" ]; then QUICK=1; shift; fi
if [ -n "${1:-}" ]; then echo "usage: $(basename "$0") [--quick]"; exit 2; fi

[ -f "$MESA_SRC/kosmickrisp/vulkan/kk_device_address.c" ] || {
  echo "no kk_device_address.c under $MESA_SRC — run third_party/bootstrap.sh mesa first"
  exit 1
}

BUILD="${TMPDIR:-/tmp}/aemu-addr-map.$USER"
mkdir -p "$BUILD"

# -DNDEBUG on purpose: the release driver is built that way, so the assert() in
# kk_addr_map_publish() is compiled out there too, and the test needs to exercise
# the refusal that remains rather than dying on the assert.
CFLAGS=(-O1 -g -DNDEBUG -Wall -Wextra -Wno-unused-parameter
        -I"$MESA_SRC" -I"$MESA_SRC/kosmickrisp/vulkan")

echo "=== kk_addr_map unit test ==="
clang "${CFLAGS[@]}" -o "$BUILD/addr-map-test" "$HERE/addr-map-test.c"
if [ "$QUICK" = 1 ]; then
  "$BUILD/addr-map-test" --quick
else
  "$BUILD/addr-map-test"

  echo
  echo "=== the same, under -fsanitize=thread ==="
  clang "${CFLAGS[@]}" -fsanitize=thread -o "$BUILD/addr-map-test-tsan" \
      "$HERE/addr-map-test.c"
  TSAN_OPTIONS=halt_on_error=1 "$BUILD/addr-map-test-tsan" --quick
fi

echo
echo "=== pure Metal: the placement address invariant, no Vulkan, no KosmicKrisp ==="
clang -fobjc-arc -O1 -o "$BUILD/mtl-heap-addr-probe" "$HERE/mtl-heap-addr-probe.m" \
    -framework Metal -framework Foundation
"$BUILD/mtl-heap-addr-probe"
