#!/usr/bin/env bash
# third_party/bootstrap.sh — bring every out-of-tree build dependency to the pinned, patched,
# built state. Idempotent, relocation-safe, and loud on failure.
#
#   third_party/bootstrap.sh                 # everything that is missing or stale
#   third_party/bootstrap.sh gfxstream       # one dep (aemu | gfxstream | mesa)
#   third_party/bootstrap.sh --verify        # check only, build nothing, exit non-zero if not ready
#   third_party/bootstrap.sh --record-digest # accept an intentional patch-series change (see below)
#
# --verify is READ-ONLY with respect to third_party/<dep>/: it never clones, fetches, checks out,
# resets, cleans or builds. ci/hvf-doctor.sh (and therefore ci/check.sh) delegates to it, so it has
# to be safe to run on a tree someone is mid-edit in. Keep it that way.
#
# This replaces a hand-managed `SampleRepos/` directory. Three failures motivated every design choice
# here, and all three are guarded:
#
#   1. RELOCATION. cmake and meson bake absolute source paths into their build directories, and
#      aemu's generated .pc files bake an absolute prefix. Moving the repo left a build tree that
#      pointed at the old path: `ninja` failed, and the dylib silently stayed at its last-built
#      revision — so the code we thought we were running was months old. Every build step below
#      detects a stale absolute path and reconfigures instead of trusting the cache.
#   2. LOST PATCHES. Our gfxstream changes lived only as uncommitted edits inside a foreign git
#      checkout. They are now a tracked patch series applied here; if one stops applying, this script
#      fails rather than building an unpatched renderer that would fail much later and less clearly.
#   3. SILENT LOCAL EDITS. Verifying the series with `git apply --check --reverse` proves the hunks
#      are PRESENT; it is blind to anything outside a hunk's context. So an edit typed straight into
#      the checkout — the exact thing (2) exists to prevent — shipped into the dylib with --verify
#      green. Every checkout is now compared to a recorded whole-tree digest (TP_*_TREE_DIGEST in
#      MANIFEST.sh), and `--record-digest` is how you accept an intentional change to a series.
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
RECORD_DIGEST=0
WANT=""
for a in "$@"; do
    case "$a" in
        --verify) VERIFY_ONLY=1 ;;
        # Recompute every TP_*_TREE_DIGEST from the checkouts and rewrite MANIFEST.sh in place. This
        # is how you ACCEPT an intentional change to a patch series: regenerate the .patch, then
        # record. Implies --verify, so it builds nothing and mutates nothing under third_party/<dep>/;
        # the only file it writes is third_party/MANIFEST.sh, which is tracked and reviewable.
        --record-digest) RECORD_DIGEST=1; VERIFY_ONLY=1 ;;
        aemu|gfxstream|mesa) WANT="$WANT $a" ;;
        *) die "unknown argument '$a' (expected --verify, --record-digest, or one of: aemu gfxstream mesa)" ;;
    esac
done
wants() { [ -z "$WANT" ] || printf '%s' "$WANT" | grep -qw "$1"; }

# ---------------------------------------------------------------------------------------------
# WHOLE-TREE INTEGRITY — everything below exists because `git apply --check --reverse` is not enough.
#
# That check proves every hunk in the series is PRESENT. It says nothing about what is OUTSIDE a
# hunk's context: add a line to VkDecoderGlobalState.cpp two hundred lines from the nearest hunk and
# the reverse-check still passes, so an unreviewed change reaches the built dylib with --verify green.
# Several phases of work land host C++ changes exclusively as hunks in the gfxstream patch, and that
# patch file is their only record, so "the checkout is exactly the pin plus the series" has to be a
# checked fact rather than a convention.
# ---------------------------------------------------------------------------------------------

# `git diff` with every knob that could vary pinned. Context width, rename detection, the diff
# algorithm, the a/ b/ prefixes and the orderfile all have config settings, and the index-line
# abbreviation length is a heuristic over a clone's object count — any of them would make the digest
# machine-dependent, i.e. a guard that fails for reasons that are not the reason it exists.
TP_DIFF_CONF="-c core.quotepath=true -c diff.noprefix=false -c diff.mnemonicprefix=false
              -c diff.relative=false"
TP_DIFF_FLAGS="--no-color --no-ext-diff --no-textconv --no-renames --full-index --unified=3
               --diff-algorithm=myers -O/dev/null --src-prefix=a/ --dst-prefix=b/"
#
# --no-optional-locks on every read: `git status`/`git diff` otherwise refresh the checkout's index as
# a side effect, which is both a write into third_party/<dep>/.git during a --verify that promises not
# to write, and a lock to lose a race on if someone is editing the tree at the same time.
tracked_delta() { # <checkout> [path] — the tree's diff against HEAD, byte-stable
    _tdl_dest="$1"; shift
    git -C "$_tdl_dest" --no-optional-locks $TP_DIFF_CONF diff $TP_DIFF_FLAGS HEAD ${1+--} "$@"
}

# Untracked, non-ignored paths, sorted. -uall on purpose: the default collapses a new directory to
# `dir/`, which would hide every file inside it — exactly the case we most want named.
untracked_paths() { # <checkout>
    git -C "$1" --no-optional-locks status --porcelain --untracked-files=all 2>/dev/null \
        | sed -n 's/^?? //p' | LC_ALL=C sort
}

# The recorded value. `git diff HEAD` cannot see untracked files and our series ADDS some, so their
# content is appended explicitly — otherwise the digest would be blind to the very files that exist
# only in the patch.
tree_delta_digest() { # <checkout>
    {
        tracked_delta "$1"
        untracked_paths "$1" | while IFS= read -r _tdd_p; do
            [ -n "$_tdd_p" ] || continue
            printf '?? %s %s\n' "$(git -C "$1" hash-object -- "$_tdd_p")" "$_tdd_p"
        done
    } | git hash-object --stdin
}

# Every path the series CREATES, parsed out of the series itself (`new file mode` followed by its
# `+++ b/<path>`) rather than hardcoded — later phases add more files and a hardcoded list would rot
# into either a false failure or a hole.
patch_added_paths() { # <patch-dir-or-empty>
    [ -n "$1" ] || return 0
    awk '
        /^diff --git /        { _new = 0 }
        /^new file mode /     { _new = 1 }
        /^\+\+\+ b\// && _new { p = substr($0, 7); sub(/\t.*$/, "", p); print p; _new = 0 }
    ' "$1"/*.patch | LC_ALL=C sort -u
}

# One path's diff as the series describes it, and as the checkout actually has it. The `index` line is
# dropped from both so an abbreviation-length difference is never mistaken for a content difference.
series_section()   { awk -v want="$2" '/^diff --git /{_in=($0=="diff --git a/" want " b/" want)} _in && !/^index /' "$1"/*.patch; }
checkout_section() { tracked_delta "$1" "$2" | grep -v '^index '; }

# Turn "digest mismatch" into "this file differs" — without this the failure is a dead end and the
# next person deletes the guard instead of fixing the tree.
report_offenders() { # <checkout> <patch-dir-or-empty>
    _ro_dest="$1"; _ro_pdir="$2"
    _ro_added="$(patch_added_paths "$_ro_pdir")"
    _ro_out="$(
        # Tracked files whose own diff does not match what the series says it should be. A file that
        # matches byte for byte is not printed: naming all sixteen every time buries the one.
        while IFS= read -r _ro_f; do
            [ -n "$_ro_f" ] || continue
            if [ -z "$_ro_pdir" ]; then
                printf '    %s\n        MODIFIED, and this dependency carries no patch series at all\n' "$_ro_f"
                continue
            fi
            cmp -s <(series_section "$_ro_pdir" "$_ro_f") <(checkout_section "$_ro_dest" "$_ro_f") && continue
            printf '    %s\n        MODIFIED, and its diff differs from the patch series\n' "$_ro_f"
        done < <(git -C "$_ro_dest" --no-optional-locks diff --name-only HEAD | LC_ALL=C sort)
        # Untracked files the series does not create: someone dropped a new source file in by hand.
        while IFS= read -r _ro_f; do
            [ -n "$_ro_f" ] || continue
            printf '%s\n' "$_ro_added" | grep -qxF "$_ro_f" && continue
            printf '    %s\n        UNTRACKED, and the patch series does not add it\n' "$_ro_f"
        done < <(untracked_paths "$_ro_dest")
        # ...and the reverse: a file the series creates that is no longer there.
        while IFS= read -r _ro_f; do
            [ -n "$_ro_f" ] || continue
            [ -e "$_ro_dest/$_ro_f" ] && continue
            printf '    %s\n        MISSING — the patch series adds this file\n' "$_ro_f"
        done <<EOF
$_ro_added
EOF
    )"
    if [ -n "$_ro_out" ]; then
        printf '%s  offending paths:\n%s%s\n' "$C_R" "$_ro_out" "$C_0" >&2
    else
        printf '%s  every individual path matches the patch series byte for byte, so the difference is in
  the diff FORM (a file mode, or a series regenerated with different diff options) — recording the
  new digest is the right fix in that case.%s\n' "$C_Y" "$C_0" >&2
    fi
}

# Rewrite one TP_*_TREE_DIGEST in MANIFEST.sh. The variable must already be declared there: silently
# appending one would let a typo'd name look like it recorded successfully forever.
record_manifest_digest() { # <var-name> <digest>
    _rmd_mf="$TP_DIR/MANIFEST.sh"
    grep -q "^$1=" "$_rmd_mf" \
        || die "$1 is not declared in $_rmd_mf — add the line (copy TP_GFXSTREAM_TREE_DIGEST) first"
    sed -i '' "s#^$1=.*#$1=\"$2\"#" "$_rmd_mf" || die "could not rewrite $1 in $_rmd_mf"
    ok "recorded $1=$2"
}

# ---------------------------------------------------------------------------------------------
# checkout <name> <url> <sha> <dest> <patch-subdir> [tree-digest-var]
# Clone if absent, fetch+detach if at the wrong commit, apply the tracked patch series, then prove the
# working tree is EXACTLY the pin plus that series.
# ---------------------------------------------------------------------------------------------
checkout() {
    _name="$1"; _url="$2"; _sha="$3"; _dest="$4"; _patches="$5"; _dvar="${6:-}"

    if [ ! -d "$_dest/.git" ]; then
        [ "$VERIFY_ONLY" = 1 ] && die "$_name: not checked out at $_dest — run third_party/bootstrap.sh"
        mkdir -p "$(dirname "$_dest")"
        # Full clone, not --depth 1: we need to check out an exact historical SHA, and a shallow
        # clone of a different tip cannot reach it.
        git clone -q "$_url" "$_dest" || die "$_name: clone failed from $_url"
    fi

    _have="$(git -C "$_dest" rev-parse HEAD 2>/dev/null)"
    _branch="$(git -C "$_dest" symbolic-ref -q --short HEAD 2>/dev/null)"
    if [ "$_have" != "$_sha" ]; then
        [ "$VERIFY_ONLY" = 1 ] && die "$_name: at $_have, manifest pins $_sha"
        # A dirty tree here is almost always our own applied patches; resetting is correct because
        # the patch series is the source of truth and is re-applied immediately below.
        git -C "$_dest" fetch -q origin "$_sha" 2>/dev/null || git -C "$_dest" fetch -q origin
        git -C "$_dest" checkout -q --force --detach "$_sha" \
            || die "$_name: cannot check out $_sha (is the manifest SHA reachable from $_url?)"
        git -C "$_dest" clean -qfd
        ok "$_name: checked out $_sha (detached)"
    elif [ -n "$_branch" ]; then
        # HEAD is at the right commit but ATTACHED to a branch, which happens whenever the pin is
        # that branch's tip at clone time. Nothing is wrong yet, and that is the problem: one `git
        # pull` in this directory silently advances HEAD off the pin, taking our patched files with
        # it. Detach so there is no branch for a pull to fast-forward. Deliberately no --force and
        # no clean: the tree content is identical at this commit, and our applied patches plus the
        # files the series adds must survive.
        if [ "$VERIFY_ONLY" = 1 ]; then
            warn "$_name: at pinned $_sha but HEAD is attached to '$_branch' — a stray 'git pull' here
    would silently move it off the pin. Detach it:  third_party/bootstrap.sh $_name"
        else
            git -C "$_dest" checkout -q --detach "$_sha" \
                || die "$_name: at $_sha on branch '$_branch' but cannot detach HEAD"
            ok "$_name: at pinned $_sha (detached from '$_branch')"
        fi
    else
        ok "$_name: at pinned $_sha (detached)"
    fi

    if [ -n "$_patches" ]; then
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
    else
        _pdir=""
    fi

    # ---- the whole tree, not just the hunks --------------------------------------------------
    # Everything above only proves the series' hunks are where they should be. This proves nothing
    # ELSE is: one digest over the complete delta from the pin, plus an exact untracked-file set.
    [ -n "$_dvar" ] || return 0
    _digest="$(tree_delta_digest "$_dest")"
    _want="${!_dvar:-}"
    # LC_ALL=C on comm to match the LC_ALL=C sort on both inputs — a collation mismatch here would
    # produce "not in sorted order" noise and a wrong set difference.
    _stray="$(LC_ALL=C comm -13 <(patch_added_paths "$_pdir") <(untracked_paths "$_dest"))"

    if [ "$RECORD_DIGEST" = 1 ]; then
        # Recording cannot express "and also this stray file is fine" — the expected untracked set is
        # derived from the series, not stored — so refuse rather than record a state that will still
        # fail verify. Either put the file in the series or delete it.
        [ -z "$_stray" ] || { report_offenders "$_dest" "$_pdir"
            die "$_name: refusing to record a digest while the checkout holds untracked files the
    patch series does not add (listed above). Add them to the series or remove them first."; }
        record_manifest_digest "$_dvar" "$_digest"
        return 0
    fi

    if [ "$_digest" != "$_want" ] || [ -n "$_stray" ]; then
        report_offenders "$_dest" "$_pdir"
        die "$_name: the checkout is NOT the pin plus the recorded patch series.
      $_dvar recorded : ${_want:-<unset>}
      $_dvar actual   : $_digest
    The reverse-apply checks above passed, which only means every hunk is still present — they are
    blind to an edit made outside every hunk's context, and that is what this digest catches.
    REGENERATE THE PATCH SERIES, DO NOT EDIT THE CHECKOUT:
      1. fold the change into the series:
           git -C $_dest add -N \$(paths the series creates)
           git -C $_dest diff HEAD > $_pdir/<series>.patch
           git -C $_dest reset -q
      2. accept the new state (rewrites $_dvar in third_party/MANIFEST.sh):
           bash third_party/bootstrap.sh --record-digest
    Recording without step 1 would bless an unreviewed change that lives nowhere but this checkout,
    which is gitignored — the next 'bootstrap.sh gfxstream' would throw it away."
    fi
    ok "$_name: working tree matches the recorded series ($_dvar)"
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
# The floor lives in MANIFEST.sh (see TP_MACOS_MIN_MAJOR) so this and ci/hvf-doctor.sh cannot drift.
# Checked before we BUILD, not before we verify: the doctor owns the run-time verdict, and refusing to
# verify an already-built tree would make this script useless on a host it can still describe. What it
# stops is spending 45 minutes building a KosmicKrisp the doctor will then reject.
if [ "$VERIFY_ONLY" = 0 ] && [ -n "${TP_MACOS_MIN_MAJOR:-}" ]; then
    _osver="$(sw_vers -productVersion 2>/dev/null || echo unknown)"
    if ! [ "${_osver%%.*}" -ge "$TP_MACOS_MIN_MAJOR" ] 2>/dev/null; then
        die "macOS $_osver — third_party/MANIFEST.sh pins a floor of macOS $TP_MACOS_MIN_MAJOR.
    KosmicKrisp is built on Metal APIs that do not exist on an older host and MoltenVK has been
    removed, so there is nothing to fall back to: building here produces a driver that cannot load."
    fi
fi
# Not in verify mode: --verify creates nothing. With AEMU_THIRD_PARTY pointed elsewhere this mkdir was
# the one write a verify-only run could still make.
[ "$VERIFY_ONLY" = 1 ] || mkdir -p "$TP_CHECKOUTS"

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
# The 6th argument names (does not dereference) the MANIFEST.sh variable holding this checkout's
# whole-tree digest, so --record-digest knows which line to rewrite.
checkout "$TP_GFXSTREAM_NAME" "$TP_GFXSTREAM_URL" "$TP_GFXSTREAM_SHA" "$TP_GFXSTREAM_SRC" "$TP_GFXSTREAM_PATCHES" \
         TP_GFXSTREAM_TREE_DIGEST

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
    nm -gU "$TP_GFXSTREAM_DYLIB" | grep -cE '_stream_renderer_(present_to_metal_layer|release_metal_layer|set_gpu_identity)$' | grep -qx 3 \
        || die "gfxstream: dylib built but does not export all three of stream_renderer_present_to_metal_layer,
    stream_renderer_release_metal_layer and stream_renderer_set_gpu_identity — the patch series did
    not reach the artifact (stale build dir?)"
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
if [ "$RECORD_DIGEST" = 1 ]; then
    ok "tree digests RECORDED in third_party/MANIFEST.sh — review that diff before committing; it is
    the reviewed statement of what the checkouts are allowed to contain"
elif [ "$VERIFY_ONLY" = 1 ]; then
    ok "third_party VERIFIED (pins + patches + whole-tree digest)"
else
    ok "third_party READY"
    printf '\n  next:  ci/hvf-doctor.sh   (proves the built artifacts actually load)\n'
fi
