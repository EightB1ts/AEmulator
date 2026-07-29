#!/usr/bin/env bash
# run-gs-smoke.sh — does a Vulkan geometry shader render on KosmicKrisp?
#
# Metal has no geometry stage; KosmicKrisp emulates one with a compute prepass
# plus libpoly's geometry lowering. gs-smoke.c's header explains what each check
# proves and why "a triangle appeared" is not enough evidence. The short version:
#
#   37/37 an ordinary VS+FS draw still renders (the control), all four output
#         shapes render with the right per-vertex data and the right
#         gl_PrimitiveIDIn, per-primitive gl_Layer routes to the right array
#         layer, the two shapes whose index buffer needs primitive restart come
#         out unrolled into independent primitives rather than stitched, and a
#         dynamic-topology pipeline survives a pipeline-cache blob round trip.
#   fewer read WHICH check failed — every one names a specific semantic. Check 0
#         first: it is the control, and if it fails the geometry work broke every
#         ordinary draw in the driver and nothing else in the run means anything.
#
# geometryShader is an OPT-IN feature bit (KK_DEBUG_GS), because transform
# feedback, multiple vertex streams, geometry-after-tessellation and indirect
# draws are not implemented. This script sets MESA_KK_DEBUG=gs; without it the
# harness stops at the first check by design.
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

BUILD="${TMPDIR:-/tmp}/aemu-gs-smoke.$USER"
mkdir -p "$BUILD"

# One header per shader. The variable names must match the externs gs-smoke.c
# expects, which is why --vn is spelled out per file rather than derived.
compile() { # <source> <variable name>
    glslangValidator --target-env vulkan1.2 -V --vn "$2" \
        -o "$BUILD/$2.h" "$HERE/$1" >/dev/null
}
compile gs-smoke.vert            gs_smoke_vert
compile gs-smoke-plain.vert      gs_smoke_plain_vert
compile gs-smoke.frag            gs_smoke_frag
compile gs-smoke-strip.geom      gs_smoke_strip_geom
compile gs-smoke-list.geom       gs_smoke_list_geom
compile gs-smoke-layer.geom      gs_smoke_layer_geom
compile gs-smoke-multistrip.geom gs_smoke_multistrip_geom
compile gs-smoke-dyn.geom        gs_smoke_dyn_geom

cat > "$BUILD/gs_smoke_spv.h" <<'EOF'
#include "gs_smoke_vert.h"
#include "gs_smoke_plain_vert.h"
#include "gs_smoke_frag.h"
#include "gs_smoke_strip_geom.h"
#include "gs_smoke_list_geom.h"
#include "gs_smoke_layer_geom.h"
#include "gs_smoke_multistrip_geom.h"
#include "gs_smoke_dyn_geom.h"
EOF

clang -O1 -g -std=c11 -o "$BUILD/gs-smoke" "$HERE/gs-smoke.c" -I"$BUILD" \
    -I"$(brew --prefix vulkan-headers)/include" \
    -L/opt/homebrew/lib -lvulkan -Wl,-rpath,/opt/homebrew/lib

echo "ICD: $ICD"
MESA_KK_DEBUG="${MESA_KK_DEBUG:-gs}" VK_DRIVER_FILES="$ICD" exec "$BUILD/gs-smoke"
