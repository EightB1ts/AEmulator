//! The interface contract (DESIGN.md §2) — the seam the Rust backend and the TS
//! frontend both build to. `serde` serializes struct fields as snake_case and
//! unit enum variants as their bare name; `src/lib/types.ts` mirrors this
//! verbatim. All Tauri commands return `Result<T, String>` (errors flatten at
//! the IPC boundary).

use serde::{Deserialize, Serialize};

/// uuid v4, assigned at create.
pub type EmulatorId = String;

/// How to launch the render pipeline. → `--gpu3d` / `SWRENDER=1` / neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderMode {
    Gpu3d,
    SwRender,
    Headless,
}

/// → `--selinux permissive|enforcing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelinuxMode {
    Permissive,
    Enforcing,
}

/// Telephony metadata (advertised strings only — no live radio path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelephonyMeta {
    pub imei: String,
    pub operator: String,
    pub mcc_mnc: String,
}

/// Sourced from `aemulator-identity::DeviceProfile`. Sensors/telephony are
/// advertised-only (metadata; no live data path — see `DeviceConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub soc: String,
    pub manufacturer: String,
    pub model: String,
    pub advertised_sensors: Vec<String>,
    pub telephony: Option<TelephonyMeta>,
}

/// A saved emulator definition (persisted under the app config dir).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmulatorProfile {
    pub id: EmulatorId,
    pub name: String,
    pub base_image: String,
    pub mem_mb: u32,
    pub cpus: u32,
    pub render_mode: RenderMode,
    pub selinux: SelinuxMode,
    pub extra_cmdline: String,
    pub device_identity: DeviceIdentity,
    /// epoch ms
    pub created_at: i64,
}

/// The lifecycle state machine (mirrors `aemulator-supervisor::DeviceState`,
/// which later builds drive this off of).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VmState {
    Provisioning,
    Booting,
    Ready,
    Stopping,
    Stopped,
    Crashed,
}

/// `127.0.0.1:adb_host` for adb-over-TCP; `control_socket` is the Unix socket path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPorts {
    pub adb_host: u16,
    pub control_socket: String,
}

/// Last-known framebuffer geometry (from the control-plane `info` verb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub gen: u64,
    pub nonzero: u64,
}

/// A running (or just-stopped) VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInstance {
    pub id: EmulatorId,
    pub state: VmState,
    pub ports: VmPorts,
    pub display: DisplayInfo,
    pub started_at: Option<i64>,
    pub pid: Option<u32>,
}

/// Honesty tier for a device capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceStatus {
    Emulated,
    Partial,
    Planned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Cpu,
    Gpu2d,
    Gpu3d,
    BlockPrimary,
    BlockExtra,
    Input,
    Console,
    Net,
    Rng,
    Keymint,
    VerifiedBoot,
    Vsock,
    Audio,
    Gps,
    Sensors,
    Camera,
    Battery,
    Telephony,
    Clipboard,
    Storage,
    Bluetooth,
    Nfc,
    Vibration,
}

/// Functional grouping for the Hardware tab. Status (`DeviceStatus`) is the
/// honesty tier and renders as a per-item dot; this is the *subsystem* axis the
/// UI groups on, so what a device DOES stays adjacent regardless of how real it
/// is yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceGroup {
    Compute,
    Graphics,
    Storage,
    Io,
    Sensors,
    Security,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub kind: DeviceKind,
    pub label: String,
    pub status: DeviceStatus,
    pub group: DeviceGroup,
    pub detail: String,
    /// Does the UI expose a live control for it today?
    pub controllable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub emulator_id: EmulatorId,
    pub devices: Vec<DeviceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionUsage {
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub on_disk_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageVolume {
    pub emulator_id: EmulatorId,
    pub instance_dir: String,
    pub logical_bytes: u64,
    pub on_disk_bytes: u64,
    pub writable_partitions: Vec<PartitionUsage>,
    pub snapshots: Vec<Snapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateEmulatorInput {
    pub name: String,
    pub base_image: String,
    pub mem_mb: u32,
    pub cpus: u32,
    pub render_mode: RenderMode,
    pub selinux: SelinuxMode,
    pub device_identity: DeviceIdentity,
    pub extra_cmdline: String,
}

/// A partial edit to a saved profile. Absent field = "leave as-is", so the
/// Settings tab can send only what the user actually touched.
///
/// Gating is enforced in `manager::update_emulator` (the UI mirrors it, but the
/// backend is the boundary). Two tiers, derived from what actually reaches the
/// launch in `spawn::compose_launch`:
///  - **Any time**: `name`, `device_identity` — neither reaches the launch today
///    (identity is advertised metadata only), so editing them is inert-but-safe.
///  - **Stopped only**: `mem_mb`, `cpus`, `render_mode`, `selinux`,
///    `extra_cmdline`, `base_image` — all composed into argv/env at start time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateEmulatorInput {
    pub name: Option<String>,
    pub base_image: Option<String>,
    pub mem_mb: Option<u32>,
    pub cpus: Option<u32>,
    pub render_mode: Option<RenderMode>,
    pub selinux: Option<SelinuxMode>,
    pub extra_cmdline: Option<String>,
    pub device_identity: Option<DeviceIdentity>,
    /// Required to accept a `base_image` change once a disk exists.
    /// `storage::provision_cf_dir` early-returns on an existing cf-dir to keep a
    /// VM's mutated state, so a new base would otherwise be silently ignored and
    /// the VM would keep booting the OLD image. Setting this erases the disk so
    /// the next start re-clones from the new base.
    #[serde(default)]
    pub reprovision: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCatalogEntry {
    pub key: String,
    pub path: String,
    pub logical_bytes: u64,
    pub present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCaps {
    pub total_ram_mb: u32,
    pub free_ram_mb: u32,
    pub libslirp: bool,
    pub binary_signed: bool,
    pub max_vms: u32,
}

// ---- Event payloads (backend → frontend via app.emit) ----

#[derive(Debug, Clone, Serialize)]
pub struct VmStateEvent {
    pub id: EmulatorId,
    pub state: VmState,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmLogEvent {
    pub id: EmulatorId,
    pub line: String,
    pub stream: String, // "stdout" | "stderr"
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmErrorEvent {
    pub id: EmulatorId,
    pub message: String,
}
