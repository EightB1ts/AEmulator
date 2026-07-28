//! Thin binary shim. All app logic lives in the `aemulator_studio_lib` library
//! crate (the proven app_frontend/desktop pattern).

// Suppress the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    aemulator_studio_lib::run()
}
