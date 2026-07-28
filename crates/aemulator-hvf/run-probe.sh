#!/usr/bin/env bash
# Build, entitle (ad-hoc codesign), and run the HVF MIDR-spoof probe on this Apple Silicon host.
#
#   ./crates/aemulator-hvf/run-probe.sh
#
# The com.apple.security.hypervisor entitlement is REQUIRED for hv_vm_create to succeed; an
# unentitled binary fails with HV_DENIED (0xfae94007). Ad-hoc signing (`--sign -`) is enough on a
# development machine with SIP in its normal state.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$HERE/Cargo.toml"
ENTITLEMENTS="$HERE/hv.entitlements"

echo "== 1. build =="
cargo build --manifest-path "$MANIFEST"

# NOTE: this crate is EXCLUDED from the workspace, so it builds into its OWN target dir here,
# not the workspace-root target/.
BIN="$HERE/target/debug/hvf-midr-probe"
echo "binary: $BIN"

echo "== 2. ad-hoc codesign with hypervisor entitlement =="
codesign --sign - --entitlements "$ENTITLEMENTS" --force "$BIN"
codesign -d --entitlements - "$BIN" 2>/dev/null || true

echo "== 3. run =="
"$BIN"
