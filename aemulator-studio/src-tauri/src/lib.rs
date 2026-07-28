//! Library crate for AEmulator Studio. Builds the `DeviceManager`, resolves the
//! config/data dirs in `setup`, wires the command surface, and runs the Tauri app
//! (one native WKWebView window). The binary (`main.rs`) is a thin shim.
//!
//! Logic is modularized; `commands.rs` wrappers only resolve managed state +
//! delegate; errors flatten to `String` at the IPC boundary; app-defined commands
//! ride `core:default` (no per-command ACL).

mod commands;
mod control;
mod devices;
mod error;
mod framebuffer;
mod host;
mod manager;
mod paths;
mod ports;
mod profiles;
mod session;
mod spawn;
mod storage;
mod types;
mod vm;

use std::sync::Mutex;

use tauri::Manager;

use manager::DeviceManager;

/// Build and run the Studio app.
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(DeviceManager::new()))
        .setup(|app| {
            // Resolve the per-user config + data dirs and load persisted profiles.
            let config_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("aemulator-studio"));
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("aemulator-studio"));
            if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                m.configure(config_dir, data_dir);
                // Re-attach to emulators that outlived a previous Studio process (the normal case
                // under `pnpm tauri:dev`, where every Rust edit relaunches the app) and garbage
                // collect records for ones that died. Without this the UI shows a running VM as
                // stopped, and pressing Start used to spawn a SECOND VMM onto its disk — which
                // corrupts the guest filesystem. See `session` for the full account.
                m.reconcile_sessions(app.handle());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_emulators,
            commands::list_instances,
            commands::create_emulator,
            commands::update_emulator,
            commands::delete_emulator,
            commands::start_emulator,
            commands::stop_emulator,
            commands::get_status,
            commands::get_logs,
            commands::stream_framebuffer,
            commands::stop_framebuffer,
            commands::screenshot,
            commands::send_tap,
            commands::send_swipe,
            commands::send_key,
            commands::send_key_down,
            commands::send_key_up,
            commands::send_text,
            commands::launch_package,
            commands::wait_stable,
            commands::device_config,
            commands::list_storage,
            commands::create_snapshot,
            commands::restore_snapshot,
            commands::delete_snapshot,
            commands::list_base_images,
            commands::host_capabilities,
        ])
        .build(tauri::generate_context!())
        .expect("error while building AEmulator Studio")
        .run(|app, event| {
            // Stop every running VM before the process goes away.
            //
            // `RunEvent::Exit` (not `ExitRequested`) is the right hook: it fires on the way out for
            // every exit path — last window closed, Cmd-Q, `app.exit()` — and it fires once. An
            // orphaned VMM is not a cosmetic leak: it keeps the cf-dir lock, so the next Studio
            // launch cannot start that emulator at all, and its guest never gets the final block
            // flush that leaves userdata consistent.
            if matches!(event, tauri::RunEvent::Exit) {
                if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                    let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                    m.shutdown_all(app);
                }
            }
        });
}
