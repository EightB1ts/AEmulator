// TS mirror of the Rust contract in src-tauri/src/types.rs (DESIGN.md §2).
// Hand-kept in sync. serde serializes Rust field names as snake_case and unit
// enum variants as their bare string name, so these mirrors use snake_case keys
// and string-literal unions.

export type EmulatorId = string // uuid v4, assigned at create

export type RenderMode = 'Gpu3d' | 'SwRender' | 'Headless' // → --gpu3d / SWRENDER=1 / neither
export type SelinuxMode = 'Permissive' | 'Enforcing' // → --selinux …

export interface TelephonyMeta {
    imei: string
    operator: string
    mcc_mnc: string
}

export interface DeviceIdentity {
    soc: string // "Snapdragon 8 Gen 2 / SM8550"
    manufacturer: string // "Xiaomi"
    model: string // "fuxi"
    advertised_sensors: string[] // ["accelerometer","magnetometer"]
    telephony: TelephonyMeta | null // IMEI/operator if a radio is claimed
}

export interface EmulatorProfile {
    id: EmulatorId
    name: string
    base_image: string // key into the image catalog (e.g. "cf64")
    mem_mb: number
    cpus: number
    render_mode: RenderMode
    selinux: SelinuxMode
    extra_cmdline: string
    device_identity: DeviceIdentity
    created_at: number // epoch ms
}

export type VmState =
    | 'Provisioning'
    | 'Booting'
    | 'Ready'
    | 'Stopping'
    | 'Stopped'
    | 'Crashed'

export interface VmPorts {
    adb_host: number
    control_socket: string
}

export interface DisplayInfo {
    width: number
    height: number
    gen: number
    nonzero: number
}

export interface VmInstance {
    id: EmulatorId
    state: VmState
    ports: VmPorts
    display: DisplayInfo
    started_at: number | null
    pid: number | null
}

export type DeviceStatus = 'Emulated' | 'Partial' | 'Planned'

export type DeviceKind =
    | 'Cpu'
    | 'Gpu2d'
    | 'Gpu3d'
    | 'BlockPrimary'
    | 'BlockExtra'
    | 'Input'
    | 'Console'
    | 'Net'
    | 'Rng'
    | 'Keymint'
    | 'VerifiedBoot'
    | 'Vsock'
    | 'Audio'
    | 'Gps'
    | 'Sensors'
    | 'Camera'
    | 'Battery'
    | 'Telephony'
    | 'Clipboard'
    | 'Storage'
    | 'Bluetooth'
    | 'Nfc'
    | 'Vibration'

// Functional (subsystem) axis, orthogonal to DeviceStatus. The Hardware tab
// groups on this and renders status as a per-item dot.
export type DeviceGroup =
    | 'Compute'
    | 'Graphics'
    | 'Storage'
    | 'Io'
    | 'Sensors'
    | 'Security'

export interface DeviceEntry {
    kind: DeviceKind
    label: string
    status: DeviceStatus
    group: DeviceGroup
    detail: string
    controllable: boolean
}

export interface DeviceConfig {
    emulator_id: EmulatorId
    devices: DeviceEntry[]
}

export interface PartitionUsage {
    name: string
    size_bytes: number
}

export interface Snapshot {
    id: string
    label: string
    created_at: number
    on_disk_bytes: number
}

export interface StorageVolume {
    emulator_id: EmulatorId
    instance_dir: string
    logical_bytes: number
    on_disk_bytes: number
    writable_partitions: PartitionUsage[]
    snapshots: Snapshot[]
}

export interface CreateEmulatorInput {
    name: string
    base_image: string
    mem_mb: number
    cpus: number
    render_mode: RenderMode
    selinux: SelinuxMode
    device_identity: DeviceIdentity
    extra_cmdline: string
}

// Partial edit to a saved profile — omitted field = leave as-is.
// Gating is enforced by the backend (manager::update_emulator), mirrored by the
// Settings tab: `name` + `device_identity` apply any time (neither reaches the
// launch); everything else is composed into argv/env at start time and is
// rejected while the VM is active. `base_image` additionally needs
// `reprovision`, which ERASES this VM's disk so the next start re-clones.
export interface UpdateEmulatorInput {
    name?: string
    base_image?: string
    mem_mb?: number
    cpus?: number
    render_mode?: RenderMode
    selinux?: SelinuxMode
    extra_cmdline?: string
    device_identity?: DeviceIdentity
    reprovision?: boolean
}

export interface ImageCatalogEntry {
    key: string
    path: string
    logical_bytes: number
    present: boolean
}

export interface HostCaps {
    total_ram_mb: number
    free_ram_mb: number
    libslirp: boolean
    binary_signed: boolean
    max_vms: number
}

// ---- Event payloads (backend → frontend via app.emit) ----

export interface VmStateEvent {
    id: EmulatorId
    state: VmState
}

export interface VmLogEvent {
    id: EmulatorId
    line: string
    stream: 'stdout' | 'stderr'
    ts: number
}

export interface VmErrorEvent {
    id: EmulatorId
    message: string
}
