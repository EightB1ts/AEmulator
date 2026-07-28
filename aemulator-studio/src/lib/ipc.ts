// ONE typed wrapper over every invoke() + event/Channel subscription.
// The frontend never calls invoke() directly — it goes through this module so
// the command surface (and its types) is defined in exactly one place.

import { invoke, Channel } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type {
    CreateEmulatorInput,
    DeviceConfig,
    EmulatorId,
    EmulatorProfile,
    HostCaps,
    ImageCatalogEntry,
    Snapshot,
    StorageVolume,
    UpdateEmulatorInput,
    VmErrorEvent,
    VmInstance,
    VmLogEvent,
    VmStateEvent,
} from './types'

// ---- Emulator lifecycle ----
export const listEmulators = () =>
    invoke<EmulatorProfile[]>('list_emulators')

export const listInstances = () => invoke<VmInstance[]>('list_instances')

export const createEmulator = (input: CreateEmulatorInput) =>
    invoke<EmulatorProfile>('create_emulator', { input })

// Partial profile edit. Rejects launch-affecting fields while the VM is active
// (the error surfaces as "emulator is running — stop it first").
export const updateEmulator = (id: EmulatorId, patch: UpdateEmulatorInput) =>
    invoke<EmulatorProfile>('update_emulator', { id, patch })

export const deleteEmulator = (id: EmulatorId) =>
    invoke<void>('delete_emulator', { id })

export const startEmulator = (id: EmulatorId) =>
    invoke<VmInstance>('start_emulator', { id })

export const stopEmulator = (id: EmulatorId) =>
    invoke<void>('stop_emulator', { id })

export const getStatus = (id: EmulatorId) =>
    invoke<VmInstance>('get_status', { id })

export const getLogs = (id: EmulatorId) =>
    invoke<VmLogEvent[]>('get_logs', { id })

// ---- Framebuffer ----
// Frames arrive as RAW BINARY, not data-URL strings: an 8-byte little-endian
// header (`w: u32, h: u32`) followed by `w*h*4` RGBA bytes, ready to hand to a
// canvas `ImageData` with no decode and no per-pixel work. Paced at ~60fps and
// gated server-side on the guest's present generation, so a still screen sends
// nothing at all.
export const FRAME_HEADER_BYTES = 8

export const streamFramebuffer = (id: EmulatorId, chan: Channel<ArrayBuffer>) =>
    invoke<void>('stream_framebuffer', { id, chan })

/// Split one frame message into its geometry + pixel view (no copy).
///
/// `Uint8ClampedArray<ArrayBuffer>` is spelled out because TS 5.7+ made typed
/// arrays generic over their backing buffer: a bare `Uint8ClampedArray` widens to
/// `ArrayBufferLike`, which `ImageData` refuses (it cannot accept a
/// `SharedArrayBuffer`).
export function decodeFrame(
    buf: ArrayBuffer,
): { width: number; height: number; pixels: Uint8ClampedArray<ArrayBuffer> } | null {
    if (buf.byteLength <= FRAME_HEADER_BYTES) return null
    const header = new DataView(buf, 0, FRAME_HEADER_BYTES)
    const width = header.getUint32(0, true)
    const height = header.getUint32(4, true)
    const need = width * height * 4
    if (width === 0 || height === 0) return null
    if (buf.byteLength - FRAME_HEADER_BYTES < need) return null
    return {
        width,
        height,
        pixels: new Uint8ClampedArray(buf, FRAME_HEADER_BYTES, need),
    }
}

export const stopFramebuffer = (id: EmulatorId) =>
    invoke<void>('stop_framebuffer', { id })

// Returns a `data:image/png;base64,…` URL for the current frame.
export const screenshot = (id: EmulatorId) =>
    invoke<string>('screenshot', { id })

// ---- Control ----
export const sendTap = (id: EmulatorId, x: number, y: number) =>
    invoke<void>('send_tap', { id, x, y })

export const sendSwipe = (
    id: EmulatorId,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    ms?: number,
) => invoke<void>('send_swipe', { id, x1, y1, x2, y2, ms })

export const sendKey = (id: EmulatorId, code: string) =>
    invoke<void>('send_key', { id, code })

// Separate press/release edges — what live keyboard capture and held modifiers
// (Cmd/Ctrl/Alt/Shift chords) need; `send_key` is an atomic press+release and can
// never express a held key. Caller owns the matching release.
export const sendKeyDown = (id: EmulatorId, code: string) =>
    invoke<void>('send_key_down', { id, code })

export const sendKeyUp = (id: EmulatorId, code: string) =>
    invoke<void>('send_key_up', { id, code })

export const sendText = (id: EmulatorId, text: string) =>
    invoke<void>('send_text', { id, text })

export const launchPackage = (id: EmulatorId, pkg: string) =>
    invoke<void>('launch_package', { id, package: pkg })

export const waitStable = (id: EmulatorId, timeoutMs?: number) =>
    invoke<number>('wait_stable', { id, timeoutMs })

// ---- Devices / storage / host ----
export const deviceConfig = (id: EmulatorId) =>
    invoke<DeviceConfig>('device_config', { id })

export const listStorage = () => invoke<StorageVolume[]>('list_storage')

export const createSnapshot = (id: EmulatorId, label: string) =>
    invoke<Snapshot>('create_snapshot', { id, label })

export const restoreSnapshot = (id: EmulatorId, snapshotId: string) =>
    invoke<void>('restore_snapshot', { id, snapshotId })

export const deleteSnapshot = (id: EmulatorId, snapshotId: string) =>
    invoke<void>('delete_snapshot', { id, snapshotId })

export const listBaseImages = () =>
    invoke<ImageCatalogEntry[]>('list_base_images')

export const hostCapabilities = () => invoke<HostCaps>('host_capabilities')

// ---- Events ----
export const onVmState = (cb: (e: VmStateEvent) => void): Promise<UnlistenFn> =>
    listen<VmStateEvent>('vm-state', (ev) => cb(ev.payload))

export const onVmLog = (cb: (e: VmLogEvent) => void): Promise<UnlistenFn> =>
    listen<VmLogEvent>('vm-log', (ev) => cb(ev.payload))

export const onVmError = (cb: (e: VmErrorEvent) => void): Promise<UnlistenFn> =>
    listen<VmErrorEvent>('vm-error', (ev) => cb(ev.payload))

export { Channel }
