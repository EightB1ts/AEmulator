#!/usr/bin/env bash
# run-rq-smoke.sh — does VK_KHR_ray_query work in the host Vulkan driver?
#
# run-gate.sh proves the three things gfxstream stands on. This proves the ray
# tracing half: it compiles rq-smoke.comp (a rayQueryEXT against a one-triangle
# TLAS) into a SPIR-V header, builds rq-smoke.c against the KosmicKrisp ICD, and
# runs it four times — identity transforms, a non-identity instance transform, a
# non-identity per-triangle transform, and one resubmission of the same command
# buffer. rq-smoke.c's own header explains what each check means, why those modes
# exist and how to read a partial pass; the short version is
#
#   16/16 x4  ray query works, real hit and real miss, a row-major 3x4
#             VkTransformMatrixKHR is applied as a row-major 3x4, and a resubmitted
#             command buffer rebuilds its structures on replay.
#   12/16     everything except the hit path works: the structure was built and
#             intersects nothing. What the Metal 4 builder produces on an Apple8
#             GPU, reproducible on purpose with MESA_KK_RT_LEGACY_BUILDER=0.
#   GATED     the driver advertises no ray tracing, because it has no builder that
#             works on this device. 0 of 16 run, and that is the correct answer.
#   other     a real driver bug: some rays hit, with the wrong answer.
#
#   ./run-rq-smoke.sh              # against the ICD paths.sh discovers
#   ./run-rq-smoke.sh --matrix     # also build+run the pure-Metal probes, which
#                                  # isolate Metal from the driver entirely
#
#   MESA_KK_RT_DEBUG=1 ./run-rq-smoke.sh
#                                  # print the driver's capability verdict and
#                                  # which of Metal's two builders it chose
#   MESA_KK_RT_LEGACY_BUILDER=0 ./run-rq-smoke.sh
#                                  # force the Metal 4 builder. On an Apple8 that
#                                  # is the 12/16, and it is what keeps "the
#                                  # legacy builder is what fixed the hit"
#                                  # measurable rather than merely asserted.
#   MESA_KK_FORCE_RAY_TRACING=1 ./run-rq-smoke.sh
#                                  # advertise ray tracing on a device the gate
#                                  # rejects, to exercise the path anyway
#
# The pure-Metal probes are the control for a partial pass. mtl4-rt-matrix builds
# the same BLAS+TLAS through both Metal builders and traces all four combinations;
# if only the legacy/legacy row hits, the driver is not what is broken. Add
# METAL_DEVICE_WRAPPER_TYPE=1 and Metal API validation names the reason in one
# line. mtl-legacy-as-probe answers the four questions the driver needs the legacy
# builder to answer that a standalone probe would otherwise get for free — buffer
# offsets, instanceCountBuffer, cross-queue event ordering, residency — and
# mtl-rt-matrix-layout checks the matrix layout with no driver in the picture.
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
    # The remaining probes are advice about kk rather than gates on this run, so a
    # non-zero status must not abort the harness under `set -e`.
    clang -fobjc-arc -O1 -o "$BUILD/mtl-legacy-as-probe" \
        "$HERE/mtl-legacy-as-probe.m" -framework Metal -framework Foundation
    echo "=== pure Metal: does the legacy builder accept kk's shapes? ==="
    "$BUILD/mtl-legacy-as-probe" || true
    echo
    clang -fobjc-arc -O1 -o "$BUILD/mtl-rt-matrix-layout" \
        "$HERE/mtl-rt-matrix-layout.m" -framework Metal -framework Foundation
    echo "=== pure Metal: is MTLMatrixLayoutRowMajor honoured on this GPU? ==="
    "$BUILD/mtl-rt-matrix-layout" || true
    echo
fi

echo "ICD: $ICD"
export VK_DRIVER_FILES="$ICD"

# All four modes, and this script's status is the worst of them. None of the extra
# modes is optional: an identity transform is its own transpose, so without the
# transform modes a 16/16 says nothing about whether kk's row-major matrix layout
# is right and a wrong layout stays invisible until an application rotates
# something; and --resubmit is the only thing here that exercises kk's replay of a
# command buffer submitted twice, which has to rebuild the acceleration structures
# from scratch because Metal command buffers are single-shot.
status=0
for mode in "" "--transform" "--triangle-transform" "--resubmit"; do
    echo
    echo "--- rq-smoke ${mode:-(identity transforms)} ---"
    # shellcheck disable=SC2086  # deliberate: an empty $mode must expand to nothing
    "$BUILD/rq-smoke" $mode || status=$?
done
exit "$status"
