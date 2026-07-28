//! Thin `#[tauri::command]` wrappers. They resolve managed state, delegate to the
//! manager / control modules, and flatten errors to `String` at the IPC boundary.
//!
//! Concurrency rule: socket I/O (control verbs, screenshots) can block up to the
//! socket timeout, so these commands take the manager lock ONLY to read the
//! socket path, release it, then do the blocking I/O unlocked — a wedged VM never
//! stalls the whole IPC surface.

use std::sync::Mutex;

use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::error::StudioError;
use crate::manager::DeviceManager;
use crate::types::*;
use crate::control;

type Managed<'a> = State<'a, Mutex<DeviceManager>>;

fn lock<'a>(m: &'a Managed) -> std::sync::MutexGuard<'a, DeviceManager> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- Emulator lifecycle ----

#[tauri::command]
pub fn list_emulators(m: Managed) -> Result<Vec<EmulatorProfile>, String> {
    Ok(lock(&m).list_emulators())
}

#[tauri::command]
pub fn list_instances(m: Managed) -> Result<Vec<VmInstance>, String> {
    Ok(lock(&m).list_instances())
}

#[tauri::command]
pub fn create_emulator(m: Managed, input: CreateEmulatorInput) -> Result<EmulatorProfile, String> {
    lock(&m).create_emulator(input).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn update_emulator(
    m: Managed,
    id: EmulatorId,
    patch: UpdateEmulatorInput,
) -> Result<EmulatorProfile, String> {
    lock(&m)
        .update_emulator(&id, patch)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_emulator(m: Managed, id: EmulatorId) -> Result<(), String> {
    lock(&m).delete_emulator(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn start_emulator(app: AppHandle, m: Managed, id: EmulatorId) -> Result<VmInstance, String> {
    lock(&m).start_emulator(&app, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn stop_emulator(app: AppHandle, m: Managed, id: EmulatorId) -> Result<(), String> {
    lock(&m).stop_emulator(&app, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_status(m: Managed, id: EmulatorId) -> Result<VmInstance, String> {
    lock(&m).get_status(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_logs(m: Managed, id: EmulatorId) -> Result<Vec<VmLogEvent>, String> {
    Ok(lock(&m).get_logs(&id))
}

// ---- Framebuffer ----

#[tauri::command]
pub fn stream_framebuffer(
    app: AppHandle,
    m: Managed,
    id: EmulatorId,
    chan: Channel<tauri::ipc::InvokeResponseBody>,
) -> Result<(), String> {
    lock(&m)
        .start_framebuffer(&app, &id, chan)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn stop_framebuffer(m: Managed, id: EmulatorId) -> Result<(), String> {
    lock(&m).stop_framebuffer(&id).map_err(|e| e.to_string())
}

/// One-shot screenshot: returns a `data:image/png;base64,…` URL string the UI can
/// drop straight into an `<img src>`.
#[tauri::command]
pub fn screenshot(m: Managed, id: EmulatorId) -> Result<String, String> {
    let (socket, shot) = lock(&m).socket_and_shot(&id).map_err(|e| e.to_string())?;
    let bytes = control::screenshot_bytes(&socket, &shot)?;
    Ok(png_data_url(&bytes))
}

// ---- Control verbs (speak the Unix socket directly) ----

fn socket_for(m: &Managed, id: &EmulatorId) -> Result<std::path::PathBuf, String> {
    lock(m).socket_of(id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn send_tap(m: Managed, id: EmulatorId, x: u32, y: u32) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::Tap { x, y }).map(|_| ())
}

#[tauri::command]
pub fn send_swipe(
    m: Managed,
    id: EmulatorId,
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
    ms: Option<u32>,
) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::Swipe { x1, y1, x2, y2, ms }).map(|_| ())
}

#[tauri::command]
pub fn send_key(m: Managed, id: EmulatorId, code: String) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::Key(code)).map(|_| ())
}

/// Press a key and leave it held (live keyboard forwarding). `code` is an evdev
/// number or a name the control plane knows; the caller owns the matching
/// `send_key_up`, which is why the UI releases every held key on blur.
#[tauri::command]
pub fn send_key_down(m: Managed, id: EmulatorId, code: String) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::KeyDown(code)).map(|_| ())
}

#[tauri::command]
pub fn send_key_up(m: Managed, id: EmulatorId, code: String) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::KeyUp(code)).map(|_| ())
}

#[tauri::command]
pub fn send_text(m: Managed, id: EmulatorId, text: String) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::Text(text)).map(|_| ())
}

#[tauri::command]
pub fn launch_package(m: Managed, id: EmulatorId, package: String) -> Result<(), String> {
    let socket = socket_for(&m, &id)?;
    control::send(&socket, &control::Verb::Launch(package)).map(|_| ())
}

#[tauri::command]
pub fn wait_stable(m: Managed, id: EmulatorId, timeout_ms: Option<u32>) -> Result<u64, String> {
    let socket = socket_for(&m, &id)?;
    let reply = control::send(&socket, &control::Verb::WaitStable { timeout_ms })?;
    // Reply: "OK stable gen=N" | "OK timeout gen=N" → parse the gen.
    let gen = reply
        .split_whitespace()
        .find_map(|t| t.strip_prefix("gen=").and_then(|v| v.parse::<u64>().ok()))
        .unwrap_or(0);
    Ok(gen)
}

// ---- Devices / storage / host ----

#[tauri::command]
pub fn device_config(m: Managed, id: EmulatorId) -> Result<DeviceConfig, String> {
    lock(&m).device_config(&id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_storage(m: Managed) -> Result<Vec<StorageVolume>, String> {
    Ok(lock(&m).list_storage())
}

#[tauri::command]
pub fn create_snapshot(_id: EmulatorId, _label: String) -> Result<Snapshot, String> {
    Err(StudioError::NotImplemented("create_snapshot (Phase 2)").to_string())
}

#[tauri::command]
pub fn restore_snapshot(_id: EmulatorId, _snapshot_id: String) -> Result<(), String> {
    Err(StudioError::NotImplemented("restore_snapshot (Phase 2)").to_string())
}

#[tauri::command]
pub fn delete_snapshot(_id: EmulatorId, _snapshot_id: String) -> Result<(), String> {
    Err(StudioError::NotImplemented("delete_snapshot (Phase 2)").to_string())
}

#[tauri::command]
pub fn list_base_images(m: Managed) -> Result<Vec<ImageCatalogEntry>, String> {
    Ok(lock(&m).list_base_images())
}

#[tauri::command]
pub fn host_capabilities(m: Managed) -> Result<HostCaps, String> {
    Ok(lock(&m).host_capabilities())
}

fn png_data_url(bytes: &[u8]) -> String {
    // Reuse the framebuffer encoder for identical output.
    crate::framebuffer::png_data_url(bytes)
}
