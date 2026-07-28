//! arm64 Linux boot protocol: load the kernel `Image` and build a minimal FDT the kernel can
//! parse to bring up `earlycon`.
//!
//! Layout inside guest DRAM (base `0x8000_0000`):
//!   * kernel `Image` at `DRAM_BASE + text_offset` (2 MiB-aligned base) — via `linux-loader`'s PE
//!     loader, which validates the `ARM\x64` magic and honours the header's `text_offset`.
//!   * FDT blob at `DRAM_BASE + FDT_OFFSET` (well above the kernel, still within the first 512 MiB
//!     of the kernel image as the boot protocol requires), on an 8-byte boundary, < 2 MiB.
//!
//! Boot regs (set by `vcpu`): PC = kernel entry, X0 = FDT IPA, X1..X3 = 0, PSTATE = EL1h+DAIF.

use crate::gic::{GICD_BASE, GICD_SIZE, GICR_BASE, GICR_FRAME_SIZE};
use crate::memory::{GuestRam, DRAM_BASE};
use crate::virtio;
use linux_loader::loader::pe::PE;
use linux_loader::loader::KernelLoader;
use std::fs::File;
use std::path::Path;
use vm_fdt::FdtWriter;
use vm_memory::{Address, Bytes, GuestAddress};

// GICv3 FDT interrupt-cell conventions (crosvm aarch64/src/fdt.rs).
const GIC_FDT_IRQ_TYPE_SPI: u32 = 0;
const GIC_FDT_IRQ_TYPE_PPI: u32 = 1;
const IRQ_TYPE_EDGE_RISING: u32 = 0x1;
const IRQ_TYPE_LEVEL_LOW: u32 = 0x8;
const GIC_PHANDLE: u32 = 1;
const CLK_PHANDLE: u32 = 0x8000;

/// Offset of the FDT within DRAM (256 MiB in): above any plausible kernel image, comfortably
/// inside the first 512 MiB, 2 MiB-aligned so it never shares a 2 MiB block with kernel data.
pub const FDT_OFFSET: u64 = 0x1000_0000;

/// Offset of the initrd within DRAM (384 MiB in): above the FDT, still inside a 512 MiB region.
pub const INITRD_OFFSET: u64 = 0x1800_0000;

/// Result of staging the guest for boot.
pub struct BootPlan {
    /// Kernel entry point (guest IPA) — goes into PC.
    pub entry_ipa: u64,
    /// FDT blob address (guest IPA) — goes into X0.
    pub fdt_ipa: u64,
}

/// Load the kernel `Image` into guest RAM and install a minimal FDT. `mem_size` is the DRAM size
/// in bytes (used for the `/memory` node). `initrd_path`, when set, is copied high in RAM and
/// described to the kernel via `/chosen/linux,initrd-{start,end}`. `num_virtio` virtio-mmio
/// transports get generic `/virtio_mmio@…` nodes. Returns the entry + FDT addresses for the vCPU.
pub fn stage_boot(
    ram: &GuestRam,
    kernel_path: &Path,
    cmdline: &str,
    num_cpus: u64,
    initrd_path: Option<&Path>,
    num_virtio: usize,
    pci_irqs: &[(u8, u32)],
    bootconfig: Option<&[u8]>,
    dt_identity: Option<(&[String], &str)>,
) -> Result<BootPlan, String> {
    // --- 1. Load the arm64 Image. PE::load validates magic + applies text_offset. ---
    let mut file =
        File::open(kernel_path).map_err(|e| format!("open kernel {kernel_path:?}: {e}"))?;
    let loaded = PE::load(&ram.mem, Some(GuestAddress(DRAM_BASE)), &mut file, None)
        .map_err(|e| format!("linux-loader PE::load failed: {e:?}"))?;
    let entry_ipa = loaded.kernel_load.raw_value();

    // --- 2. Load the initrd (raw copy high in RAM). ---
    let initrd = match initrd_path {
        Some(p) => {
            let mut bytes = std::fs::read(p).map_err(|e| format!("read initrd {p:?}: {e}"))?;
            let ramdisk_len = bytes.len();
            // Android bootconfig: append the blob + the `#BOOTCONFIG` footer the kernel scans for
            // from the *end* of the initrd (init/main.c setup_boot_config). The trailing bytes are
            // ignored by the initramfs (LZ4/cpio) decompressor, which stops at the archive end.
            if let Some(bc) = bootconfig {
                append_bootconfig(&mut bytes, bc);
                println!(
                    "[ok] appended bootconfig: {} bytes + 20-byte #BOOTCONFIG footer (ramdisk {ramdisk_len} B → {} B)",
                    bc.len(),
                    bytes.len()
                );
            }
            let start = DRAM_BASE + INITRD_OFFSET;
            let end = start + bytes.len() as u64;
            if end > DRAM_BASE + ram.size as u64 {
                return Err(format!(
                    "initrd ({} bytes) overflows guest RAM at {:#x}",
                    bytes.len(),
                    end
                ));
            }
            ram.mem
                .write_slice(&bytes, GuestAddress(start))
                .map_err(|e| format!("write initrd to guest RAM @ {start:#x}: {e:?}"))?;
            println!(
                "[ok] initrd: {} bytes @ IPA {:#x}..{:#x}",
                bytes.len(),
                start,
                end
            );
            Some((start, end))
        }
        None => None,
    };

    // --- 3. Build + install the FDT. ---
    let fdt = build_fdt(
        ram.size as u64,
        cmdline,
        num_cpus,
        initrd,
        num_virtio,
        pci_irqs,
        dt_identity,
    )
    .map_err(|e| format!("build FDT: {e}"))?;
    if fdt.len() as u64 > 0x20_0000 {
        return Err(format!("FDT too large: {} bytes", fdt.len()));
    }
    let fdt_ipa = DRAM_BASE + FDT_OFFSET;
    ram.mem
        .write_slice(&fdt, GuestAddress(fdt_ipa))
        .map_err(|e| format!("write FDT to guest RAM @ {fdt_ipa:#x}: {e:?}"))?;

    Ok(BootPlan { entry_ipa, fdt_ipa })
}

/// Append an Android **bootconfig** section to the tail of the initrd image, in the exact layout
/// the kernel scans for from `initrd_end` (init/main.c `get_boot_config_from_initrd`):
///
/// ```text
///   [ bootconfig text (`size` bytes) ][ u32 size ][ u32 checksum ][ "#BOOTCONFIG\n" (12 bytes) ]
/// ```
///
/// `checksum` is the plain byte-sum of the text (kernel `xbc_calc_checksum`). The text is forced to
/// end in a newline. The magic MUST be the final bytes of the initrd, so the caller sets
/// `linux,initrd-end` to cover the whole appended region.
fn append_bootconfig(initrd: &mut Vec<u8>, bootconfig: &[u8]) {
    const MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";
    let mut text = bootconfig.to_vec();
    if text.last() != Some(&b'\n') {
        text.push(b'\n');
    }
    let size = text.len() as u32;
    let csum = text.iter().fold(0u32, |acc, &b| acc.wrapping_add(b as u32));
    initrd.extend_from_slice(&text);
    initrd.extend_from_slice(&size.to_le_bytes());
    initrd.extend_from_slice(&csum.to_le_bytes());
    initrd.extend_from_slice(MAGIC);
}

/// A flattened device tree describing: memory, `num_cpus` PSCI-enabled CPUs, the PSCI node, a
/// userspace **GICv3** (`/intc`, `arm,gic-v3`), the **generic timer** (`/timer`, `arm,armv8-timer`,
/// PPIs 13/14/11/10), and a PL011 UART wired to an SPI. `interrupt-parent` at the root points every
/// device at the GIC, so the guest brings up its interrupt controller and architected timer instead
/// of panicking as it did in the BootCore phase.
fn build_fdt(
    mem_size: u64,
    cmdline: &str,
    num_cpus: u64,
    initrd: Option<(u64, u64)>,
    num_virtio: usize,
    pci_irqs: &[(u8, u32)],
    dt_identity: Option<(&[String], &str)>,
) -> Result<Vec<u8>, vm_fdt::Error> {
    let mut fdt = FdtWriter::new()?;

    let root = fdt.begin_node("")?;
    // Phase 5 (stealth): stamp the real-SoC root `compatible`/`model` when the Android path supplies
    // a DeviceProfile identity (e.g. `["xiaomi,fuxi","qcom,kalama"]` / `"Qualcomm Technologies, Inc
    // SM8550"`). `of_flat_dt_get_machine_name()` surfaces `model` as `/proc/cpuinfo` `Hardware:`, and
    // the root `compatible` is what a board-DT sniffer reads. Without an identity (Linux path) the
    // Phase-1 placeholders are kept so those regression boots stay byte-identical.
    match dt_identity {
        Some((compatible, model)) if !compatible.is_empty() => {
            fdt.property_string_list("compatible", compatible.to_vec())?;
            fdt.property_u32("#address-cells", 0x2)?;
            fdt.property_u32("#size-cells", 0x2)?;
            fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;
            fdt.property_string("model", model)?;
        }
        _ => {
            fdt.property_string("compatible", "linux,dummy-virt")?;
            fdt.property_u32("#address-cells", 0x2)?;
            fdt.property_u32("#size-cells", 0x2)?;
            fdt.property_u32("interrupt-parent", GIC_PHANDLE)?;
            fdt.property_string("model", "aemulator-hvf")?;
        }
    }

    // /chosen — bootargs carry the earlycon spec; stdout-path points at the UART. When an initrd
    // is staged, describe it so the kernel unpacks it as the initramfs root.
    let chosen = fdt.begin_node("chosen")?;
    fdt.property_string("bootargs", cmdline)?;
    fdt.property_string("stdout-path", "/pl011@9000000")?;
    // With a PCI host bridge present, keep the firmware-assigned BARs — we pre-program each device's
    // BAR0 and route MMIO to it, so the guest must not re-size or reassign.
    if !pci_irqs.is_empty() {
        fdt.property_u32("linux,pci-probe-only", 1)?;
    }
    if let Some((start, end)) = initrd {
        fdt.property_u64("linux,initrd-start", start)?;
        fdt.property_u64("linux,initrd-end", end)?;
    }
    fdt.end_node(chosen)?;

    // /memory@80000000
    let memory = fdt.begin_node("memory@80000000")?;
    fdt.property_string("device_type", "memory")?;
    fdt.property_array_u64("reg", &[DRAM_BASE, mem_size])?;
    fdt.end_node(memory)?;

    // /cpus/cpu@N — one per vCPU, flat Aff0 = index (matches MPIDR + redistributor affinity).
    let cpus = fdt.begin_node("cpus")?;
    fdt.property_u32("#address-cells", 0x1)?;
    fdt.property_u32("#size-cells", 0x0)?;
    for i in 0..num_cpus {
        let cpu = fdt.begin_node(&format!("cpu@{i}"))?;
        fdt.property_string("device_type", "cpu")?;
        fdt.property_string("compatible", "arm,armv8")?;
        fdt.property_string("enable-method", "psci")?;
        fdt.property_u32("reg", i as u32)?;
        fdt.end_node(cpu)?;
    }
    fdt.end_node(cpus)?;

    // /psci — method hvc, so the guest traps to us on PSCI calls (VERSION, CPU_ON, …).
    let psci = fdt.begin_node("psci")?;
    fdt.property_string_list(
        "compatible",
        vec![
            "arm,psci-1.0".to_string(),
            "arm,psci-0.2".to_string(),
            "arm,psci".to_string(),
        ],
    )?;
    fdt.property_string("method", "hvc")?;
    fdt.end_node(psci)?;

    // /intc — GICv3. reg = [GICD base+size, GICR base+size]; redist region is one frame per cpu.
    let redist_size = GICR_FRAME_SIZE * num_cpus;
    let intc = fdt.begin_node(&format!("intc@{GICD_BASE:x}"))?;
    fdt.property_string("compatible", "arm,gic-v3")?;
    fdt.property_null("interrupt-controller")?;
    fdt.property_u32("#interrupt-cells", 0x3)?;
    fdt.property_u32("#address-cells", 0x2)?;
    fdt.property_u32("#size-cells", 0x2)?;
    fdt.property_array_u64("reg", &[GICD_BASE, GICD_SIZE, GICR_BASE, redist_size])?;
    fdt.property_u32("phandle", GIC_PHANDLE)?;
    fdt.end_node(intc)?;

    // /timer — architected generic timer. Secure-phys / non-secure-phys / virtual / hyp PPIs.
    let cpu_mask = (((1u32 << num_cpus) - 1) << 8) & 0xff00;
    let flags = cpu_mask | IRQ_TYPE_LEVEL_LOW;
    let timer = fdt.begin_node("timer")?;
    fdt.property_string("compatible", "arm,armv8-timer")?;
    fdt.property_array_u32(
        "interrupts",
        &[
            GIC_FDT_IRQ_TYPE_PPI, 13, flags,
            GIC_FDT_IRQ_TYPE_PPI, 14, flags,
            GIC_FDT_IRQ_TYPE_PPI, 11, flags,
            GIC_FDT_IRQ_TYPE_PPI, 10, flags,
        ],
    )?;
    fdt.property_null("always-on")?;
    fdt.end_node(timer)?;

    // A fixed 24 MHz clock for the UART's apb_pclk (referenced by the pl011 node).
    let clk = fdt.begin_node("apb-pclk")?;
    fdt.property_string("compatible", "fixed-clock")?;
    fdt.property_u32("#clock-cells", 0x0)?;
    fdt.property_u32("clock-frequency", 24_000_000)?;
    fdt.property_string("clock-output-names", "clk24mhz")?;
    fdt.property_u32("phandle", CLK_PHANDLE)?;
    fdt.end_node(clk)?;

    // /pl011@9000000 — SPI 1 (→ INTID 33). The kernel's polled console_write drives TX regardless,
    // so declaring the IRQ satisfies the amba-pl011 probe without us needing to inject it yet.
    let uart = fdt.begin_node("pl011@9000000")?;
    fdt.property_string_list(
        "compatible",
        vec!["arm,pl011".to_string(), "arm,primecell".to_string()],
    )?;
    fdt.property_array_u64("reg", &[0x0900_0000, 0x1000])?;
    fdt.property_array_u32("interrupts", &[GIC_FDT_IRQ_TYPE_SPI, 1, IRQ_TYPE_EDGE_RISING])?;
    fdt.property_array_u32("clocks", &[CLK_PHANDLE, CLK_PHANDLE])?;
    fdt.property_string_list(
        "clock-names",
        vec!["uartclk".to_string(), "apb_pclk".to_string()],
    )?;
    fdt.property_string("status", "okay")?;
    fdt.end_node(uart)?;

    // /virtio_mmio@… — one generic node per transport. The device TYPE is discovered at runtime by
    // the guest reading the DeviceID register, so every node is identical bar reg + interrupts.
    // Each is wired to an edge-rising SPI (matches our one-shot GIC injection on used-buffer).
    for i in 0..num_virtio {
        let ipa = virtio::device_ipa(i);
        let spi = virtio::device_spi(i);
        let node = fdt.begin_node(&format!("virtio_mmio@{ipa:x}"))?;
        fdt.property_string("compatible", "virtio,mmio")?;
        fdt.property_array_u64("reg", &[ipa, virtio::VIRTIO_MMIO_STRIDE])?;
        fdt.property_array_u32(
            "interrupts",
            &[GIC_FDT_IRQ_TYPE_SPI, spi, IRQ_TYPE_EDGE_RISING],
        )?;
        fdt.end_node(node)?;
    }

    // /pci@… — generic PCI host bridge (CAM). Only emitted when the PCI fleet is non-empty (Android).
    // The Cuttlefish GKI kernel is virtio-pci-only; this node lets it enumerate our virtio devices.
    if !pci_irqs.is_empty() {
        use crate::virtio::pci::{PCI_CAM_BASE, PCI_CAM_SIZE, PCI_MMIO_BASE, PCI_MMIO_SIZE};
        const IRQ_TYPE_LEVEL_HIGH: u32 = 0x4;
        const PCI_PHANDLE: u32 = 0x9000;

        let node = fdt.begin_node(&format!("pci@{PCI_CAM_BASE:x}"))?;
        fdt.property_string("compatible", "pci-host-cam-generic")?;
        fdt.property_string("device_type", "pci")?;
        fdt.property_u32("#address-cells", 0x3)?;
        fdt.property_u32("#size-cells", 0x2)?;
        fdt.property_u32("#interrupt-cells", 0x1)?;
        fdt.property_array_u64("reg", &[PCI_CAM_BASE, PCI_CAM_SIZE])?;
        fdt.property_array_u32("bus-range", &[0, 0])?;
        // ranges: one 32-bit non-prefetchable memory window, identity-mapped.
        //   child:  [ (space=0b10)<<24 , bus_addr_hi, bus_addr_lo ]
        //   parent: [ cpu_hi, cpu_lo ]   size: [ hi, lo ]
        // Under `--features gfxstream` a second **64-bit prefetchable** window is appended for the
        // virtio-gpu host-visible shared-memory BAR (`PCI_SHM_BASE`, `PCI_SHM_SIZE`) — the blob-map
        // region the gfxstream ASG ring lives in. Space code 0x4300_0000 = prefetchable(0x40) |
        // 64-bit memory(0x03). Only emitted under the feature so the default build's FDT is unchanged.
        #[allow(unused_mut)]
        let mut ranges: Vec<u32> = vec![
            0x0200_0000,
            (PCI_MMIO_BASE >> 32) as u32,
            PCI_MMIO_BASE as u32,
            (PCI_MMIO_BASE >> 32) as u32,
            PCI_MMIO_BASE as u32,
            (PCI_MMIO_SIZE >> 32) as u32,
            PCI_MMIO_SIZE as u32,
        ];
        {
            use crate::virtio::pci::{PCI_SHM_BASE, PCI_SHM_SIZE};
            ranges.extend_from_slice(&[
                0x4300_0000,
                (PCI_SHM_BASE >> 32) as u32,
                PCI_SHM_BASE as u32,
                (PCI_SHM_BASE >> 32) as u32,
                PCI_SHM_BASE as u32,
                (PCI_SHM_SIZE >> 32) as u32,
                PCI_SHM_SIZE as u32,
            ]);
        }
        fdt.property_array_u32("ranges", &ranges)?;
        fdt.property_null("dma-coherent")?;
        fdt.property_u32("phandle", PCI_PHANDLE)?;

        // interrupt-map: route each device's INTA to its GIC SPI (level-high, per PCI convention).
        let mut imap: Vec<u32> = Vec::new();
        for &(dev, spi) in pci_irqs {
            imap.extend_from_slice(&[
                (dev as u32) << 11, // child PCI addr hi (device number in bits [15:11])
                0,
                0,
                1,           // INTA#
                GIC_PHANDLE, // interrupt parent = GIC
                0,
                0,
                GIC_FDT_IRQ_TYPE_SPI,
                spi,
                IRQ_TYPE_LEVEL_HIGH,
            ]);
        }
        fdt.property_array_u32("interrupt-map", &imap)?;
        fdt.property_array_u32("interrupt-map-mask", &[0xf800, 0, 0, 0x7])?;
        fdt.end_node(node)?;
    }

    fdt.end_node(root)?;
    fdt.finish()
}
