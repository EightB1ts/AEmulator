# AEmulator Studio

A Mac-native desktop app (**Tauri** — Rust backend + React/Vite frontend) that sets up,
runs, drives, and manages the **`aemulator-hvf`** ARM64 Android emulator — the from-scratch
Rust VMM that boots 64-bit Android on Apple Silicon via Hypervisor.framework.

Studio does **not** re-implement the VM. It **spawns and supervises the real `aemulator-hvf`
binary as a child process** (the same launch the `run-control64.sh` script performs) and talks
its host-side **control plane over a Unix socket** (framebuffer capture + virtio-input
injection). Postgres/Zero are not involved; this is a standalone tool.

---

## What it is

- **Multi-emulator manager.** Create named emulator *profiles* (base image, RAM, vCPUs, render
  mode, SELinux mode, device identity), start/stop them, and drive each live.
- **Real per-VM isolation.** Every start provisions the profile its own cf-dir via APFS
  `clonefile` (instant copy-on-write of the base image), a unique control socket, and a unique
  adb host-forward port.
- **One page per emulator.** Opening a card lands on that emulator's hub — five tabs
  (**Display · Hardware · Settings · Storage · Logs**) over a shared header with its state pill,
  Start/Stop and adb-connect hint. Everything about one VM is in one place; the two global
  surfaces are the emulator grid and the image catalog.
- **Live driving.** The Display tab gives the guest framebuffer the full window width (a ~4–6 fps
  screenshot poll) and turns clicks into taps, drags into swipes. The controls sit in a menu-bar
  strip above it: hardware keys, text injection, package launch, one-shot screenshot, and
  wait-stable — all over the control socket.
- **Click the screen to drive it.** That click captures the mouse *and* the keyboard: keystrokes go
  to Android key-by-key, held modifiers included (Shift/Ctrl/Option/**Command** → evdev
  `KEY_LEFTMETA`), over the control plane's `keydown`/`keyup` verbs. A blue pill on the frame shows
  capture is live and how to leave it (**Cmd+Esc**). macOS keeps a handful of chords for itself
  (Cmd+Tab/Space/`` ` `` never reach a WebView; the app menu owns Cmd+Q/W/M/H and Cmd+A/C/V/X/Z),
  so those stay with the host rather than being half-swallowed. The guest's serial
  console (logcat) streams into the full-height Logs tab (substring + stderr-only filters).
- **Honest device inventory.** The Hardware tab lists what the emulator *actually* provides,
  grouped by subsystem (compute / graphics / storage / I/O / sensors / security) with the honesty
  tier as a per-item dot — **Emulated** (real data path), **Partial** (metadata/stub only),
  **Planned** (collapsed behind one disclosure, with the Phase-4 inject-control shapes behind a
  toggle). Items with live control link straight to the Display tab.
- **Editable settings.** The Settings tab edits a saved profile in place. Name and device
  identity apply immediately; memory, vCPUs, render pipeline, SELinux and kernel cmdline are
  composed into the launch command at start, so they're gated on the VM being stopped rather than
  silently deferred. Changing the base image additionally erases the provisioned COW clone (the
  clone is otherwise reused, which would keep booting the old image behind a new label).
- **Storage view.** Per-emulator COW-aware disk accounting (on-disk vs logical vs COW-saved) on
  the instance's Storage tab; the global Images page keeps the base-image catalog and a
  cross-instance roll-up. Snapshots are a Phase-2 placeholder.

---

## Requirements

- **Apple Silicon Mac** (arm64) — Hypervisor.framework + the emulator are aarch64-only.
- **The `aemulator-hvf` crate**, with its **release** binary already built and its `testdata/cf64`
  base image prepped. Studio resolves the crate automatically by walking up from its own exe/CWD
  to find `crates/aemulator-hvf`; a relocated `.app` bundle needs `AEMU_HVF_DIR` set to the
  crate path. Build the emulator binary first if it's missing:
  ```bash
  cargo build --release --manifest-path ../crates/aemulator-hvf/Cargo.toml --bin aemulator-hvf
  ```
  **No `--features` flag** — GPU (gfxstream/Metal) and networking (libslirp NAT) are compiled in
  unconditionally and cannot be turned off. They used to be cargo features; that is exactly how a
  `--features gfxstream`-only release binary ended up on disk being spawned by Studio, with a NIC
  that enumerated `eth0`, took its `10.0.2.15` lease and started adbd while dropping every packet —
  so it read as "adb is broken", not "wrong binary". The features are gone; `--features net` now
  errors instead of silently meaning something.

  **Still build `release`.** Studio prefers `target/release/aemulator-hvf` over `target/debug` (see
  `paths.rs` — an unoptimized VMM cannot feed the live viewer), so a *stale* release binary silently
  shadows a freshly built debug one. Studio refuses to launch a binary with no NAT backend (the
  stale-binary case) and names the rebuild, and the Hardware tab reports the NIC as Partial rather
  than claiming NAT it does not have. Override the profile choice with `AEMU_HVF_PROFILE=debug` when
  you deliberately want the debug VMM.
- **libslirp** for networking: `brew install libslirp`.
- **Node + pnpm** and a **Rust toolchain** to build Studio itself.
- Studio ad-hoc **codesigns** the emulator binary with the hypervisor entitlement on every start
  (required, or `hv_vm_create` is denied) — no manual signing step needed.

---

## How to run

From this directory (`aemulator-studio/`):

```bash
pnpm install            # first time only

pnpm tauri:dev          # dev: native window + Vite HMR (RECOMMENDED)
pnpm tauri:build        # release: ad-hoc-signed .app + .dmg under src-tauri/target/release/bundle
pnpm tauri:build --debug  # faster unoptimized bundle

# frontend-only sanity checks (no native window / IPC is a no-op):
pnpm build              # tsc --noEmit && vite build
pnpm dev                # Vite dev server in a browser
```

Backend unit tests:

```bash
cd src-tauri && cargo test      # control/info parsing, launch compose, base64, socket-path guard
```

**Try the core loop:** `pnpm tauri:dev` → on the **Emulators** dashboard click **Start** on a
profile → Studio provisions the cf-dir, signs + spawns `aemulator-hvf control …`, and drops you
into the **Running** view. The status flips **Booting → Ready** when the control socket answers;
the lock screen appears once Android renders (SwRender first boot is a few minutes). Drag up on
the framebuffer to unlock. `adb connect 127.0.0.1:<port>` (shown top-right) attaches adb.

---

## MVP status

**Working end-to-end (verified):** build + launch, profile create/list/delete (persisted),
Start → real `aemulator-hvf` child (SwRender), boot to the Android lock screen, live framebuffer
poll in the Running view, `info`/screenshot/tap/swipe/key over the control socket, streamed
console log, Stop (graceful `quit` → SIGTERM → SIGKILL), live host capability strip, storage
accounting. See `screenshots/` for a captured run (dashboard, create wizard, devices, storage,
and the live Running view showing the booted home screen).

### Functional (real data path)
- **VM lifecycle** — provision (clonefile) → sign → spawn → supervise (stdout/stderr → log ring +
  `vm-log` events; child reap → `vm-state`) → readiness monitor (socket `info` → Booting→Ready).
- **Control** — tap / swipe / key / text / launch / wait-stable / screenshot, spoken directly on
  the Unix socket (lock held only to read the socket path, released before blocking I/O).
- **Framebuffer** — one-shot screenshot (PNG data-URL) + a change-gated streaming poll over a
  Tauri Channel.
- **Profiles** — atomic JSON persistence at
  `~/Library/Application Support/com.avnir.aemulator-studio/profiles.json` (seeded on first run).
- **Host / images / storage** — live `hw.memsize` / `vm_stat` / libslirp + codesign probes;
  `testdata/` base-image scan; COW-aware `du` accounting.

### Render modes
- **SwRender** (default, verified) — `SWRENDER=1`, guest-CPU SwiftShader/ANGLE. The stable
  headless-capable boot path.
- **Metal GPU (gpu3d)** — composes `--display --gpu3d` and stages the MoltenVK Vulkan ICD like
  `run-control64.sh`; the GPU path is always compiled in. Wired but not
  re-verified in this build (the pre-built binary booted fine under SwRender).

### Stubbed / Planned (honestly marked in the UI)
- **Snapshots** — `create/restore/delete_snapshot` return `NotImplemented("Phase 2")`; the
  Storage cards show a "Phase 2 placeholder" badge.
- **Planned devices** — GPS, camera, battery/power, clipboard, SD card, Bluetooth, NFC,
  vibration: inert disabled forms (the shape of the eventual inject verbs), no data path yet.
- **Partial devices** — GPU 3D, extra disks, KeyMint, verified-boot state: advertised metadata,
  no live values.
- **`launch` / text** — best-effort over the guest **serial console**, not adb (inherent to the
  emulator's control plane).

---

## Notes / gotchas

- **Control socket path must be short.** macOS caps a unix-domain socket's `sun_path` at 104
  bytes. The per-VM instance dir (`~/Library/Application Support/…/instances/<uuid>/`) is ~125
  bytes, so the socket lives at `/tmp/aemu-<uuid>.sock` (~51 bytes) instead — otherwise
  `UnixListener::bind` fails "path too long" and the whole control plane (readiness, screenshot,
  tap) silently dies while only the stdout console log survives. Guarded by a unit test.
- **Contract deviations from the original IPC sketch:** `screenshot` returns a
  `data:image/png;base64,…` **String** (drops into `<img src>`); `stream_framebuffer`'s Channel
  is `String` (base64 frames); one extra command `get_logs(id)` primes the Running log pane.
- **First run is slow twice:** the first `tauri:dev`/`build` compiles the Rust backend, and the
  first SwRender boot renders on the guest CPU (a few minutes to the lock screen).

## Top follow-ups
1. Wire **snapshots** (clonefile the cf-dir + a restore ledger) — the Storage UI is already there.
2. Re-verify the **Metal GPU (gpu3d)** path end-to-end. (The build-feature check this used to also
   ask for is **moot**: GPU and networking are no longer optional features, so no build can omit
   them. Studio still hard-fails on a *stale* binary with no NAT backend, and the Hardware tab
   probes the binary rather than hardcoding the NIC as real.)
3. Give the **Planned devices** real inject verbs as the emulator grows the HALs (GPS, battery,
   clipboard first — they need the least guest plumbing). The inject-control shapes already exist
   behind the Hardware tab's disclosure toggle.
4. **Wire `device_identity` into the launch** — the Settings tab is honest that it's inert today
   (`compose_launch` takes no identity args), but the profile field exists and the wizard implies
   it reaches the guest.
4. Remove the stale `/tmp/aemu-*.sock` on Stop/delete (the emulator already clears it on next
   bind, so this is cosmetic).

---

## Layout

```
aemulator-studio/
├── src/                      # React + Vite frontend (state-based router, no react-router)
│   ├── App.tsx               # screen switch: dashboard / storage / create / instance
│   ├── screens/              # Dashboard, Create (4-step wizard), Instance (5-tab hub), Storage
│   ├── components/           # Sidebar, EmulatorCard
│   ├── components/instance/  # {Display,Hardware,Settings,Storage,Logs}Tab
│   └── lib/                  # ipc.ts (typed command wrappers), types.ts, format.ts
├── src-tauri/                # Rust backend
│   └── src/
│       ├── manager.rs        # DeviceManager: lifecycle, supervisor + readiness threads
│       ├── spawn.rs          # launch-plan compose, codesign, Vulkan ICD staging, spawn
│       ├── control.rs        # unix-socket control client (info/screenshot/tap/… wire protocol)
│       ├── framebuffer.rs    # screenshot poll → base64 PNG frames over a Channel
│       ├── storage.rs        # cf-dir clonefile provision, COW accounting, InstanceLayout
│       ├── host.rs           # live host capability + base-image probes
│       ├── paths.rs          # resolve the aemulator-hvf crate/binary/entitlements/testdata
│       ├── profiles.rs       # atomic JSON persistence
│       ├── ports.rs          # adb host-forward port pool
│       ├── devices.rs        # Emulated/Partial/Planned inventory per profile
│       └── commands.rs       # #[tauri::command] IPC surface
└── screenshots/              # captured verification run
```
