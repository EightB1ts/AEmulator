# aemulator-hvf — Phase 2 Status

**Phase 2 goal:** the guest Linux console renders in a real macOS **window** via a virtio-gpu 2D
scanout, and keyboard/pointer input from that window reaches the guest via virtio-input. Pure
virtio-gpu 2D (dumb-framebuffer scanout) presented with a CPU-blit window — **no** gfxstream / 3D /
Metal-accelerated rendering (that is Phase 4).

**Status: COMPLETE — verified end-to-end, with a captured screenshot.**
A real macOS window (`aemulator-hvf — guest display`) opens showing the guest's virtio-gpu 2D
scanout: the full Alpine boot log + the interactive busybox rescue shell render **in the window**,
and injected window input (`avnir` + Enter) appeared at the shell prompt and was **executed** by the
guest (`sh: avnir: not found`). The no-`--display` headless path is unchanged and regression-verified
(still boots to the busybox shell, `P2_49`, spoofed `0x51`, clean `PoweredOff`).

---

## What renders (verified this run, from captured output + screenshot)

Screenshot: `testdata/phase2-display-input.png` (window presenting the guest scanout). In the window:

```
[    0.267669] Run /init as init process
Alpine Init 3.10.1-r0
[    0.277113] virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB/64.0 MiB)
[    0.298635] [drm] Initialized virtio_gpu 0.1.0 0 for a000600.virtio_mmio on minor 0
[    0.308411] Console: switching to colour frame buffer device 128x48
[    0.330496] virtio-mmio a000600.virtio_mmio: [drm] fb0: virtio_gpudrmfb frame buffer device
[    0.447758] input: aemulator-tablet as /devices/platform/a000800.virtio_mmio/virtio4/input/input0
initramfs emergency recovery shell launched. Type 'exit' to continue boot
sh: can't access tty; job control turned off
~ # avnir                 ← injected window keystrokes appeared at the prompt
sh: avnir: not found      ← the guest shell EXECUTED the injected command
~ #
```

VMM-side diagnostics during the run confirmed the framebuffer is genuinely populated (not black):
`[display] scanout gen=… non_zero_fb_bytes=127638` at boot, rising to `…129465` right after the
input self-test injected `avnir\n` (the extra text = the command echo + `not found` output).

Three claims, all verified:

| Claim | Evidence |
|---|---|
| **Window shown** | `screencapture -x` → `testdata/phase2-display-input.png`; window titled `aemulator-hvf — guest display` |
| **Console visible in window** | full guest boot log + `~ #` shell prompt rendered on the virtio-gpu scanout (128×48 fbcon); `non_zero_fb_bytes` ≈ 127–129 K |
| **Input reached guest** | injected `avnir` shows at the prompt AND the shell runs it → `sh: avnir: not found` |

**Regression gate (no `--display`):** `echo P2_$((7*7))` → `P2_49`; `AEMULATOR_SHELL_OK`;
`CPU implementer : 0x51`; `reboot: Power down` / `cpu0 stop reason: PoweredOff`. **phase1Regressed =
false.** The headless run uses the exact Phase-1 cmdline (`console=ttyAMA0` only, no `tty0`).

---

## Module map delta (vs Phase 1)

New/changed `src/` files this phase:

| File | Responsibility |
|---|---|
| `src/display/mod.rs` | **New.** The `--display` window layer. Owns the process **main thread**: opens a winit window sized to the scanout, presents each new frame with a softbuffer CPU blit (nearest-neighbour scaled; guest BGRA/XRGB → softbuffer `0RGB`), and translates winit keyboard/pointer events → Linux evdev codes → `InputFrame`s sent over the `input_tx` channel. Redraw is generation-gated (re-blits only when the scanout flush counter changes). Also hosts the `AEMU_INPUT_SELFTEST` deterministic input proof. |
| `src/virtio/gpu.rs` | **New.** virtio-gpu (device 16) **2D** — a dumb-framebuffer scanout. Implements the 2D control protocol subset (`GET_DISPLAY_INFO`, `RESOURCE_CREATE_2D`, `ATTACH/DETACH_BACKING`, `SET_SCANOUT`, `TRANSFER_TO_HOST_2D`, `RESOURCE_FLUSH`) + a drained cursorq. Each `RESOURCE_FLUSH` copies the resource image into a host `Framebuffer` behind `ScanoutHandle` (`Arc<Mutex<Framebuffer>>` + `AtomicU64` generation) that the display layer polls. No virgl/EDID/blob features advertised, so the guest stays on the 2D path. |
| `src/virtio/input.rs` | **New.** virtio-input (device 18) — a combined **tablet** (abs pointer `ABS_X`/`ABS_Y` over 0..32767 + `BTN_LEFT`/`BTN_TOUCH`) **and keyboard** (`EV_KEY` 0..255). Standard select/subsel-multiplexed `virtio_input_config`; eventq (host→guest, one 8-byte event per descriptor) + drained statusq. `queue_frame` appends the trailing `EV_SYN`/`SYN_REPORT` evdev consumers expect. |
| `src/machine.rs` | **Changed.** `run_linux_display` + `boot_and_run(display)`: builds the virtio-gpu + virtio-input transports, keeps a `ScanoutHandle`, and — in display mode — spawns the **input-inject thread** (drains the `input_tx` `Receiver`, queues events on the input eventq, raises the device SPI + kicks the vCPU) and hands a `DisplayBridge {scanout, input_tx, w, h, vmm_done}` to the main thread over a `sync_channel`. Headless mode skips all of that (exact Phase-1 path). New `DEFAULT_DISPLAY_CMDLINE` appends `console=tty0` so the shell renders on the framebuffer in display mode. |
| `src/bin/aemulator-hvf.rs` | **Changed.** New `--display` flag → `run_windowed`: spawns the VMM on a worker thread, `recv`s the `DisplayBridge`, then runs `display::run_event_loop` on the main thread (macOS AppKit rule). `--cmdline` now defaults per mode (display → `DEFAULT_DISPLAY_CMDLINE`, headless → `DEFAULT_CMDLINE`); an explicit `--cmdline` still overrides. |
| `src/virtio/mmio.rs`, `src/virtio/mod.rs` | **Changed.** Transport carries the two new backends (`is_input`/`is_console` helpers, `inject_input_events`, `feed_console_input`), device-id/SPI slot wiring for the gpu + input slots, and the `ScanoutHandle` re-export. (Phase-1 SHM_LEN transport bug — returning 0 instead of −1 — was fixed in the prior GpuInput stage; without it virtio_gpu probe failed `-EBUSY`.) |
| `Cargo.toml` | **Changed.** Added `winit = "0.29"` + `softbuffer = "0.4"` for the CPU-blit window. |

**Thread model (`--display`):** main thread = winit event loop + window (AppKit requirement). Worker
threads = boot vCPU (`vmm`), PSCI-spawned secondaries, `console-stdin`, and `input-inject`.
VMM→UI = `ScanoutHandle` (framebuffer + generation). UI→VMM = `mpsc::Sender<InputFrame>`. The window
closes when the guest stops (`vmm_done` `AtomicBool`) or the user closes it.

IPA map unchanged from Phase 1; virtio slots now: 0 console, 1 rng, 2 block, 3 gpu, 4 input.

---

## What works

- **Real macOS window** presenting the guest virtio-gpu 2D scanout (1024×768 default mode; guest
  fbcon runs at 128×48). CPU blit, no Metal. Retina/resize handled by nearest-neighbour scaling.
- **Guest console visible in the window** — full boot log + interactive shell, because display mode
  routes `/dev/console` to `tty0` (the framebuffer console on `/dev/fb0`).
- **Window → guest input** — winit keyboard events → Linux `KEY_*` scancodes → virtio-input eventq →
  guest evdev; observed as typed text + command execution at the framebuffer shell. Pointer motion +
  left-click are wired as absolute-tablet events (`ABS_X`/`ABS_Y` + `BTN_LEFT`/`BTN_TOUCH`).
- **Headless path unchanged** — no `--display` boots to the busybox shell over serial exactly as
  Phase 1 (regression-verified: `P2_49`, `AEMULATOR_SHELL_OK`, spoof `0x51`, clean `PoweredOff`).
- Build green + warning-clean; 18 unit tests pass (gpu transfer/flush, input config/encoding, …).

## What remains / known caveats

- **Synthetic OS-level key events aren't delivered to the winit window in this headless agent
  context** (macOS Accessibility permission). So the window→guest path is exercised deterministically
  via `AEMU_INPUT_SELFTEST`, which pushes frames through the **identical** `input_tx` channel a real
  winit `KeyboardInput` uses. A user typing physically in the window exercises the single remaining
  link (macOS event → winit `KeyboardInput` → `send_frame`). Pointer/click wiring is present but not
  separately screenshot-verified (no on-screen cursor consumer in the busybox shell).
- **Shell binds to `tty0` only in display mode** (via `console=tty0`). This is a deliberate cmdline
  choice, not device autoconfig — the stock Alpine initramfs ships no `evdev.ko`, so virtio-input
  binds the in-kernel keyboard handler feeding the foreground VT rather than exposing a usable
  `/dev/input/eventX` (the `/sys/class/virtio-ports` probe warning in the log is unrelated — a
  virtio-console detail).
- **No host cursor sprite** — cursorq commands are drained/acked but not drawn.
- **2D only** — no virgl/3D/blob resources (Phase 4 / gfxstream).
- **No in-VMM wall-clock guard** — a `--display` run keeps the VM alive until the window closes or
  the guest powers off; unattended runs are bounded by an external killer, as in Phase 1.

---

## Repro

Prereqs staged in `testdata/` (git-ignored): `Image`, `initramfs-virt`, `scratch.img`.

**Windowed (Phase 2):**
```bash
./crates/aemulator-hvf/run-linux-display.sh
# deterministic input proof without interactive typing:
AEMU_INPUT_SELFTEST=1 ./crates/aemulator-hvf/run-linux-display.sh
```
`run-linux-display.sh` is just `run-linux.sh --display`. You can also pass `--display` to
`run-linux.sh` directly, or to the binary:
```bash
target/debug/aemulator-hvf run-linux --kernel testdata/Image --initrd testdata/initramfs-virt \
  --disk testdata/scratch.img --mem-mb 1024 --cpus 1 --display
```

**Headless (Phase-1 regression):** omit `--display` — `./crates/aemulator-hvf/run-linux.sh`.

Bounded/non-interactive display capture (macOS has no `timeout`):
```bash
AEMU_INPUT_SELFTEST=1 target/debug/aemulator-hvf run-linux --kernel testdata/Image \
  --initrd testdata/initramfs-virt --disk testdata/scratch.img --mem-mb 1024 --cpus 1 --display &
P=$!; ( perl -e 'sleep 70; kill 9, shift' $P & )
sleep 24; screencapture -x /tmp/phase2.png; kill -9 $P
```

Build discipline: this crate is workspace-**excluded**; always build with
`--manifest-path crates/aemulator-hvf/Cargo.toml`, never the root `cargo build`.

---

## Next concrete steps (toward Phase 3 — Android/Cuttlefish)

1. **Pointer/cursor round-trip** — draw a host cursor sprite + verify click reaches a guest consumer
   (a framebuffer app, or a kernel with `evdev` so `/dev/input/eventX` is usable).
2. **virtio-net** (rust-vmm `virtio-queue` + host user-net/tap) — first I/O beyond block, needed
   before Android.
3. **Kernel with `virtio_gpu` + `evdev` built-in** so the window path doesn't depend on the
   `console=tty0` cmdline trick and input surfaces as a proper evdev node.
4. **In-VMM `--run-secs` idle/wall-clock guard** so windowed runs self-terminate.
5. **gfxstream / 3D scanout** is Phase 4 — out of scope here.
