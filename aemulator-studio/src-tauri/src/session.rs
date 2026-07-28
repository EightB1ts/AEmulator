//! Durable VM **session records** — how the Studio survives its own restarts without orphaning or
//! duplicating a running emulator.
//!
//! ## The failure this exists to prevent
//!
//! The Studio's double-start guard used to live only in `DeviceManager.running`, an in-memory map.
//! But the Studio is not the longest-lived process in the system — a spawned VMM is. Under
//! `pnpm tauri:dev` every Rust edit relaunches the Studio, and each relaunch started with an EMPTY
//! map while the previously spawned VMM kept running, unsupervised and unreferenced. Pressing Start
//! then spawned a *second* VMM on the same cf-dir. In one session that reached **three** live VMMs
//! plus a fourth from a test script, all writing the same `userdata.img`:
//!
//! ```text
//! EXT4-fs error (device vda9): ext4_lookup: inode #265170: iget: checksum invalid
//! Kernel panic - not syncing: EXT4-fs (device vda9): panic forced after error
//! ```
//!
//! It also made the display look broken in a way that pointed nowhere near the cause: three
//! processes had bound the same control-socket path (last binder wins), so the frame *generation*
//! the viewer polled came from one VM while the *pixels* came from another.
//!
//! ## Design
//!
//! Two independent mechanisms, deliberately not one:
//!
//! * **The cf-dir `flock`** (in the emulator, `aemulator_hvf::cf_lock`) is the *enforcement*. It is
//!   authoritative because it lives in the process that does the writing and the kernel releases it
//!   on exit — including SIGKILL. No launcher can defeat it, including a launcher that has forgotten
//!   the VM exists.
//! * **This session record** is the *bookkeeping*. `flock` can tell you "someone holds it"; it
//!   cannot tell you which emulator id, which control socket, or which adb port that someone owns.
//!   Without those the Studio can detect a conflict but not RESUME the session, which is what the
//!   user actually wants after a dev reload.
//!
//! The record is treated as a cache that may be stale or absent, never as truth: liveness is always
//! re-derived from the pid and the lock. A record whose process is gone is garbage-collected.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the instance dir.
pub const SESSION_FILE: &str = "session.json";
/// Lock file name inside the cf-dir. MUST match `aemulator_hvf::cf_lock::LOCK_FILE` — the Studio
/// probes the same file the emulator locks, and they are separate crates by design (the Studio must
/// not pull in the VMM's dependency tree), so this constant is a deliberate, tested mirror.
pub const CF_LOCK_FILE: &str = ".aemu-instance.lock";

/// What the Studio knew about a VM at spawn time. Everything needed to re-attach to it later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub emulator_id: String,
    pub pid: u32,
    pub socket: PathBuf,
    pub cf_dir: PathBuf,
    pub live_frame: PathBuf,
    pub adb_port: u16,
    /// Epoch millis, for the resumed VM's uptime display.
    ///
    /// Framebuffer geometry is deliberately NOT recorded: it is authoritative only from the running
    /// VM's `info`/`frame` reply, and a cached copy would go stale the moment the guest rotates or a
    /// profile edit changes the display.
    pub started_at: i64,
}

/// Liveness verdict for an instance dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// No session record on disk and no lock held — nothing is running.
    None,
    /// A VMM is live. `record` is present when the bookkeeping survived (the normal case).
    Live { pid: u32, record: Option<SessionRecord> },
    /// A record exists but its process is gone — stale bookkeeping to garbage-collect.
    Stale,
}

/// Persist the session record. Best-effort: a failure here degrades resume-after-reload, it must
/// never block starting a VM (the `flock` still prevents the dangerous case).
pub fn write(instance_dir: &Path, rec: &SessionRecord) {
    let path = instance_dir.join(SESSION_FILE);
    match serde_json::to_vec_pretty(rec) {
        Ok(bytes) => {
            // Temp + rename: a reader must never observe a half-written record.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Err(e) => eprintln!("[studio] session record encode failed: {e}"),
    }
}

/// Read the session record, if any (absent / unparseable → `None`).
pub fn read(instance_dir: &Path) -> Option<SessionRecord> {
    let bytes = std::fs::read(instance_dir.join(SESSION_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Drop the session record (VM stopped, or the record was stale).
pub fn clear(instance_dir: &Path) {
    let _ = std::fs::remove_file(instance_dir.join(SESSION_FILE));
}

/// Is `pid` a live process? `kill(pid, 0)` — succeeds when it exists and we may signal it, and
/// reports `EPERM` (not `ESRCH`) when it exists but is owned by someone else, which still means
/// "alive". Signal 0 delivers nothing.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: `kill` with signal 0 performs an existence/permission check and no delivery.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Does a live VMM hold the cf-dir lock? Returns the holder pid when the lock file records one.
///
/// Probe-only — takes the lock and releases it immediately, so this must never be the enforcement
/// point (that is the emulator's own `acquire`, held for its lifetime). Used to detect a running VM
/// whose session record was lost.
pub fn cf_lock_holder(cf_dir: &Path) -> Option<Option<u32>> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    let path = cf_dir.join(CF_LOCK_FILE);
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).ok()?;
    // Safety: valid fd owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // Safety: same fd; release what we just took so we don't block the next spawn.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    let mut s = String::new();
    let pid = file.read_to_string(&mut s).ok().and_then(|_| s.trim().parse().ok());
    Some(pid)
}

/// Classify an instance: is a VMM running for it, and do we still have its bookkeeping?
///
/// Order matters. The lock is checked FIRST because it is the authoritative signal — a VM whose
/// `session.json` was deleted is still dangerous to double-start, and reporting it as `None` is the
/// bug this whole module exists to prevent.
pub fn classify(instance_dir: &Path, cf_dir: &Path) -> Liveness {
    let record = read(instance_dir);

    if let Some(holder) = cf_lock_holder(cf_dir) {
        // Lock held → a VMM is alive. Prefer the lock file's pid; fall back to the record's.
        let pid = holder.or_else(|| record.as_ref().map(|r| r.pid)).unwrap_or(0);
        // Only trust the record if it describes the SAME process now holding the lock; otherwise it
        // belongs to an older VM and its socket/port would be wrong.
        let record = record.filter(|r| pid == 0 || r.pid == pid);
        return Liveness::Live { pid, record };
    }

    match record {
        // Lock free but a record exists → the VMM died (crash, SIGKILL, host reboot). Note we do
        // NOT trust `pid_alive` alone here: pids are recycled, so a live-but-unrelated process
        // could masquerade as our VM. The lock being free is the decisive evidence it is gone.
        Some(_) => Liveness::Stale,
        None => Liveness::None,
    }
}

/// Garbage-collect a dead session: drop the record and any stale control socket.
///
/// The socket file matters. A unix-domain socket left on disk by a killed VMM still `connect()`s far
/// enough to be confusing (`ECONNREFUSED` rather than `ENOENT`), and a stale one at the path a new
/// VM wants to bind can make `bind` fail outright.
pub fn cleanup_dead(instance_dir: &Path, socket: Option<&Path>) {
    clear(instance_dir);
    if let Some(s) = socket {
        let _ = std::fs::remove_file(s);
    }
}

/// Wait (bounded) for `pid` to exit. Returns true if it is gone by the deadline.
pub fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    !pid_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("aemu-session-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rec(pid: u32) -> SessionRecord {
        SessionRecord {
            emulator_id: "id-1".into(),
            pid,
            socket: PathBuf::from("/tmp/aemu-id-1.sock"),
            cf_dir: PathBuf::from("/tmp/cf"),
            live_frame: PathBuf::from("/tmp/live.raw"),
            adb_port: 5556,
            started_at: 1,
        }
    }

    /// The Studio probes the very file the emulator locks, but the two live in separate crates (the
    /// Studio must not pull in the VMM's dependency tree), so the name is a hand-mirrored constant.
    /// A silent rename on either side would make the probe always report "free" — and the
    /// double-start protection would evaporate without a single test failing. So assert the mirror
    /// against the emulator's source directly.
    #[test]
    fn cf_lock_file_name_mirrors_the_emulator() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/aemulator-hvf/src/cf_lock.rs");
        let text = match std::fs::read_to_string(&src) {
            Ok(t) => t,
            // Don't fail the Studio build in a checkout without the emulator crate.
            Err(_) => return,
        };
        let expected = format!(r#"pub const LOCK_FILE: &str = "{CF_LOCK_FILE}";"#);
        assert!(
            text.contains(&expected),
            "emulator cf_lock::LOCK_FILE no longer matches session::CF_LOCK_FILE ({CF_LOCK_FILE:?}) \
             — the Studio would probe the wrong path and stop detecting running VMs"
        );
    }

    #[test]
    fn record_roundtrips_and_clears() {
        let dir = tmp("roundtrip");
        assert!(read(&dir).is_none());
        write(&dir, &rec(4242));
        let got = read(&dir).expect("record must read back");
        assert_eq!(got.pid, 4242);
        assert_eq!(got.adb_port, 5556);
        clear(&dir);
        assert!(read(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Our own pid is alive; pid 1 exists but is root-owned (EPERM → still alive); a very high pid
    /// almost certainly does not exist.
    #[test]
    fn pid_liveness_handles_eperm_and_absent() {
        assert!(pid_alive(std::process::id()));
        assert!(pid_alive(1), "pid 1 exists (EPERM must count as alive)");
        assert!(!pid_alive(0));
        assert!(!pid_alive(0x7FFF_FFFE));
    }

    /// A record with no lock held is STALE, not Live — the VMM died. Getting this backwards would
    /// leave the user unable to start a VM that is not actually running.
    #[test]
    fn record_without_lock_is_stale() {
        let dir = tmp("stale");
        let cf = dir.join("cf");
        std::fs::create_dir_all(&cf).unwrap();
        write(&dir, &rec(std::process::id())); // pid IS alive — the lock decides, not the pid
        assert_eq!(classify(&dir, &cf), Liveness::Stale);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No record and no lock → nothing running.
    #[test]
    fn empty_instance_is_none() {
        let dir = tmp("empty");
        let cf = dir.join("cf");
        std::fs::create_dir_all(&cf).unwrap();
        assert_eq!(classify(&dir, &cf), Liveness::None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A held lock means Live even when the session record is missing entirely — the case that
    /// caused the multi-VMM corruption (Studio restarted, bookkeeping gone, VM still running).
    #[test]
    fn held_lock_without_record_is_live() {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        let dir = tmp("lockonly");
        let cf = dir.join("cf");
        std::fs::create_dir_all(&cf).unwrap();

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(cf.join(CF_LOCK_FILE))
            .unwrap();
        writeln!(f, "31337").unwrap();
        f.flush().unwrap();
        // Safety: valid fd owned by `f`, held for the duration of the assertion below.
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);

        match classify(&dir, &cf) {
            Liveness::Live { pid, record } => {
                assert_eq!(pid, 31337, "holder pid comes from the lock file");
                assert!(record.is_none(), "no session record was written");
            }
            other => panic!("expected Live, got {other:?}"),
        }

        drop(f);
        assert_eq!(classify(&dir, &cf), Liveness::None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record left behind by an OLDER VM must not be paired with a DIFFERENT live holder — its
    /// socket and adb port would be wrong, so the Studio would adopt a VM it cannot talk to.
    #[test]
    fn record_from_a_different_pid_is_not_adopted() {
        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        let dir = tmp("mismatch");
        let cf = dir.join("cf");
        std::fs::create_dir_all(&cf).unwrap();
        write(&dir, &rec(11111)); // bookkeeping from a previous VM

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(cf.join(CF_LOCK_FILE))
            .unwrap();
        writeln!(f, "22222").unwrap(); // a DIFFERENT process holds it now
        f.flush().unwrap();
        // Safety: valid fd owned by `f`.
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);

        match classify(&dir, &cf) {
            Liveness::Live { pid, record } => {
                assert_eq!(pid, 22222);
                assert!(record.is_none(), "mismatched record must be discarded, not adopted");
            }
            other => panic!("expected Live, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
