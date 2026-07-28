#!/usr/bin/env bash
# build-gfxstream.sh — SUPERSEDED by third_party/bootstrap.sh. Kept as a forwarding shim so the
# name in PHASE3-STATUS.md / PHASE3A-STATUS.md and in muscle memory still does the right thing.
#
# What this script used to do — clone aemu by hand, hand-write the egl/libdrm stub .pc files macOS
# needs for gfxstream's DEPENDENCY_RESOLUTION=SYSTEM path, then configure and build the host
# renderer — bootstrap.sh now does from the pinned manifest, plus the two guards this script lacked:
#   * it detects a cmake cache configured for a different absolute source path and reconfigures,
#     instead of letting ninja no-op and leave a months-old dylib in place after a repo move;
#   * it checks our tracked patch series actually reached the dylib (nm on the present entry point),
#     instead of shipping an unpatched renderer that fails at run time rather than at build time.
#
# aemu is built too, not only gfxstream: gfxstream's configure consumes aemu's generated .pc files.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# <root>/crates/aemulator-hvf/gfxstream -> <root>; never hardcode an absolute path.
ROOT="$(cd "$HERE/../../.." && pwd)"
exec "$ROOT/third_party/bootstrap.sh" aemu gfxstream "$@"
