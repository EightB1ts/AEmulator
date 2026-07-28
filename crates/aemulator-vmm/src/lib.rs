//! # aemulator-vmm
//!
//! Platform-independent **identity injection** and **launcher-arg generation**. This crate turns a
//! [`DeviceProfile`] + [`BootAssets`] into the concrete inputs a hypervisor needs:
//!
//! - [`qemu_hvf_args`] — the aarch64 QEMU/Hypervisor.framework argument vector (the **dev** backend
//!   that builds and runs on Apple Silicon today).
//! - [`kernel_cmdline`] — the kernel command line (deliberately **never** `androidboot.qemu`).
//! - [`build_device_tree`] — the hand-authored DTB bytes (delegates to `aemulator-dtb`).
//!
//! The production **KVM/crosvm** backend is a Linux-only seam behind the non-default `kvm` feature
//! (see [`crosvm_backend`]); it is absent from the default macOS build.
//!
//! ## Anti-detection posture of this crate
//!
//! Everything here is authored so the *launcher command line itself* carries none of the emulator /
//! VM tells the CI merge-gate greps built images for (`ranchu`, `goldfish`, `qemu`, `dummy-virt`,
//! `swiftshader`, `houdini`, `exagear`, `cutf`, `vsoc`, `crosvm`) and none of the QEMU/VirtualBox
//! network defaults (`52:54:00`, `08:00:27`, `10.0.2.15`). The guest's *identity* comes from the
//! hand-authored `-dtb` + the `DeviceProfile`, **never** from QEMU's `virt`-machine auto-DTB (whose
//! root `compatible = "linux,dummy-virt"` is a one-line tell). This backend is for **functional
//! bring-up**, not stealth validation — see the `-cpu host` caveat on [`qemu_hvf_args`].

use std::path::{Path, PathBuf};

use aemulator_identity::DeviceProfile;
use thiserror::Error;

/// The KVM/crosvm backend seam. Present only on `cfg(all(target_os = "linux", feature = "kvm"))`;
/// the default macOS dev build never compiles it.
#[cfg(all(target_os = "linux", feature = "kvm"))]
pub mod crosvm_backend;

/// Filename of the hand-authored device tree the launcher points `-dtb` at. The boot pipeline
/// writes [`build_device_tree`]'s bytes to this name next to the other boot assets; the launcher
/// and the writer therefore agree on one name. **Never** the QEMU `virt` auto-DTB.
const DTB_FILENAME: &str = "aemulator.dtb";

/// Guest RAM in MiB. A single source for both the HVF `-m` arg and the crosvm seam so the two
/// backends can never disagree on the advertised memory size.
pub(crate) const DEFAULT_MEM_MIB: u32 = 6144;

/// SLIRP user-net subnet overrides. QEMU's *default* user network hands the guest `10.0.2.15`,
/// gateway `10.0.2.2`, DNS `10.0.2.3` — a block framgia / CalebFenton flag on sight (see
/// research `detection-adversary` § network identity). Re-homing user-net onto a plausible
/// home-LAN subnet erases that tell at the argument level. (The KVM/stealth path uses a bridged
/// tap netdev on a real subnet instead of SLIRP entirely.)
const GUEST_NET_CIDR: &str = "192.168.1.0/24";
const GUEST_NET_HOST: &str = "192.168.1.1";
const GUEST_NET_DHCP_START: &str = "192.168.1.30";

/// Fallback OEM OUI when the profile omits one. A **real Xiaomi OUI** — never `52:54:00` (QEMU) or
/// `08:00:27` (VirtualBox → BlueStacks/Genymotion).
const FALLBACK_OEM_OUI: &str = "F0:B4:29";

/// Errors from VMM operations.
#[derive(Debug, Error)]
pub enum VmmError {
    /// A required boot asset path was empty/missing.
    #[error("boot asset missing: {0}")]
    MissingAsset(String),
    /// The requested hypervisor backend is not available in this build.
    #[error("backend `{0}` is not available in this build")]
    BackendUnavailable(String),
    /// Device-tree construction failed.
    #[error("device tree build failed: {0}")]
    Dtb(String),
}

/// The on-disk artifacts a launch consumes. Empty paths mean "not supplied".
#[derive(Debug, Clone, Default)]
pub struct BootAssets {
    /// Android arm64 GKI kernel image.
    pub kernel: PathBuf,
    /// Initial ramdisk / vendor_boot ramdisk.
    pub initrd: PathBuf,
    /// Stock GSI system image (`aosp_arm64`, AVB intact).
    pub system_img: PathBuf,
    /// Our de-branded vendor image.
    pub vendor_img: PathBuf,
    /// Firmware / bootloader blob (e.g. u-boot / EDK2 for the `virt` machine).
    pub firmware: PathBuf,
}

/// Build the hand-authored device-tree blob for `profile` (delegates to `aemulator-dtb`).
pub fn build_device_tree(profile: &DeviceProfile) -> Result<Vec<u8>, VmmError> {
    aemulator_dtb::build_fdt(profile).map_err(|e| VmmError::Dtb(e.to_string()))
}

/// Construct the kernel command line for `profile`.
///
/// It sets `androidboot.hardware` (load-bearing for HAL resolution — `hw_get_module` iterates
/// `variant_keys = {ro.hardware, ro.product.board, ro.board.platform, ro.arch}`, so a wrong value
/// breaks `.so` loading), mirrors the profile's **verified-boot** identity, and **never emits
/// `androidboot.qemu`** — that property simply must not exist, so there is nothing to hide (`init`
/// synthesizes `ro.boot.*` from the `androidboot.*` prefix; `Build.IS_EMULATOR` reads `ro.boot.qemu`).
///
/// The verified-boot triple is here because ACE and other RASP SDKs read
/// `ro.boot.verifiedbootstate` / `ro.boot.flash.locked` / `ro.boot.vbmeta.device_state` **directly**
/// as tamper signals (research `detection-adversary` § verified-boot props). On a locked/green retail
/// device these are `green` / `1` / `locked` / `enforcing`.
///
/// > Stealth nuance: `/proc/cmdline` is SELinux-labeled `proc_cmdline` and **denied to
/// > `untrusted_app`** on modern Android, so these `androidboot.*` args are not directly readable by
/// > a normal app — they surface only via the `ro.boot.*` props `init` derives (which carry the
/// > *correct* green/locked values). The full bootconfig set (serialno, etc.) rides in the
/// > `vendor_boot` ramdisk via [`DeviceProfile::emit_bootconfig`] once that method lands (task #2);
/// > bootconfig is appended to the ramdisk, not passed as a QEMU flag.
pub fn kernel_cmdline(profile: &DeviceProfile) -> String {
    let hardware = nonempty(&profile.build.hardware, "qcom");

    let mut parts: Vec<String> = vec![
        // PL011 (ttyAMA0) is the `virt` board's UART. `console=` on the cmdline is not app-readable
        // (proc_cmdline is denied to untrusted_app); the stealth/KVM path renames the console anyway.
        "console=ttyAMA0".to_string(),
        // Load-bearing: drives init.<hw>.rc, fstab.<hw>, and /vendor/lib64/hw/<module>.<hw>.so.
        format!("androidboot.hardware={hardware}"),
        // Coherent with ro.secure=1 / veritymode=enforcing — never `permissive`.
        "androidboot.selinux=enforcing".to_string(),
    ];

    // Verified-boot identity, mirrored into ro.boot.* by init and read directly by RASP SDKs.
    let vb = &profile.verified_boot;
    push_kv(
        &mut parts,
        "androidboot.verifiedbootstate",
        &vb.verifiedbootstate,
    );
    parts.push(format!(
        "androidboot.flash.locked={}",
        if vb.flash_locked { "1" } else { "0" }
    ));
    push_kv(
        &mut parts,
        "androidboot.vbmeta.device_state",
        &vb.device_state,
    );
    push_kv(&mut parts, "androidboot.veritymode", &vb.veritymode);

    // NOTE: deliberately NO `androidboot.qemu` / `ro.kernel.qemu` / `qemu=1` — the whole point.
    parts.join(" ")
}

/// Generate the aarch64 QEMU argument vector for the Hypervisor.framework (HVF) **dev** backend.
///
/// This is the path that builds and runs on Apple Silicon today. Returns only the argument vector
/// (no binary name), so the caller invokes `qemu-system-aarch64` with it. Every knob below is chosen
/// for a *reason*, annotated inline:
///
/// - `-machine virt` + `-accel hvf` — hardware ARM-on-ARM. Choosing HVF (never TCG) structurally
///   deletes whole detection classes for free: EmuID's I-/D-cache-incoherency test and Proteus's
///   TCG instruction-profile fingerprinting are DBT artifacts that do not exist under hardware virt.
///   The host machine string `virt` is **not** guest-visible — the guest reads our `-dtb`, not the
///   QEMU auto-DTB.
/// - `-cpu host` — see the stealth caveat below.
/// - `-dtb` — our **hand-authored** device tree, never QEMU's `virt` auto-DTB (whose root
///   `compatible = "linux,dummy-virt"` is readable at `/proc/device-tree/compatible`). The DTB root
///   `model` also makes `/proc/cpuinfo`'s `Hardware:` line read as the claimed SoC.
/// - `-smbios` — overrides QEMU/SeaBIOS default DMI strings (which would leak `QEMU`) with the
///   profile's OEM identity, in case EDK2 firmware surfaces SMBIOS at `/sys/class/dmi/id/*`.
/// - `-rtc base=utc,clock=host` — the guest RTC tracks host wall-clock (no detectable emulator drift).
/// - virtio-net MAC uses the profile's **real OEM OUI** (never `52:54:00` / `08:00:27`) and the user
///   network is re-homed off SLIRP's flagged `10.0.2.0/24` block.
/// - `-append` carries [`kernel_cmdline`] — no `androidboot.qemu`.
///
/// # Stealth caveat (in-code by design)
///
/// `-cpu host` is the fastest path, but on Apple Silicon it leaks `MIDR_EL1` implementer `0x61`
/// (Apple) — readable at `/sys/devices/system/cpu/cpu*/regs/identification/midr_el1` and in
/// `/proc/cpuinfo`, and a **fatal** tell (no Android SoC reports `0x61`). The production KVM backend
/// clamps MIDR per-cluster via `KVM_SET_ONE_REG` (`KVM_CAP_ARM_WRITABLE_IMP_ID_REGS`) from
/// [`DeviceProfile::midr_regs`]; under HVF that override is not available, so this dev backend is for
/// **functional bring-up only, not stealth validation**. `ID_AA64*` feature clamps toward the claimed
/// SoC's exact vector likewise belong to the KVM path.
pub fn qemu_hvf_args(profile: &DeviceProfile, assets: &BootAssets) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // ── CPU / machine / memory ───────────────────────────────────────────────────────────────
    // gic-version=3 matches a modern SoC's GICv3 interrupt controller; highmem=on maps RAM above
    // the 32-bit hole (needed once memory > ~3 GiB).
    push(
        &mut args,
        "-machine",
        "virt,gic-version=3,highmem=on".to_string(),
    );
    push(&mut args, "-accel", "hvf".to_string());
    // See the stealth caveat in the doc comment (Apple 0x61 MIDR leak under HVF).
    push(&mut args, "-cpu", "host".to_string());
    // -smp mirrors the SoC's real core count so /proc/cpuinfo core count agrees with the DTB.
    push(&mut args, "-smp", total_cores(profile).max(1).to_string());
    push(&mut args, "-m", DEFAULT_MEM_MIB.to_string());

    // ── Hand-authored device tree — NEVER the QEMU auto-DTB (`linux,dummy-virt`) ─────────────────
    push(&mut args, "-dtb", DTB_FILENAME.to_string());

    // ── RTC: track host wall-clock; no emulator-characteristic drift. base=utc matches Android's
    //    UTC hwclock convention (persist.sys.timezone applies the offset in userspace). ────────────
    push(
        &mut args,
        "-rtc",
        "base=utc,clock=host,driftfix=slew".to_string(),
    );

    // ── SMBIOS / DMI: override QEMU & SeaBIOS defaults (which leak the literal `QEMU`). Real ARM
    //    phones expose no DMI, but if the EDK2 firmware surfaces SMBIOS we must not let QEMU's
    //    defaults through — shape every table from the profile's OEM identity. ─────────────────────
    let mfr = nonempty(&profile.build.manufacturer, "manufacturer");
    let model = nonempty(&profile.build.model, "model");
    let brand = nonempty(&profile.build.brand, "brand");
    let device = nonempty(&profile.build.device, "device");
    let product = nonempty(&profile.build.product_name, "product");
    let bios_version = nonempty(&profile.build.build_id, "1.0");
    push(
        &mut args,
        "-smbios",
        format!("type=0,vendor={mfr},version={bios_version}"),
    );
    push(
        &mut args,
        "-smbios",
        format!("type=1,manufacturer={mfr},product={model},version={device},family={brand},sku={product}"),
    );
    push(&mut args, "-smbios", format!("type=3,manufacturer={mfr}"));

    // ── Kernel + ramdisk + firmware ──────────────────────────────────────────────────────────────
    push(&mut args, "-kernel", path_arg(&assets.kernel, "kernel.img"));
    if !assets.initrd.as_os_str().is_empty() {
        push(&mut args, "-initrd", path_arg(&assets.initrd, "initrd.img"));
    }
    if !assets.firmware.as_os_str().is_empty() {
        push(
            &mut args,
            "-bios",
            path_arg(&assets.firmware, "firmware.bin"),
        );
    }

    // ── Block devices: stock GSI system + our vendor image over virtio-blk. Both mount read-only —
    //    AVB dm-verity protects `system`, and a writable vendor is itself anomalous on a locked
    //    device. ────────────────────────────────────────────────────────────────────────────────
    push(
        &mut args,
        "-drive",
        format!(
            "file={},if=none,id=system,format=raw,readonly=on",
            path_arg(&assets.system_img, "system.img")
        ),
    );
    push(
        &mut args,
        "-device",
        "virtio-blk-pci,drive=system".to_string(),
    );
    push(
        &mut args,
        "-drive",
        format!(
            "file={},if=none,id=vendor,format=raw,readonly=on",
            path_arg(&assets.vendor_img, "vendor.img")
        ),
    );
    push(
        &mut args,
        "-device",
        "virtio-blk-pci,drive=vendor".to_string(),
    );

    // ── NIC: real OEM OUI, user-net re-homed off the flagged 10.0.2.0/24 SLIRP block. `romfile=`
    //    disables the PXE option ROM so its iPXE/QEMU strings can't leak. ──────────────────────────
    push(
        &mut args,
        "-netdev",
        format!(
            "user,id=net0,net={GUEST_NET_CIDR},host={GUEST_NET_HOST},dhcpstart={GUEST_NET_DHCP_START}"
        ),
    );
    push(
        &mut args,
        "-device",
        format!(
            "virtio-net-pci,netdev=net0,mac={},romfile=",
            derive_mac(&profile.network.mac_oui)
        ),
    );

    // ── Serial console renamed away from goldfish/qemu service tells. ────────────────────────────
    push(
        &mut args,
        "-device",
        "virtio-serial-pci,id=sysserial".to_string(),
    );

    // ── Display: virtio-gpu for host-side framebuffer capture (the perception path). The KVM/stealth
    //    build drives this through rutabaga_gfx → Venus / forked-Mesa ICD, presenting an
    //    INTEGRATED_GPU identity — NEVER SwiftShader/lavapipe (`VK_PHYSICAL_DEVICE_TYPE_CPU` is a
    //    fatal tell). Here it is a plain virtio-gpu for bring-up. ─────────────────────────────────
    push(&mut args, "-device", "virtio-gpu-pci".to_string());

    // ── Entropy source (hardware RNG). Harmless — real devices expose a hwrng — and avoids a
    //    blocking getrandom() early in boot. ─────────────────────────────────────────────────────
    push(&mut args, "-device", "virtio-rng-pci".to_string());

    // ── Kernel command line (identity-derived; never androidboot.qemu). ──────────────────────────
    push(&mut args, "-append", kernel_cmdline(profile));

    args
}

/// Push a `flag value` pair onto the argument vector.
fn push(args: &mut Vec<String>, flag: &str, value: String) {
    args.push(flag.to_string());
    args.push(value);
}

/// Push `key=value` onto `parts` only when `value` is non-empty (so we never emit a bare
/// `androidboot.foo=` that itself looks synthetic).
fn push_kv(parts: &mut Vec<String>, key: &str, value: &str) {
    if !value.trim().is_empty() {
        parts.push(format!("{key}={value}"));
    }
}

/// Sum of all cluster core counts (the guest's `-smp` value).
pub(crate) fn total_cores(profile: &DeviceProfile) -> u32 {
    profile.soc.midr_clusters.iter().map(|c| c.core_count).sum()
}

/// Derive a full 48-bit MAC from a real OEM OUI prefix. The NIC-unique suffix is minted per-instance
/// elsewhere; here we append a fixed placeholder suffix. A blank OUI falls back to a real Xiaomi OUI
/// so the arg is never `52:54:00` (QEMU) / `08:00:27` (VirtualBox). Output is lowercased to match
/// QEMU's canonical MAC form.
pub(crate) fn derive_mac(oui: &str) -> String {
    let oui = oui.trim();
    let oui = if oui.is_empty() {
        FALLBACK_OEM_OUI
    } else {
        oui
    };
    format!("{}:00:00:01", oui.to_lowercase())
}

/// A path as a launcher arg, or a named placeholder when the asset was not supplied.
fn path_arg(path: &Path, placeholder: &str) -> String {
    if path.as_os_str().is_empty() {
        placeholder.to_string()
    } else {
        path.display().to_string()
    }
}

/// `value` if non-empty, else `fallback`.
fn nonempty(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aemulator_identity::profiles::reference_profile;

    /// Canonical emulator/VM tells (verbatim from the plan's hard blocklist + the QEMU/VirtualBox
    /// network defaults). The generated launcher command line must contain NONE of these — the same
    /// invariant the CI merge-gate enforces against built images.
    const CANONICAL_TELLS: &[&str] = &[
        "ranchu",
        "goldfish",
        "qemu",
        "cutf",
        "vsoc",
        "crosvm",
        "swiftshader",
        "dummy-virt",
        "houdini",
        "exagear",
        "androidboot.qemu",
        "52:54:00",  // QEMU default MAC OUI
        "08:00:27",  // VirtualBox / Genymotion / BlueStacks MAC OUI
        "10.0.2.15", // QEMU SLIRP default guest IP
    ];

    #[test]
    fn qemu_args_never_leak_androidboot_qemu() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        let joined = args.join(" ");
        assert!(
            !joined.contains("androidboot.qemu"),
            "must never emit androidboot.qemu"
        );
        assert!(!joined.contains("linux,dummy-virt"));
    }

    #[test]
    fn qemu_args_use_hvf_and_hand_authored_dtb() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        assert!(
            args.windows(2).any(|w| w[0] == "-accel" && w[1] == "hvf"),
            "must select the HVF accelerator"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-machine" && w[1].starts_with("virt")),
            "must use the `virt` machine"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-cpu" && w[1] == "host"),
            "dev backend uses -cpu host (with the documented Apple 0x61 caveat)"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-dtb" && w[1] == DTB_FILENAME),
            "must pass our hand-authored -dtb, not QEMU's auto-DTB"
        );
    }

    /// The load-bearing anti-detection assertion: the whole command line carries no tell from the
    /// canonical blocklist, and no pattern from the live `aemulator_detection::blocklist()` matches
    /// it either (the same source the CI gate scans built images with).
    #[test]
    fn qemu_args_contain_no_blocklisted_substring() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        let joined = args.join(" ").to_lowercase();

        for tell in CANONICAL_TELLS {
            assert!(
                !joined.contains(&tell.to_lowercase()),
                "launcher args leaked a blocklisted tell `{tell}`: {joined}"
            );
        }

        // Cross-check against the detection crate's blocklist (empty until task #3 populates it;
        // this wires the same contract the merge-gate uses so it stays honest as the list grows).
        for signal in aemulator_detection::blocklist() {
            if let Ok(re) = regex::Regex::new(&format!("(?i){}", signal.pattern)) {
                assert!(
                    !re.is_match(&joined),
                    "launcher args matched detection signal `{}` ({}): {joined}",
                    signal.pattern,
                    signal.category
                );
            }
        }
    }

    #[test]
    fn mac_uses_real_oui_not_qemu_default() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        let joined = args.join(" ");
        assert!(
            joined.contains("mac=f0:b4:29:00:00:01"),
            "must use the profile's real OEM OUI"
        );
        assert!(!joined.contains("52:54:00"));
        assert!(!joined.contains("08:00:27"));
    }

    #[test]
    fn mac_falls_back_to_real_oui_when_profile_blank() {
        // A profile with no OUI must still never produce a QEMU/VirtualBox default MAC.
        let mac = derive_mac("");
        assert_eq!(mac, "f0:b4:29:00:00:01");
        assert!(!mac.starts_with("52:54:00"));
    }

    #[test]
    fn smp_reflects_total_cores() {
        // Reference SoC has 1 + 2 + 2 + 3 = 8 cores.
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        assert!(args.windows(2).any(|w| w[0] == "-smp" && w[1] == "8"));
    }

    #[test]
    fn smbios_is_shaped_from_profile_and_hides_qemu() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        let joined = args.join(" ");
        // Three SMBIOS tables shaped from the OEM identity.
        assert!(joined.contains("-smbios"), "must set SMBIOS");
        assert!(joined.contains("manufacturer=Xiaomi"));
        assert!(joined.contains("product=2211133C"));
        // QEMU's default SMBIOS strings must not survive (case-insensitive: `QEMU`/`SeaBIOS`).
        let lower = joined.to_lowercase();
        assert!(!lower.contains("qemu"));
        assert!(!lower.contains("seabios"));
    }

    #[test]
    fn rtc_tracks_host_wall_clock() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-rtc" && w[1].contains("clock=host")),
            "RTC must track the host wall-clock"
        );
    }

    #[test]
    fn user_net_is_rehomed_off_the_slirp_default_block() {
        let args = qemu_hvf_args(&reference_profile(), &BootAssets::default());
        let joined = args.join(" ");
        assert!(
            joined.contains("net=192.168.1.0/24"),
            "user-net must be re-homed off 10.0.2.0/24"
        );
        assert!(
            !joined.contains("10.0.2."),
            "the flagged SLIRP block must not appear"
        );
    }

    #[test]
    fn supplied_assets_appear_in_args() {
        let assets = BootAssets {
            kernel: PathBuf::from("/img/boot/Image"),
            initrd: PathBuf::from("/img/boot/initrd.img"),
            system_img: PathBuf::from("/img/system.raw"),
            vendor_img: PathBuf::from("/img/vendor.raw"),
            firmware: PathBuf::from("/img/edk2.fd"),
        };
        let args = qemu_hvf_args(&reference_profile(), &assets);
        let joined = args.join(" ");
        assert!(joined.contains("/img/boot/Image"));
        assert!(joined.contains("file=/img/system.raw"));
        assert!(joined.contains("file=/img/vendor.raw"));
        assert!(joined.contains("/img/edk2.fd"));
        // System/vendor mount read-only (dm-verity / locked-device coherence).
        assert!(joined.contains("id=system,format=raw,readonly=on"));
    }

    #[test]
    fn kernel_cmdline_sets_hardware_and_omits_qemu() {
        let cmdline = kernel_cmdline(&reference_profile());
        assert!(cmdline.contains("androidboot.hardware=qcom"));
        assert!(!cmdline.contains("qemu"));
    }

    #[test]
    fn kernel_cmdline_mirrors_verified_boot_identity() {
        let cmdline = kernel_cmdline(&reference_profile());
        // Reference profile is a locked/green retail device.
        assert!(cmdline.contains("androidboot.verifiedbootstate=green"));
        assert!(cmdline.contains("androidboot.flash.locked=1"));
        assert!(cmdline.contains("androidboot.vbmeta.device_state=locked"));
        assert!(cmdline.contains("androidboot.veritymode=enforcing"));
        assert!(cmdline.contains("androidboot.selinux=enforcing"));
    }

    #[test]
    fn kernel_cmdline_skips_empty_verified_boot_fields() {
        // A profile with unset verified-boot fields must not emit bare `androidboot.foo=` params.
        let mut p = reference_profile();
        p.verified_boot.verifiedbootstate.clear();
        p.verified_boot.device_state.clear();
        p.verified_boot.veritymode.clear();
        let cmdline = kernel_cmdline(&p);
        assert!(!cmdline.contains("androidboot.verifiedbootstate="));
        assert!(!cmdline.contains("androidboot.vbmeta.device_state="));
        assert!(!cmdline.contains("androidboot.veritymode="));
        // The boolean flag is always well-formed.
        assert!(cmdline.contains("androidboot.flash.locked=1"));
    }

    #[test]
    fn kernel_cmdline_hardware_falls_back_when_blank() {
        let mut p = reference_profile();
        p.build.hardware.clear();
        assert!(kernel_cmdline(&p).contains("androidboot.hardware=qcom"));
    }

    #[test]
    fn build_device_tree_delegates_to_dtb_crate() {
        let blob = build_device_tree(&reference_profile()).expect("dtb should build");
        assert_eq!(&blob[0..4], &[0xd0, 0x0d, 0xfe, 0xed], "FDT magic");
    }
}
