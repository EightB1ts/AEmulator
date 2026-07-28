//! Backend error type. Flattened to `String` at the Tauri IPC boundary
//! (`commands.rs` does `.map_err(|e| e.to_string())`).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StudioError {
    #[error("emulator not found: {0}")]
    NotFound(String),

    #[error("emulator is running — stop it first")]
    Running,

    #[error("RAM budget exceeded: launching would use {want} MB of {cap} MB")]
    RamBudget { want: u32, cap: u32 },

    /// Anything not yet implemented in this build. Honest by construction — the
    /// frontend surfaces the message so a stub is never mistaken for real work.
    #[error("not implemented in this build: {0}")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StudioError>;
