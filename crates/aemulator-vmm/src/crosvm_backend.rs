//! KVM/crosvm backend seam — **Linux + `kvm` feature only**.
//!
//! This module is the documented integration point for a **crosvm** fork (BSD-3,
//! <https://chromium.googlesource.com/crosvm/crosvm>). It is compiled only under
//! `cfg(all(target_os = "linux", feature = "kvm"))`, so the default macOS dev build never pulls the
//! Linux/KVM-only crosvm crates, and a non-Linux build with `kvm` on still compiles (this module is
//! simply absent).
//!
//! ## Why a seam, not a dependency (yet)
//!
//! Adding a `crosvm` path dependency to this crate's `[dependencies]` would break the macOS default
//! build (crosvm's arch/hypervisor crates are Linux/KVM-only). So the crosvm wiring lives here,
//! behind the double gate, and is filled in Phase 1 by the VMM implementer.
//!
//! [`crosvm_config`] is the **platform-independent half** of that seam: it turns a [`DeviceProfile`]
//! + [`BootAssets`] into a fully-resolved, backend-agnostic [`CrosvmConfig`] (pure data — assembling
//! it pulls in **no** crosvm crates). The Phase-1 launcher consumes a `CrosvmConfig` to program the
//! real machine; keeping it pure means the asset/identity resolution is unit-testable without KVM.
//!
//! ## Implementation plan (Phase 1)
//!
//! Depend on the fork's `hypervisor`, `kvm_sys`, `aarch64`, `aarch64_sys_reg` (the MIDR/ID-reg
//! model), `cros_fdt` (or our `aemulator-dtb`, whose bytes [`CrosvmConfig::device_tree`] already
//! carries), `kernel_loader`, `devices`, and the graphics path (`gpu_display` / upstream
//! `rutabaga_gfx`). Boot the aarch64 machine under KVM, load the kernel + initrd + our DTB, attach
//! the virtio-blk disks, and — critically — program the per-cluster identification registers from
//! [`aemulator_identity::DeviceProfile::midr_regs`] via `KVM_SET_ONE_REG`. That last step needs
//! `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` (~6.14 kernel), so **stealth boot is gated on the cap**: without
//! it, `MIDR_EL1` reports the host implementer (fatal on any non-target host) and the launch must
//! refuse to claim stealth. `midr_regs()` is deliberately **not** called from [`crosvm_config`] (it
//! is the runtime programming step, applied against a live vCPU by the launcher).

use std::path::PathBuf;

use aemulator_identity::DeviceProfile;

use crate::{BootAssets, VmmError};

/// One virtio-blk backing store for the guest, tagged by the partition role it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrosvmDisk {
    /// Host path to the raw disk image.
    pub path: PathBuf,
    /// Whether the guest mounts it read-only. `system` is AVB dm-verity protected and `vendor` is
    /// read-only on a locked device, so both are `true`.
    pub read_only: bool,
    /// Partition role: `"system"` or `"vendor"`.
    pub role: &'static str,
}

/// A fully-resolved, backend-agnostic launch description for the KVM/crosvm backend.
///
/// Pure data: it carries the already-built device-tree **bytes** and the assembled kernel cmdline,
/// so the Phase-1 launcher never re-derives identity. The per-cluster MIDR programming is applied at
/// boot from [`DeviceProfile::midr_regs`] against the live vCPU (see the module docs) — it is not a
/// field here because it targets a `KVM_SET_ONE_REG` ioctl, not a static config value.
#[derive(Debug, Clone)]
pub struct CrosvmConfig {
    /// Guest vCPU count (sum of the profile's per-cluster core counts).
    pub cpus: u32,
    /// Guest RAM in MiB.
    pub memory_mib: u32,
    /// Android arm64 GKI kernel image.
    pub kernel: PathBuf,
    /// Optional vendor_boot / initial ramdisk.
    pub initrd: Option<PathBuf>,
    /// Backing disks (system + vendor), in mount order.
    pub disks: Vec<CrosvmDisk>,
    /// Hand-authored device-tree blob (from [`crate::build_device_tree`]).
    pub device_tree: Vec<u8>,
    /// Kernel command line (from [`crate::kernel_cmdline`]; never `androidboot.qemu`).
    pub kernel_cmdline: String,
    /// virtio-net MAC with the profile's real OEM OUI.
    pub mac_address: String,
}

/// Resolve a [`CrosvmConfig`] from `profile` + `assets`.
///
/// Validates that the identity-critical assets are present (kernel + system + vendor images), builds
/// the hand-authored DTB, and assembles the launch description. Returns [`VmmError::MissingAsset`]
/// for an absent required asset and [`VmmError::Dtb`] if the device tree fails to build. This is the
/// pure, KVM-free half of the crosvm seam — safe to unit-test on any Linux host.
pub fn crosvm_config(
    profile: &DeviceProfile,
    assets: &BootAssets,
) -> Result<CrosvmConfig, VmmError> {
    if assets.kernel.as_os_str().is_empty() {
        return Err(VmmError::MissingAsset("kernel".to_string()));
    }
    if assets.system_img.as_os_str().is_empty() {
        return Err(VmmError::MissingAsset("system_img".to_string()));
    }
    if assets.vendor_img.as_os_str().is_empty() {
        return Err(VmmError::MissingAsset("vendor_img".to_string()));
    }

    // Delegates to aemulator-dtb — never QEMU's `linux,dummy-virt` auto-DTB.
    let device_tree = crate::build_device_tree(profile)?;

    let disks = vec![
        CrosvmDisk {
            path: assets.system_img.clone(),
            read_only: true, // AVB dm-verity protected.
            role: "system",
        },
        CrosvmDisk {
            path: assets.vendor_img.clone(),
            read_only: true, // Read-only on a locked device.
            role: "vendor",
        },
    ];

    let initrd = if assets.initrd.as_os_str().is_empty() {
        None
    } else {
        Some(assets.initrd.clone())
    };

    Ok(CrosvmConfig {
        cpus: crate::total_cores(profile).max(1),
        memory_mib: crate::DEFAULT_MEM_MIB,
        kernel: assets.kernel.clone(),
        initrd,
        disks,
        device_tree,
        kernel_cmdline: crate::kernel_cmdline(profile),
        mac_address: crate::derive_mac(&profile.network.mac_oui),
    })
}

/// Boot the guest on the KVM/crosvm backend.
///
/// Assembles the [`CrosvmConfig`] (validating assets + building the DTB) and then returns
/// [`VmmError::BackendUnavailable`] because the actual crosvm launch is not yet wired — the config is
/// ready, but standing up the fork's `hypervisor`/`devices`/vCPU machine is Phase-1 work. Wire the
/// launch here without changing the signature.
pub fn boot(profile: &DeviceProfile, assets: &BootAssets) -> Result<(), VmmError> {
    let _config = crosvm_config(profile, assets)?;
    Err(VmmError::BackendUnavailable(
        "crosvm/kvm launch not yet wired (CrosvmConfig assembled; see the module docs — Phase 1)"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aemulator_identity::profiles::reference_profile;

    fn full_assets() -> BootAssets {
        BootAssets {
            kernel: PathBuf::from("/img/boot/Image"),
            initrd: PathBuf::from("/img/boot/initrd.img"),
            system_img: PathBuf::from("/img/system.raw"),
            vendor_img: PathBuf::from("/img/vendor.raw"),
            firmware: PathBuf::from("/img/edk2.fd"),
        }
    }

    #[test]
    fn config_resolves_from_full_assets() {
        let cfg = crosvm_config(&reference_profile(), &full_assets()).expect("config");
        assert_eq!(cfg.cpus, 8); // 1 + 3 + 4 clusters.
        assert_eq!(cfg.memory_mib, crate::DEFAULT_MEM_MIB);
        assert_eq!(cfg.initrd, Some(PathBuf::from("/img/boot/initrd.img")));
        assert_eq!(cfg.disks.len(), 2);
        assert!(cfg.disks.iter().all(|d| d.read_only));
        assert!(cfg.disks.iter().any(|d| d.role == "system"));
        assert!(cfg.disks.iter().any(|d| d.role == "vendor"));
        assert_eq!(
            &cfg.device_tree[0..4],
            &[0xd0, 0x0d, 0xfe, 0xed],
            "FDT magic"
        );
        assert!(cfg.mac_address.starts_with("f0:b4:29"));
        assert!(!cfg.kernel_cmdline.contains("qemu"));
    }

    #[test]
    fn config_requires_kernel_and_images() {
        // Missing kernel.
        let mut a = full_assets();
        a.kernel = PathBuf::new();
        assert!(matches!(
            crosvm_config(&reference_profile(), &a),
            Err(VmmError::MissingAsset(_))
        ));
        // Missing vendor image.
        let mut a = full_assets();
        a.vendor_img = PathBuf::new();
        assert!(matches!(
            crosvm_config(&reference_profile(), &a),
            Err(VmmError::MissingAsset(_))
        ));
    }

    #[test]
    fn config_omits_initrd_when_absent() {
        let mut a = full_assets();
        a.initrd = PathBuf::new();
        let cfg = crosvm_config(&reference_profile(), &a).expect("config");
        assert!(cfg.initrd.is_none());
    }

    #[test]
    fn boot_reports_backend_unavailable_with_valid_assets() {
        // Assets valid → config assembles → still unavailable (launch not wired).
        assert!(matches!(
            boot(&reference_profile(), &full_assets()),
            Err(VmmError::BackendUnavailable(_))
        ));
    }
}
