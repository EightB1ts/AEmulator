#!/usr/bin/env bash
#
# hvf-doctor.sh — verify the emulator binary's host dependency chain BEFORE you hit Start.
#
# Every check here corresponds to a real failure that produced a green build and a runtime crash:
#
#   1. third_party is at its pinned SHAs with   — nothing else in this script looks at the dependency
#      our gfxstream patch series applied         SOURCE trees, only at artifacts built from them. The
#                                                 patch series is the entire difference between our
#                                                 renderer and upstream's, and it spent months as
#                                                 uncommitted edits inside a foreign git checkout: if
#                                                 it stops being applied, every check below still
#                                                 passes against the dylib built when it was, and the
#                                                 next rebuild silently produces an unpatched one.
#   2. libgfxstream_backend.dylib exists        — the GPU path is a hard requirement, not optional.
#   3. the KosmicKrisp ICD + the dylib it       — KosmicKrisp (Mesa's conformant Vulkan-on-Metal
#      names exist, on macOS 26 or newer          driver) is the ONLY host Vulkan driver this build
#                                                 has: MoltenVK is gone from the render path and
#                                                 there is no fallback. A missing manifest — or a
#                                                 manifest naming a dylib that a relocated/rebuilt
#                                                 tree left behind — makes the Vulkan loader
#                                                 enumerate ZERO drivers, and gfxstream then logs
#                                                 "Failed to initialize global Vulkan emulation",
#                                                 returns 0 from stream_renderer_init anyway, and
#                                                 SEGFAULTs a moment later without ever naming
#                                                 Vulkan. Nothing else in this script would notice.
#   4. every baked-in -rpath still exists       — the tree was relocated once and left a dead rpath
#                                                 pointing at the old checkout; dyld only complains
#                                                 at launch, long after the build went green.
#   5. the binary actually loads under dyld     — the end-to-end proof of (2) and (4).
#   6. hypervisor entitlement is present        — a bare `cargo build` drops the ad-hoc signature,
#                                                 and hv_vm_create then returns HV_DENIED.
#   7. libslirp is installed + linked in        — a binary without the NAT backend enumerates a NIC
#                                                 that silently drops every packet (see spawn.rs).
#
# Usage:  bash ci/hvf-doctor.sh          # check only, exit non-zero on the first hard failure
#         bash ci/hvf-doctor.sh --fix    # additionally re-sign the binary if the entitlement is missing

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Every out-of-tree path (gfxstream dylib, KosmicKrisp ICD) comes from third_party/paths.sh only —
# this script spelling them itself is how it once kept passing against a tree that had moved.
. "$ROOT/third_party/paths.sh"
HVF="$ROOT/crates/aemulator-hvf"
BIN="$HVF/target/release/aemulator-hvf"
ENT="$HVF/hv.entitlements"
FIX=0
[ "${1:-}" = "--fix" ] && FIX=1

if [ -t 1 ]; then C_G=$'\033[32m'; C_R=$'\033[31m'; C_Y=$'\033[33m'; C_B=$'\033[1m'; C_0=$'\033[0m'
else C_G=; C_R=; C_Y=; C_B=; C_0=; fi
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '%s  ! %s%s\n' "$C_Y" "$*" "$C_0"; }
fail() { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

step "emulator binary present"
[ -x "$BIN" ] || fail "no binary at $BIN — build it:  pnpm hvf:build"
ok "$BIN"

step "third_party dependencies at their pinned, patched revisions"
# Delegated, not reimplemented: bootstrap.sh owns the manifest pins and the patch series, and its
# --verify mode builds nothing. This is the ONLY check anywhere that the gfxstream patches are still
# applied to the SOURCE tree — the symbol probes below only ever see an already-built dylib.
BOOTSTRAP="$ROOT/third_party/bootstrap.sh"
[ -f "$BOOTSTRAP" ] || fail "no $BOOTSTRAP — the out-of-tree dependency set has no guard at all"
if VERIFY_OUT="$(bash "$BOOTSTRAP" --verify 2>&1)"; then
    ok "pins + patches verified (third_party/MANIFEST.sh)"
else
    printf '%s\n' "$VERIFY_OUT" >&2
    fail "third_party is not at its pinned, patched state (see above) — run:  third_party/bootstrap.sh"
fi

step "gfxstream host renderer"
[ -f "$TP_GFXSTREAM_DYLIB" ] \
    || fail "libgfxstream_backend.dylib not found at $TP_GFXSTREAM_DYLIB — build it:  third_party/bootstrap.sh gfxstream   (or point GFXSTREAM_BUILD_HOST at an existing build dir)"
ok "$TP_GFXSTREAM_DYLIB"

step "gfxstream ABI match (every stream_renderer_* the binaries import is exported by the dylib)"
# This check exists because of a real SIGSEGV, and it is the only one here that can catch that class.
#
# The renderer dylib is loaded by @rpath and its C entry points are resolved LAZILY. Delete or rename
# an exported symbol — Phase 1 removed stream_renderer_get_color_buffer_mtl_texture when the
# VK_EXT_metal_objects present path was replaced — and a binary compiled against the OLD dylib does
# NOT fail at load with a helpful "symbol not found". It launches, boots Android, and then jumps
# through a lazy stub that resolved to address 0 the first time that codepath runs, i.e.
# "EXC_BAD_ACCESS / KERN_INVALID_ADDRESS at 0x0" deep inside a vcpu thread, minutes in.
#
# It bit us in the worst possible shape: `cargo build` had been run for debug only, Studio prefers the
# RELEASE binary (see paths.rs::hvf_binary), and spawn::ensure_signed re-signs on every launch, which
# rewrites the mtime — so the stale binary looked freshly built. Comparing imports to exports is
# cheap, needs no GPU, and is the one signal that does not lie.
#
# BOTH profiles are checked when present: whichever one is stale is the one that will crash.
abi_check() {
    _b="$1"; _label="$2"
    # Undefined (imported) gfxstream symbols this binary needs, minus the leading Mach-O underscore.
    nm -u "$_b" 2>/dev/null | sed 's/^ *//; s/^_//' \
        | grep -E '^(stream_renderer|gfxstream_backend|android_[A-Za-z]*Opengles)' | sort -u > "$TMP_NEEDS"
    if [ ! -s "$TMP_NEEDS" ]; then
        ok "$_label imports no gfxstream symbols (nothing to match)"
        return 0
    fi
    _missing="$(comm -23 "$TMP_NEEDS" "$TMP_EXPORTS")"
    if [ -n "$_missing" ]; then
        # printf '%s\n' (not '%s'): without the trailing newline `read` drops the final line, which
        # for a single missing symbol means printing nothing at all.
        printf '%s\n' "$_missing" | while IFS= read -r s; do
            [ -n "$s" ] && printf '      missing: %s\n' "$s" >&2
        done
        fail "$_label imports gfxstream symbols the dylib does not export (listed above). This binary
    WILL segfault at address 0 once it reaches that codepath — it is stale relative to
    libgfxstream_backend.dylib. Rebuild it:  cargo build --release --manifest-path
    crates/aemulator-hvf/Cargo.toml --bin aemulator-hvf   (and the same without --release for debug)"
    fi
    ok "$_label: $(wc -l < "$TMP_NEEDS" | tr -d ' ') gfxstream symbols, all exported"
}
TMP_EXPORTS="$(mktemp)"; TMP_NEEDS="$(mktemp)"
trap 'rm -f "$TMP_EXPORTS" "$TMP_NEEDS"' EXIT
nm -gU "$TP_GFXSTREAM_DYLIB" 2>/dev/null \
    | awk '{print $NF}' | sed 's/^_//' | sort -u > "$TMP_EXPORTS"
[ -s "$TMP_EXPORTS" ] || fail "could not read exported symbols from $TP_GFXSTREAM_DYLIB"
for _p in release debug; do
    _cand="$ROOT/crates/aemulator-hvf/target/$_p/aemulator-hvf"
    [ -x "$_cand" ] && abi_check "$_cand" "target/$_p/aemulator-hvf"
done

step "KosmicKrisp Vulkan driver (the only host Vulkan driver — no MoltenVK fallback)"
# Deliberately NOT honoring $VK_DRIVER_FILES: this check exists to prove the canonical install
# prefix is populated, which is the one every launcher (run-android64.sh, run-linux-gpu3d.sh,
# gfxstream/run.sh, kosmickrisp/run-gate.sh, Studio's spawn.rs) exports VK_DRIVER_FILES *to*.
# paths.sh discovers the manifest by glob: mesa arch-qualifies the basename from meson's
# host_machine.cpu() (`aarch64`), which is not `uname -m` (`arm64`), so it must not be hand-built.
KK_ICD="$TP_KOSMICKRISP_ICD"
[ -f "$KK_ICD" ] \
    || fail "no KosmicKrisp ICD manifest in $TP_KOSMICKRISP_ICD_DIR (expected $TP_KOSMICKRISP_ICD_BASENAME) — build the driver:  third_party/bootstrap.sh mesa"
ok "ICD manifest $KK_ICD"

# The manifest is a POINTER, not the driver. `ninja install` into a fresh prefix, or a moved tree,
# leaves the JSON naming a dylib that no longer exists — and the Vulkan loader treats an unloadable
# ICD as "no ICD", silently, with no error of its own. Resolve and stat what it actually names.
KK_LIB="$(sed -n 's/.*"library_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$KK_ICD" | head -1)"
[ -n "$KK_LIB" ] \
    || fail "$KK_ICD has no \"library_path\" — the manifest is truncated or not a Vulkan ICD; rebuild:  third_party/bootstrap.sh mesa"
# A relative library_path is resolved by the loader against the manifest's own directory.
case "$KK_LIB" in /*) ;; *) KK_LIB="$(dirname "$KK_ICD")/$KK_LIB" ;; esac
[ -f "$KK_LIB" ] \
    || fail "the ICD manifest names a driver that is not there: $KK_LIB — the install prefix was moved or wiped; rebuild:  third_party/bootstrap.sh mesa"
ok "driver $KK_LIB"

# KosmicKrisp is built on Metal APIs that only exist on macOS 26+; this is a hard floor, not a
# preference, and with MoltenVK removed there is nothing to fall back to on an older host.
MACOS_VER="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
MACOS_MAJOR="${MACOS_VER%%.*}"
if [ "${MACOS_MAJOR:-0}" -ge 26 ] 2>/dev/null; then
    ok "macOS $MACOS_VER (macOS 26 or newer is REQUIRED — KosmicKrisp is the only driver)"
else
    fail "macOS $MACOS_VER — macOS 26 or NEWER is required on Apple Silicon. KosmicKrisp is the only host Vulkan driver in this build and MoltenVK has been removed, so there is no supported configuration on this host."
fi

step "baked-in rpaths all resolve"
DEAD=0
while IFS= read -r rp; do
    if [ -d "$rp" ]; then
        ok "rpath $rp"
    else
        warn "DEAD rpath baked into the binary: $rp"
        DEAD=1
    fi
done < <(otool -l "$BIN" 2>/dev/null | awk '/LC_RPATH/{f=1} f&&/^ *path /{print $2; f=0}')
[ "$DEAD" -eq 0 ] || fail "a baked-in rpath no longer exists — the tree moved. Rebuild:  pnpm hvf:build"

step "binary loads under dyld (resolves every @rpath dependency)"
LOAD_ERR="$("$BIN" --help 2>&1 >/dev/null)"
case "$LOAD_ERR" in
    *"Library not loaded"*|*"dyld"*) printf '%s\n' "$LOAD_ERR" >&2
        fail "dyld cannot load the binary's dependencies (see above)" ;;
esac
ok "all dynamic dependencies resolve"

step "hypervisor entitlement (hv_vm_create returns HV_DENIED without it)"
if codesign -d --entitlements :- "$BIN" 2>&1 | grep -q "com.apple.security.hypervisor"; then
    ok "signed with com.apple.security.hypervisor"
elif [ "$FIX" -eq 1 ]; then
    codesign --sign - --entitlements "$ENT" --force "$BIN" || fail "codesign failed"
    ok "re-signed with com.apple.security.hypervisor"
else
    fail "binary is NOT signed with the hypervisor entitlement — run:  pnpm hvf:sign
    This is expected after a plain \`cargo build\`: RELINKING DISCARDS the ad-hoc signature, and only
    \`pnpm hvf:build\` chains the signing step. So a rebuild always costs you a re-sign, and without it
    hv_vm_create returns HV_DENIED. (\`hvf:sign\` signs both debug and release, whichever exist.)"
fi

step "libslirp NAT backend (a net-less binary takes an IP and drops every packet)"
command -v brew >/dev/null && BREW_LIB="$(brew --prefix)/lib" || BREW_LIB=/opt/homebrew/lib
ls "$BREW_LIB"/libslirp.*dylib >/dev/null 2>&1 \
    || fail "libslirp not installed — run:  brew install libslirp"
LC_ALL=C grep -qa "libslirp" "$BIN" \
    || fail "binary links no NAT backend (STALE build) — rebuild:  pnpm hvf:build"
ok "libslirp present and linked in"

printf '\n%s==> EMULATOR DEPENDENCY CHAIN OK%s\n' "$C_G" "$C_0"
