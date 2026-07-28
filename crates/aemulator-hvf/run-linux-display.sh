#!/usr/bin/env bash
# Phase 2: build, entitle, and boot the aarch64 Linux guest into a REAL macOS WINDOW. The guest's
# virtio-gpu 2D scanout is presented in the window (a CPU blit via winit + softbuffer) and keyboard
# + pointer input from the window are forwarded into the guest's virtio-input device.
#
#   ./crates/aemulator-hvf/run-linux-display.sh
#
# This is just `run-linux.sh … --display`. In display mode the kernel cmdline routes /dev/console to
# tty0 (the framebuffer console on /dev/fb0), so the interactive busybox rescue shell renders IN THE
# WINDOW and the keys you type in the window reach it (the in-kernel keyboard handler feeds the
# foreground VT). `console=ttyAMA0` is kept as well, so the serial boot log still prints to this
# terminal alongside the window. Close the window (or `poweroff -f` in the guest) to stop.
#
# Optional deterministic input proof (no interactive typing needed):
#   AEMU_INPUT_SELFTEST=1 ./crates/aemulator-hvf/run-linux-display.sh
# ~18s after launch a background thread pushes "avnir\n" through the SAME input channel a real winit
# keystroke uses; you will see `~ # avnir` / `sh: avnir: not found` appear in the window — proof the
# window→virtio-input→guest-evdev→shell path is live. A physical keystroke takes the identical path.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$HERE/run-linux.sh" --display "$@"
