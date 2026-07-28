//! `DeviceManager` — the process-wide backbone. Built once, `.manage()`d behind a
//! `Mutex`. Owns the profile registry (persisted to `profiles.json`), the running
//! `VmHandle` map, the adb-port allocator, and the resolved config/instance dirs.
//!
//! REAL lifecycle: `start_emulator` provisions a per-VM cf-dir (APFS clonefile of
//! the base image), ad-hoc-signs the emulator binary, spawns it as a supervised
//! child (`aemulator-hvf control …`), and launches two background threads — a
//! supervisor (stdout/stderr → `vm-log`, reaps the child → `vm-state`) and a
//! readiness monitor (poll the control socket → flip Booting→Ready). Control +
//! screenshot + framebuffer commands then speak the Unix socket directly.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::devices::device_config_for;
use crate::error::{Result, StudioError};
use crate::ports::PortPool;
use crate::types::*;
use crate::vm::{VmHandle, LOG_RING_CAP};
use crate::{control, framebuffer, host, paths, profiles, session, spawn, storage};

/// How long a `quit` gets to tear the VM down before SIGTERM. Generous on purpose: a clean stop
/// fdatasyncs the writable images (a multi-GB `userdata.img`), and interrupting that flush is exactly
/// how the guest filesystem gets left inconsistent.
const QUIT_GRACE_SECS: u64 = 10;
/// Additional grace after SIGTERM before the SIGKILL of last resort.
const TERM_GRACE_SECS: u64 = 5;
/// Per-VM budget when stopping everything on app exit.
const SHUTDOWN_GRACE_SECS: u64 = QUIT_GRACE_SECS + TERM_GRACE_SECS + 2;
/// Poll interval for the adopted-VM liveness watcher.
const EXIT_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Per-VM log ring (shared between the stdout/stderr readers and `get_logs`).
type LogRing = Arc<Mutex<VecDeque<VmLogEvent>>>;

pub struct DeviceManager {
    profiles: HashMap<EmulatorId, EmulatorProfile>,
    running: HashMap<EmulatorId, VmHandle>,
    logs: HashMap<EmulatorId, LogRing>,
    port_pool: PortPool,
    config_dir: Option<PathBuf>,
    instances_root: Option<PathBuf>,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            profiles: HashMap::new(),
            running: HashMap::new(),
            logs: HashMap::new(),
            port_pool: PortPool::new(),
            config_dir: None,
            instances_root: None,
        }
    }

    /// Wire the resolved dirs (called once from the Tauri `setup` hook) and load
    /// persisted profiles (seeding on first run).
    pub fn configure(&mut self, config_dir: PathBuf, data_dir: PathBuf) {
        self.instances_root = Some(data_dir.join("instances"));
        let loaded = profiles::load(&config_dir);
        self.profiles.clear();
        for p in loaded {
            self.profiles.insert(p.id.clone(), p);
        }
        self.config_dir = Some(config_dir);
    }

    fn persist(&self) {
        if let Some(dir) = &self.config_dir {
            let list: Vec<_> = self.profiles.values().cloned().collect();
            if let Err(e) = profiles::save(dir, &list) {
                eprintln!("[studio] persist profiles failed: {e}");
            }
        }
    }

    fn profile(&self, id: &EmulatorId) -> Result<&EmulatorProfile> {
        self.profiles
            .get(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))
    }

    fn instances_root(&self) -> Result<PathBuf> {
        self.instances_root
            .clone()
            .ok_or_else(|| StudioError::Other("manager not configured".into()))
    }

    // ---- Profiles ----

    pub fn list_emulators(&self) -> Vec<EmulatorProfile> {
        let mut v: Vec<_> = self.profiles.values().cloned().collect();
        v.sort_by_key(|p| p.created_at);
        v
    }

    pub fn create_emulator(&mut self, input: CreateEmulatorInput) -> Result<EmulatorProfile> {
        let profile = EmulatorProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name: input.name,
            base_image: input.base_image,
            mem_mb: input.mem_mb.max(host::VM_RAM_FLOOR_MB),
            cpus: input.cpus.clamp(1, 8),
            render_mode: input.render_mode,
            selinux: input.selinux,
            extra_cmdline: input.extra_cmdline,
            device_identity: input.device_identity,
            created_at: now_ms(),
        };
        self.profiles.insert(profile.id.clone(), profile.clone());
        self.persist();
        Ok(profile)
    }

    /// Apply a partial edit to a saved profile.
    ///
    /// Two gates, derived from what `spawn::compose_launch` actually consumes:
    ///  - `name` / `device_identity` are metadata (neither reaches the launch)
    ///    and apply at any time.
    ///  - `mem_mb` / `cpus` / `render_mode` / `selinux` / `extra_cmdline` /
    ///    `base_image` are composed into argv+env at start, so editing them while
    ///    the VM is active is rejected rather than silently deferred — a saved
    ///    value that doesn't match the running VM is a lie the UI can't undo.
    ///
    /// `base_image` additionally needs `reprovision`, because
    /// `storage::provision_cf_dir` reuses an existing cf-dir: without erasing it
    /// the VM would keep booting the OLD image behind a changed label.
    pub fn update_emulator(
        &mut self,
        id: &EmulatorId,
        patch: UpdateEmulatorInput,
    ) -> Result<EmulatorProfile> {
        let active = self.running.get(id).map(|h| h.is_active()).unwrap_or(false);
        let current = self.profile(id)?;

        // Does the patch actually change a launch-affecting field? Comparing
        // against the current value means a no-op resubmit of the whole form
        // (the Settings tab sends every field) doesn't trip the gate.
        let touches_launch = patch
            .base_image
            .as_ref()
            .is_some_and(|v| *v != current.base_image)
            || patch.mem_mb.is_some_and(|v| v != current.mem_mb)
            || patch.cpus.is_some_and(|v| v != current.cpus)
            || patch.render_mode.is_some_and(|v| v != current.render_mode)
            || patch.selinux.is_some_and(|v| v != current.selinux)
            || patch
                .extra_cmdline
                .as_ref()
                .is_some_and(|v| *v != current.extra_cmdline);

        if active && touches_launch {
            return Err(StudioError::Running);
        }

        let base_image_changed = patch
            .base_image
            .as_ref()
            .is_some_and(|v| *v != current.base_image);
        if base_image_changed && !patch.reprovision {
            return Err(StudioError::Other(
                "changing the base image discards this VM's disk — pass reprovision to confirm"
                    .into(),
            ));
        }

        // Erase the provisioned clone so the next start re-clones the new base.
        if base_image_changed {
            if let Ok(root) = self.instances_root() {
                let layout = storage::InstanceLayout::new(&root, id);
                storage::remove_instance_dir(&layout.dir);
            }
        }

        let profile = self
            .profiles
            .get_mut(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))?;
        if let Some(v) = patch.name {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                profile.name = trimmed.to_string();
            }
        }
        if let Some(v) = patch.base_image {
            profile.base_image = v;
        }
        // Same clamps create_emulator applies, so an edit can't produce a profile
        // that create would have rejected.
        if let Some(v) = patch.mem_mb {
            profile.mem_mb = v.max(host::VM_RAM_FLOOR_MB);
        }
        if let Some(v) = patch.cpus {
            profile.cpus = v.clamp(1, 8);
        }
        if let Some(v) = patch.render_mode {
            profile.render_mode = v;
        }
        if let Some(v) = patch.selinux {
            profile.selinux = v;
        }
        if let Some(v) = patch.extra_cmdline {
            profile.extra_cmdline = v;
        }
        if let Some(v) = patch.device_identity {
            profile.device_identity = v;
        }
        let updated = profile.clone();
        self.persist();
        Ok(updated)
    }

    pub fn delete_emulator(&mut self, id: &EmulatorId) -> Result<()> {
        if self.running.get(id).map(|h| h.is_active()).unwrap_or(false) {
            return Err(StudioError::Running);
        }
        // Drop a terminal handle + free its port.
        if let Some(h) = self.running.remove(id) {
            self.port_pool.free(h.adb_port);
        }
        self.logs.remove(id);
        // Remove the provisioned instance dir (COW clone) if present.
        if let Ok(root) = self.instances_root() {
            let layout = storage::InstanceLayout::new(&root, id);
            storage::remove_instance_dir(&layout.dir);
        }
        self.profiles
            .remove(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))?;
        self.persist();
        Ok(())
    }

    // ---- Instances ----

    pub fn list_instances(&self) -> Vec<VmInstance> {
        self.running.values().map(VmHandle::to_instance).collect()
    }

    /// Provision + spawn the emulator child and start its supervisor + readiness
    /// threads. Returns the initial (Booting) instance.
    pub fn start_emulator(&mut self, app: &AppHandle, id: &EmulatorId) -> Result<VmInstance> {
        // Already active → idempotent.
        if let Some(h) = self.running.get(id) {
            if h.is_active() {
                return Ok(h.to_instance());
            }
        }
        // Clear a terminal (Stopped/Crashed) handle so we can relaunch cleanly.
        if let Some(h) = self.running.remove(id) {
            self.port_pool.free(h.adb_port);
        }

        let profile = self.profile(id)?.clone();

        // RAM budget across active VMs.
        let budget = host::ram_budget_mb();
        let used: u32 = self
            .running
            .values()
            .filter(|h| h.is_active())
            .filter_map(|h| self.profiles.get(&h.id))
            .map(|p| p.mem_mb)
            .sum();
        if used + profile.mem_mb > budget {
            return Err(StudioError::RamBudget {
                want: used + profile.mem_mb,
                cap: budget,
            });
        }

        // Resolve paths.
        let binary = paths::hvf_binary().map_err(StudioError::Other)?;
        let base_dir = paths::base_image_dir(&profile.base_image).map_err(StudioError::Other)?;
        let root = self.instances_root()?;
        let layout = storage::InstanceLayout::new(&root, id);
        std::fs::create_dir_all(&layout.dir)
            .map_err(|e| StudioError::Other(format!("mkdir instance dir: {e}")))?;

        // Provision the per-VM cf-dir (clonefile; idempotent — reuse if present).
        storage::provision_cf_dir(&base_dir, &layout.cf_dir).map_err(StudioError::Other)?;

        // ── Cross-process double-start gate ──────────────────────────────────────────────────────
        // The in-memory check at the top of this function only covers VMs THIS Studio process
        // launched. It cannot see a VM spawned by a previous Studio process — and under
        // `pnpm tauri:dev` that is the common case, since every Rust edit relaunches the app while
        // the VMM keeps running. Spawning here anyway put a second (and third) VMM on this cf-dir
        // and corrupted the guest filesystem. Consult the durable evidence before spawning.
        match session::classify(&layout.dir, &layout.cf_dir) {
            session::Liveness::Live { pid, record } => {
                return match record {
                    // Bookkeeping survived → resume the session rather than refusing. This is what
                    // the user wants after a dev reload: the running VM reappears in the UI.
                    Some(rec) => {
                        println!(
                            "[studio] adopting already-running VM for {}: pid {pid} socket {}",
                            profile.name,
                            rec.socket.display()
                        );
                        Ok(self.adopt_session(app, id, rec))
                    }
                    // A VMM holds the lock but we have no record of it (session.json deleted, or it
                    // was launched by hand / by a test script). We cannot talk to it — its socket and
                    // adb port are unknown — but we must NOT spawn alongside it.
                    None => Err(StudioError::Other(format!(
                        "an emulator is already running on this instance (pid {pid}) but the Studio \
                         has no session record for it, so it cannot be resumed. Stop that process \
                         first: kill {pid}"
                    ))),
                };
            }
            // A record with no live holder is leftover bookkeeping from a VM that died. Collect the
            // garbage (record + stale socket) so the fresh launch below starts clean.
            session::Liveness::Stale => {
                let stale_socket = session::read(&layout.dir).map(|r| r.socket);
                println!(
                    "[studio] clearing stale session for {} (VM is gone)",
                    profile.name
                );
                session::cleanup_dead(&layout.dir, stale_socket.as_deref());
            }
            session::Liveness::None => {}
        }

        // Ad-hoc sign + (for gpu3d) stage the Vulkan ICD.
        spawn::ensure_signed(&binary).map_err(StudioError::Other)?;
        // Networking is not optional for a Studio launch: the plan below allocates a host adb port
        // and sets AEMU_HOSTFWD unconditionally. A binary built without `--features net` still boots
        // and still enumerates eth0, so the degrade is invisible until adb quietly fails to connect.
        spawn::ensure_net_support(&binary).map_err(StudioError::Other)?;
        let gpu3d = matches!(profile.render_mode, RenderMode::Gpu3d);
        if gpu3d {
            // FATAL for gpu3d, not best-effort. The comment here used to say "may fall back",
            // but it does not fall back: with no ICD the loader finds no Vulkan library,
            // `stream_renderer_init` still returns **0**, gfxstream logs "Failed to initialize
            // global Vulkan emulation. Disable the Vulkan support." and the VMM then SEGFAULTS
            // a moment later. Refusing to launch turns an unattributable crash into a message
            // naming the cause. (The staging dir is derived from the binary, so this also
            // covers switching between the debug and release binaries — each needs its own
            // `target/<profile>/lib64/vulkan`.)
            spawn::ensure_vulkan_icd(&binary).map_err(|e| {
                StudioError::Other(format!(
                    "gpu3d needs a staged Vulkan ICD next to the emulator binary and staging \
                     failed: {e}\nThe host Vulkan driver is Mesa KosmicKrisp — build it with \
                     crates/aemulator-hvf/kosmickrisp/build-kosmickrisp.sh (macOS 26 or newer on \
                     Apple Silicon; there is no MoltenVK fallback any more), or pick the SwRender \
                     pipeline in Settings."
                ))
            })?;
        }

        let adb_port = self.port_pool.alloc_adb();
        let plan =
            spawn::compose_launch(binary, &profile, &layout.cf_dir, &layout.socket, adb_port);
        println!("[studio] launching {}: {}", profile.name, plan.preview());

        // Fresh socket path each run.
        let _ = std::fs::remove_file(&layout.socket);

        let child = match spawn::spawn_child(&plan, gpu3d) {
            Ok(c) => c,
            Err(e) => {
                self.port_pool.free(adb_port);
                return Err(StudioError::Other(e));
            }
        };
        let pid = child.id();

        let intentional_stop = Arc::new(AtomicBool::new(false));
        let fb_stop = Arc::new(AtomicBool::new(false));
        let log_ring: LogRing = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING_CAP)));
        self.logs.insert(id.clone(), log_ring.clone());

        let handle = VmHandle {
            id: id.clone(),
            state: VmState::Booting,
            ports: VmPorts {
                adb_host: adb_port,
                control_socket: layout.socket.display().to_string(),
            },
            display: DisplayInfo {
                width: 0,
                height: 0,
                gen: 0,
                nonzero: 0,
            },
            started_at: Some(now_ms()),
            pid,
            instance_dir: layout.dir.clone(),
            socket: layout.socket.clone(),
            live_shot: layout.live_shot.clone(),
            live_frame: layout.live_frame.clone(),
            adb_port,
            intentional_stop: intentional_stop.clone(),
            fb_stop,
            fb_running: false,
        };
        let instance = handle.to_instance();
        self.running.insert(id.clone(), handle);

        // Persist the session so a Studio restart can RESUME this VM instead of orphaning it (and
        // then double-spawning onto its disk). See `session` for the corruption this prevents.
        session::write(
            &layout.dir,
            &session::SessionRecord {
                emulator_id: id.clone(),
                pid,
                socket: layout.socket.clone(),
                cf_dir: layout.cf_dir.clone(),
                live_frame: layout.live_frame.clone(),
                adb_port,
                started_at: now_ms(),
            },
        );

        // Supervisor: stdout/stderr → vm-log + reap → vm-state.
        spawn_supervisor(
            app.clone(),
            id.clone(),
            child,
            intentional_stop.clone(),
            log_ring,
        );
        // Readiness monitor: poll the control socket → flip Booting→Ready.
        spawn_ready_monitor(app.clone(), id.clone(), layout.socket, intentional_stop);

        let _ = app.emit(
            "vm-state",
            VmStateEvent {
                id: id.clone(),
                state: VmState::Booting,
            },
        );
        Ok(instance)
    }

    /// Re-attach to a VM that is already running (spawned by a previous Studio process).
    ///
    /// We do NOT own this child, so there is no `std::process::Child` to reap and therefore no
    /// supervisor thread: stdout/stderr belonged to the process that spawned it and are gone. What we
    /// do get is everything the UI needs — control socket, adb port, frame path, pid — so input,
    /// screenshots, the live view and Stop all work on a resumed session. Liveness is watched by a
    /// pid poller instead of a `wait()`.
    fn adopt_session(
        &mut self,
        app: &AppHandle,
        id: &EmulatorId,
        rec: session::SessionRecord,
    ) -> VmInstance {
        // Keep the resumed VM's adb port out of the allocator so the next launch can't collide.
        self.port_pool.reserve(rec.adb_port);

        let log_ring: LogRing = self
            .logs
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING_CAP))))
            .clone();
        emit_log(
            app,
            id,
            &log_ring,
            format!(
                "[studio] resumed an already-running emulator (pid {}). Guest log lines from before \
                 this Studio session are unavailable — they went to the process that launched it.",
                rec.pid
            ),
            "stdout",
        );

        let intentional_stop = Arc::new(AtomicBool::new(false));
        let root = self
            .instances_root()
            .unwrap_or_else(|_| std::env::temp_dir());
        let layout = storage::InstanceLayout::new(&root, id);

        let handle = VmHandle {
            id: id.clone(),
            // Ready, not Booting: the readiness monitor below re-confirms over the socket, and a VM
            // we are resuming has by definition been up for a while.
            state: VmState::Ready,
            ports: VmPorts {
                adb_host: rec.adb_port,
                control_socket: rec.socket.display().to_string(),
            },
            display: DisplayInfo {
                width: 0,
                height: 0,
                gen: 0,
                nonzero: 0,
            },
            started_at: Some(rec.started_at),
            pid: rec.pid,
            instance_dir: layout.dir.clone(),
            socket: rec.socket.clone(),
            live_shot: layout.live_shot.clone(),
            live_frame: rec.live_frame.clone(),
            adb_port: rec.adb_port,
            intentional_stop: intentional_stop.clone(),
            fb_stop: Arc::new(AtomicBool::new(false)),
            fb_running: false,
        };
        let instance = handle.to_instance();
        self.running.insert(id.clone(), handle);

        spawn_exit_watcher(
            app.clone(),
            id.clone(),
            rec.pid,
            layout.dir,
            intentional_stop,
        );
        let _ = app.emit(
            "vm-state",
            VmStateEvent {
                id: id.clone(),
                state: VmState::Ready,
            },
        );
        instance
    }

    /// Reconcile persisted sessions with reality at Studio startup.
    ///
    /// Called once from `setup`, after profiles load. Without this, a VM that survived a Studio
    /// restart is invisible in the UI until the user presses Start (which is exactly when the old
    /// code spawned a duplicate onto its disk). Adopting up front means the UI simply shows the truth.
    pub fn reconcile_sessions(&mut self, app: &AppHandle) {
        let Ok(root) = self.instances_root() else {
            return;
        };
        let ids: Vec<EmulatorId> = self.profiles.keys().cloned().collect();
        for id in ids {
            let layout = storage::InstanceLayout::new(&root, &id);
            if !layout.dir.exists() {
                continue;
            }
            match session::classify(&layout.dir, &layout.cf_dir) {
                session::Liveness::Live {
                    pid,
                    record: Some(rec),
                } => {
                    println!("[studio] reconcile: adopting running VM {id} (pid {pid})");
                    self.adopt_session(app, &id, rec);
                }
                session::Liveness::Live { pid, record: None } => {
                    // Surfaced as a log line rather than swallowed: the user needs to know why Start
                    // will refuse, and which pid to kill.
                    eprintln!(
                        "[studio] reconcile: {id} has a running emulator (pid {pid}) with no session \
                         record — it cannot be resumed; Start will refuse until it is stopped"
                    );
                }
                session::Liveness::Stale => {
                    let sock = session::read(&layout.dir).map(|r| r.socket);
                    println!("[studio] reconcile: clearing stale session for {id}");
                    session::cleanup_dead(&layout.dir, sock.as_deref());
                }
                session::Liveness::None => {}
            }
        }
    }

    /// Stop every active VM and wait for them to exit. Called on app shutdown.
    ///
    /// Orphaning a VMM on exit is not merely untidy — the orphan keeps the cf-dir lock, so the next
    /// Studio launch cannot start that emulator at all. Waiting also lets the guest's final block
    /// flush land before the process dies.
    pub fn shutdown_all(&mut self, app: &AppHandle) {
        let ids: Vec<EmulatorId> = self
            .running
            .iter()
            .filter(|(_, h)| h.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        println!(
            "[studio] shutting down {} running VM(s) before exit",
            ids.len()
        );
        let mut pids = Vec::new();
        for id in &ids {
            if let Some(h) = self.running.get(id) {
                pids.push(h.pid);
            }
            let _ = self.stop_emulator(app, id);
        }
        // Bounded wait: a clean guest teardown (final fdatasync of a multi-GB userdata) can take a
        // few seconds, and cutting it short is what we are trying to avoid.
        for pid in pids {
            session::wait_for_exit(pid, Duration::from_secs(SHUTDOWN_GRACE_SECS));
        }
    }

    pub fn stop_emulator(&mut self, app: &AppHandle, id: &EmulatorId) -> Result<()> {
        let handle = match self.running.get_mut(id) {
            Some(h) => h,
            None => return Ok(()), // already gone
        };
        handle.state = VmState::Stopping;
        handle.intentional_stop.store(true, Ordering::Release);
        handle.fb_stop.store(true, Ordering::Release);
        let pid = handle.pid;
        let socket = handle.socket.clone();
        let instance_dir = handle.instance_dir.clone();
        let _ = app.emit(
            "vm-state",
            VmStateEvent {
                id: id.clone(),
                state: VmState::Stopping,
            },
        );

        // Escalation ladder: `quit` → SIGTERM → SIGKILL, each stage WAITING for the process to
        // actually exit rather than sleeping a fixed interval.
        //
        // The old version slept 400 ms after `quit`, then SIGTERM'd, then slept 4 s and SIGKILL'd
        // unconditionally. Two problems, both of which cost real data:
        //
        //   1. A clean teardown fdatasyncs a multi-GB userdata image. That can exceed 400 ms, so the
        //      signal arrived *during* the flush — the ladder was cutting short the very step that
        //      makes the disk consistent.
        //   2. It escalated even when the process had already exited, and it never checked, so the
        //      log gave no evidence of which stage actually did the work.
        //
        // Now each stage is skipped entirely if the process is already gone, and SIGKILL is a genuine
        // last resort that is reported when it happens (it means the VMM hung — worth knowing).
        let app_for_thread = app.clone();
        let id_for_thread = id.clone();
        std::thread::spawn(move || {
            let _ = control::send(&socket, &control::Verb::Quit);
            let mut how = "quit";
            if !session::wait_for_exit(pid, Duration::from_secs(QUIT_GRACE_SECS)) {
                how = "SIGTERM";
                // Safety: signalling a pid we spawned (or adopted); SIGTERM is catchable/normal.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                if !session::wait_for_exit(pid, Duration::from_secs(TERM_GRACE_SECS)) {
                    how = "SIGKILL";
                    eprintln!(
                        "[studio] VM {id_for_thread} ignored quit + SIGTERM after {}s — SIGKILL. \
                         The guest filesystem relies on its journal to recover from this.",
                        QUIT_GRACE_SECS + TERM_GRACE_SECS
                    );
                    // Safety: last-resort teardown of an unresponsive VMM.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    session::wait_for_exit(pid, Duration::from_secs(2));
                }
            }
            println!("[studio] VM {id_for_thread} stopped via {how}");

            // Drop the session record only once the process is really gone; a record removed while
            // the VMM still holds the cf-dir lock would make the next Start refuse with "no session
            // record" instead of resuming.
            let _ = std::fs::remove_file(&socket);
            session::clear(&instance_dir);
            let _ = app_for_thread.emit(
                "vm-state",
                VmStateEvent {
                    id: id_for_thread.clone(),
                    state: VmState::Stopped,
                },
            );
        });

        // NOTE: state stays `Stopping` here, and the adb port stays reserved. Both used to be
        // released synchronously — the handle was marked `Stopped` while the VMM was still very much
        // alive, so a user who clicked Stop then Start got straight past the double-start guard and
        // spawned a SECOND VMM onto the same disk. The teardown thread above flips to `Stopped` when
        // the process has actually exited; `reap_stopped` then frees the port.
        Ok(())
    }

    /// Free resources for VMs whose teardown has completed. Called when a stop finishes, so the adb
    /// port returns to the pool only after the process that was using it is gone.
    pub fn reap_stopped(&mut self, id: &EmulatorId) {
        let Some(h) = self.running.get(id) else {
            return;
        };
        if matches!(h.state, VmState::Stopped | VmState::Crashed) && !session::pid_alive(h.pid) {
            let port = h.adb_port;
            self.port_pool.free(port);
        }
    }

    pub fn get_status(&self, id: &EmulatorId) -> Result<VmInstance> {
        if let Some(h) = self.running.get(id) {
            Ok(h.to_instance())
        } else {
            let profile = self.profile(id)?;
            Ok(VmInstance {
                id: profile.id.clone(),
                state: VmState::Stopped,
                ports: VmPorts {
                    adb_host: 0,
                    control_socket: String::new(),
                },
                display: DisplayInfo {
                    width: 0,
                    height: 0,
                    gen: 0,
                    nonzero: 0,
                },
                started_at: None,
                pid: None,
            })
        }
    }

    pub fn get_logs(&self, id: &EmulatorId) -> Vec<VmLogEvent> {
        self.logs
            .get(id)
            .map(|r| {
                r.lock()
                    .map(|q| q.iter().cloned().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Resolve the live control socket for a running VM (control/screenshot).
    pub fn socket_of(&self, id: &EmulatorId) -> Result<PathBuf> {
        let h = self
            .running
            .get(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))?;
        if !h.is_active() {
            return Err(StudioError::Other(format!("{id} is not running")));
        }
        Ok(h.socket.clone())
    }

    /// Socket + live-shot path (framebuffer + screenshot).
    pub fn socket_and_shot(&self, id: &EmulatorId) -> Result<(PathBuf, PathBuf)> {
        let h = self
            .running
            .get(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))?;
        Ok((h.socket.clone(), h.live_shot.clone()))
    }

    /// Start the framebuffer poll thread for a VM (guarded against double-start).
    pub fn start_framebuffer(
        &mut self,
        app: &AppHandle,
        id: &EmulatorId,
        chan: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
    ) -> Result<()> {
        let h = self
            .running
            .get_mut(id)
            .ok_or_else(|| StudioError::NotFound(id.clone()))?;
        // ACTUALLY honor the double-start guard. This used to set `fb_running = true` and never
        // read it, despite the doc claiming otherwise — so every extra call spawned another poll
        // thread. React re-invokes effects on mount in dev, so the Display tab reliably started
        // two, and they raced each other over the single capture file (each one's truncate was the
        // other's 0-byte read). Atomic writes fix the corruption; this stops the duplicate work.
        if h.fb_running {
            return Ok(());
        }
        h.fb_stop.store(false, Ordering::Release);
        h.fb_running = true;
        framebuffer::spawn(framebuffer::FbPoll {
            socket: h.socket.clone(),
            live_shot: h.live_frame.clone(),
            stop: h.fb_stop.clone(),
            chan,
            app: app.clone(),
            id: id.clone(),
        });
        Ok(())
    }

    pub fn stop_framebuffer(&mut self, id: &EmulatorId) -> Result<()> {
        if let Some(h) = self.running.get_mut(id) {
            h.fb_stop.store(true, Ordering::Release);
            h.fb_running = false;
        }
        Ok(())
    }

    // ---- Devices / storage / host ----

    pub fn device_config(&self, id: &EmulatorId) -> Result<DeviceConfig> {
        // Probe the binary rather than assuming: the Hardware tab must not claim real NAT from a
        // build that only has the stub NIC. An unresolvable binary reports the conservative answer.
        let net_available = paths::hvf_binary()
            .map(|b| spawn::binary_has_net(&b))
            .unwrap_or(false);
        Ok(device_config_for(self.profile(id)?, net_available))
    }

    pub fn list_storage(&self) -> Vec<StorageVolume> {
        let root = match self.instances_root() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<_> = self
            .profiles
            .values()
            .map(|p| {
                let layout = storage::InstanceLayout::new(&root, &p.id);
                let base = paths::base_image_dir(&p.base_image)
                    .map(|d| d.join("img"))
                    .unwrap_or_default();
                storage::volume_for(&p.id, &layout.dir, &layout.cf_dir, &base)
            })
            .collect();
        out.sort_by(|a, b| a.emulator_id.cmp(&b.emulator_id));
        out
    }

    pub fn list_base_images(&self) -> Vec<ImageCatalogEntry> {
        host::base_images()
    }

    pub fn host_capabilities(&self) -> HostCaps {
        host::capabilities()
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Background threads (own an AppHandle; re-lock the manager as needed) ----

/// Supervisor: stream stdout/stderr as `vm-log`, reap the child on exit, and emit
/// the terminal `vm-state` (Stopped when intentional, Crashed otherwise).
fn spawn_supervisor(
    app: AppHandle,
    id: EmulatorId,
    mut child: Child,
    intentional: Arc<AtomicBool>,
    ring: LogRing,
) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // stderr on its own thread.
    if let Some(err) = stderr {
        let app_e = app.clone();
        let id_e = id.clone();
        let ring_e = ring.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().map_while(std::result::Result::ok) {
                emit_log(&app_e, &id_e, &ring_e, line, "stderr");
            }
        });
    }

    std::thread::Builder::new()
        .name("vm-supervisor".into())
        .spawn(move || {
            if let Some(out) = stdout {
                let reader = BufReader::new(out);
                for line in reader.lines().map_while(std::result::Result::ok) {
                    emit_log(&app, &id, &ring, line, "stdout");
                }
            }
            // stdout closed → the child is exiting. Reap it.
            let status = child.wait();
            let was_intentional = intentional.load(Ordering::Acquire);
            let final_state = if was_intentional {
                VmState::Stopped
            } else {
                match status {
                    Ok(s) if s.success() => VmState::Stopped,
                    _ => VmState::Crashed,
                }
            };

            // Update the stored handle + emit, then release the session.
            let mut instance_dir = None;
            if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(h) = m.running.get_mut(&id) {
                    h.state = final_state;
                    h.fb_stop.store(true, Ordering::Release);
                    instance_dir = Some(h.instance_dir.clone());
                }
                // Frees the adb port now that the process holding it is reaped.
                m.reap_stopped(&id);
            }
            // Clear the session record on EVERY exit, crash included. A record left behind by a
            // crashed VM is what `Liveness::Stale` exists to clean up, but doing it here means the
            // next Start sees a clean instance instead of relying on the reconcile sweep.
            if let Some(dir) = instance_dir {
                session::clear(&dir);
            }
            let _ = app.emit(
                "vm-state",
                VmStateEvent {
                    id: id.clone(),
                    state: final_state,
                },
            );
            if final_state == VmState::Crashed {
                let code = status
                    .ok()
                    .and_then(|s| s.code())
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                let _ = app.emit(
                    "vm-error",
                    VmErrorEvent {
                        id,
                        message: format!("emulator process exited unexpectedly (status {code})"),
                    },
                );
            }
        })
        .ok();
}

/// Liveness watcher for an **adopted** VM — one this Studio process did not spawn.
///
/// A spawned child is reaped by `spawn_supervisor`'s `child.wait()`, which is both the exit signal
/// and the exit status. For an adopted VM we have neither: it is not our child, so `wait()` is not
/// available and its pipes belong to a process that is gone. Polling `kill(pid, 0)` is the portable
/// substitute. It cannot distinguish a clean exit from a crash — hence `Stopped` unless the user
/// asked for the stop — so it deliberately reports less than the supervisor rather than guessing.
fn spawn_exit_watcher(
    app: AppHandle,
    id: EmulatorId,
    pid: u32,
    instance_dir: PathBuf,
    intentional: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("vm-exit-watch".into())
        .spawn(move || {
            while session::pid_alive(pid) {
                std::thread::sleep(EXIT_WATCH_INTERVAL);
            }
            let was_intentional = intentional.load(Ordering::Acquire);
            let final_state = if was_intentional {
                VmState::Stopped
            } else {
                VmState::Crashed
            };

            if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(h) = m.running.get_mut(&id) {
                    h.state = final_state;
                    h.fb_stop.store(true, Ordering::Release);
                }
                m.reap_stopped(&id);
            }
            session::clear(&instance_dir);
            let _ = app.emit(
                "vm-state",
                VmStateEvent {
                    id: id.clone(),
                    state: final_state,
                },
            );
            if final_state == VmState::Crashed {
                let _ = app.emit(
                    "vm-error",
                    VmErrorEvent {
                        id,
                        message: "the resumed emulator process exited unexpectedly".into(),
                    },
                );
            }
        })
        .ok();
}

fn emit_log(app: &AppHandle, id: &EmulatorId, ring: &LogRing, line: String, stream: &str) {
    let ev = VmLogEvent {
        id: id.clone(),
        line,
        stream: stream.to_string(),
        ts: now_ms(),
    };
    if let Ok(mut q) = ring.lock() {
        if q.len() >= LOG_RING_CAP {
            q.pop_front();
        }
        q.push_back(ev.clone());
    }
    let _ = app.emit("vm-log", ev);
}

/// Readiness monitor: poll the control socket until it answers `info`, then flip
/// Booting→Ready + record the scanout geometry. After Ready, refresh the display
/// counters every couple seconds so the UI can show boot progress (rising
/// nonzero) until the VM stops.
fn spawn_ready_monitor(
    app: AppHandle,
    id: EmulatorId,
    socket: PathBuf,
    intentional: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("vm-ready".into())
        .spawn(move || {
            let mut ready = false;
            loop {
                if intentional.load(Ordering::Acquire) {
                    return;
                }
                // Bail if the handle is gone or terminal.
                if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                    let terminal = {
                        let m = state.lock().unwrap_or_else(|e| e.into_inner());
                        match m.running.get(&id) {
                            None => true,
                            Some(h) => matches!(h.state, VmState::Stopped | VmState::Crashed),
                        }
                    };
                    if terminal {
                        return;
                    }
                }

                if let Ok(info) = control::info(&socket) {
                    if let Some(state) = app.try_state::<Mutex<DeviceManager>>() {
                        let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(h) = m.running.get_mut(&id) {
                            h.display = DisplayInfo {
                                width: info.width,
                                height: info.height,
                                gen: info.gen,
                                nonzero: info.nonzero,
                            };
                            if !ready && h.state == VmState::Booting {
                                h.state = VmState::Ready;
                            }
                        }
                    }
                    if !ready {
                        ready = true;
                        let _ = app.emit(
                            "vm-state",
                            VmStateEvent {
                                id: id.clone(),
                                state: VmState::Ready,
                            },
                        );
                    }
                    std::thread::sleep(Duration::from_secs(2));
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .ok();
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager holding one seeded profile and NO resolved dirs — `persist()` and
    /// the reprovision path both no-op without a config dir, so these tests touch
    /// no filesystem.
    fn mgr() -> (DeviceManager, EmulatorId) {
        let profile = profiles::seed().remove(0);
        let id = profile.id.clone();
        let mut m = DeviceManager::new();
        m.profiles.insert(id.clone(), profile);
        (m, id)
    }

    /// Mark the profile as running. `VmHandle` is plain data — no child process is
    /// needed to exercise the is_active() gate.
    fn mark_active(m: &mut DeviceManager, id: &EmulatorId) {
        m.running.insert(
            id.clone(),
            VmHandle {
                id: id.clone(),
                state: VmState::Ready,
                ports: VmPorts {
                    adb_host: 5556,
                    control_socket: String::new(),
                },
                display: DisplayInfo {
                    width: 0,
                    height: 0,
                    gen: 0,
                    nonzero: 0,
                },
                started_at: Some(0),
                pid: 0,
                instance_dir: PathBuf::new(),
                socket: PathBuf::new(),
                live_shot: PathBuf::new(),
                live_frame: PathBuf::new(),
                adb_port: 5556,
                intentional_stop: Arc::new(AtomicBool::new(false)),
                fb_stop: Arc::new(AtomicBool::new(false)),
                fb_running: false,
            },
        );
    }

    #[test]
    fn metadata_applies_while_running() {
        let (mut m, id) = mgr();
        mark_active(&mut m, &id);
        let out = m
            .update_emulator(
                &id,
                UpdateEmulatorInput {
                    name: Some("Renamed".into()),
                    ..Default::default()
                },
            )
            .expect("name is metadata — must apply while running");
        assert_eq!(out.name, "Renamed");
    }

    #[test]
    fn launch_fields_rejected_while_running() {
        let (mut m, id) = mgr();
        mark_active(&mut m, &id);
        let err = m
            .update_emulator(
                &id,
                UpdateEmulatorInput {
                    cpus: Some(8),
                    ..Default::default()
                },
            )
            .expect_err("cpus is composed into argv at start — must be rejected while running");
        assert!(matches!(err, StudioError::Running));
    }

    #[test]
    fn unchanged_launch_fields_do_not_trip_the_gate() {
        // The Settings tab may submit the whole form; resubmitting the CURRENT
        // launch values alongside a name edit must not read as a launch change.
        let (mut m, id) = mgr();
        let current = m.profile(&id).unwrap().clone();
        mark_active(&mut m, &id);
        m.update_emulator(
            &id,
            UpdateEmulatorInput {
                name: Some("Still fine".into()),
                mem_mb: Some(current.mem_mb),
                cpus: Some(current.cpus),
                render_mode: Some(current.render_mode),
                selinux: Some(current.selinux),
                extra_cmdline: Some(current.extra_cmdline.clone()),
                base_image: Some(current.base_image.clone()),
                ..Default::default()
            },
        )
        .expect("a no-op resubmit of launch fields is not a change");
    }

    #[test]
    fn launch_fields_apply_when_stopped() {
        let (mut m, id) = mgr();
        let out = m
            .update_emulator(
                &id,
                UpdateEmulatorInput {
                    cpus: Some(6),
                    render_mode: Some(RenderMode::Gpu3d),
                    ..Default::default()
                },
            )
            .expect("stopped VM accepts launch edits");
        assert_eq!(out.cpus, 6);
        assert_eq!(out.render_mode, RenderMode::Gpu3d);
    }

    #[test]
    fn base_image_change_requires_reprovision() {
        let (mut m, id) = mgr();
        m.update_emulator(
            &id,
            UpdateEmulatorInput {
                base_image: Some("other".into()),
                ..Default::default()
            },
        )
        .expect_err("changing the base discards the disk — must be confirmed");

        let out = m
            .update_emulator(
                &id,
                UpdateEmulatorInput {
                    base_image: Some("other".into()),
                    reprovision: true,
                    ..Default::default()
                },
            )
            .expect("reprovision confirms the disk erase");
        assert_eq!(out.base_image, "other");
    }

    #[test]
    fn clamps_match_create_emulator() {
        let (mut m, id) = mgr();
        let out = m
            .update_emulator(
                &id,
                UpdateEmulatorInput {
                    mem_mb: Some(512),
                    cpus: Some(64),
                    name: Some("   ".into()), // whitespace-only is ignored, not applied
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(out.mem_mb, host::VM_RAM_FLOOR_MB);
        assert_eq!(out.cpus, 8);
        assert_eq!(out.name, profiles::seed()[0].name);
    }
}
