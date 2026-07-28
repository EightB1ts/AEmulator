# SCAFFOLD.md — AEmulator workspace contract

This file is the **contract between the scaffold and the parallel implementers**. The workspace is
a set of compiling stubs with **frozen public APIs**. Fill the bodies; do not change the
signatures.

## Rules for implementers (read first)

1. **Fill bodies, do not change signatures.** Every `pub` struct field, enum variant, function
   signature, and trait method below is frozen. Other crates and their tests compile against these
   exact shapes. If you believe a signature is wrong, raise it — do not silently change it.
2. **Only touch your own crate directory.** The scaffold owns the shared manifests (root
   `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore`) and this file. Do not edit
   another crate's `src/` or `Cargo.toml`.
3. **`cargo build` must stay green on macOS/Apple Silicon with default features.** Anything needing
   Linux/KVM (crosvm) goes behind the non-default `kvm` cargo feature **and**
   `cfg(target_os = "linux")` (see `aemulator-vmm::crosvm_backend`).
4. **No `unwrap()`/`expect()` in library paths that can fail at runtime.** Return `Result` with a
   `thiserror` error type. `todo!()`/`unimplemented!()` are acceptable placeholders for unfilled
   bodies; tests must not depend on them.
5. **Use the verified research strings, not recall.** Detection strings, blocklist entries, and
   real-device values come from the research dossier referenced by the plan — copy them exactly.
6. **Write unit tests** for every body you fill. Standard rustfmt (4-space, Rust 2021).

## Dependency DAG (no cycles)

```
                      aemulator-detection ─────┐   (leaf: no aemulator deps)
                                               │
                      aemulator-identity ◀──────┘
                          ▲     ▲
              vm-fdt      │     │
                 └── aemulator-dtb
                          ▲
                          │
   aemulator-control      aemulator-vmm ── (opt) crosvm_backend  [cfg linux + feature kvm]
      (leaf)   ▲            ▲
               │            │
               └── aemulator-supervisor
                          
   attestation-relay  (leaf, optional Phase-5, no aemulator deps)

   aemulator-cli ── identity + detection + dtb + vmm
```

Edges (X → Y means "X depends on Y"):

| Crate                   | Depends on (intra-workspace)                          | Kind   |
|-------------------------|-------------------------------------------------------|--------|
| `aemulator-detection`   | *(none)*                                              | leaf   |
| `aemulator-control`     | *(none)*                                              | leaf   |
| `attestation-relay`     | *(none)*                                              | leaf   |
| `aemulator-identity`    | `aemulator-detection`                                 | lib    |
| `aemulator-dtb`         | `aemulator-identity` + `vm-fdt`                       | lib    |
| `aemulator-vmm`         | `aemulator-identity` + `aemulator-dtb`                | lib    |
| `aemulator-supervisor`  | `aemulator-vmm` + `aemulator-control` + `aemulator-identity` | lib |
| `aemulator-cli`         | `identity` + `detection` + `dtb` + `vmm`              | bin    |

## Shared dependencies

`[workspace.dependencies]` (root `Cargo.toml`): `serde` (derive), `serde_json`, `toml`, `thiserror`
(v1), `anyhow`, `clap` (derive), `regex`, `log`, `env_logger`, `vm-fdt` (v0.3). Crates opt in with
`<dep> = { workspace = true }`.

## Build status (this session)

`cargo build` (macOS `aarch64-apple-darwin`, Rust 1.96, default features): **GREEN**.
`cargo test`: **GREEN — 30 tests pass, 0 warnings** (`cargo build --all-targets` reports 0 warnings).
`cargo build -p aemulator-vmm --features kvm`: **GREEN** (the `crosvm_backend` module is
`cfg(target_os="linux")`-gated, so it is correctly absent on macOS).

All external deps (incl. `vm-fdt` and `env_logger`) resolved from crates.io — network was available.
Nothing is unavailable offline in this checkout; a future clean offline build would need the crates
in `[workspace.dependencies]` + their transitive tree in the cargo cache.

---

## Crate-by-crate frozen API

### `aemulator-identity` — the CORE (single source of identity truth)

`DeviceProfile` is ingested from a real per-partition `build.prop` dump and emits, atomically and
coherently, every identity surface. **Inconsistency is the #1 detector**, so one struct feeds
everything.

Top-level struct (all fields `pub`):

```rust
pub struct DeviceProfile {
    pub meta: ProfileMeta,
    pub build: BuildIdentity,
    pub soc: SocProfile,
    pub abi: AbiProfile,
    pub gpu: GpuProfile,
    pub sensors: Vec<SensorSpec>,
    pub telephony: TelephonyProfile,
    pub verified_boot: VerifiedBoot,
    pub network: NetworkProfile,
}
```

Sub-structs (all `#[derive(Serialize, Deserialize, Default, Clone, Debug)]`, `#[serde(default)]`):

- `ProfileMeta { name, source, legal_note }` — all `String` (never emitted to guest).
- `BuildIdentity { brand, product_name, device, model, manufacturer, board, bootloader, hardware,
  host, user, tags, build_type, build_id, incremental, release, sdk: u32, security_patch,
  display_id }` — `product_name`=`ro.product.name`; `board`=`ro.product.board` (codename, **not**
  `ro.board.platform`); `hardware`=`ro.hardware` (load-bearing for HAL resolution).
- `SocProfile { soc_manufacturer, soc_model, board_platform, midr_clusters: Vec<MidrCluster>,
  socinfo: Socinfo }` — `board_platform`=`ro.board.platform` (separate from `board`).
- `MidrCluster { part_name, implementer: u8, part: u16, variant: u8, revision: u8, core_count: u32,
  max_freq_khz: u32 }` — per-cluster `MIDR_EL1`; heterogeneous across clusters. **Never
  `implementer == 0x61` (Apple).**
- `Socinfo { family, machine, soc_id: u32, revision, serial_number, foundry_id: u32,
  chip_family: u32, raw_device_family: u32, raw_device_number: u32, hardware_platform, pmic_model }`
  — Qualcomm `/sys/devices/soc0/*` shape.
- `AbiProfile { abilist, abilist32, abilist64 }` — `ro.product.cpu.abilist{,32,64}`.
- `GpuProfile { gl_vendor, gl_renderer, gl_version, vk_device_name, vk_vendor_id: u32,
  vk_device_type: VkDeviceType, vk_driver_id, ro_hardware_egl }`.
- `enum VkDeviceType { IntegratedGpu, DiscreteGpu, VirtualGpu, Cpu, Other }` (default `Other`).
  **Never `Cpu`** (SwiftShader/lavapipe tell).
- `SensorSpec { sensor_type, name, vendor, max_range: f32, resolution: f32, power: f32,
  min_delay: i32 }` — real datasheet parts, plausible values.
- `enum TelephonyProfile { WifiOnly, Cellular { baseband, imei_tac, imsi_mcc, imsi_mnc,
  operator_name, operator_numeric } }` — `#[serde(tag = "mode", rename_all = "snake_case")]`;
  default `WifiOnly`.
- `VerifiedBoot { verifiedbootstate, flash_locked: bool, device_state, veritymode }`.
- `NetworkProfile { mac_oui, interfaces: Vec<String> }` — real OEM OUI, never `52:54:00`/`08:00:27`.
- `MidrRegWrite { cluster_index: usize, reg: IdReg, value: u64 }`, `enum IdReg { MidrEl1, RevidrEl1 }`.

Methods (frozen):

```rust
impl DeviceProfile {
    pub fn from_toml(path: impl AsRef<Path>) -> Result<Self, IdentityError>;   // IMPLEMENTED
    pub fn fingerprint(&self) -> String;                                       // IMPLEMENTED (ConstructBuildFingerprint)
    pub fn emit_properties(&self) -> BTreeMap<String, String>;                 // TODO (task #2)
    pub fn emit_bootconfig(&self) -> Vec<(String, String)>;                    // TODO (task #2) — NEVER androidboot.qemu
    pub fn midr_regs(&self) -> Vec<MidrRegWrite>;                              // TODO (task #2)
    pub fn validate(&self) -> Result<(), Vec<CoherenceError>>;                 // TODO (task #2)
    pub fn lint_property_map(props: &BTreeMap<String, String>) -> Vec<Finding>;// IMPLEMENTED (delegates to detection)
}
```

Errors: `IdentityError` (`Io`, `Toml`), `CoherenceError` (`FingerprintMismatch`, `BlocklistedString`,
`TacModelMismatch`, `OperatorMccMncMismatch`, `SocIncoherent`, `MidrAnomaly`, `Other`).

Module `profiles`: `pub fn reference_profile() -> DeviceProfile` — a **minimal, coherent** Xiaomi 13
(`fuxi`/SM8550, Adreno 740 / Vulkan 1.3) literal (expand into the full catalogue in task #2). Its
fingerprint is `Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys`.

### `aemulator-detection` — the adversary's-eye view (leaf, NO identity dep)

```rust
pub struct Signal  { pub category: String, pub pattern: String, pub source: String }
pub struct Finding { pub signal: Signal, pub location: String, pub matched: String }

pub fn blocklist() -> &'static [Signal];         // TODO(task #3): populate from research strings
pub fn probe_catalogue() -> &'static [Signal];   // TODO(task #3): populate (rnidbg probe set, etc.)
pub fn scan_dir(root: &Path) -> Vec<Finding>;     // WIRED (walks + regex-matches paths); grep contents in full impl
pub fn lint_properties(props: &BTreeMap<String, String>) -> Vec<Finding>;  // WIRED
```

`lint_properties` takes a **plain map** (no identity dep) so the CI validator can lint any dump.
`blocklist`/`probe_catalogue` return `&'static [Signal]` via a lazy `OnceLock<Vec<Signal>>` — push
entries in `get_or_init`.

### `aemulator-dtb` — hand-authored device tree (identity + vm-fdt)

```rust
pub fn build_fdt(profile: &DeviceProfile) -> Result<Vec<u8>, DtbError>;  // IMPLEMENTED (minimal valid tree; expand SoC leaf nodes in task #3)
pub fn assert_no_vm_tells(fdt: &[u8]) -> Result<(), DtbError>;           // IMPLEMENTED (byte-scan blocklist)
pub enum DtbError { Fdt(String), VmTell(String) }
```

`build_fdt` sets the root `compatible`/`model` from the SoC (so `/proc/cpuinfo` `Hardware:` reads
right) and self-checks with `assert_no_vm_tells`. **Never** emit `linux,dummy-virt`, `goldfish`,
`qemu`, `vcpu-stall-detector`, `kvm-cpufreq`, `pkvm`, … (the `VM_TELLS` list).

### `aemulator-control` — LLM control-plane contract + MockController (leaf)

```rust
pub struct Rect { pub x: i32, pub y: i32, pub width: i32, pub height: i32 }  // + fn center()
pub struct UiNode { pub index: u32, pub text, pub content_desc, pub class: String, pub bounds: Rect, pub clickable: bool, pub focusable: bool }
pub struct UiTree { pub nodes: Vec<UiNode>, pub generation: u64 }
pub struct Observation { pub screenshot_png: Vec<u8>, pub width: u32, pub height: u32, pub ui_tree: UiTree, pub generation: u64 }

pub enum Action {
    Tap { index: u32, generation: u64 },   // index-first; stale generation → error
    TapXy { x: i32, y: i32 },              // raw-coordinate escape hatch
    Swipe { x1, y1, x2, y2: i32, duration_ms: u32 },
    Text { text: String }, Key { code: i32 },
    LaunchApp { pkg: String }, InstallApk { path: PathBuf },
    SetGps { lat: f64, lng: f64 },
    WaitStable, Snapshot { name: String }, Restore { name: String },
}

pub trait DeviceController {
    fn observe(&self) -> Result<Observation, ControlError>;
    fn act(&mut self, action: Action) -> Result<Observation, ControlError>;
}

pub struct MockController { … }  // REAL, fully tested in-memory impl
pub enum ControlError { StaleGeneration { expected, got: u64 }, NodeIndexOutOfRange { index: u32, len: usize }, SnapshotNotFound(String), NotReady }
```

Design invariant (tested): a `Tap` whose `generation` ≠ the live tree's generation returns
`StaleGeneration` **and does not mutate state**; every mutating `act` returns the post-action
`Observation`.

### `aemulator-vmm` — identity injection + launcher args (identity + dtb)

```rust
pub struct BootAssets { pub kernel, pub initrd, pub system_img, pub vendor_img, pub firmware: PathBuf }
pub fn qemu_hvf_args(profile: &DeviceProfile, assets: &BootAssets) -> Vec<String>;  // IMPLEMENTED — builds on macOS
pub fn kernel_cmdline(profile: &DeviceProfile) -> String;                            // IMPLEMENTED — never androidboot.qemu
pub fn build_device_tree(profile: &DeviceProfile) -> Result<Vec<u8>, VmmError>;      // delegates to aemulator-dtb
pub enum VmmError { MissingAsset(String), BackendUnavailable(String), Dtb(String) }

// Feature `kvm` (default OFF) + cfg(all(target_os="linux", feature="kvm")):
#[cfg(all(target_os = "linux", feature = "kvm"))]
pub mod crosvm_backend;   // documented seam → the crosvm project (chromium.googlesource.com/crosvm/crosvm); NOT a path dep (would break macOS)
```

`qemu_hvf_args` = aarch64 `-accel hvf`, `-cpu host` (with the documented Apple-`0x61`-MIDR caveat),
custom `-dtb`, `-smbios` from the profile, real MAC OUI, renamed virtio serial, `-append`
`kernel_cmdline` — and never `androidboot.qemu`. The crosvm/KVM backend is Linux-only work
(task #3 / Phase 1); wire it inside `crosvm_backend` only, never as a top-level dependency.

### `aemulator-supervisor` — device lifecycle (vmm + control + identity)

```rust
pub enum DeviceState { Created, Booting, Ready, Stopped, Failed }
pub struct Supervisor { … }
impl Supervisor {
    pub fn new(profile: DeviceProfile) -> Self;
    pub fn profile(&self) -> &DeviceProfile;
    pub fn state(&self) -> DeviceState;
    pub fn boot(&mut self, assets: &BootAssets) -> Result<(), SupervisorError>;
    pub fn observe(&self) -> Result<Observation, SupervisorError>;
    pub fn act(&mut self, action: Action) -> Result<Observation, SupervisorError>;
    pub fn shutdown(&mut self) -> Result<(), SupervisorError>;
}
pub enum SupervisorError { NotReady(DeviceState), Control(ControlError), Vmm(VmmError) }
```

Wired against a `MockController` today (observe/act + state machine tested). Swap the real VMM launch
+ guest control channel into `boot` without changing signatures.

### `attestation-relay` — OPTIONAL Phase-5, per-flow (leaf)

```rust
pub struct RelayRequest  { pub nonce: String, pub package: String }
pub struct RelayResponse { pub token: String, pub device_id: String }
pub struct DeviceHandle  { pub device_id: String }
pub trait DevicePool { fn acquire(&mut self) -> Result<DeviceHandle, RelayError>; fn release(&mut self, h: DeviceHandle); fn available(&self) -> usize; }
pub struct NullPool;          // always-empty pool (testable)
pub struct RelayBroker { … }  // new(Box<dyn DevicePool + Send>), available(), relay() [TODO Phase 5]
pub enum RelayError { NoDeviceAvailable, RelayFailed { device_id, detail: String } }
```

Module doc states the honest constraints (correlation / rate / `deviceRecall` / latency / legal).
Not a v1 promise — ship behind explicit per-app enablement.

### `aemulator-cli` — the `aemulator` binary (identity + detection + dtb + vmm)

`clap`-derive. Subcommands (global `--profile <PATH>`, defaults to the bundled reference profile):
`profile-show`, `profile-validate`, `props`, `bootconfig`, `fingerprint`, `lint <dir>`,
`qemu-args`, `dtb <out>`. Wiring is final; `props`/`bootconfig`/`profile-validate` call the
`todo!()` identity bodies and will panic until task #2 fills them (`fingerprint`, `qemu-args`,
`lint`, `dtb`, `profile-show` all run today).

---

## Filling order (suggested)

1. **identity** (`emit_properties`, `emit_bootconfig`, `midr_regs`, `validate`) — unblocks the CLI
   and the CI gate. Mirror the exact property/bootconfig strings from the research dossier.
2. **detection** (`blocklist`, `probe_catalogue`) — populate from the verified strings; this turns
   `scan_dir`/`lint_properties` and the CLI `lint` into a real merge gate.
3. **dtb** — expand `build_fdt` with the full SoC leaf-node topology (CPUs, GIC, clocks, soc0).
4. **vmm** — the Linux/KVM `crosvm_backend` (Phase 1); keep it behind the double gate.
5. **oracle** — the Phase-0 host bring-up scripts (see `oracle/README.md`).
