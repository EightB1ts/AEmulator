//! Per-VM cf-dir provisioning (APFS `clonefile` copy-on-write of a base image),
//! disk-usage accounting, and snapshots.
//!
//! Provisioning is REAL: each emulator gets its OWN cf-dir cloned from the base
//! image, because the binary mutates `img/` in place (userdata, misc, metadata,
//! frp, and the prep of super.raw.img) — two VMs must never share a cf-dir. `cp -c -R`
//! performs the clone via `clonefile(2)`, so the multi-GB image costs only its
//! private write delta on disk. Snapshot CRUD is still a Phase-2 placeholder.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::host;
use crate::types::{EmulatorId, PartitionUsage, Snapshot, StorageVolume};

/// Cuttlefish writable partitions (the ones a snapshot would clone). Sizes are
/// the on-disk file sizes when provisioned; used for the writable-breakdown UI.
pub const WRITABLE_PARTITIONS: &[&str] = &["userdata.img", "misc.img", "metadata.img", "frp.img"];

/// Provision an instance cf-dir by APFS clonefile of the base image directory.
/// Idempotent: if `cf_dir` already exists it is reused (so per-VM userdata
/// persists across restarts). Returns the provisioned cf-dir path.
pub fn provision_cf_dir(base_dir: &Path, cf_dir: &Path) -> Result<(), String> {
    if cf_dir.exists() {
        return Ok(()); // already provisioned — keep the VM's mutated state.
    }
    if !base_dir.exists() {
        return Err(format!("base image not found: {}", base_dir.display()));
    }
    if let Some(parent) = cf_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }

    // `cp -c -R` uses clonefile(2) on APFS (instant COW). Fall back to a plain
    // recursive copy on failure (non-APFS / cross-volume).
    let cow = Command::new("cp")
        .args([
            "-c",
            "-R",
            &base_dir.display().to_string(),
            &cf_dir.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("cp -c spawn: {e}"))?;
    if cow.status.success() {
        return Ok(());
    }
    let plain = Command::new("cp")
        .args([
            "-R",
            &base_dir.display().to_string(),
            &cf_dir.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("cp -R spawn: {e}"))?;
    if plain.status.success() {
        Ok(())
    } else {
        Err(format!(
            "clone {} → {} failed: {}",
            base_dir.display(),
            cf_dir.display(),
            String::from_utf8_lossy(&plain.stderr).trim()
        ))
    }
}

/// Compute a real storage volume view for an emulator's instance dir. When the
/// instance is unprovisioned, `logical` reflects the base image and on-disk is 0.
pub fn volume_for(
    id: &EmulatorId,
    instance_dir: &Path,
    cf_dir: &Path,
    base_img_dir: &Path,
) -> StorageVolume {
    let provisioned = cf_dir.exists();
    let img_dir = if provisioned {
        cf_dir.join("img")
    } else {
        base_img_dir.to_path_buf()
    };

    let logical_bytes = host::dir_logical_bytes(&img_dir);
    let on_disk_bytes = if provisioned {
        host::dir_on_disk_bytes(cf_dir)
    } else {
        0
    };

    let writable_partitions: Vec<PartitionUsage> = WRITABLE_PARTITIONS
        .iter()
        .map(|name| {
            let p = img_dir.join(name);
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            PartitionUsage {
                name: name.trim_end_matches(".img").to_string(),
                size_bytes: size,
            }
        })
        .collect();

    StorageVolume {
        emulator_id: id.clone(),
        instance_dir: instance_dir.display().to_string(),
        logical_bytes,
        on_disk_bytes,
        writable_partitions,
        snapshots: list_snapshots(instance_dir),
    }
}

/// Snapshots live under `<instance_dir>/snapshots/`. Phase-2 CRUD is not wired,
/// so today this always resolves to an empty list (honest: no snapshots exist).
pub fn list_snapshots(_instance_dir: &Path) -> Vec<Snapshot> {
    Vec::new()
}

/// Best-effort recursive delete of an instance dir (on emulator delete).
pub fn remove_instance_dir(instance_dir: &Path) {
    if instance_dir.exists() {
        let _ = std::fs::remove_dir_all(instance_dir);
    }
}

/// The conventional layout for a provisioned instance.
pub struct InstanceLayout {
    pub dir: PathBuf,
    pub cf_dir: PathBuf,
    pub socket: PathBuf,
    /// PNG target for the ONE-SHOT `screenshot` verb.
    pub live_shot: PathBuf,
    /// Raw-RGBA target for the live-view `frame` stream.
    ///
    /// Deliberately a DIFFERENT file from `live_shot`. They used to share one path, so pressing
    /// Screenshot dropped a 2.3 MB PNG where the stream expected 3,145,728 bytes of RGBA (and the
    /// stream overwrote the screenshot) — two formats racing over one filename.
    pub live_frame: PathBuf,
}

impl InstanceLayout {
    pub fn new(instances_root: &Path, id: &EmulatorId) -> Self {
        let dir = instances_root.join(id);
        Self {
            cf_dir: dir.join("cf"),
            // The control socket MUST live at a short path: a macOS unix-domain
            // socket's `sun_path` is capped at 104 bytes, and the instance dir
            // (`~/Library/Application Support/com.avnir.aemulator-studio/instances/<uuid>/…`)
            // is ~125 bytes — `UnixListener::bind` there fails with "path too
            // long", silently disabling the whole control plane (no readiness,
            // no screenshot, no tap). Bind under `/tmp` instead (≈51 bytes).
            socket: control_socket_path(id),
            live_shot: dir.join("live.png"),
            live_frame: dir.join("live.raw"),
            dir,
        }
    }
}

/// A short, per-VM control-socket path under `/tmp` (see `InstanceLayout::new`
/// for why the instance dir can't hold it). `/tmp/aemu-<uuid>.sock` ≈ 51 bytes,
/// comfortably under the 104-byte `sun_path` limit and unique per emulator.
pub fn control_socket_path(id: &EmulatorId) -> PathBuf {
    PathBuf::from(format!("/tmp/aemu-{id}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the macOS `sun_path` 104-byte limit: the control
    /// socket must bind, so its path must stay short — even for a full UUID id.
    #[test]
    fn control_socket_path_under_sun_path_limit() {
        let id = "11111111-1111-4111-8111-111111111111".to_string();
        let p = control_socket_path(&id);
        let len = p.to_string_lossy().len();
        assert!(
            len <= 104,
            "control socket path is {len} bytes (> 104): {}",
            p.display()
        );
        assert!(p.to_string_lossy().ends_with(".sock"));
        assert!(
            p.to_string_lossy().contains(&id),
            "socket path must be unique per emulator id"
        );
    }
}
