#!/usr/bin/env bash
#
# check.sh — the AEmulator merge gate.
#
# Runs, in order, and fails hard (exit non-zero) on the first failure:
#   1. cargo fmt --all --check     (formatting is canonical rustfmt, 4-space)
#   2. cargo clippy -D warnings    (all workspace crates, all targets — zero warnings)
#   3. cargo test                  (default features; whole workspace)
#   4. the IDENTITY-VALIDATOR GATE  — the load-bearing anti-detection check:
#        a. `profile-validate`  recomputes ConstructBuildFingerprint + coherence asserts
#        b. build the reference profile's GUEST-FACING artifacts (props, bootconfig, DTB)
#           via the CLI and grep them against the hard virtualization blocklist
#           (ranchu|goldfish|qemu|cutf|vsoc|crosvm|swiftshader|dummy-virt|houdini|exagear).
#           ANY hit is a full compromise (plan §"the leak surface is the whole image") → fail.
#   5. ci/hvf-doctor.sh            (host Vulkan driver + gfxstream ABI match)
#   6. the WORKSPACE-EXCLUDED crates — `crates/aemulator-hvf` and `aemulator-studio/src-tauri` are
#      in `[workspace].exclude`, so steps 1-3 provably cannot reach them. Without step 6 the whole
#      VMM + graphics stack and the Studio backend are unformatted, unlinted and untested by this
#      gate. Steps 5 and 6 are REQUIRED on Apple-Silicon macOS and skipped LOUDLY elsewhere.
#
# NOTE the blocklist is grepped against the GUEST-facing artifacts only — NOT `qemu-args`,
# which is the HOST launcher command line and legitimately contains "qemu-system-aarch64".
#
# Default features only (macOS/Apple-Silicon safe). The Linux/KVM crosvm backend is behind the
# `kvm` cargo feature + cfg(target_os="linux"); this gate does not build it.
#
# Usage:  bash ci/check.sh            # full gate
#         AEMU_SKIP_BUILD_CHECKS=1 bash ci/check.sh   # blocklist gate only (fast, assumes built)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"
# Out-of-tree artifact paths (the gfxstream dylib this gate preconditions on) come from here only.
. "$ROOT/third_party/paths.sh"

# --- pretty logging ---------------------------------------------------------
if [ -t 1 ]; then C_G=$'\033[32m'; C_R=$'\033[31m'; C_Y=$'\033[33m'; C_B=$'\033[1m'; C_0=$'\033[0m'; else C_G=; C_R=; C_Y=; C_B=; C_0=; fi
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
fail() { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

# The hard virtualization blocklist (plan §"CI validator", verbatim).
BLOCKLIST='ranchu|goldfish|qemu|cutf|vsoc|crosvm|swiftshader|dummy-virt|houdini|exagear'

# --- 1..3: format / lint / test --------------------------------------------
if [ "${AEMU_SKIP_BUILD_CHECKS:-0}" != "1" ]; then
    step "cargo fmt --all --check"
    cargo fmt --all --check || fail "rustfmt: run 'cargo fmt --all'"
    ok "formatting clean"

    step "cargo clippy --all-targets --all-features? (default features) -D warnings"
    cargo clippy --all-targets -- -D warnings || fail "clippy: fix warnings above"
    ok "clippy clean (zero warnings)"

    step "cargo test (default features)"
    cargo test || fail "cargo test: see failures above"
    ok "tests green"
else
    step "AEMU_SKIP_BUILD_CHECKS=1 — skipping fmt/clippy/test, running blocklist gate only"
fi

# --- 4: identity-validator gate --------------------------------------------
# Build the CLI once so the artifact steps are fast and don't re-trigger a compile mid-grep.
step "building aemulator-cli (release-independent debug binary)"
cargo build -q -p aemulator-cli || fail "failed to build aemulator-cli"
BIN="$ROOT/target/debug/aemulator"
[ -x "$BIN" ] || fail "CLI binary not found at $BIN"
ok "CLI built: $BIN"

# 4a. coherence + recomputed fingerprint
step "identity coherence: profile-validate (recompute ConstructBuildFingerprint + coherence asserts)"
"$BIN" profile-validate || fail "reference profile is INCOHERENT"
ok "profile is coherent"

# 4b. blocklist grep over the guest-facing artifacts
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

scan_text() { # name  <text on stdin>
    local name="$1" hits
    hits="$(grep -Eino "$BLOCKLIST" || true)"
    if [ -n "$hits" ]; then
        printf '%s\n' "$hits" | sed "s|^|    ${name}: |" >&2
        return 1
    fi
    return 0
}

step "blocklist gate: props (Android system-property map)"
if ! "$BIN" props | scan_text "props"; then fail "BLOCKLISTED string in emitted properties"; fi
ok "props clean"

step "blocklist gate: bootconfig (androidboot.* — must never carry androidboot.qemu)"
BOOTCFG="$("$BIN" bootconfig)"
if printf '%s\n' "$BOOTCFG" | scan_text "bootconfig"; then :; else fail "BLOCKLISTED string in bootconfig"; fi
# explicit belt-and-braces: androidboot.qemu must be absent
if printf '%s\n' "$BOOTCFG" | grep -qi 'androidboot\.qemu'; then fail "bootconfig emits androidboot.qemu"; fi
ok "bootconfig clean (no androidboot.qemu)"

step "blocklist gate: device-tree blob (binary byte-scan)"
DTB="$WORK/reference.dtb"
"$BIN" dtb "$DTB" >/dev/null || fail "failed to build DTB"
DTB_HITS="$(LC_ALL=C grep -Eiao "$BLOCKLIST" "$DTB" || true)"
if [ -n "$DTB_HITS" ]; then
    printf '    dtb: %s\n' "$DTB_HITS" >&2
    fail "BLOCKLISTED string in device-tree blob"
fi
ok "DTB clean ($(wc -c <"$DTB" | tr -d ' ') bytes)"

step "host graphics stack (delegates to ci/hvf-doctor.sh)"
# hvf-doctor.sh is the ONLY guard for the host Vulkan driver and for the gfxstream ABI match, and
# until now nothing invoked it — it was a manual tool, so the checks that would have caught a stale
# binary segfaulting at address 0, or a silent revert to MoltenVK, never ran in the merge gate.
#
# Its dependencies are host-local (a built dylib, the KosmicKrisp install, an ad-hoc codesign), none
# of which exist on a hosted Linux runner. So it is REQUIRED on an Apple-Silicon macOS host and
# SKIPPED (loudly, never silently) anywhere else — a skip that reads as a pass is how the gap above
# got created in the first place.
DOCTOR="$(dirname "$0")/hvf-doctor.sh"
if [ ! -x "$DOCTOR" ] && [ ! -f "$DOCTOR" ]; then
    fail "ci/hvf-doctor.sh is missing — the host graphics stack has no guard"
elif [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    printf '%s  ! skipped: needs Apple Silicon macOS (host is %s/%s) — the graphics stack is UNVERIFIED on this runner%s\n' \
        "$C_Y" "$(uname -s)" "$(uname -m)" "$C_0"
elif [ ! -f "$TP_GFXSTREAM_DYLIB" ]; then
    printf '%s  ! skipped: gfxstream host renderer not built on this machine — run third_party/bootstrap.sh%s\n' \
        "$C_Y" "$C_0"
else
    bash "$DOCTOR" || fail "hvf-doctor failed — see its output above"
    ok "host graphics stack verified (driver ICD + gfxstream ABI match)"
fi

# --- 6: the workspace-EXCLUDED crates ---------------------------------------
# Steps 1-3 above run `cargo fmt --all` / `clippy --all-targets` / `test` against the WORKSPACE, and
# `crates/aemulator-hvf` + `aemulator-studio/src-tauri` are both in `[workspace].exclude`
# (Cargo.toml:24-27). Cargo cannot reach an excluded crate from the workspace root, so until this
# step existed ~10k lines — the entire VMM and graphics stack, plus the Studio backend — were never
# formatted, linted or tested by the gate. Not merely untested: unlinted and unformatted.
#
# Gated exactly like hvf-doctor above, and for the same reason: both crates' build.rs hard-fail
# without host-local dependencies (aemulator-hvf links -framework Hypervisor and needs libslirp +
# the gfxstream dylib; the Studio pulls the Tauri/wry tree). So this is REQUIRED on an
# Apple-Silicon macOS host with a built dylib and SKIPPED LOUDLY anywhere else. A skip that reads as
# a pass is how the original gap was created.
if [ "${AEMU_SKIP_BUILD_CHECKS:-0}" != "1" ]; then
    step "workspace-excluded crates: aemulator-hvf + aemulator-studio/src-tauri (fmt · clippy · test)"
    if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
        printf '%s  ! skipped: needs Apple Silicon macOS (host is %s/%s) — aemulator-hvf and the Studio backend are UNVERIFIED on this runner%s\n' \
            "$C_Y" "$(uname -s)" "$(uname -m)" "$C_0"
    elif [ ! -f "$TP_GFXSTREAM_DYLIB" ]; then
        printf '%s  ! skipped: gfxstream host renderer not built on this machine — run third_party/bootstrap.sh%s\n' \
            "$C_Y" "$C_0"
    else
        for m in crates/aemulator-hvf/Cargo.toml aemulator-studio/src-tauri/Cargo.toml; do
            cargo fmt --manifest-path "$m" --check || fail "rustfmt: run 'cargo fmt --manifest-path $m'"
            cargo clippy --manifest-path "$m" --all-targets -- -D warnings || fail "clippy: fix warnings in $m"
            cargo test --manifest-path "$m" || fail "cargo test failed for $m"
            ok "$(dirname "$m"): fmt + clippy + tests green"
        done
    fi
fi

printf '\n%s==> ALL CHECKS PASSED%s\n' "$C_G" "$C_0"
