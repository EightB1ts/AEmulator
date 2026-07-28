#!/usr/bin/env bash
# build-kosmickrisp.sh — SUPERSEDED by third_party/bootstrap.sh. Kept as a forwarding shim so the
# name in PHASE-KOSMICKRISP-STATUS.md and in muscle memory still does the right thing.
#
#   ./build-kosmickrisp.sh          # build + install the driver
#   ./build-kosmickrisp.sh --gate   # ...then run the capability gate
#
# This script cloned mesa `--depth 1` at whatever `main` happened to be that day. bootstrap.sh builds
# the same driver from the SHA pinned in third_party/MANIFEST.sh — the pin is load-bearing, since
# KK_WORKAROUND_15 fixes an MSL miscompile that only manifests on the macOS version we run — and adds
# the guards this script lacked: it reconfigures a meson build dir that was configured for a
# different absolute source path, recreates the python venv after a move breaks its shebangs, and
# fails if the installed ICD manifest names a driver that is not there (the Vulkan loader treats an
# unloadable ICD as *no* ICD, silently). The three build constraints this header used to spell out
# (libclc required, venv first on PATH, every compiler pinned to Apple clang) are enforced there.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# <root>/crates/aemulator-hvf/kosmickrisp -> <root>; never hardcode an absolute path.
ROOT="$(cd "$HERE/../../.." && pwd)"

# --gate is consumed here rather than forwarded: bootstrap.sh rejects arguments it does not know.
GATE=0
[ "${1:-}" = "--gate" ] && { GATE=1; shift; }

"$ROOT/third_party/bootstrap.sh" mesa "$@"

if [ "$GATE" = 1 ]; then
  exec "$HERE/run-gate.sh"
fi
