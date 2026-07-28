//! `VmHandle` — one running (or just-stopped) emulator: its lifecycle state,
//! resolved ports/paths, the spawned child's PID, and the stop tokens that let
//! the manager wind down the supervisor + framebuffer-poll threads.
//!
//! The `std::process::Child` itself is MOVED into the supervisor thread (which
//! owns its stdout/stderr pipes and reaps it on exit); the handle keeps only the
//! `pid` for signal-based teardown (SIGTERM → SIGKILL). This avoids sharing a
//! `&mut Child` across the manager mutex and the reader thread.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::types::{DisplayInfo, EmulatorId, VmInstance, VmPorts, VmState};

/// Max recent log lines retained per VM (also emitted as `vm-log` events).
pub const LOG_RING_CAP: usize = 800;

pub struct VmHandle {
    pub id: EmulatorId,
    pub state: VmState,
    pub ports: VmPorts,
    pub display: DisplayInfo,
    pub started_at: Option<i64>,
    pub pid: u32,
    /// Provisioned instance dir (kept for teardown/inspection).
    #[allow(dead_code)]
    pub instance_dir: PathBuf,
    pub socket: PathBuf,
    pub live_shot: PathBuf,
    /// Raw-RGBA target for the live-view frame stream (distinct from `live_shot`).
    pub live_frame: PathBuf,
    pub adb_port: u16,
    /// Set true by the manager on teardown so the supervisor reports Stopped
    /// (intentional) rather than Crashed when the child exits.
    pub intentional_stop: Arc<AtomicBool>,
    /// Set true to wind down the framebuffer poll thread.
    pub fb_stop: Arc<AtomicBool>,
    /// True while a framebuffer poll thread is live (guards double-start).
    pub fb_running: bool,
}

impl VmHandle {
    /// Project the handle to the wire `VmInstance` the frontend consumes.
    pub fn to_instance(&self) -> VmInstance {
        VmInstance {
            id: self.id.clone(),
            state: self.state,
            ports: self.ports.clone(),
            display: self.display,
            started_at: self.started_at,
            pid: Some(self.pid),
        }
    }

    /// Is this VM in an active (RAM-consuming, controllable-soon) state?
    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            VmState::Provisioning | VmState::Booting | VmState::Ready | VmState::Stopping
        )
    }
}
