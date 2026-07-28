#!/usr/bin/env bash
# third_party/paths.sh — resolve every out-of-tree artifact path from ONE place.
#
# Source this instead of hardcoding a path. Every consumer (build.rs via env, ci/hvf-doctor.sh, the
# run-*.sh harnesses, kosmickrisp/run-gate.sh) went through a period of each spelling these paths
# itself, which is how a repo move managed to break the gfxstream build while every script still
# "looked right". One definition, and every override is an env var checked here.
#
#   . third_party/paths.sh
#   echo "$TP_GFXSTREAM_BUILD_HOST"  "$TP_KOSMICKRISP_ICD"
#
# Overrides (all optional, useful for a checkout somewhere else or a prebuilt dep):
#   AEMU_THIRD_PARTY   — root holding the checkouts (default: <repo>/third_party)
#   GFXSTREAM_SRC / GFXSTREAM_BUILD_HOST / AEMU_SRC / MESA_SRC / KOSMICKRISP_PREFIX

# Resolve the repo root from THIS file, never from $PWD or a caller-relative guess.
_tp_self="${BASH_SOURCE[0]:-$0}"
TP_DIR="$(cd "$(dirname "$_tp_self")" && pwd)"
TP_ROOT="$(cd "$TP_DIR/.." && pwd)"

# Where the checkouts live. Contents are gitignored; only the manifest, patches and scripts are tracked.
TP_CHECKOUTS="${AEMU_THIRD_PARTY:-$TP_DIR}"

TP_GFXSTREAM_SRC="${GFXSTREAM_SRC:-$TP_CHECKOUTS/gfxstream}"
TP_GFXSTREAM_BUILD_HOST="${GFXSTREAM_BUILD_HOST:-$TP_GFXSTREAM_SRC/build-host}"
TP_GFXSTREAM_DYLIB="$TP_GFXSTREAM_BUILD_HOST/libgfxstream_backend.dylib"

TP_AEMU_SRC="${AEMU_SRC:-$TP_CHECKOUTS/aemu}"
TP_AEMU_INSTALL="$TP_AEMU_SRC/install"
TP_AEMU_PKGCONFIG="$TP_AEMU_INSTALL/lib/pkgconfig"

TP_MESA_SRC="${MESA_SRC:-$TP_CHECKOUTS/mesa}"
TP_KOSMICKRISP_PREFIX="${KOSMICKRISP_PREFIX:-$TP_CHECKOUTS/kosmickrisp-install}"
TP_KOSMICKRISP_DYLIB="$TP_KOSMICKRISP_PREFIX/lib/libvulkan_kosmickrisp.dylib"
# The ICD basename is arch-qualified by mesa's meson as kosmickrisp_mesa_icd.<host_machine.cpu()>.json.
# DISCOVER it rather than reconstructing it: meson's cpu name is not `uname -m` — on Apple Silicon it
# emits `aarch64` while `uname -m` says `arm64`, so a hand-built name silently misses the file.
TP_KOSMICKRISP_ICD_DIR="$TP_KOSMICKRISP_PREFIX/share/vulkan/icd.d"
TP_KOSMICKRISP_ICD="${KOSMICKRISP_ICD:-$(ls "$TP_KOSMICKRISP_ICD_DIR"/kosmickrisp_mesa_icd.*.json 2>/dev/null | head -1)}"
# Fall back to the aarch64 name so error messages name a concrete path before the first build.
: "${TP_KOSMICKRISP_ICD:=$TP_KOSMICKRISP_ICD_DIR/kosmickrisp_mesa_icd.aarch64.json}"
TP_KOSMICKRISP_ICD_BASENAME="$(basename "$TP_KOSMICKRISP_ICD")"

# Python venv mesa's meson needs (see bootstrap.sh for why it must be first on PATH).
TP_VENV="$TP_CHECKOUTS/.venv"

unset _tp_self
