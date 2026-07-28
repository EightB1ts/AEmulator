#!/usr/bin/env bash
#
# build.sh — build the gfxstream CI LINK STUB and prove its symbol set still matches ffi.rs.
#
# THIS IS NOT A RENDERER. See README.md in this directory before using it for anything.
#
#   bash ci/stub-gfxstream/build.sh                 # build into this directory
#   bash ci/stub-gfxstream/build.sh --out DIR       # build into DIR
#   bash ci/stub-gfxstream/build.sh --verify        # symbol-parity check only (temp build, then discard)
#
# On success prints the absolute output directory as the LAST line of stdout, so a caller can do:
#   export GFXSTREAM_BUILD_HOST="$(bash ci/stub-gfxstream/build.sh --out "$RUNNER_TEMP/stub" | tail -1)"
#
# THE SYMBOL-PARITY GUARD (the reason this is a script and not a one-line cc invocation)
# -------------------------------------------------------------------------------------
# The stub silently going stale as ffi.rs grows would put us straight back to zero CI coverage of
# crates/aemulator-hvf: a newly-imported `stream_renderer_*` would fail to link, and the honest repair
# under time pressure is to drop the job. So after every build we re-derive the authoritative symbol
# list from crates/aemulator-hvf/src/gfxstream/ffi.rs (bindgen output — the crate's actual import set)
# and diff it against what the freshly-built stub exports, failing with the missing NAMES.
#
# This is deliberately the same idiom as ci/hvf-doctor.sh's "gfxstream ABI match" step (`nm -u` on the
# binary vs `nm -gU` on the dylib, compared with `comm -23`), just with the *declared* imports standing
# in for a linked binary's undefined symbols. Using ffi.rs rather than a built binary is what makes the
# check strictly stronger than "does cargo link succeed": an `extern "C"` declaration that no Rust code
# calls *yet* produces no undefined symbol and no link error, so the plain build would go green and the
# stub would only break on the later commit that first calls it.
#
# The guard is NOT a Rust test on purpose: crates/aemulator-hvf/src/** is under concurrent edit by
# other workstreams, and a shell step here also runs on the Linux gate (ci/check.sh) where the crate
# itself cannot be built at all.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FFI="$ROOT/crates/aemulator-hvf/src/gfxstream/ffi.rs"
SRC="$SCRIPT_DIR/gfxstream_stub.c"

# Guards against a silently-vacuous extraction (e.g. bindgen changing its output shape). The real
# count is 53 as of 2026-07; the floor only has to be high enough that "parsed nothing" cannot pass.
MIN_SYMBOLS=40

if [ -t 1 ]; then C_G=$'\033[32m'; C_R=$'\033[31m'; C_Y=$'\033[33m'; C_0=$'\033[0m'; else C_G=; C_R=; C_Y=; C_0=; fi
say()  { printf '  %s\n' "$*" >&2; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*" >&2; }
warn() { printf '%s  ! %s%s\n' "$C_Y" "$*" "$C_0" >&2; }
fail() { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

VERIFY_ONLY=0
OUT="$SCRIPT_DIR"
while [ $# -gt 0 ]; do
    case "$1" in
        --out) OUT="${2:?--out needs a directory}"; shift 2 ;;
        --verify|--verify-only) VERIFY_ONLY=1; shift ;;
        -h|--help) sed -n '3,12p' "$0"; exit 0 ;;
        *) fail "unknown argument: $1 (see --help)" ;;
    esac
done

[ -f "$SRC" ] || fail "stub source missing: $SRC"
[ -f "$FFI" ] || fail "cannot derive the symbol list: $FFI not found"

CC_BIN="${CC:-cc}"
command -v "$CC_BIN" >/dev/null 2>&1 || fail "no C compiler ($CC_BIN) — cannot build the link stub"

# --- platform: macOS is the requirement; Linux falls out for free (see README) ------------------
case "$(uname -s)" in
    Darwin)
        LIB_BASENAME="libgfxstream_backend.dylib"
        # -install_name @rpath/... is REQUIRED, not cosmetic: build.rs bakes $GFXSTREAM_BUILD_HOST in
        # as an -rpath and links -lgfxstream_backend, exactly as it does for the real dylib (whose
        # install_name is @rpath/libgfxstream_backend.dylib). With an absolute or bare install_name the
        # link succeeds and then dyld fails to find the library when the test binary runs.
        LINK_FLAGS=(-dynamiclib -install_name "@rpath/$LIB_BASENAME")
        NM_DEFINED=(nm -gU)
        # macOS is the platform that matters (it is the only one that links the crate), and this is
        # the compiler the macos-14 job uses, so hold the source to -Werror here.
        WARN_FLAGS=(-Wall -Wextra -Werror -Wno-unused-parameter)
        ;;
    Linux)
        LIB_BASENAME="libgfxstream_backend.so"
        LINK_FLAGS=(-shared -fPIC "-Wl,-soname,$LIB_BASENAME")
        NM_DEFINED=(nm --dynamic --defined-only)
        # NO -Werror on Linux, deliberately. Nothing on Linux links this .so — build.rs returns early
        # for any non-macOS target — so the Linux build exists ONLY to give the symbol-parity guard
        # something to inspect on the ubuntu gate. Letting a gcc-only pedantic warning about 300 lines
        # of no-ops fail the entire Linux merge gate would be a self-inflicted wound with no upside.
        # Warnings are still printed; the parity check is the assertion that matters.
        WARN_FLAGS=(-Wall -Wextra -Wno-unused-parameter)
        ;;
    *)
        fail "unsupported host $(uname -s) — the stub builds on macOS (required) and Linux (bonus)"
        ;;
esac

# One cleanup handler, registered once. TMP_* are filled in later; ${VAR:-} keeps `set -u` happy if we
# exit before then. In --verify mode the whole output directory is temporary and goes with it.
TMP_DECL=""; TMP_EXPORTS=""; TMP_OUT=""
cleanup() {
    [ -n "${TMP_DECL:-}" ] && rm -f "$TMP_DECL"
    [ -n "${TMP_EXPORTS:-}" ] && rm -f "$TMP_EXPORTS"
    [ -n "${TMP_OUT:-}" ] && rm -rf "$TMP_OUT"
    return 0
}
trap cleanup EXIT

if [ "$VERIFY_ONLY" = 1 ]; then
    TMP_OUT="$(mktemp -d)"
    OUT="$TMP_OUT"
fi
mkdir -p "$OUT" || fail "cannot create output directory $OUT"
OUT="$(cd "$OUT" && pwd)"
LIB="$OUT/$LIB_BASENAME"

# --- build --------------------------------------------------------------------------------------
# No optimisation and no -DNDEBUG: this thing does nothing, and every line of it is a diagnostic.
# -Wno-unused-parameter because ignoring every argument is the entire point.
"$CC_BIN" "${LINK_FLAGS[@]}" -std=c11 -O0 -g0 "${WARN_FLAGS[@]}" -o "$LIB" "$SRC" \
    || fail "failed to compile the link stub from $SRC"
[ -f "$LIB" ] || fail "compiler reported success but $LIB does not exist"
ok "built $LIB ($(wc -c <"$LIB" | tr -d ' ') bytes)"

# --- symbol parity: every extern "C" fn ffi.rs declares must be exported by the stub -------------
TMP_DECL="$(mktemp)"; TMP_EXPORTS="$(mktemp)"

# The authoritative import set: `pub fn NAME` inside an `[unsafe] extern "C" { ... }` block. Function
# POINTER types (`unsafe extern "C" fn(` — no brace, and always indented) are not declarations and are
# correctly not matched. Accepts both the Rust-2024 `unsafe extern "C" {` bindgen emits today and the
# older bare `extern "C" {`, so a bindgen bump does not silently empty this list.
awk '
    /^[[:space:]]*(unsafe[[:space:]]+)?extern[[:space:]]+"C"[[:space:]]*\{/ { inblk = 1; next }
    inblk && /^[[:space:]]*\}/                                             { inblk = 0; next }
    inblk && /^[[:space:]]*pub[[:space:]]+fn[[:space:]]/ {
        line = $0
        sub(/^[[:space:]]*pub[[:space:]]+fn[[:space:]]+/, "", line)
        sub(/[(<].*$/, "", line)
        if (line != "") print line
    }
' "$FFI" | sort -u > "$TMP_DECL"

DECL_N="$(wc -l <"$TMP_DECL" | tr -d ' ')"
if [ "$DECL_N" -lt "$MIN_SYMBOLS" ]; then
    fail "only $DECL_N extern \"C\" symbols parsed out of $FFI (floor is $MIN_SYMBOLS) — the
    extraction in this script has gone stale relative to bindgen's output shape. A vacuous parity
    check is worse than none: fix the awk above, do not lower MIN_SYMBOLS."
fi

# Same shape as ci/hvf-doctor.sh: strip the leading Mach-O underscore, take the last field, sort -u.
"${NM_DEFINED[@]}" "$LIB" 2>/dev/null | awk '{print $NF}' | sed 's/^_//' | sort -u > "$TMP_EXPORTS"
[ -s "$TMP_EXPORTS" ] || fail "could not read exported symbols from $LIB"

MISSING="$(comm -23 "$TMP_DECL" "$TMP_EXPORTS")"
# Extras are not a failure: ffi.rs shrinking (a symbol the renderer dropped, as happened to
# stream_renderer_get_color_buffer_mtl_texture) leaves harmless dead weight here. Report them BEFORE
# the missing-symbol failure — when the cause is a typo in the stub, seeing both halves of the rename
# is what identifies it.
EXTRA="$(comm -13 "$TMP_DECL" "$TMP_EXPORTS" | grep -E '^(stream_renderer|gfxstream_backend|android_[A-Za-z]*Opengles)' || true)"
if [ -n "$EXTRA" ]; then
    warn "the stub exports symbol(s) ffi.rs does not declare (harmless if ffi.rs shrank — a typo if not):"
    printf '%s\n' "$EXTRA" | sed 's/^/      extra: /' >&2
fi

if [ -n "$MISSING" ]; then
    printf '%s\n' "$MISSING" | while IFS= read -r s; do
        [ -n "$s" ] && printf '      missing from the stub: %s\n' "$s" >&2
    done
    fail "$(printf '%s\n' "$MISSING" | grep -c .) symbol(s) declared in
    crates/aemulator-hvf/src/gfxstream/ffi.rs are NOT exported by ci/stub-gfxstream (listed above).
    The macOS CI job cannot link crates/aemulator-hvf until they are added. Add a matching no-op to
    ci/stub-gfxstream/gfxstream_stub.c for each one — faithful RETURN type, ignored arguments, an
    AEMU_STUB(name) announce line, and -1 (or NULL) for anything that returns a value. See
    ci/stub-gfxstream/README.md § Regenerating the symbol list."
fi

ok "symbol parity: all $DECL_N extern \"C\" symbols in gfxstream/ffi.rs are exported by the stub"

if [ "$VERIFY_ONLY" = 1 ]; then
    say "verify-only: discarding the temporary build in $OUT"
    exit 0
fi

# Last line of stdout, and the only thing on stdout: the directory to point GFXSTREAM_BUILD_HOST at.
printf '%s\n' "$OUT"
