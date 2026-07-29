#!/usr/bin/env bash
# run-sparse-smoke.sh — does sparse binding work in the host Vulkan driver?
#
# run-gate.sh proves the three things gfxstream stands on. This proves sparse
# binding: it builds sparse-smoke.c against the KosmicKrisp ICD and runs it.
# sparse-smoke.c's own header says what each check means; the short version is
#
#   32/35  sparse binding works. The three failures are the UNBIND checks, a
#          known and documented driver limitation (see kk_sparse.c).
#   35/35  unbinding started working too — update both headers.
#   other  a real regression; the header lists which checks are load-bearing.
#
#   ./run-sparse-smoke.sh              # against the ICD paths.sh discovers
#   ./run-sparse-smoke.sh --matrix     # also build+run the pure-Metal probes,
#                                      # which isolate Metal from the driver
#
# KK_SPARSE_DEBUG=1 in the environment makes the driver print every mapping
# update as it is issued, which is how to tell a translation bug in kk from a
# Metal behaviour.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

MATRIX=0
if [ "${1:-}" = "--matrix" ]; then MATRIX=1; shift; fi
if [ -n "${1:-}" ]; then echo "usage: $(basename "$0") [--matrix]"; exit 2; fi

# paths.sh globs for the ICD, and that glob legitimately matches nothing before
# the first bootstrap; under pipefail an empty glob would abort before the check.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
ICD="${VK_DRIVER_FILES:-$TP_KOSMICKRISP_ICD}"
[ -f "$ICD" ] || { echo "no ICD manifest at $ICD — run third_party/bootstrap.sh mesa first"; exit 1; }

BUILD="${TMPDIR:-/tmp}/aemu-sparse-smoke.$USER"
mkdir -p "$BUILD"

clang -O1 -g -o "$BUILD/sparse-smoke" "$HERE/sparse-smoke.c" \
    -I"$(brew --prefix vulkan-headers)/include" \
    -L/opt/homebrew/lib -lvulkan -Wl,-rpath,/opt/homebrew/lib

if [ "$MATRIX" = 1 ]; then
  echo "=== pure Metal: placement sparse, no Vulkan and no KosmicKrisp ==="
  clang -fobjc-arc -O1 -o "$BUILD/mtl4-sparse-matrix" "$HERE/mtl4-sparse-matrix.m" \
      -framework Metal -framework Foundation
  "$BUILD/mtl4-sparse-matrix" || true
  echo
fi

echo "ICD: $ICD"
VK_DRIVER_FILES="$ICD" exec "$BUILD/sparse-smoke"
