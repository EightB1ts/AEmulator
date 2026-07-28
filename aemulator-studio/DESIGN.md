# AEmulator Studio — Design & Build Plan

A Mac-native (Apple Silicon) GUI app to set up, run, drive, and manage **multiple**
`aemulator-hvf` Android emulators. Stack (final): **Tauri v2** — Rust backend
(`src-tauri`) + web frontend (React + Vite), one native WKWebView window.

Lives at: `AEmulator/aemulator-studio/` (new, standalone). Does **not** disturb the
existing crates or the hermetic default build of `aemulator-hvf`.

Ground truth this design is built on (verified, not assumed):

- The emulator is a **child process**, not a library. There is no in-process VM API.
  The Studio spawns `aemulator-hvf` (or a `run-*.sh` script) and drives it via CLI
  flags + env + a **Unix-socket control plane** speaking a **one-line text protocol**.
- The control wire protocol (`src/control/mod.rs`): request = one line `<verb> [args]`,
  response = one line `OK …` or `ERR …`. Verbs: `screenshot|shot [path]`, `tap X Y`,
  `swipe X1 Y1 X2 Y2 [ms]`, `text <s>`, `key <code|name>`, `keydown <code|name>`,
  `keyup <code|name>`, `launch <pkg>`, `sh <cmd>`, `wait_stable [timeout_ms] [interval_ms]`,
  `hash`, `info`, `help`, `quit`.
- `key` is press+release in one verb; `keydown`/`keyup` are the separate edges — the only way
  to express a HELD modifier, so every chord (and live keyboard forwarding) needs them.
- **`screenshot` writes a PNG to a FILE PATH** and returns metadata
  (`OK <path> WxH gen=N nonzero=M bytes=B`) — it does **not** stream bytes over the
  socket. The live framebuffer is therefore obtained by the backend polling
  `screenshot <tmpfile>` and reading the file; `hash` / `info` return a frame
  `gen=` counter + hash the backend uses to skip unchanged frames.
- Binary must be **ad-hoc codesigned** with `hv.entitlements`
  (`com.apple.security.hypervisor`) or `hv_vm_create` fails. Studio ships/points at
  ONE binary built `--features net,gfxstream`; render mode is chosen per-run at spawn
  (`--gpu3d` = Metal, `SWRENDER=1` = guest-CPU, headless = neither).
- Per-VM uniqueness (none auto-allocated today — **Studio owns allocation**): a private
  `--cf-dir` (the binary mutates `img/` in place and prep scripts rewrite `super.raw.img`
  in place, so **two VMs must never share a cf-dir**), a unique `--socket`, a unique adb
  host port via `AEMU_HOSTFWD="tcp:<port>:5555"`, and `--mem-mb` / `--cpus` budgeted
  against host RAM (4096 MiB is the reliable Cuttlefish floor).

---

## 1. APP ARCHITECTURE

### 1.1 Project layout

```
aemulator-studio/
├── src-tauri/
│   ├── Cargo.toml            # [lib] crate-type = ["staticlib","cdylib","rlib"]
│   ├── tauri.conf.json       # devUrl http://localhost:1420, frontendDist ../dist
│   ├── capabilities/
│   │   └── default.json      # plugin ACLs only; app commands ride core:default
│   ├── hv.entitlements       # copied from crates/aemulator-hvf (build+sign parity)
│   ├── build.rs
│   └── src/
│       ├── main.rs           # 6-line shim → aemulator_studio_lib::run()
│       ├── lib.rs            # tauri::Builder, .manage(DeviceManager), .setup()
│       ├── commands.rs       # thin #[tauri::command] wrappers → delegate to modules
│       ├── manager.rs        # DeviceManager: the registry of all VmHandles (Arc<Mutex<…>>)
│       ├── vm.rs             # VmHandle: one child process + its supervisor state
│       ├── spawn.rs          # build+sign the binary, compose argv+env, spawn child
│       ├── control.rs        # Unix-socket client: send one verb, parse OK/ERR reply
│       ├── framebuffer.rs    # screenshot-poll loop → tauri::ipc::Channel<Vec<u8>>
│       ├── storage.rs        # cf-dir provisioning (APFS clonefile), disk usage, snapshots
│       ├── profiles.rs       # EmulatorProfile persistence (JSON under app config dir)
│       ├── ports.rs          # host adb-port + socket-path allocation
│       └── devices.rs        # DeviceConfig assembly (emulated + Planned placeholders)
├── src/                      # React + Vite frontend
│   ├── main.tsx
│   ├── App.tsx               # router: Dashboard / Create / Instance(hub) / Storage
│   ├── lib/ipc.ts            # ONE typed wrapper over every invoke() + Channel/event sub
│   ├── lib/types.ts          # TS mirror of the contract types in §2 (hand-kept in sync)
│   ├── screens/{Dashboard,Create,Instance,Storage}.tsx
│   ├── components/instance/{Display,Hardware,Settings,Storage,Logs}Tab.tsx
│   └── components/{Sidebar,EmulatorCard}.tsx
├── vite.config.ts            # clearScreen:false, strictPort, server.port 1420, target safari15
├── package.json
└── DESIGN.md                 # this file
```

Mirrors the proven `app_frontend/desktop` template: `main.rs` is a shim into a lib
crate; logic is modularized; `commands.rs` wrappers only resolve managed state and
delegate; errors flatten to `String` at the IPC boundary (`.map_err(|e| e.to_string())`);
app-defined commands need **no** per-command ACL (they ride `core:default`), so
`capabilities/default.json` lists only plugin permissions.

### 1.1b Session lifecycle: one VMM per instance, ever (data-integrity invariant)

**Two VMMs on one cf-dir corrupt the guest filesystem.** The emulator mutates `img/` in place —
`userdata.img`, `misc.img`, `metadata.img`, `frp.img` are attached writable — so two VMMs are two
kernels writing one ext4 device with no coordination. It shows up minutes into boot as
`EXT4-fs error (device vda9): … iget: checksum invalid` → `Kernel panic`, and once it happens the
instance is unbootable until its writable partitions are replaced.

This actually happened, repeatedly, and the mechanism is worth remembering because nothing in the
UI hinted at it: the Studio's double-start guard lived only in the in-memory `running` map, and
under `pnpm tauri:dev` **every Rust edit relaunches the Studio** with an empty map while the VMM it
spawned keeps running, orphaned. Pressing Start again spawned another. One session reached three
live VMMs on one instance, all bound to the same control-socket path (last binder wins) — so the
frame *generation* the live view polled came from one VM while the *pixels* came from another, and
the display looked broken in a way that pointed nowhere near the cause.

Three layers now, deliberately independent:

| Layer | Where | Role |
| --- | --- | --- |
| `flock` on `<cf_dir>/.aemu-instance.lock` | `aemulator_hvf::cf_lock`, held for process lifetime | **Enforcement.** Authoritative because it lives in the process that does the writing, and the kernel releases it on exit — including SIGKILL. No launcher can defeat it, including one that forgot the VM exists. A second VMM refuses to boot and names the holder's pid. |
| `<instance_dir>/session.json` | `session.rs` | **Bookkeeping.** `flock` can say "someone holds it" but not which emulator id / socket / adb port, which is what RESUMING needs. Treated as a cache that may be stale; liveness is always re-derived from the lock. |
| `VIRTIO_BLK_F_FLUSH` + `fdatasync` | `virtio::block_composite` | **Crash consistency.** See below. |

Consequences for the manager:

- **`reconcile_sessions`** runs once at startup: adopt VMs that outlived a previous Studio process,
  garbage-collect records whose VMM is gone. A resumed VM is `Ready` with working input, screenshots,
  live view and Stop; it has no supervisor thread (it is not our child, so `wait()` is unavailable and
  its pipes died with its launcher) — a pid poller (`spawn_exit_watcher`) substitutes.
- **`start_emulator`** consults `session::classify` BEFORE spawning: `Live` + record → adopt; `Live`
  without a record → refuse with the pid to kill; `Stale` → clean up and launch fresh.
- **`stop_emulator`** escalates `quit` → SIGTERM → SIGKILL, **waiting for actual process exit** at
  each stage instead of sleeping a fixed interval, and leaves the handle in `Stopping` until the
  process is really gone. It used to report `Stopped` immediately, which let Stop-then-Start walk
  straight past the double-start guard.
- **`shutdown_all`** runs on `RunEvent::Exit`. An orphaned VMM keeps the cf-dir lock, so the next
  Studio launch cannot start that emulator at all.

### 1.1c Guest-disk durability (why writes are slower than they were)

`VIRTIO_BLK_F_FLUSH` is advertised and `VIRTIO_BLK_T_FLUSH` performs a real `fdatasync`. Both halves
were missing, and each alone was enough to lose data:

- The feature bit was never advertised, so per the virtio spec the guest assumed **no volatile write
  cache** — and Linux stops issuing barriers entirely when told that, because there is nothing to
  barrier against.
- The FLUSH handler called `File::flush()`, which for `std::fs::File` is a **no-op** — it flushes
  Rust-side buffering (of which `File` has none) and never reaches the kernel.

Net effect: ext4's journal ordering guarantee was void, and every write sat in the host page cache.
A host-side kill could leave a journal commit whose data blocks never landed — unrecoverable, and
indistinguishable from the multi-VMM corruption above. Now a `SIGKILL` degrades to a journal replay.
`CompositeBlock`'s `Drop` also syncs, so every normal exit path leaves a clean disk.

### 1.2 The Rust VM-manager module (the backbone)

**`DeviceManager`** — one process-wide struct, built once and `.manage()`d, wrapped
`Arc<Mutex<…>>` so background threads hold clones. Owns:

- `profiles: HashMap<EmulatorId, EmulatorProfile>` — persisted definitions (config dir).
- `running: HashMap<EmulatorId, VmHandle>` — live instances only.
- `port_pool` / `socket_pool` — allocators (adb host ports from 5556↑ skipping in-use;
  sockets at `<instanceDir>/control.sock`).
- `ram_budget` — Σ(mem_mb) of running VMs vs a host-RAM cap; `start_emulator` refuses
  (returns a typed error) when a launch would exceed it.

**`VmHandle`** — one running emulator:

- the spawned `std::process::Child` (or a `CommandChild` if the shell plugin is used),
- resolved `VmPorts { adb_host, control_socket }`,
- a `VmState` (see §2) transitioned by a supervisor thread,
- a ring buffer of recent stdout/stderr log lines (emitted to the UI as events),
- a stop token for the framebuffer poll loop.

**Supervision.** On `start_emulator` the backend:

1. Provisions the instance cf-dir (APFS `clonefile` of the profile's base image — §4
   Storage) if not already provisioned.
2. Allocates a socket path + adb host port.
3. Composes argv + env (`spawn.rs`) and spawns the child (build+sign first if the
   binary is stale/unsigned).
4. Starts a **stdout/stderr reader thread** that parses boot milestones
   (`[FAIL]`, boot-completed markers) → transitions `VmState` and emits `vm-log` +
   `vm-state` events (`app.emit`).
5. Marks the VM **Ready** once the control socket accepts a connection AND a
   `wait_stable` / non-zero `info` frame returns.

Lifecycle transitions and the state-machine vocabulary come from
**`aemulator-supervisor`** (reused — see §1.3). Process death (child exit) → supervisor
flips to `Stopped`/`Crashed` and emits the terminal `vm-state`.

**Control-plane client (`control.rs`).** The backend speaks the socket protocol
**directly** (opens the `UnixStream`, writes `"<verb> args\n"`, reads one line) rather
than shelling out to the one-shot client verbs — cleaner, no extra process per tap.
Every control command is an `async` Tauri command so socket round-trips never block the
UI thread.

**Framebuffer (`framebuffer.rs`).** Because `screenshot` writes a file, the live view is
a poll loop: every ~150–250 ms issue `screenshot <instanceDir>/live.png`, and — to avoid
re-encoding/re-sending unchanged frames — first check `info`'s `gen=`/`hash=`; only when
the gen advanced, read `live.png` bytes and `chan.send(bytes)` over a
**`tauri::ipc::Channel<Vec<u8>>`** (Tauri's documented path for large/continuous binary
data — no JSON re-serialization). The frontend `chan.onmessage` draws the PNG to a
`<canvas>`/`<img>`. (Post-MVP: swap file-poll for a shared-memory scanout if the emulator
grows a bytes-over-socket screenshot verb.)

### 1.3 Reuse of existing workspace crates

The Studio's `src-tauri` **is** in the main Cargo workspace (unlike `aemulator-hvf`,
which is workspace-excluded), so it can depend on:

| Crate | Used for |
|---|---|
| `aemulator-supervisor` | Device-lifecycle **state machine** — the canonical `VmState` transitions (provisioning → booting → ready → stopping → stopped/crashed). Drive our `VmHandle` state off it instead of re-inventing. |
| `aemulator-control` | The LLM control-plane **contract types** (`UiTree` / `Observation` / `Action`) + `MockController`. Lets the Running-view control bar and any future agent panel share one action vocabulary; `MockController` backs UI dev before a VM is up. |
| `aemulator-identity` | `DeviceProfile` / `SensorSpec` / `TelephonyProfile` — the source of the **identity fields** in a profile (SoC/MIDR spoof, advertised sensors, telephony metadata). The Create wizard edits these; `aemulator-vmm` turns them into launcher args/cmdline. |
| `aemulator-vmm` | **Identity injection + launcher-arg generation** — translate a `DeviceProfile` into the `--extra-cmdline` / bootconfig knobs the binary consumes, so we don't hand-format cmdline strings. |
| `aemulator-detection` | (optional) host capability probing — is `libslirp` present, is the binary signed, is there enough RAM. |

`aemulator-hvf` itself is **not** a dependency — it stays a spawned binary. The Studio
either (a) points at the pre-built `crates/aemulator-hvf/target/debug/aemulator-hvf` and
verifies/refreshes its signature, or (b) shells the `run-*.sh` scripts for the full
prep+build+sign+run path. MVP uses (a) for start/stop speed and (b)'s prep steps folded
into `storage.rs` provisioning.

---

## 2. THE INTERFACE CONTRACT

**This is the seam the backend and frontend both build to, verbatim.** Rust types are
`serde`-serialized; the TS mirror lives in `src/lib/types.ts`. All commands are
`async` and return `Result<T, String>` (errors flatten to a string at the IPC boundary);
JS calls pass **camelCase** keys, Rust receives snake_case (Tauri maps automatically).

### 2.1 Shared data types

```rust
type EmulatorId = String;   // uuid v4, assigned at create

// ---- Profile: a saved emulator definition (persisted) ----
struct EmulatorProfile {
    id: EmulatorId,
    name: String,                     // "Pixel-ish 8GB", user-facing
    base_image: String,               // key into the image catalog (e.g. "cf64")
    mem_mb: u32,                       // default 4096
    cpus: u32,                        // default 2
    render_mode: RenderMode,          // how to launch: Gpu3d | SwRender | Headless
    selinux: SelinuxMode,             // Permissive | Enforcing
    extra_cmdline: String,            // appended verbatim (advanced)
    device_identity: DeviceIdentity,  // SoC spoof + advertised sensors/telephony
    created_at: i64,                  // epoch ms
}

enum RenderMode { Gpu3d, SwRender, Headless }   // → --gpu3d / SWRENDER=1 / neither
enum SelinuxMode { Permissive, Enforcing }       // → --selinux …

struct DeviceIdentity {               // sourced from aemulator-identity::DeviceProfile
    soc: String,                      // "Snapdragon 8 Gen 2 / SM8550"
    manufacturer: String,             // "Xiaomi"
    model: String,                    // "fuxi"
    // advertised-only sensors/telephony (metadata; no live data path — see DeviceConfig)
    advertised_sensors: Vec<String>,  // ["accelerometer","magnetometer"]
    telephony: Option<TelephonyMeta>, // IMEI/operator strings if a radio is claimed
}
struct TelephonyMeta { imei: String, operator: String, mcc_mnc: String }

// ---- Instance: a running (or just-stopped) VM ----
struct VmInstance {
    id: EmulatorId,
    state: VmState,
    ports: VmPorts,
    display: DisplayInfo,             // last-known framebuffer geometry
    started_at: Option<i64>,
    pid: Option<u32>,
}
enum VmState { Provisioning, Booting, Ready, Stopping, Stopped, Crashed }
struct VmPorts { adb_host: u16, control_socket: String }  // 127.0.0.1:adb_host ; socket path
struct DisplayInfo { width: u32, height: u32, gen: u64, nonzero: u64 } // from `info`

// ---- Framebuffer frame (over a Channel, not a return value) ----
// Channel<Vec<u8>> carries raw PNG bytes; geometry travels via get_status()/DisplayInfo.

// ---- Device inventory (real + placeholder) ----
struct DeviceConfig {
    emulator_id: EmulatorId,
    devices: Vec<DeviceEntry>,
}
struct DeviceEntry {
    kind: DeviceKind,
    label: String,                    // "GPU (Metal 3D)", "GPS / Location"
    status: DeviceStatus,             // Emulated | Partial | Planned
    detail: String,                   // one-line honest description
    controllable: bool,               // does the UI expose a live control for it today?
}
enum DeviceStatus { Emulated, Partial, Planned }
enum DeviceKind {
    Cpu, Gpu2d, Gpu3d, BlockPrimary, BlockExtra, Input, Console, Net, Rng,
    Keymint, VerifiedBoot,            // ^ Emulated / Partial today
    Vsock, Audio, Gps, Sensors, Camera, Battery, Telephony,
    Clipboard, Storage, Bluetooth, Nfc, Vibration,  // ^ Planned / metadata-only
}

// ---- Storage ----
struct StorageVolume {
    emulator_id: EmulatorId,
    instance_dir: String,             // the private cf-dir
    logical_bytes: u64,               // sum of image sizes
    on_disk_bytes: u64,               // actual APFS blocks (clonefile COW → « logical)
    writable_partitions: Vec<PartitionUsage>, // userdata/misc/metadata/frp
    snapshots: Vec<Snapshot>,
}
struct PartitionUsage { name: String, size_bytes: u64 }   // "userdata" 6144 MiB, …
struct Snapshot { id: String, label: String, created_at: i64, on_disk_bytes: u64 }

// ---- Create input ----
struct CreateEmulatorInput {
    name: String,
    base_image: String,
    mem_mb: u32,
    cpus: u32,
    render_mode: RenderMode,
    selinux: SelinuxMode,
    device_identity: DeviceIdentity,
    extra_cmdline: String,
}
```

### 2.2 Tauri commands

| Command | Args | Returns | Notes |
|---|---|---|---|
| `list_emulators` | — | `Vec<EmulatorProfile>` | All saved profiles (running or not). |
| `list_instances` | — | `Vec<VmInstance>` | Only currently running/recently-stopped VMs. |
| `create_emulator` | `input: CreateEmulatorInput` | `EmulatorProfile` | Persists a profile + provisions its private cf-dir (clonefile). No boot. |
| `update_emulator` | `id: EmulatorId, patch: UpdateEmulatorInput` | `EmulatorProfile` | Partial profile edit (absent field = unchanged). `name`/`device_identity` apply any time; launch-affecting fields (`mem_mb`/`cpus`/`render_mode`/`selinux`/`extra_cmdline`/`base_image`) are refused while the VM is active — see §3.3 Settings. A `base_image` change needs `reprovision: true`, which ERASES the instance disk. |
| `delete_emulator` | `id: EmulatorId` | `()` | Refuses if running; removes profile + instance dir. |
| `start_emulator` | `id: EmulatorId` | `VmInstance` | Allocates socket+adb port, spawns child, returns once `Booting`. Refuses if RAM budget exceeded. |
| `stop_emulator` | `id: EmulatorId` | `()` | Sends `quit` over the socket, then SIGTERM→SIGKILL fallback; frees port/socket. |
| `get_status` | `id: EmulatorId` | `VmInstance` | Current state + ports + `DisplayInfo` (issues `info`). |
| `stream_framebuffer` | `id: EmulatorId, chan: Channel<Vec<u8>>` | `()` | Starts the screenshot-poll loop; PNG frames pushed over `chan` until `stop_framebuffer`. |
| `stop_framebuffer` | `id: EmulatorId` | `()` | Stops that VM's poll loop. |
| `screenshot` | `id: EmulatorId` | `Vec<u8>` (PNG bytes) | One-shot: `screenshot <tmp>` then read+return the file bytes. |
| `send_tap` | `id, x: u32, y: u32` | `()` | `tap X Y`. |
| `send_swipe` | `id, x1,y1,x2,y2: u32, ms: Option<u32>` | `()` | `swipe X1 Y1 X2 Y2 [ms]`. |
| `send_key` | `id, code: String` | `()` | `key <code\|name>` (BACK/HOME/POWER/… or numeric). |
| `send_key_down` | `id, code: String` | `()` | `keydown <code\|name>` — press and hold; caller owns the release. Live keyboard capture + modifier chords. |
| `send_key_up` | `id, code: String` | `()` | `keyup <code\|name>`. |
| `send_text` | `id, text: String` | `()` | `text <s>`. |
| `launch_package` | `id, package: String` | `()` | `launch <pkg>` (am/monkey over console; best-effort). |
| `wait_stable` | `id, timeout_ms: Option<u32>` | `u64` (frame gen) | `wait_stable`; returns settled gen. |
| `device_config` | `id: EmulatorId` | `DeviceConfig` | The full emulated + Planned inventory for this profile (§2.3). |
| `list_storage` | — | `Vec<StorageVolume>` | Per-emulator disk usage + snapshots. |
| `create_snapshot` | `id, label: String` | `Snapshot` | Clonefile the instance's writable partitions (VM should be stopped). |
| `restore_snapshot` | `id, snapshot_id: String` | `()` | Swap writable partitions back from a snapshot clone. |
| `delete_snapshot` | `id, snapshot_id: String` | `()` | — |
| `list_base_images` | — | `Vec<ImageCatalogEntry>` | Discover installed base images (e.g. `cf64`) + their sizes. |
| `host_capabilities` | — | `HostCaps` | libslirp present, binary signed, total/free RAM, max concurrent VMs. |

`ImageCatalogEntry { key, path, logical_bytes, present: bool }`.
`HostCaps { total_ram_mb, free_ram_mb, libslirp: bool, binary_signed: bool, max_vms: u32 }`.

### 2.3 Events (backend → frontend, via `app.emit`)

| Event | Payload | Meaning |
|---|---|---|
| `vm-state` | `{ id, state: VmState }` | Lifecycle transition. |
| `vm-log` | `{ id, line, stream: "stdout"\|"stderr", ts }` | Boot/console log line for the log pane. |
| `vm-error` | `{ id, message }` | Spawn/boot failure. |

Small JSON status → **events**. Large/continuous framebuffer bytes → **Channel** (never
an event). This split is deliberate (Tauri guidance + reference-app convention).

### 2.4 DeviceConfig contents (the honest inventory)

`device_config(id)` returns one `DeviceEntry` per line below. Each entry carries two
orthogonal axes: `status` (**Emulated** / **Partial** / **Planned** — the honesty tier) and
`group` (`Compute` / `Graphics` / `Storage` / `Io` / `Sensors` / `Security` — the
subsystem). The Hardware tab groups on `group` and renders `status` as a dot (§3.3).
Nothing here fabricates a capability.

**Emulated:**
- `Cpu` — "CPU identity spoof (MIDR)" — big.LITTLE MIDR writes; blocks Apple `0x61` leak. controllable=false.
- `Gpu2d` — "GPU 2D scanout (virtio-gpu)" — always on, EDID 1024×768. controllable=false.
- `Gpu3d` — "GPU 3D (Metal via gfxstream)" — needs `gfxstream` build + `--gpu3d`/`--display`. controllable=false (toggled at profile level).
- `BlockPrimary` — "Storage: system (GPT composite → /dev/vda)". controllable=false.
- `Input` — "Touch + keyboard (virtio-input)" — the tap/swipe/text/key surface. controllable=true.
- `Console` — "Serial console (12 HVC channels)" — backs `launch`/`sh`. controllable=true.
- `Net` — "Network NAT (libslirp)" — status is **probed from the emulator binary**
  (`spawn::binary_has_net`), not assumed: `Emulated` (real NAT + adb-over-TCP) when libslirp is
  linked, `Partial` (enumerate-only stub — links up, carries nothing) when it is not. Hardcoding
  this to `Emulated` is what let a `--features gfxstream`-only binary claim NAT it did not have.
  controllable=false.
- `Rng` — "Entropy (virtio-rng)". controllable=false.

**Partial:**
- `BlockExtra` — "Extra disks (fixed files)" — `--disk` only; **no removable/SD model**. Partial.
- `Keymint` — "KeyMint (software stub, NOT a TEE)" — CBOR responder brings keystore2 up; Gatekeeper/oemlock logged-only. Partial.
- `VerifiedBoot` — "Verified boot state (metadata only)" — static `verifiedbootstate=green`. Partial.
- `Telephony` — "Telephony (metadata only)" — advertises IMEI/operator strings; **no live radio / no call/SMS path**. Partial (only if profile declares a radio; else omit).
- `Sensors` — "Sensors (declared, no live data)" — profile advertises accel/magnetometer to `getSensorList()`; **no HAL delivers values**. Partial.

**Planned (missing — placeholder UI only):**
- `Vsock` — "HAL transport (vsock) — not implemented" — blocks real camera/other HALs. Planned.
- `Audio` — "Audio playback/capture — planned" (virtio-snd → CoreAudio). Planned.
- `Gps` — "GPS / location — planned" (README's `gps` verb does **not** exist). Planned; UI shows a lat/long form marked planned.
- `Camera` — "Camera — planned" (needs vsock + camera HAL). Planned.
- `Battery` — "Battery / power — planned". Planned.
- `Clipboard` — "Host↔guest clipboard — planned". Planned.
- `Storage` — "Removable storage / SD card — planned". Planned.
- `Bluetooth` — "Bluetooth — planned". Planned.
- `Nfc` — "NFC — planned". Planned.
- `Vibration` — "Vibration/haptics — planned". Planned.

---

## 3. SCREENS

Mac-native styling: WKWebView `target: safari15`; system font stack
(`-apple-system, SF Pro`), `prefers-color-scheme` dark/light, translucent
`vibrancy`-style sidebar, hairline dividers, SF-Symbols-like iconography, generous
padding, rounded 10px cards, traffic-light-safe titlebar inset.

**Navigation shape.** Exactly TWO global surfaces in the sidebar — the emulator grid
(§3.1) and the image catalog (§3.5). Everything scoped to one emulator lives in the
per-instance hub (§3.3), reached by opening a card. Device configuration was briefly a
third global nav item, which inverted the navigation: it was a page whose first act was
asking "which emulator?" via a dropdown, when device config is a property OF an
emulator. It is now the hub's Hardware tab.

### 3.1 Dashboard / emulator list
Grid of **`EmulatorCard`**s — one per profile: name, base image, mem/cpu, a state pill
(colored by `VmState`), a live thumbnail (last framebuffer frame when Ready), and a
quick **Start/Stop** button. Header "+ New Emulator". Card click → the instance hub
(§3.3), whichever state the VM is in. A host-capacity strip shows Σ RAM in use vs
`max_vms`.

### 3.2 Create-emulator wizard
Multi-step sheet feeding `CreateEmulatorInput`:
1. **Basics** — name, base image (`list_base_images`), mem_mb slider (floor 4096), cpus.
2. **Render & security** — `RenderMode` (Metal 3D / SW render / Headless) with a plain
   explanation of each, `SelinuxMode`.
3. **Device identity** — SoC/manufacturer/model (from `aemulator-identity` presets),
   advertised sensors, optional telephony metadata.
4. **Devices** — a read-only preview of the resulting `DeviceConfig` (Emulated toggles
   that are on, Planned ones greyed) so the user sees exactly what they'll get.
5. **Advanced** — raw `extra_cmdline`.
Finish → `create_emulator` (persist + clonefile-provision) → land on the new card.

### 3.3 Instance hub (the per-emulator surface)
One page per emulator. A shared header — back-to-grid, name, state pill, adb-connect hint
(`adb connect 127.0.0.1:<adb_host>`), Start/Stop — over five tabs. The hub owns the state
that must survive a tab switch (profile, polled `get_status`, the `vm-log` ring); the
framebuffer poll deliberately does not, so it starts and stops with the Display tab
rather than screenshot-polling a VM the user isn't watching.

**Display** — the live surface. The framebuffer `<img>` fed by `stream_framebuffer`'s
Channel; click/drag maps view→guest coords and calls `send_tap`/`send_swipe`. The controls
are a full-width **menu-bar strip** (`.display-bar`) directly under the tabbar — hardware
keys (Back/Home/Menu/Power/Vol±/Enter → `send_key`), text injection (`send_text`), package
launcher (`launch_package`), one-shot screenshot, and `wait_stable` — so the guest picture
gets the entire window width. (It was a 330px right rail through the
first cut of the hub; the rail cost the framebuffer a third of the window at every size, and
every control in it fits in one row.) A stopped VM gets an empty state with a Start
affordance instead of a spinner claiming it's booting.

**Input capture — no toggle.** Clicking the screen hands the mouse AND the keyboard to the
guest, the same gesture as clicking into any other app; that click still taps, so arming is
free. The blue pill on the frame (`.fb-capture-chip`) is the armed indicator and the only
place the exit (**Cmd+Esc**) is stated. Capture also releases on window blur, on clicking a
field in the control bar (the key handlers already route by focus — the release is so the
pill can't claim keys that are going to a text box), when the VM stops, and on unmount.

Mechanically: window-level `keydown`/`keyup` in the capture phase → `KeyboardEvent.code` →
evdev code (`src/lib/keymap.ts`) → the `keydown`/`keyup` control verbs. Design points that
are not obvious:

- **`code`, not `key`.** Both `KeyboardEvent.code` and evdev are positional, so the map is
  1:1 and layout-independent — the guest applies its own keymap, as real hardware does.
  Mapping `event.key` would double-apply the host layout.
- **Separate down/up edges.** `send_key` is an atomic press+release and can therefore never
  express a HELD modifier, which is what a chord is. Cmd/Ctrl/Option/Shift work only because
  the control plane grew `keydown`/`keyup`; an old `aemulator-hvf` build replies
  `ERR unknown verb`, which the tab detects once and degrades to press+release (typing keeps
  working, chords don't).
- **The release ledger.** Every forwarded press is recorded in a `held` set and released on
  disarm / window blur / VM stop / unmount — a dropped `keyup` is a modifier stuck down
  inside Android. Releasing Command releases the whole set, because macOS suppresses the
  keyup of a letter released while Command is down.
- **Sends are serialized** through a promise chain: each verb is its own socket round-trip,
  so concurrent sends can deliver a modifier's press *after* the key it modifies.
- **What macOS keeps.** Cmd+Tab / Cmd+Space / Cmd+` never reach a WebView, and Tauri's
  default app menu claims Cmd+Q/W/M/H plus the Edit chords (Cmd+A/C/V/X/Z) as key
  equivalents. Those are passed through to the host rather than half-swallowed
  (`isHostReservedChord`) — they cannot be delivered to the guest from a normal window, and
  pretending otherwise would just break quit/copy/paste in the Studio itself. Cmd+Esc is the
  release valve; the armed state is a chip on the frame so capture is never invisible.

**Hardware** — `device_config(id)`, grouped by `DeviceEntry.group` (the SUBSYSTEM axis:
compute / graphics / storage / I/O / sensors / security) with `status` rendered as a
per-item dot: filled green = **Emulated** (real data path), hollow amber = **Partial**
(metadata/stub), hollow grey = **Planned**. Two axes instead of one keeps a subsystem's
real and aspirational halves adjacent. Planned items collapse behind a single disclosure
— they are the majority of the inventory and none of them are usable, so they must not
dominate the page; their inert inject-control shapes (GPS lat/long, sensor sliders,
battery level — Phase 4, §4) sit behind one further toggle. `controllable` items link to
the Display tab rather than merely advertising that controls exist elsewhere.

**Settings** — `update_emulator(id, patch)`; a partial patch, absent field = leave as-is.
Gating derives from what `spawn::compose_launch` actually consumes, and the backend is the
boundary (the tab mirrors it):
- *Any time*: `name`, `device_identity` — neither reaches the launch. Identity is
  advertised metadata only today; the tab says so rather than implying a restart applies
  it.
- *VM stopped*: `mem_mb`, `cpus`, `render_mode`, `selinux`, `extra_cmdline` — composed
  into argv/env at start. Rejected while active rather than deferred: a saved value that
  doesn't match the running VM is a lie the UI can't walk back.
- *VM stopped + confirm*: `base_image`. `storage::provision_cf_dir` early-returns on an
  existing cf-dir to preserve a VM's mutated state, so a changed base without erasing the
  clone would silently keep booting the OLD image. `reprovision` erases the disk.
Plus the danger zone (`delete_emulator`, itself refused while running).

**Storage** — this instance's disk: **logical vs on-disk** bytes (COW savings explicit), a
writable-partition breakdown (userdata 6144 MiB, misc, metadata, frp), and a **Snapshots**
list with create/restore/delete (`create_snapshot`/`restore_snapshot`/`delete_snapshot`;
restore/create require the VM stopped).

**Logs** — the streamed `vm-log` ring at full window height with a substring filter and a
stderr-only toggle, plus tail-follow that releases when the user scrolls up. Boot output is
the one thing you read when a VM won't come up; it does not belong in a 200px rail.

### 3.5 Images & storage (global)
What is genuinely cross-instance: the base-image catalog (`list_base_images`) and a
per-emulator roll-up from `list_storage()` (on-disk, COW saved, snapshot count) whose rows
link into that emulator's Storage tab. A host-level free-space bar up top.

---

## 4. PHASED BUILD PLAN

### MVP (this workflow) — "one real emulator, live, controllable"
Definition of done — the app must:
1. **Compile** (`cargo build` in `src-tauri`, `vite build`) and **launch a native
   window** (`tauri dev`).
2. **List + create + start + stop** at least ONE emulator against the **real binary**:
   - `spawn.rs` verifies/refreshes the ad-hoc `hv.entitlements` signature, composes
     `run-android --control --cf-dir <instanceDir> --socket <sock> --mem-mb --cpus`
     (+ `--gpu3d` or `SWRENDER=1` env per `RenderMode`, `AEMU_HOSTFWD="tcp:<port>:5555"`),
     and spawns/supervises the child.
   - `storage.rs` provisions the instance cf-dir via APFS `clonefile` of the base `cf64`
     image (fast COW; MVP may clonefile the whole cf-dir).
   - `stop_emulator` sends `quit` then SIGTERM/SIGKILL.
3. **Show the live framebuffer** via `stream_framebuffer` (screenshot-poll + gen-gated
   Channel) and support **basic control**: `send_tap`, `send_key`, `send_text`
   (swipe/launch wired if cheap).
4. **Render the device-config placeholders** — `device_config` returns the full §2.4
   inventory and the panel shows Emulated/Partial/Planned distinctly.

MVP scope cuts (stub honestly, mark in UI): single base image (`cf64`); snapshots list
read-only (create/restore deferred); RAM-budget check present but conservative;
one running VM exercised even though the manager is multi-instance by construction.

### Phase 2 — multi-instance & storage depth
Concurrent VMs with real port/socket/RAM allocation + the capacity strip; full snapshot
create/restore/delete (clonefile the 4 writable partitions only, share read-only
partitions); per-partition usage; base-image catalog + import.

### Phase 3 — richer control & agent seam
Swipe-drag polish, `sh`/`launch` UX, adb-connect helper, and wire `aemulator-control`'s
`UiTree`/`Observation`/`Action` so an agent/LLM panel can drive the same VM the human
does (MockController-backed in dev).

### Phase 4 — placeholders become real (tracks emulator work)
As the VMM grows backends, flip Planned→Emulated: vsock transport first (unblocks
camera), then audio (virtio-snd→CoreAudio), then a GPS/sensor injection verb (new control
verbs `gps <lat> <lng>` / `sensor …`) surfaced by the already-built placeholder forms.

### Phase 5 — distribution
Adopt the reference app's `tauri-plugin-updater` + `tauri-plugin-process` relaunch with
an R2-hosted `latest.json` + minisign; ad-hoc→developer-ID signing + notarization for the
Studio bundle (the emulator binary keeps its own hv-entitlement ad-hoc signing).

---

## Honesty ledger (what is stubbed / not real)
- **Live framebuffer = file-poll**, not a streaming scanout — bounded by the emulator's
  file-based `screenshot` verb. Adequate at ~4–6 fps; not a smooth video.
- **`launch`/`sh`** are best-effort over the console (`am`/`monkey`), not adb.
- **Sensors/Telephony/Verified-boot** are **metadata claims only** — no live data path
  exists; the UI marks them Partial and any injection control is Planned/inert.
- **GPS verb does not exist** despite the README mention — Planned only.
- **Snapshots** in MVP are read-only placeholders; create/restore land Phase 2.
- **No daemon / no JSON API** — everything is child-process + one-line socket text.
