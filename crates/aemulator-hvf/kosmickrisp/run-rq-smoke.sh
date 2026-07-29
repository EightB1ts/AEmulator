#!/usr/bin/env bash
# run-rq-smoke.sh — does VK_KHR_ray_query work in the host Vulkan driver?
#
# run-gate.sh proves the three things gfxstream stands on. This proves the ray
# tracing half: it compiles rq-smoke.comp (a rayQueryEXT against a one-triangle
# TLAS) into a SPIR-V header, builds rq-smoke.c against the KosmicKrisp ICD, and
# runs it. rq-smoke.c's own header explains what each check means and how to read
# a partial pass; the short version is
#
#   16/16  ray query works, real hit and real miss.
#   12/16  everything except the hit path works. Expected on Apple8 (M2) and
#          earlier, which have no Metal 4 ray tracing — see mtl4-rt-matrix.m.
#   other  a real driver bug: some rays hit, with the wrong answer.
#
#   ./run-rq-smoke.sh              # against the ICD paths.sh discovers
#   ./run-rq-smoke.sh --matrix     # also build+run the pure-Metal probe, which
#                                  # isolates Metal from the driver entirely
#
# The probe is the control for a 12/16: it builds the same BLAS+TLAS through both
# the legacy MTLAccelerationStructureCommandEncoder and the Metal 4 compute
# encoder and traces all four combinations. If only the legacy/legacy row hits,
# the driver is not what is broken. Add METAL_DEVICE_WRAPPER_TYPE=1 and Metal API
# validation names the reason in one line.
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

command -v glslangValidator >/dev/null \
    || { echo "glslangValidator not on PATH (brew install glslang)"; exit 1; }

BUILD="${TMPDIR:-/tmp}/aemu-rq-smoke.$USER"
mkdir -p "$BUILD"

glslangValidator --target-env vulkan1.2 -V --vn rqsmoke_spv \
    -o "$BUILD/rqsmoke_spv.h" "$HERE/rq-smoke.comp" >/dev/null

clang -O1 -g -o "$BUILD/rq-smoke" "$HERE/rq-smoke.c" -I"$BUILD" \
    -I"$(brew --prefix vulkan-headers)/include" \
    -L/opt/homebrew/lib -lvulkan -Wl,-rpath,/opt/homebrew/lib

if [ "$MATRIX" = 1 ]; then
    clang -fobjc-arc -O1 -o "$BUILD/mtl4-rt-matrix" "$HERE/mtl4-rt-matrix.m" \
        -framework Metal -framework Foundation
    echo "=== pure Metal: which build path traces on this GPU? ==="
    "$BUILD/mtl4-rt-matrix"
    echo
fi

echo "ICD: $ICD"
# `exec` so the harness's exit status is this script's: 0 only on 16/16.
VK_DRIVER_FILES="$ICD" exec "$BUILD/rq-smoke"
