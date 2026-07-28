#!/usr/bin/env bash
# third_party/bootstrap.sh — bring every out-of-tree build dependency to the pinned, patched,
# built state. Idempotent, relocation-safe, and loud on failure.
#
#   third_party/bootstrap.sh            # everything that is missing or stale
#   third_party/bootstrap.sh gfxstream  # one dep (aemu | gfxstream | mesa)
#   third_party/bootstrap.sh --verify   # check only, build nothing, exit non-zero if not ready
#
# This replaces a hand-managed `SampleRepos/` directory. Two failures motivated every design choice
# here, and both are guarded:
#
#   1. RELOCATION. cmake and meson bake absolute source paths into their build directories, and
#      aemu's generated .pc files bake an absolute prefix. Moving the repo left a build tree that
#      pointed at the old path: `ninja` failed, and the dylib silently stayed at its last-built
#      revision — so the code we thought we were running was months old. Every build step below
#      detects a stale absolute path and reconfigures instead of trusting the cache.
#   2. LOST PATCHES. Our gfxstream changes lived only as uncommitted edits inside a foreign git
#      checkout. They are now a tracked patch series applied here; if one stops applying, this script
#      fails rather than building an unpatched renderer that would fail much later and less clearly.
set -uo pipefail

TP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$TP_DIR/MANIFEST.sh"
. "$TP_DIR/paths.sh"

C_B=$'\033[1m'; C_G=$'\033[32m'; C_Y=$'\033[33m'; C_R=$'\033[31m'; C_0=$'\033[0m'
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '%s  ! %s%s\n' "$C_Y" "$*" "$C_0"; }
die()  { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

VERIFY_ONLY=0
WANT=""
for a in "$@"; do
    case "$a" in
        --verify) VERIFY_ONLY=1 ;;
        aemu|gfxstream|mesa) WANT="$WANT $a" ;;
        *) die "unknown argument '$a' (expected --verify, or one of: aemu gfxstream mesa)" ;;
    esac
done
wants() { [ -z "$WANT" ] || printf '%s' "$WANT" | grep -qw "$1"; }

# ---------------------------------------------------------------------------------------------
# checkout <name> <url> <sha> <dest> <patch-subdir>
# Clone if absent, fetch+detach if at the wrong commit, then apply the tracked patch series.
# ---------------------------------------------------------------------------------------------
checkout() {
    _name="$1"; _url="$2"; _sha="$3"; _dest="$4"; _patches="$5"

    if [ ! -d "$_dest/.git" ]; then
        [ "$VERIFY_ONLY" = 1 ] && die "$_name: not checked out at $_dest — run third_party/bootstrap.sh"
        mkdir -p "$(dirname "$_dest")"
        # Full clone, not --depth 1: we need to check out an exact historical SHA, and a shallow
        # clone of a different tip cannot reach it.
        git clone -q "$_url" "$_dest" || die "$_name: clone failed from $_url"
    fi

    _have="$(git -C "$_dest" rev-parse HEAD 2>/dev/null)"
    if [ "$_have" != "$_sha" ]; then
        [ "$VERIFY_ONLY" = 1 ] && die "$_name: at $_have, manifest pins $_sha"
        # A dirty tree here is almost always our own applied patches; resetting is correct because
        # the patch series is the source of truth and is re-applied immediately below.
        git -C "$_dest" fetch -q origin "$_sha" 2>/dev/null || git -C "$_dest" fetch -q origin
        git -C "$_dest" checkout -q --force --detach "$_sha" \
            || die "$_name: cannot check out $_sha (is the manifest SHA reachable from $_url?)"
        git -C "$_dest" clean -qfd
        ok "$_name: checked out $_sha"
    else
        ok "$_name: at pinned $_sha"
    fi

    [ -n "$_patches" ] || return 0
    _pdir="$TP_DIR/patches/$_patches"
    [ -d "$_pdir" ] || die "$_name: manifest names patch set '$_patches' but $_pdir does not exist"
    for p in "$_pdir"/*.patch; do
        [ -e "$p" ] || die "$_name: no .patch files in $_pdir"
        if git -C "$_dest" apply --check --reverse "$p" 2>/dev/null; then
            ok "$_name: $(basename "$p") already applied"
        elif git -C "$_dest" apply --check "$p" 2>/dev/null; then
            [ "$VERIFY_ONLY" = 1 ] && die "$_name: $(basename "$p") not applied"
            git -C "$_dest" apply "$p" || die "$_name: $(basename "$p") failed to apply"
            ok "$_name: applied $(basename "$p")"
        else
            die "$_name: $(basename "$p") neither applies nor is already applied against $_sha.
    The pin and the patch have diverged. Re-export the series against the new base rather than
    forcing it — a partially-applied renderer patch fails at runtime, not at build time."
        fi
    done
}

# True when a cmake/meson build dir was configured for a different source path than the one we want.
stale_cmake() {  # <builddir> <expected-source-dir>
    [ -f "$1/CMakeCache.txt" ] || return 0
    _home="$(sed -n 's/^CMAKE_HOME_DIRECTORY:INTERNAL=//p' "$1/CMakeCache.txt" | head -1)"
    [ "$_home" != "$2" ]
}
stale_meson() { # <builddir> <expected-source-dir>
    [ -f "$1/build.ninja" ] || return 0
    ! grep -q "$2" "$1/build.ninja" 2>/dev/null
}

need_tools() {
    for t in "$@"; do command -v "$t" >/dev/null || die "missing required tool: $t"; done
}

# =============================================================================================
step "third_party root: $TP_CHECKOUTS"
[ "$(uname -s)" = "Darwin" ] || die "this dependency set is macOS-only (host is $(uname -s))"
mkdir -p "$TP_CHECKOUTS"

# ---------------------------------------------------------------------------------------------
if wants aemu; then
step "aemu (aemu-base / aemu-host-common — emits the .pc files gfxstream's cmake consumes)"
checkout "$TP_AEMU_NAME" "$TP_AEMU_URL" "$TP_AEMU_SHA" "$TP_AEMU_SRC" "$TP_AEMU_PATCHES"

if [ "$VERIFY_ONLY" = 0 ]; then
    need_tools cmake ninja
    if [ ! -f "$TP_AEMU_INSTALL/lib/libaemu-base.a" ]; then
        rm -rf "$TP_AEMU_SRC/build"
        ( cd "$TP_AEMU_SRC" && mkdir -p build && cd build &&
          cmake -G Ninja -DCMAKE_INSTALL_PREFIX="$TP_AEMU_INSTALL" \
                -DAEMU_COMMON_GEN_PKGCONFIG=ON -DAEMU_COMMON_BUILD_CONFIG=gfxstream \
                -DENABLE_VKCEREAL_TESTS=OFF \
                -DCMAKE_C_COMPILER=/usr/bin/clang -DCMAKE_CXX_COMPILER=/usr/bin/clang++ .. >/dev/null &&
          ninja >/dev/null && ninja install >/dev/null ) || die "aemu build failed"
        ok "aemu built"
    else
        ok "aemu already built"
    fi

    # aemu's generated .pc files bake an absolute prefix. After any relocation they point at the old
    # tree and gfxstream's configure silently resolves the wrong (or no) libs — rewrite them every
    # run so the prefix always matches where the tree actually is now.
    for pc in "$TP_AEMU_PKGCONFIG"/*.pc; do
        [ -e "$pc" ] || die "aemu: no .pc files in $TP_AEMU_PKGCONFIG"
        sed -i '' "s#^prefix=.*#prefix=\"$TP_AEMU_INSTALL\"#" "$pc"
    done
    # gfxstream's SYSTEM path requires all six .pc unconditionally, but aemu only emits the
    # *_testing_support pair when tests are ON (and they are never linked with tests OFF).
    for t in aemu_base_testing_support aemu_host_common_testing_support; do
        [ -f "$TP_AEMU_PKGCONFIG/$t.pc" ] && continue
        lib="$(printf '%s' "$t" | sed 's/aemu_/aemu-/; s/_/-/g')"
        cat > "$TP_AEMU_PKGCONFIG/$t.pc" <<EOF
prefix="$TP_AEMU_INSTALL"
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: AEMUCommon $t
Description: Android emulation utilities (testing support shim; never linked with tests OFF)
Version: 0.1.2

Libs: -L\${libdir} -l$lib
Cflags: -I\${includedir}
EOF
    done
    ok "aemu pkg-config prefixes point at $TP_AEMU_INSTALL"
fi
fi

# ---------------------------------------------------------------------------------------------
if wants gfxstream; then
step "gfxstream (host renderer + our patch series)"
checkout "$TP_GFXSTREAM_NAME" "$TP_GFXSTREAM_URL" "$TP_GFXSTREAM_SHA" "$TP_GFXSTREAM_SRC" "$TP_GFXSTREAM_PATCHES"

if [ "$VERIFY_ONLY" = 0 ]; then
    need_tools cmake ninja pkg-config
    # egl and libdrm do not exist on macOS; gfxstream's SYSTEM resolution still demands both .pc.
    # Point egl at gfxstream's own bundled headers and libdrm at its host tree (both header-only here).
    STUBS="$TP_CHECKOUTS/.pkgconfig-stubs"; mkdir -p "$STUBS"
    printf 'prefix=%s/common/opengl/include\nincludedir=${prefix}\n\nName: egl\nDescription: stub EGL headers (macOS)\nVersion: 1.5\n\nCflags: -I${includedir}\n' "$TP_GFXSTREAM_SRC" > "$STUBS/egl.pc"
    printf 'prefix=%s/host\nincludedir=${prefix}\n\nName: libdrm\nDescription: stub libdrm headers (macOS)\nVersion: 2.4.0\n\nCflags: -I${includedir}\n' "$TP_GFXSTREAM_SRC" > "$STUBS/libdrm.pc"

    if stale_cmake "$TP_GFXSTREAM_BUILD_HOST" "$TP_GFXSTREAM_SRC"; then
        warn "gfxstream build dir is absent or was configured for a different source path — reconfiguring"
        rm -rf "$TP_GFXSTREAM_BUILD_HOST"
        VKH="$(brew --prefix vulkan-headers 2>/dev/null)"; VKL="$(brew --prefix vulkan-loader 2>/dev/null)"
        [ -n "$VKH" ] && [ -n "$VKL" ] || die "need brew vulkan-headers + vulkan-loader"
        PKG_CONFIG_PATH="$TP_AEMU_PKGCONFIG:$STUBS:/opt/homebrew/lib/pkgconfig:/opt/homebrew/share/pkgconfig" \
        cmake -S "$TP_GFXSTREAM_SRC" -B "$TP_GFXSTREAM_BUILD_HOST" -G Ninja \
            -DDEPENDENCY_RESOLUTION=SYSTEM -DBUILD_STANDALONE=ON -DENABLE_VKCEREAL_TESTS=OFF \
            -DCMAKE_C_COMPILER=/usr/bin/clang -DCMAKE_CXX_COMPILER=/usr/bin/clang++ \
            -DCMAKE_PREFIX_PATH="/opt/homebrew;$VKH;$VKL" >/dev/null || die "gfxstream configure failed"
    fi
    ninja -C "$TP_GFXSTREAM_BUILD_HOST" gfxstream_backend >/dev/null || die "gfxstream build failed"
    [ -f "$TP_GFXSTREAM_DYLIB" ] || die "gfxstream: no dylib at $TP_GFXSTREAM_DYLIB"
    # The patches are only real if they reached the artifact. Check a symbol from each phase.
    nm -gU "$TP_GFXSTREAM_DYLIB" | grep -q "_stream_renderer_present_to_metal_layer" \
        || die "gfxstream: dylib built but does not export stream_renderer_present_to_metal_layer —
    the patch series did not reach the artifact (stale build dir?)"
    nm -gU "$TP_GFXSTREAM_DYLIB" | grep -q "_stream_renderer_get_color_buffer_mtl_texture" \
        && die "gfxstream: dylib still exports the removed metal_objects entry point — stale build"
    ok "gfxstream dylib built and patch-verified: $TP_GFXSTREAM_DYLIB"
fi
fi

# ---------------------------------------------------------------------------------------------
if wants mesa; then
step "mesa/kosmickrisp (the host Vulkan driver — no MoltenVK fallback exists)"
checkout "$TP_MESA_NAME" "$TP_MESA_URL" "$TP_MESA_SHA" "$TP_MESA_SRC" "$TP_MESA_PATCHES"

if [ "$VERIFY_ONLY" = 0 ]; then
    need_tools ninja
    # Three constraints, each of which fails `meson setup` if you get it wrong:
    #  1. libclc is REQUIRED — mesa lists with_kosmickrisp_vk in with_driver_using_cl, which turns on
    #     with_clc, which hard-requires dependency('libclc').
    #  2. The venv must be FIRST on PATH — mesa probes interpreters by name (python3.14 … python3) and
    #     skips any that cannot import mako/packaging/yaml, then reports "Python >= 3.10 not found"
    #     while listing pythons it just rejected.
    #  3. Every compiler pinned to Apple clang — brew llvm must be on PATH for llvm-config, but that
    #     also makes meson pick LLVM's clang for Objective-C while C/C++ stay on Apple clang.
    command -v brew >/dev/null || die "mesa: needs Homebrew"
    brew list --versions libclc >/dev/null 2>&1 || die "mesa: brew install libclc  (required: kosmickrisp turns on with_clc)"
    for f in llvm spirv-tools spirv-llvm-translator bison flex pkgconf; do
        brew list --versions "$f" >/dev/null 2>&1 || die "mesa: brew install $f"
    done

    # Test that meson RUNS, not merely that the file exists. A Python venv is not relocatable: its
    # scripts hardcode the interpreter path in their shebang, so a moved venv leaves an executable
    # `meson` that dies with "bad interpreter". Existence checks pass; the build does not.
    if ! "$TP_VENV/bin/meson" --version >/dev/null 2>&1; then
        [ -e "$TP_VENV" ] && warn "python venv is missing or was relocated (shebangs point elsewhere) — recreating"
        rm -rf "$TP_VENV"
        PY=/opt/homebrew/opt/python@3.12/bin/python3.12
        [ -x "$PY" ] || PY="$(command -v python3)"
        "$PY" -m venv "$TP_VENV" || die "mesa: venv creation failed"
        "$TP_VENV/bin/pip" install -q --upgrade pip mako pyyaml packaging 'meson>=1.9.1' \
            || die "mesa: venv package install failed"
        "$TP_VENV/bin/meson" --version >/dev/null 2>&1 || die "mesa: venv built but meson still will not run"
    fi

    export PATH="$TP_VENV/bin:$(brew --prefix llvm)/bin:$(brew --prefix bison)/bin:$(brew --prefix flex)/bin:$PATH"
    export CC=/usr/bin/clang CXX=/usr/bin/clang++ OBJC=/usr/bin/clang OBJCXX=/usr/bin/clang++
    MB="$TP_MESA_SRC/build"
    if stale_meson "$MB" "$TP_MESA_SRC"; then
        warn "mesa build dir is absent or configured for a different source path — reconfiguring"
        rm -rf "$MB"
        "$TP_VENV/bin/meson" setup "$MB" "$TP_MESA_SRC" --prefix="$TP_KOSMICKRISP_PREFIX" \
            -Dbuildtype=release -Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers= \
            -Dopengl=false -Dzstd=disabled --prefer-static >/dev/null || die "mesa configure failed"
    fi
    ninja -C "$MB" >/dev/null || die "mesa build failed"
    "$TP_VENV/bin/meson" install -C "$MB" >/dev/null || die "mesa install failed"
    [ -f "$TP_KOSMICKRISP_DYLIB" ] || die "mesa: no driver at $TP_KOSMICKRISP_DYLIB"
    [ -f "$TP_KOSMICKRISP_ICD" ] || die "mesa: no ICD manifest at $TP_KOSMICKRISP_ICD"
    # The manifest is a POINTER. After a relocated install it can name a dylib that is not there, and
    # the Vulkan loader treats an unloadable ICD as *no* ICD, silently.
    _lib="$(sed -n 's/.*"library_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$TP_KOSMICKRISP_ICD" | head -1)"
    case "$_lib" in /*) ;; *) _lib="$(dirname "$TP_KOSMICKRISP_ICD")/$_lib" ;; esac
    [ -f "$_lib" ] || die "mesa: ICD manifest names a driver that is not there: $_lib"
    ok "kosmickrisp built: $TP_KOSMICKRISP_ICD -> $_lib"
fi
fi

# =============================================================================================
step "summary"
printf '  gfxstream dylib : %s\n' "$TP_GFXSTREAM_DYLIB"
printf '  kosmickrisp ICD : %s\n' "$TP_KOSMICKRISP_ICD"
printf '  aemu pkgconfig  : %s\n' "$TP_AEMU_PKGCONFIG"
if [ "$VERIFY_ONLY" = 1 ]; then
    ok "third_party VERIFIED (pins + patches)"
else
    ok "third_party READY"
    printf '\n  next:  ci/hvf-doctor.sh   (proves the built artifacts actually load)\n'
fi
