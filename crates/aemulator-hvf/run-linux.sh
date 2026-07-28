#!/usr/bin/env bash
# Build, entitle (ad-hoc codesign), and boot an aarch64 Linux guest to an INTERACTIVE SHELL under
# Apple's Hypervisor.framework, with a userspace GICv3 + PL011 + virtio-mmio (console/rng/block).
#
#   ./crates/aemulator-hvf/run-linux.sh
#
# Prereqs staged in testdata/ (fetched once, all git-ignored):
#   * Image            — gunzipped Alpine 3.20 aarch64 vmlinuz-virt (kernel 6.6.134-0-virt)
#   * initramfs-virt   — Alpine 3.20 aarch64 netboot initramfs (its /init drops to a busybox
#                        rescue shell when it finds no boot media — that is our shell)
#   * scratch.img      — 64 MiB raw file backing virtio-block (/dev/vda); created below if missing
#
# The shell runs on ttyAMA0 (the built-in PL011 — it is the only console registered when the kernel
# opens PID 1's initial console; virtio-console hvc0 is a module and registers too late to be the
# initial console). virtio-console (/dev/hvc0) and virtio-block (/dev/vda) are still enumerated by
# the guest. Type commands directly; they run in the guest and echo back over the console.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$HERE/Cargo.toml"
ENTITLEMENTS="$HERE/hv.entitlements"
BIN="$HERE/target/debug/aemulator-hvf"
TD="$HERE/testdata"

echo "== build =="
cargo build --manifest-path "$MANIFEST"

echo "== ad-hoc codesign with hypervisor entitlement =="
codesign --sign - --entitlements "$ENTITLEMENTS" --force "$BIN"

[ -f "$TD/scratch.img" ] || dd if=/dev/zero of="$TD/scratch.img" bs=1m count=64 2>/dev/null

echo "== run (interactive; Ctrl-C to stop) =="
# Pass through extra flags, e.g. `./run-linux.sh --display` to open a window showing the guest
# virtio-gpu scanout with keyboard + pointer forwarded into the guest's virtio-input device.
exec "$BIN" run-linux \
  --kernel "$TD/Image" \
  --initrd "$TD/initramfs-virt" \
  --disk   "$TD/scratch.img" \
  --mem-mb 1024 \
  --cpus 1 \
  "$@"
