#!/usr/bin/env bash
# scripts/setup.sh — get a checkout from "just cloned" to "Studio can start a VM", idempotently.
#
#   pnpm run setup     # this script on its own
#   pnpm run dev       # this script, then the Studio dev loop
#
# Every step checks first and only does work when something is missing or stale, so on a set-up
# machine the whole thing costs about a second (mostly a no-op `cargo build`). A first run on a
# fresh machine takes a long time — the GPU stack build is ~45 minutes and the Android image is a
# ~1.1 GB download — and everything it builds or downloads is kept, so it happens once.
#
# What it handles:
#   1. Homebrew packages the build and the emulator need     (brew install, only the missing ones)
#   2. the out-of-tree GPU stack: gfxstream + KosmicKrisp     (third_party/bootstrap.sh)
#   3. Studio's JavaScript dependencies                       (pnpm install --frozen-lockfile)
#   4. the emulator binary, release build, hypervisor-signed  (cargo build + codesign)
#   5. the base Android image Studio boots                    (crates/aemulator-hvf/fetch-cf64.sh)
#
# What it will not install for you, because each needs your say-so or an installer of its own:
# Xcode (or its Command Line Tools), Homebrew, Rust (rustup), Node 22+ and pnpm 11+.
#
#   AEMU_SKIP_IMAGE=1 pnpm run dev    # skip step 5 (Studio runs; Start needs the image)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HVF="$ROOT/crates/aemulator-hvf"
cd "$ROOT"
# paths.sh discovers the KosmicKrisp ICD with a glob that matches nothing before the first bootstrap;
# under pipefail that empty glob would abort here.
set +o pipefail; . "$ROOT/third_party/paths.sh"; . "$ROOT/third_party/MANIFEST.sh"; set -o pipefail

if [ -t 1 ]; then C_G=$'\033[32m'; C_Y=$'\033[33m'; C_R=$'\033[31m'; C_B=$'\033[1m'; C_0=$'\033[0m'; else C_G=; C_Y=; C_R=; C_B=; C_0=; fi
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '%s  ! %s%s\n' "$C_Y" "$*" "$C_0"; }
die()  { printf '\n%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

# --- 0. host + tools you install yourself -----------------------------------------------------
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || die "AEmulator needs an Apple Silicon Mac"
_os="$(sw_vers -productVersion)"
[ "${_os%%.*}" -ge "${TP_MACOS_MIN_MAJOR:-26}" ] ||
    die "macOS $_os is too old — the Metal GPU driver needs macOS ${TP_MACOS_MIN_MAJOR:-26} or newer"
[ "$(sysctl -n kern.hv_support 2>/dev/null)" = 1 ] || die "this Mac reports no Hypervisor.framework support"
xcode-select -p >/dev/null 2>&1 || die "Xcode Command Line Tools are missing — run: xcode-select --install"
command -v brew >/dev/null || die "Homebrew is missing — install it from https://brew.sh, then re-run"
command -v cargo >/dev/null || die "Rust is missing — install it from https://rustup.rs, then re-run"

BREW_PREFIX="$(brew --prefix)"
ensure_brew() { # <why> <formula>...
    local why="$1" f missing=(); shift
    for f in "$@"; do [ -e "$BREW_PREFIX/opt/$f" ] || missing+=("$f"); done
    [ "${#missing[@]}" -eq 0 ] && return 0
    step "installing Homebrew packages for $why: ${missing[*]}"
    brew install "${missing[@]}" || die "brew install failed"
}

# --- 1. runtime packages: libslirp is linked into the emulator, vulkan-loader is loaded by it ----
ensure_brew "the emulator" libslirp vulkan-loader

# --- 2. GPU stack -----------------------------------------------------------------------------
# Built only when an artifact is missing. bootstrap.sh is idempotent and refuses (rather than
# overwrites) a checkout with edits that are not in the tracked patch series.
if [ ! -f "$TP_GFXSTREAM_DYLIB" ] || [ ! -f "$TP_KOSMICKRISP_DYLIB" ] || [ ! -f "$TP_KOSMICKRISP_ICD" ]; then
    ensure_brew "the GPU stack build" cmake ninja pkgconf vulkan-headers llvm libclc spirv-tools \
        spirv-llvm-translator bison flex python@3.12
    step "building the GPU stack (gfxstream + KosmicKrisp) — first time only, ~45 minutes"
    bash "$ROOT/third_party/bootstrap.sh" || die "third_party/bootstrap.sh failed (see above)"
fi
ok "GPU stack: gfxstream + KosmicKrisp"

# --- 3. Studio JS deps ------------------------------------------------------------------------
STUDIO="$ROOT/aemulator-studio"
if [ ! -f "$STUDIO/node_modules/.modules.yaml" ] || [ "$STUDIO/pnpm-lock.yaml" -nt "$STUDIO/node_modules/.modules.yaml" ]; then
    step "installing Studio's JavaScript dependencies"
    pnpm -C "$STUDIO" install --frozen-lockfile || die "pnpm install failed"
fi
ok "Studio dependencies"

# --- 4. emulator binary -----------------------------------------------------------------------
# Always run cargo: it is a no-op when nothing changed and rebuilds when the VMM source did, so
# Studio never launches a stale binary. -q because a no-op build otherwise replays every cached
# build-script warning. Even a no-op build re-copies target/release/aemulator-hvf from deps/, which
# drops the hypervisor entitlement, so sign after every build. Both cargo and codesign write a new
# file (new inode), so a VM already running from the old binary is never touched.
step "building the emulator (release)"
cargo build -q --release --manifest-path "$HVF/Cargo.toml" --bin aemulator-hvf || die "emulator build failed"
BIN="$HVF/target/release/aemulator-hvf"
if ! codesign -d --entitlements :- "$BIN" 2>&1 | grep -q com.apple.security.hypervisor; then
    codesign --sign - --entitlements "$HVF/hv.entitlements" --force "$BIN" 2>/dev/null || die "codesign failed"
fi
ok "emulator: $BIN (hypervisor-signed)"

# --- 5. base Android image --------------------------------------------------------------------
if [ "${AEMU_SKIP_IMAGE:-0}" = 1 ]; then
    "$HVF/fetch-cf64.sh" --check && ok "Android image" ||
        warn "AEMU_SKIP_IMAGE=1 — no Android image yet, so Start will fail until you run: pnpm run setup"
elif ! "$HVF/fetch-cf64.sh" --check; then
    ensure_brew "the Android image preps" e2fsprogs
    step "setting up the Android image (testdata/cf64)"
    "$HVF/fetch-cf64.sh" || die "Android image setup failed (see above) — re-run to resume"
else
    ok "Android image: $HVF/testdata/cf64"
fi

printf '\n%sReady.%s\n' "$C_G" "$C_0"
