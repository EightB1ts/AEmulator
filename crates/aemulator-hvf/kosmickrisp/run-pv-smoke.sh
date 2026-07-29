#!/usr/bin/env bash
# run-pv-smoke.sh — does VK_EXT_provoking_vertex pick the right vertex on
# KosmicKrisp, in both modes, on every path that produces a primitive?
#
# Metal is FIRST-vertex provoking and has no state to change that. Vulkan's
# last-vertex mode — which ANGLE requests on every pipeline it builds, and
# without which ANGLE caps the guest at OpenGL ES 2.0 — is emulated by
# decomposing each draw into independent primitives and rotating them so the
# provoking vertex lands in the slot Metal reads. pv-smoke.c's header explains
# what each group proves. The short version:
#
#   28/28 the extension is advertised with provokingVertexLast VK_TRUE, a
#         flat-shaded triangle STRIP names vertices 0,1,2,3 in first-vertex mode
#         and 2,3,4,5 in last-vertex mode (the case where the two modes actually
#         diverge), both modes rasterize pixel-identical coverage, lists, fans,
#         line topologies and indexed draws all follow, two pipelines with
#         different modes work inside one render pass, a pipeline with no flat
#         input renders byte-identically in both modes, and all four geometry
#         shader output shapes agree with the rasterizer.
#   fewer read WHICH check failed — every one names a specific semantic and
#         prints the vertex index that actually won next to the one that should
#         have. Group 1 first: it is the core claim and everything else is a
#         different producer of the same answer.
#
# BEFORE the driver implemented the extension this script could not run at all:
# check 0 fails on the very first line and the harness exits, because every
# other case needs the extension enabled on the device.
#
# The geometry-shader group needs the opt-in geometryShader bit, so this sets
# MESA_KK_DEBUG=gs the way run-gs-smoke.sh does. Without it those cases are
# counted as failures rather than skipped: the geometry paths are where the
# provoking vertex is subtlest.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

# paths.sh globs for the ICD, and that glob legitimately matches nothing before
# the first bootstrap; under pipefail an empty glob would abort before the check.
set +o pipefail; . "$ROOT/third_party/paths.sh"; set -o pipefail
ICD="${VK_DRIVER_FILES:-$TP_KOSMICKRISP_ICD}"
[ -f "$ICD" ] || { echo "no ICD manifest at $ICD — run third_party/bootstrap.sh mesa first"; exit 1; }

command -v glslangValidator >/dev/null \
    || { echo "glslangValidator not on PATH (brew install glslang)"; exit 1; }

BUILD="${TMPDIR:-/tmp}/aemu-pv-smoke.$USER"
mkdir -p "$BUILD"

# One header per shader. The variable names must match the externs pv-smoke.c
# expects, which is why --vn is spelled out per file rather than derived.
compile() { # <source> <variable name>
    glslangValidator --target-env vulkan1.2 -V --vn "$2" \
        -o "$BUILD/$2.h" "$HERE/$1" >/dev/null
}
compile pv-smoke.vert         pv_smoke_vert
compile pv-smoke-point.vert   pv_smoke_point_vert
compile pv-smoke.frag         pv_smoke_frag
compile pv-smoke-smooth.frag  pv_smoke_smooth_frag
compile pv-smoke-quad.geom    pv_smoke_quad_geom
compile pv-smoke-tri.geom     pv_smoke_tri_geom
compile pv-smoke-2strip.geom  pv_smoke_2strip_geom
compile pv-smoke-dyn.geom     pv_smoke_dyn_geom

cat > "$BUILD/pv_smoke_spv.h" <<'EOF'
#include "pv_smoke_vert.h"
#include "pv_smoke_point_vert.h"
#include "pv_smoke_frag.h"
#include "pv_smoke_smooth_frag.h"
#include "pv_smoke_quad_geom.h"
#include "pv_smoke_tri_geom.h"
#include "pv_smoke_2strip_geom.h"
#include "pv_smoke_dyn_geom.h"
EOF

clang -O1 -g -std=c11 -o "$BUILD/pv-smoke" "$HERE/pv-smoke.c" -I"$BUILD" \
    -I"$(brew --prefix vulkan-headers)/include" \
    -L/opt/homebrew/lib -lvulkan -Wl,-rpath,/opt/homebrew/lib

echo "ICD: $ICD"
MESA_KK_DEBUG="${MESA_KK_DEBUG:-gs}" VK_DRIVER_FILES="$ICD" exec "$BUILD/pv-smoke"
