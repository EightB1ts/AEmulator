//! # aemulator-dtb
//!
//! Hand-authored ARM64 **device tree**, driven by the [`DeviceProfile`]. We NEVER use the QEMU
//! `virt` machine's auto-generated DTB — its root `compatible = "linux,dummy-virt"` is an instant
//! tell, readable at `/proc/device-tree/compatible`. Instead the root `compatible`/`model` are set
//! to the claimed SoC, which is what makes `/proc/cpuinfo`'s `Hardware:` line correct (the vendor
//! kernel populates it from `of_flat_dt_get_machine_name()` = the DT root `model`).
//!
//! [`assert_no_vm_tells`] is the self-check the CI gate runs on the *built* blob.
//!
//! ## What this authors (and what it deliberately omits)
//!
//! [`build_fdt`] hand-authors a real-Android-shaped ARM64 tree:
//!
//! - root `compatible` (a stringlist like `["xiaomi,fuxi", "qcom,kalama"]`) + `model`
//!   (`"Qualcomm Technologies, Inc SM8550"`), so the guest `Hardware:` line reads as the claimed SoC;
//! - `/cpus` with one `cpu@<mpidr>` per core, **heterogeneous per-cluster** `compatible`
//!   (`arm,cortex-x3` / `arm,cortex-a715` / `arm,cortex-a710` / `arm,cortex-a510`), plus a
//!   `/cpus/cpu-map` cluster topology;
//! - `/psci` (`method = "smc"` — real hardware, not the emulator's `hvc`), `/timer`
//!   (`arm,armv8-timer`), `/intc@…` GICv3 (`arm,gic-v3` — Qualcomm flagships are GICv3), `/pmu`
//!   (`arm,armv8-pmuv3`);
//! - a `/firmware/android` node (`android,firmware` + an `android,vbmeta` child), which real
//!   Android bootloaders populate.
//!
//! It **never** re-introduces the crosvm/QEMU tells emitted by crosvm's own aarch64 FDT builder
//! (`aarch64/src/fdt.rs` in <https://chromium.googlesource.com/crosvm/crosvm>, read for reference
//! while this was written): the `linux,dummy-virt` root `compatible`, the
//! `google,goldfish-battery` node, `qemu,vcpu-stall-detector` (the vmwdt), `virtual,kvm-cpufreq` /
//! `qemu,virtual-cpufreq`, and the `pkvm,*` pvIOMMU / power-domain nodes. Every built blob is
//! self-checked with [`assert_no_vm_tells`] before it is returned, so a regression cannot escape.

use aemulator_identity::{DeviceProfile, MidrCluster};
use thiserror::Error;
use vm_fdt::FdtWriter;

/// Errors from building or checking a device tree.
#[derive(Debug, Error)]
pub enum DtbError {
    /// The underlying `vm-fdt` writer failed.
    #[error("fdt writer error: {0}")]
    Fdt(String),
    /// A blocklisted virtualization tell is present in the built device tree.
    #[error("blocklisted virtualization tell `{0}` present in the device tree")]
    VmTell(String),
}

/// Substrings that must never appear anywhere in a shipped device-tree blob. These are the
/// canonical QEMU-`virt` / goldfish / pKVM tells (see the plan's "NEVER emit" list). The scan is a
/// raw byte-substring match, so it catches a tell regardless of whether it was emitted as a node
/// name, a property name, or a property value.
const VM_TELLS: &[&str] = &[
    "linux,dummy-virt",
    "dummy-virt",
    "goldfish",
    "google,goldfish-battery",
    "ranchu",
    "qemu",
    "vcpu-stall-detector",
    "kvm-cpufreq",
    "pkvm",
];

// GIC phandle — referenced by the root `interrupt-parent` and defined on the `intc` node.
const PHANDLE_GIC: u32 = 1;
// CPU phandles are assigned from here upward (one per core), referenced by `/cpus/cpu-map`.
const PHANDLE_CPU0: u32 = 0x100;

// GICv3 register window (Qualcomm SM8550 "kalama" layout, matching upstream `sm8550.dtsi`:
// GICD @ 0x17100000 size 0x10000, GICR @ 0x17180000 size 0x200000 = 8 redistributors).
// A real-SoC placement; the VMM may re-place it at launch (the HVF backend runs its own emulated
// GIC window — see `aemulator_hvf::gic`), but the identity default is a real address, never a
// QEMU-`virt` one.
const GIC_DIST_BASE: u64 = 0x1710_0000;
const GIC_DIST_SIZE: u64 = 0x1_0000;
const GIC_REDIST_BASE: u64 = 0x1718_0000;
const GIC_REDIST_SIZE: u64 = 0x20_0000;

// A plausible main-memory window (base 2 GiB, 4 GiB span). The VMM fills the real base/size at
// launch; this keeps the standalone base blob valid and non-degenerate.
const MEM_BASE: u64 = 0x8000_0000;
const MEM_SIZE: u64 = 0x1_0000_0000;

// Device-tree interrupt-specifier encoding (Linux GIC bindings), mirrored from crosvm's
// aarch64/src/fdt.rs (<https://chromium.googlesource.com/crosvm/crosvm>).
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
const IRQ_TYPE_LEVEL_HIGH: u32 = 0x0000_0004;
const IRQ_TYPE_LEVEL_LOW: u32 = 0x0000_0008;

/// Build a Flattened Device Tree blob from a [`DeviceProfile`].
///
/// The root node carries the SoC identity so the guest's `/proc/cpuinfo` `Hardware:` line reads as
/// the claimed part. The resulting blob is self-checked with [`assert_no_vm_tells`] before return —
/// a profile whose values would inject a tell (e.g. `soc_model = "goldfish"`) fails here.
pub fn build_fdt(profile: &DeviceProfile) -> Result<Vec<u8>, DtbError> {
    let mut fdt = FdtWriter::new().map_err(fdt_err)?;

    let root = fdt.begin_node("").map_err(fdt_err)?;
    // ALL root properties must be written before any subnode (vm-fdt enforces this).
    // Root `compatible`/`model` → `/proc/cpuinfo` `Hardware:` via of_flat_dt_get_machine_name().
    fdt.property_string_list("compatible", root_compatible(profile))
        .map_err(fdt_err)?;
    fdt.property_string("model", &machine_name(profile))
        .map_err(fdt_err)?;
    fdt.property_u32("#address-cells", 0x2).map_err(fdt_err)?;
    fdt.property_u32("#size-cells", 0x2).map_err(fdt_err)?;
    // Forward reference to the GIC phandle (defined on the `intc` node below); plain u32.
    fdt.property_u32("interrupt-parent", PHANDLE_GIC)
        .map_err(fdt_err)?;

    write_chosen(&mut fdt)?;
    write_memory(&mut fdt)?;
    write_cpus(&mut fdt, profile)?;
    write_gic(&mut fdt)?;
    write_timer(&mut fdt)?;
    write_pmu(&mut fdt)?;
    write_psci(&mut fdt)?;
    write_firmware_android(&mut fdt)?;

    fdt.end_node(root).map_err(fdt_err)?;

    let blob = fdt.finish().map_err(fdt_err)?;
    assert_no_vm_tells(&blob)?;
    Ok(blob)
}

/// Assert a built FDT blob contains no blocklisted virtualization tell. Property names and string
/// values are stored as ASCII in the FDT strings/struct blocks, so a raw byte-substring scan
/// catches a tell regardless of which node emitted it.
pub fn assert_no_vm_tells(fdt: &[u8]) -> Result<(), DtbError> {
    for tell in VM_TELLS {
        if contains_bytes(fdt, tell.as_bytes()) {
            return Err(DtbError::VmTell((*tell).to_string()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Node authors
// ---------------------------------------------------------------------------------------------

/// `/chosen` — the kernel cmdline / bootconfig is injected by the boot pipeline, not baked here.
fn write_chosen(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let node = fdt.begin_node("chosen").map_err(fdt_err)?;
    fdt.property_string("bootargs", "").map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/memory@…` — plausible placeholder window; the VMM fills the real base/size at launch.
fn write_memory(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let name = format!("memory@{MEM_BASE:x}");
    let node = fdt.begin_node(&name).map_err(fdt_err)?;
    fdt.property_string("device_type", "memory")
        .map_err(fdt_err)?;
    fdt.property_array_u64("reg", &[MEM_BASE, MEM_SIZE])
        .map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/cpus` — one `cpu@<mpidr>` per core across the profile's clusters (heterogeneous `compatible`),
/// plus a `/cpus/cpu-map` describing the cluster topology (what a real big.LITTLE SoC reports).
fn write_cpus(fdt: &mut FdtWriter, profile: &DeviceProfile) -> Result<(), DtbError> {
    let node = fdt.begin_node("cpus").map_err(fdt_err)?;
    fdt.property_u32("#address-cells", 0x2).map_err(fdt_err)?;
    fdt.property_u32("#size-cells", 0x0).map_err(fdt_err)?;

    // Fall back to a single generic core if the profile declares no clusters, so the tree stays
    // valid rather than emitting an empty /cpus.
    let fallback = [MidrCluster {
        part_name: String::new(),
        implementer: 0,
        part: 0,
        variant: 0,
        revision: 0,
        core_count: 1,
        max_freq_khz: 0,
    }];
    let clusters: &[MidrCluster] = if profile.soc.midr_clusters.is_empty() {
        &fallback
    } else {
        &profile.soc.midr_clusters
    };

    // Emit cpu nodes; collect (cluster -> phandles) for the cpu-map.
    let mut cluster_members: Vec<Vec<u32>> = Vec::new();
    let mut global: u32 = 0;
    for (cluster_index, cluster) in clusters.iter().enumerate() {
        let mut members = Vec::new();
        // MPIDR affinity: aff1 = cluster index, aff0 = core index within the cluster.
        for core_index in 0..cluster.core_count.max(1) {
            let mpidr: u64 = ((cluster_index as u64) << 8) | (core_index as u64);
            let cpu_name = format!("cpu@{mpidr:x}");
            let cpu = fdt.begin_node(&cpu_name).map_err(fdt_err)?;
            fdt.property_string("device_type", "cpu").map_err(fdt_err)?;
            fdt.property_string("compatible", &cpu_compatible(&cluster.part_name))
                .map_err(fdt_err)?;
            fdt.property_string("enable-method", "psci")
                .map_err(fdt_err)?;
            fdt.property_array_u32("reg", &[0, mpidr as u32])
                .map_err(fdt_err)?;
            let phandle = PHANDLE_CPU0 + global;
            fdt.property_phandle(phandle).map_err(fdt_err)?;
            fdt.end_node(cpu).map_err(fdt_err)?;
            members.push(phandle);
            global += 1;
        }
        cluster_members.push(members);
    }

    // /cpus/cpu-map — cluster{N}/core{M} { cpu = <&cpuX> }.
    let map = fdt.begin_node("cpu-map").map_err(fdt_err)?;
    for (cluster_index, members) in cluster_members.iter().enumerate() {
        let cluster_node = fdt
            .begin_node(&format!("cluster{cluster_index}"))
            .map_err(fdt_err)?;
        for (core_index, phandle) in members.iter().enumerate() {
            let core_node = fdt
                .begin_node(&format!("core{core_index}"))
                .map_err(fdt_err)?;
            fdt.property_u32("cpu", *phandle).map_err(fdt_err)?;
            fdt.end_node(core_node).map_err(fdt_err)?;
        }
        fdt.end_node(cluster_node).map_err(fdt_err)?;
    }
    fdt.end_node(map).map_err(fdt_err)?;

    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/intc@…` — GICv3 interrupt controller (Qualcomm flagships are GICv3, never the crosvm
/// `arm,cortex-a15-gic`/GICv2 fallback).
fn write_gic(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let name = format!("intc@{GIC_DIST_BASE:x}");
    let node = fdt.begin_node(&name).map_err(fdt_err)?;
    fdt.property_string("compatible", "arm,gic-v3")
        .map_err(fdt_err)?;
    fdt.property_u32("#interrupt-cells", 0x3).map_err(fdt_err)?;
    fdt.property_null("interrupt-controller").map_err(fdt_err)?;
    fdt.property_u32("#address-cells", 0x2).map_err(fdt_err)?;
    fdt.property_u32("#size-cells", 0x2).map_err(fdt_err)?;
    fdt.property_array_u64(
        "reg",
        &[
            GIC_DIST_BASE,
            GIC_DIST_SIZE,
            GIC_REDIST_BASE,
            GIC_REDIST_SIZE,
        ],
    )
    .map_err(fdt_err)?;
    fdt.property_phandle(PHANDLE_GIC).map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/timer` — the architected generic timer, present on every real arm64 device.
fn write_timer(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let node = fdt.begin_node("timer").map_err(fdt_err)?;
    fdt.property_string("compatible", "arm,armv8-timer")
        .map_err(fdt_err)?;
    // Four PPIs: secure(13), non-secure(14), virtual(11), hypervisor(10), all level-low.
    let mut cells = Vec::with_capacity(12);
    for irq in [13u32, 14, 11, 10] {
        cells.push(GIC_FDT_IRQ_TYPE_PPI);
        cells.push(irq);
        cells.push(IRQ_TYPE_LEVEL_LOW);
    }
    fdt.property_array_u32("interrupts", &cells)
        .map_err(fdt_err)?;
    fdt.property_null("always-on").map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/pmu` — the ARMv8 performance-monitor unit (PMUv3), PPI 7.
fn write_pmu(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let node = fdt.begin_node("pmu").map_err(fdt_err)?;
    fdt.property_string("compatible", "arm,armv8-pmuv3")
        .map_err(fdt_err)?;
    fdt.property_array_u32(
        "interrupts",
        &[GIC_FDT_IRQ_TYPE_PPI, 7, IRQ_TYPE_LEVEL_HIGH],
    )
    .map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/psci` — power state coordination. `method = "smc"` matches real hardware (firmware conduit);
/// the emulator default `"hvc"` is itself a VM tell.
fn write_psci(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let node = fdt.begin_node("psci").map_err(fdt_err)?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,psci-1.0".to_string(), "arm,psci-0.2".to_string()],
    )
    .map_err(fdt_err)?;
    fdt.property_string("method", "smc").map_err(fdt_err)?;
    fdt.end_node(node).map_err(fdt_err)?;
    Ok(())
}

/// `/firmware/android` — the node real Android bootloaders populate (`android,firmware`), carrying
/// an `android,vbmeta` child. Presence is itself a "this is an Android device" signal.
fn write_firmware_android(fdt: &mut FdtWriter) -> Result<(), DtbError> {
    let firmware = fdt.begin_node("firmware").map_err(fdt_err)?;
    let android = fdt.begin_node("android").map_err(fdt_err)?;
    fdt.property_string("compatible", "android,firmware")
        .map_err(fdt_err)?;
    let vbmeta = fdt.begin_node("vbmeta").map_err(fdt_err)?;
    fdt.property_string("compatible", "android,vbmeta")
        .map_err(fdt_err)?;
    fdt.property_string("parts", "vbmeta,boot,system,vendor,product,system_ext,odm")
        .map_err(fdt_err)?;
    fdt.end_node(vbmeta).map_err(fdt_err)?;
    fdt.end_node(android).map_err(fdt_err)?;
    fdt.end_node(firmware).map_err(fdt_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Identity string derivation
// ---------------------------------------------------------------------------------------------

/// The DT root `model` (a.k.a. the machine name), e.g. `"Qualcomm Technologies, Inc SM8550"`. This
/// is what the vendor kernel surfaces as `/proc/cpuinfo` `Hardware:` via `of_flat_dt_get_machine_name()`.
///
/// `soc.soc_manufacturer` holds the *property* form (`ro.soc.manufacturer=QTI`); the human machine
/// name spells it out, so `QTI` is expanded to the full Qualcomm marketing name a real Snapdragon
/// device prints. Other vendors pass through verbatim.
pub fn machine_name(profile: &DeviceProfile) -> String {
    let manufacturer = expand_soc_manufacturer(&profile.soc.soc_manufacturer);
    let model = profile.soc.soc_model.trim();
    match (manufacturer.is_empty(), model.is_empty()) {
        (false, false) => format!("{manufacturer} {model}"),
        (true, false) => model.to_string(),
        (false, true) => manufacturer,
        // Both blank: fall back to the build identity so the model is never empty.
        (true, true) => {
            let b = &profile.build;
            format!("{} {}", b.manufacturer, b.model).trim().to_string()
        }
    }
}

/// Expand the `ro.soc.manufacturer` short code into the human name a real device shows in its
/// `Hardware:` line. `QTI` / `Qualcomm*` → `"Qualcomm Technologies, Inc"`; everything else passes
/// through trimmed.
fn expand_soc_manufacturer(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_lowercase();
    if lower == "qti" || lower.starts_with("qualcomm") {
        "Qualcomm Technologies, Inc".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The DT root `compatible` stringlist, most-specific first, e.g. `["xiaomi,fuxi", "qcom,kalama"]`.
/// Built from the OEM `manufacturer,device` pair plus the `soc_vendor,board_platform` pair — the
/// exact shape a real OEM board DT uses. Never `"linux,dummy-virt"`; never empty.
pub fn root_compatible(profile: &DeviceProfile) -> Vec<String> {
    let mut compatible = Vec::new();

    let vendor = slug_or_empty(&profile.build.manufacturer);
    let device = slug_or_empty(&profile.build.device);
    if !vendor.is_empty() && !device.is_empty() {
        compatible.push(format!("{vendor},{device}"));
    }

    let soc_vendor = compat_soc_vendor(&profile.soc.soc_manufacturer);
    let platform = slug_or_empty(&profile.soc.board_platform);
    if !soc_vendor.is_empty() && !platform.is_empty() {
        compatible.push(format!("{soc_vendor},{platform}"));
    }

    // Never emit an empty `compatible`; a generic ARMv8 string is the safe, non-tell fallback.
    if compatible.is_empty() {
        compatible.push("arm,armv8".to_string());
    }
    compatible
}

/// The DT `compatible` vendor prefix for a SoC (`qcom`, `samsung`, `mediatek`, …). Qualcomm's short
/// code `QTI` maps to the canonical DT vendor prefix `qcom`.
fn compat_soc_vendor(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    if lower.is_empty() {
        String::new()
    } else if lower == "qti" || lower.starts_with("qualcomm") {
        "qcom".to_string()
    } else {
        slug_or_empty(raw)
    }
}

/// Map a human CPU part name (`"Cortex-A715 (gold)"`) to its DT `compatible`
/// (`"arm,cortex-a715"`). Covers both the `Cortex-A*` and `Cortex-X*` families — a 1+2+2+3 part
/// like SM8550 leads with a Cortex-X3 prime core, and falling that back to the generic
/// `"arm,armv8"` while its siblings carry real `arm,cortex-*` strings is itself an incoherence.
/// Falls back to `"arm,armv8"` only when no Cortex token is present at all (e.g. `"Kryo 585"`).
fn cpu_compatible(part_name: &str) -> String {
    let lower = part_name.to_lowercase();
    if let Some(idx) = lower.find("cortex-") {
        let token: String = lower[idx..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // token == "cortex-a715" / "cortex-x3"
        return format!("arm,{token}");
    }
    "arm,armv8".to_string()
}

/// Lowercase; keep `[a-z0-9-]`, drop everything else. Returns an **empty** string for
/// empty/punctuation-only input, so callers building `compatible` lists can drop the entry rather
/// than emit a bogus token.
fn slug_or_empty(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

fn fdt_err(e: vm_fdt::Error) -> DtbError {
    DtbError::Fdt(e.to_string())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aemulator_identity::profiles::reference_profile;

    /// Count non-overlapping-ish occurrences of `needle` in `haystack` (windowed count is fine for
    /// distinct node-name tokens like `cpu@`).
    fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || haystack.len() < needle.len() {
            return 0;
        }
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    #[test]
    fn build_fdt_from_reference_profile_is_valid_and_clean() {
        let blob = build_fdt(&reference_profile()).expect("reference profile should build a DTB");
        // A real FDT starts with the magic 0xd00dfeed.
        assert_eq!(&blob[0..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        assert_no_vm_tells(&blob).expect("reference DTB must carry no VM tells");
        // The SoC model string must be embedded.
        assert!(contains_bytes(&blob, b"SM8550"));
    }

    #[test]
    fn reference_dtb_has_the_full_qualcomm_machine_name() {
        // This exact string is what /proc/cpuinfo `Hardware:` reads via of_flat_dt_get_machine_name.
        let blob = build_fdt(&reference_profile()).unwrap();
        assert!(
            contains_bytes(&blob, b"Qualcomm Technologies, Inc SM8550"),
            "root model must be the full Qualcomm machine name"
        );
    }

    #[test]
    fn reference_dtb_carries_the_expected_soc_leaf_nodes() {
        let blob = build_fdt(&reference_profile()).unwrap();
        // Root compatible list (OEM board + SoC platform), never linux,dummy-virt.
        assert!(contains_bytes(&blob, b"xiaomi,fuxi"));
        assert!(contains_bytes(&blob, b"qcom,kalama"));
        // Heterogeneous CPU compatibles (X3 prime, A715/A710 gold, A510 silver).
        assert!(contains_bytes(&blob, b"arm,cortex-x3"));
        assert!(contains_bytes(&blob, b"arm,cortex-a715"));
        assert!(contains_bytes(&blob, b"arm,cortex-a710"));
        assert!(contains_bytes(&blob, b"arm,cortex-a510"));
        // Interrupt controller / timer / pmu / psci / firmware.
        assert!(contains_bytes(&blob, b"arm,gic-v3"));
        assert!(contains_bytes(&blob, b"arm,armv8-timer"));
        assert!(contains_bytes(&blob, b"arm,armv8-pmuv3"));
        assert!(contains_bytes(&blob, b"arm,psci-1.0"));
        assert!(contains_bytes(&blob, b"android,firmware"));
        // PSCI conduit is the real-hardware "smc", never the emulator "hvc".
        assert!(contains_bytes(&blob, b"smc"));
    }

    #[test]
    fn reference_dtb_emits_one_cpu_node_per_core() {
        // Snapdragon 8 Gen 2: 1 (X3) + 2 (A715) + 2 (A710) + 3 (A510) = 8 cores.
        let blob = build_fdt(&reference_profile()).unwrap();
        assert_eq!(
            count_bytes(&blob, b"cpu@"),
            8,
            "expected 8 cpu@<mpidr> nodes"
        );
    }

    #[test]
    fn build_fdt_rejects_a_profile_that_would_inject_a_tell() {
        // A poisoned profile (SoC model carries a goldfish tell) must be caught by build_fdt's
        // own assert_no_vm_tells self-check, not silently shipped.
        let mut poisoned = reference_profile();
        poisoned.soc.soc_model = "SM8550-goldfish".to_string();
        match build_fdt(&poisoned) {
            Err(DtbError::VmTell(t)) => assert_eq!(t, "goldfish"),
            other => panic!("expected VmTell(goldfish), got {other:?}"),
        }
    }

    #[test]
    fn assert_no_vm_tells_catches_dummy_virt() {
        let poisoned = b"....\x00model\x00linux,dummy-virt\x00....".to_vec();
        match assert_no_vm_tells(&poisoned) {
            Err(DtbError::VmTell(t)) => assert!(t.contains("dummy-virt")),
            other => panic!("expected VmTell, got {other:?}"),
        }
    }

    #[test]
    fn assert_no_vm_tells_catches_every_required_tell() {
        // The exact "NEVER emit" strings from the plan + task brief.
        for needle in [
            "linux,dummy-virt",
            "google,goldfish-battery",
            "qemu,vcpu-stall-detector",
            "virtual,kvm-cpufreq",
            "qemu,virtual-cpufreq",
            "pkvm,pviommu",
            "pkvm,device-power",
        ] {
            let mut blob = b"\xd0\x0d\xfe\xed padding ".to_vec();
            blob.extend_from_slice(needle.as_bytes());
            blob.push(0);
            assert!(
                assert_no_vm_tells(&blob).is_err(),
                "must catch the tell `{needle}`"
            );
        }
    }

    #[test]
    fn clean_tree_passes_assert() {
        let blob = build_fdt(&reference_profile()).unwrap();
        assert!(assert_no_vm_tells(&blob).is_ok());
    }

    #[test]
    fn machine_name_expands_qti_to_qualcomm() {
        let p = reference_profile();
        assert_eq!(machine_name(&p), "Qualcomm Technologies, Inc SM8550");
    }

    #[test]
    fn root_compatible_is_oem_then_soc_platform() {
        let p = reference_profile();
        assert_eq!(root_compatible(&p), vec!["xiaomi,fuxi", "qcom,kalama"]);
    }

    #[test]
    fn root_compatible_never_empty() {
        let p = DeviceProfile::default();
        // A blank profile still yields a valid, non-tell compatible.
        let c = root_compatible(&p);
        assert!(!c.is_empty());
        assert_eq!(c, vec!["arm,armv8"]);
    }

    #[test]
    fn cpu_compatible_maps_cortex_parts() {
        assert_eq!(cpu_compatible("Cortex-X3 (prime)"), "arm,cortex-x3");
        assert_eq!(cpu_compatible("Cortex-A715 (gold)"), "arm,cortex-a715");
        assert_eq!(cpu_compatible("Cortex-A510 (silver)"), "arm,cortex-a510");
        assert_eq!(cpu_compatible("Cortex-A77 (prime)"), "arm,cortex-a77");
        assert_eq!(cpu_compatible("Kryo 585"), "arm,armv8");
        assert_eq!(cpu_compatible(""), "arm,armv8");
    }

    #[test]
    fn empty_profile_still_builds_a_clean_valid_tree() {
        // No clusters, no SoC strings — must not panic, must be a valid FDT with no tells.
        let blob = build_fdt(&DeviceProfile::default()).expect("empty profile should still build");
        assert_eq!(&blob[0..4], &[0xd0, 0x0d, 0xfe, 0xed]);
        assert_no_vm_tells(&blob).unwrap();
        // Falls back to exactly one generic cpu node.
        assert_eq!(count_bytes(&blob, b"cpu@"), 1);
    }

    #[test]
    fn slug_or_empty_returns_empty_for_blank() {
        assert_eq!(slug_or_empty(""), "");
        assert_eq!(slug_or_empty("  , "), "");
        assert_eq!(slug_or_empty("Xiaomi"), "xiaomi");
        assert_eq!(slug_or_empty("fuxi"), "fuxi");
        assert_eq!(
            slug_or_empty("Qualcomm Technologies, Inc."),
            "qualcommtechnologiesinc"
        );
    }

    #[test]
    fn compat_soc_vendor_maps_qti_to_qcom() {
        assert_eq!(compat_soc_vendor("QTI"), "qcom");
        assert_eq!(compat_soc_vendor("Qualcomm"), "qcom");
        assert_eq!(compat_soc_vendor("Samsung"), "samsung");
        assert_eq!(compat_soc_vendor(""), "");
    }
}
