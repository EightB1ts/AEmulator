//! Single-writer lock on a Cuttlefish cf-dir.
//!
//! **Why this exists.** The VMM mutates `img/` IN PLACE — `userdata.img`, `misc.img`,
//! `metadata.img`, `frp.img` are attached writable, and `super.raw.img` is prepped on boot. Two VMMs
//! on one cf-dir are therefore two kernels writing the same ext4/f2fs block device with no
//! coordination, and the guest notices within a minute:
//!
//! ```text
//! EXT4-fs error (device vda9): ext4_lookup:1819: inode #265170: iget: checksum invalid
//! Kernel panic - not syncing: EXT4-fs (device vda9): panic forced after error
//! ```
//!
//! That was not hypothetical. The Studio's double-start guard lived only in its in-memory
//! `running` map, so every `tauri:dev` rebuild relaunched the Studio with an empty map while the
//! previously spawned VMM kept running as an orphan — pressing Start again spawned a second (then
//! third) VMM on the same disk. It also poisoned diagnosis for an afternoon: three processes had
//! bound the same control-socket path (last binder wins), so the frame generation the viewer polled
//! came from one VM while the pixels came from another.
//!
//! A guard in the launcher cannot fix this, because the launcher is exactly the thing that
//! restarts. The lock has to live where the damage happens (the cf-dir) and be enforced by the
//! process doing the writing, so ANY launcher — Studio, a shell, a test script — is covered.
//!
//! **Mechanism.** `flock(LOCK_EX|LOCK_NB)` on `<cf_dir>/.aemu-instance.lock`. flock is tied to the
//! open file description, so the kernel releases it on process exit — including SIGKILL and a panic,
//! which a pidfile-with-liveness-check scheme handles only by heuristic. The holder's pid is written
//! into the file purely so the refusal message can name it.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Lock file name inside the cf-dir.
pub const LOCK_FILE: &str = ".aemu-instance.lock";

/// Acquire the cf-dir's exclusive lock, or fail with a message naming the current holder.
///
/// The returned `File` MUST stay alive for as long as the VM runs. Callers that want the lock for
/// the whole process lifetime should `std::mem::forget` it (see [`hold_for_process_lifetime`]) —
/// dropping the `File` closes the fd and releases the lock while the guest is still writing.
pub fn acquire(cf_dir: &Path) -> Result<std::fs::File, String> {
    let path = cf_dir.join(LOCK_FILE);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;

    // Safety: `file` owns a valid fd for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let holder = read_pid(&mut file)
            .map(|pid| format!("pid {pid}"))
            .unwrap_or_else(|| "another process".to_string());
        return Err(format!(
            "another emulator ({holder}) is already running on this instance:\n  {}\n\
             Two VMMs on one cf-dir corrupt the guest filesystem (both write userdata.img), so this \
             one is refusing to boot. Stop the running emulator first.\n  ({err})",
            cf_dir.display()
        ));
    }

    // Record who holds it — for the refusal message above, not for enforcement.
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(file)
}

/// Acquire and hold the lock until the process exits.
///
/// Deliberately leaks the fd: the lock must outlive every scope in the boot path, and the kernel
/// drops it when the process dies (normally, on panic, or on SIGKILL). Leaking one fd for the
/// process lifetime is the point, not an oversight.
pub fn hold_for_process_lifetime(cf_dir: &Path) -> Result<(), String> {
    let file = acquire(cf_dir)?;
    std::mem::forget(file);
    Ok(())
}

/// Is the cf-dir currently locked by a live VMM? Returns the holder's pid when known.
///
/// Probe-only: takes the lock and immediately releases it, so it must never be used as the
/// enforcement point (that is [`acquire`], inside the process that does the writing). Launchers use
/// this to report "already running" without racing the child they are about to spawn.
pub fn probe(cf_dir: &Path) -> Option<Option<u32>> {
    let path = cf_dir.join(LOCK_FILE);
    let mut file = OpenOptions::new().read(true).write(true).open(&path).ok()?;
    // Safety: valid fd owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // We got it → nobody holds it. Release immediately.
        // Safety: same fd.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    Some(read_pid(&mut file))
}

fn read_pid(file: &mut std::fs::File) -> Option<u32> {
    let mut s = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: a second acquirer is refused while the first holds it, and the refusal names
    /// the holder's pid so an operator can act on it.
    #[test]
    fn second_acquire_is_refused_while_first_is_held() {
        let dir = std::env::temp_dir().join(format!("aemu-cflock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let held = acquire(&dir).expect("first acquire must succeed");
        assert!(probe(&dir).is_some(), "probe must see the lock as held");

        // A second acquire from THIS process still fails: flock is per open file description, so a
        // fresh `open` + `LOCK_NB` conflicts even within one process.
        let err = acquire(&dir).expect_err("second acquire must be refused");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "refusal should name the holder pid, got: {err}"
        );
        assert!(err.contains("corrupt"), "refusal should say why, got: {err}");

        drop(held);
        assert!(probe(&dir).is_none(), "probe must see the lock as free after release");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
