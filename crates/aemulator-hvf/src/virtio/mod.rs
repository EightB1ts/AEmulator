//! A minimal but real **virtio-mmio (modern / v2)** transport plus three backends — rng, console
//! and block — enough for a Linux guest to negotiate features, set up split virtqueues, and reach
//! an interactive shell over `hvc0`.
//!
//! Layout: N devices packed at [`VIRTIO_MMIO_BASE`] + i*[`VIRTIO_MMIO_STRIDE`], each a modern
//! MMIO v2 register block (`MagicValue`/`Version=2`/`DeviceID`/feature + queue registers + a
//! per-device config space at `0x100`). Each device is wired to one SPI; when a queue notify
//! drains used buffers the transport raises its SPI through the userspace GIC.
//!
//! The virtqueue descriptor/avail/used ring walking is done with rust-vmm's `virtio-queue`
//! (`Queue` + `QueueT`) over `vm-memory`, so the ring semantics are the audited upstream ones and
//! we only own the device behaviour.

pub mod block;
/// virtio-block GPT **composite** disk backend (Android/Cuttlefish by-name partitions).
pub mod block_composite;
pub mod console;
pub mod gpu;
/// virtio-gpu 3D/virgl transport (Phase 3c). Behind `--features gfxstream` — the default build never
/// compiles it, so the 2D scanout path stays free of any gfxstream/Metal link.
pub mod gpu3d;
pub mod input;
pub mod mmio;
pub mod net;
/// virtio-**PCI** (modern) transport + a generic PCI host bridge — the transport the Cuttlefish
/// android14 GKI kernel actually speaks (`CONFIG_VIRTIO_MMIO=n`, `virtio_pci.ko` only).
pub mod pci;
pub mod rng;
/// In-process virtqueue harness for the device tests — a real `virtio_queue::Queue` over real guest
/// memory, programmed exactly the way [`pci`] programs it from the guest's config registers. Test-only
/// (`#[cfg(test)]`), so it never reaches a shipped binary.
#[cfg(test)]
pub mod testkit;
pub mod vsock;

pub use gpu::ScanoutHandle;
pub use mmio::{Backend, MmioTransport};
pub use pci::{PciBus, PciDevice};

use vm_memory::GuestMemoryMmap;

/// Base IPA of the virtio-mmio window. Sits in the low-MMIO hole below DRAM (`0x8000_0000`),
/// clear of the PL011 (`0x0900_0000`) and the GIC (`0x3fxx_xxxx`). Matches the region crosvm/QEMU
/// place virtio-mmio transports in.
pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;

/// Per-device register-block stride. 0x200 is the QEMU/crosvm convention (each transport owns
/// `[base + i*0x200, base + (i+1)*0x200)`).
pub const VIRTIO_MMIO_STRIDE: u64 = 0x200;

/// First SPI (distributor-relative) handed to a virtio device. INTID = 32 + SPI. 16 keeps clear of
/// the low SPIs and of the PL011's SPI 1.
pub const VIRTIO_FIRST_SPI: u32 = 16;

/// virtio device type ids (virtio spec §5).
pub const VIRTIO_ID_NET: u32 = 1;
pub const VIRTIO_ID_BLOCK: u32 = 2;
pub const VIRTIO_ID_CONSOLE: u32 = 3;
pub const VIRTIO_ID_RNG: u32 = 4;
pub const VIRTIO_ID_GPU: u32 = 16;
pub const VIRTIO_ID_INPUT: u32 = 18;
pub const VIRTIO_ID_VSOCK: u32 = 19;

/// `VIRTIO_F_VERSION_1` (feature bit 32) — mandatory for a modern (v2) device.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Standard negotiated virtqueue size we advertise for every queue (a power of two).
pub const QUEUE_SIZE: u16 = 256;

/// Convenience type alias for the guest memory the backends read/write.
pub type Mem = GuestMemoryMmap;

/// INTID (GIC absolute) of the `i`-th virtio device's SPI.
pub fn device_intid(index: usize) -> u32 {
    32 + VIRTIO_FIRST_SPI + index as u32
}

/// Distributor-relative SPI number of the `i`-th virtio device (for the FDT `interrupts` cell).
pub fn device_spi(index: usize) -> u32 {
    VIRTIO_FIRST_SPI + index as u32
}

/// IPA of the `i`-th virtio device's register block.
pub fn device_ipa(index: usize) -> u64 {
    VIRTIO_MMIO_BASE + index as u64 * VIRTIO_MMIO_STRIDE
}

/// If `ipa` lands in the virtio-mmio window for one of `count` devices, return
/// `(device_index, register_offset)`.
pub fn locate(ipa: u64, count: usize) -> Option<(usize, u64)> {
    if ipa < VIRTIO_MMIO_BASE {
        return None;
    }
    let rel = ipa - VIRTIO_MMIO_BASE;
    let idx = (rel / VIRTIO_MMIO_STRIDE) as usize;
    if idx >= count {
        return None;
    }
    Some((idx, rel % VIRTIO_MMIO_STRIDE))
}
