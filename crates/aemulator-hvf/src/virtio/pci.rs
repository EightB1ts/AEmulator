//! A minimal **virtio-pci (modern / 1.0)** transport plus a generic **PCI host bridge** (CAM config
//! access), enough for a virtio-PCI *guest* — like the Cuttlefish android14 GKI kernel, which is
//! built `CONFIG_VIRTIO_MMIO=n` / `CONFIG_PCI=y` and ships only `virtio_pci.ko` — to enumerate our
//! virtio devices over PCI, negotiate features, program split virtqueues, and drive them.
//!
//! Why this exists: the pass-2 GPT composite disk was correct but never *read* — the guest has no
//! virtio-MMIO transport, so our MMIO devices were invisible. This module presents the SAME
//! [`Backend`] fleet (console, rng, block, gpu, input, net) over PCI instead.
//!
//! Design (kept deliberately small):
//!   * **One PCI segment, bus 0**, `pci-host-cam-generic` addressing: config offset
//!     `= (bus<<16)|(dev<<11)|(func<<8)|reg`, 256 bytes/function. Each virtio device is a single
//!     function at `00:<dev>.0`.
//!   * **virtio-pci-modern** capability layout inside one 32-bit non-prefetchable memory **BAR0**:
//!     common-cfg @ `0x0000`, notify @ `0x1000` (`notify_off_multiplier = 0` → all queues notify at
//!     the one address, value written = the vq index), ISR @ `0x2000`, device-cfg @ `0x3000`.
//!   * **Legacy INTx** (INTA) → a GIC SPI. No MSI-X (we advertise `msix_config`/`queue_msix_vector`
//!     as `NO_VECTOR = 0xffff`), so Linux virtio-pci falls back to INTx, routed by the FDT
//!     `interrupt-map`. On a used-buffer notification we set ISR bit0 and raise the device's SPI —
//!     the same one-shot injection the MMIO transport used.
//!   * BARs are pre-assigned; the FDT sets `linux,pci-probe-only = 1` so the guest keeps them.

use super::mmio::Backend;
use super::{Mem, QUEUE_SIZE};
use virtio_queue::{Queue, QueueT};

/// Base IPA of the PCI CAM configuration window (bus 0 lives in the first 64 KiB). Sits in the MMIO
/// hole below the GIC redistributor (`0x3fde_0000`).
pub const PCI_CAM_BASE: u64 = 0x3000_0000;
/// CAM window size — one bus is 64 KiB; we size 1 MiB (16 buses) of headroom. `bus-range` = [0,0].
pub const PCI_CAM_SIZE: u64 = 0x0010_0000;

/// Base IPA of the 32-bit non-prefetchable PCI MMIO window that backs every device's BAR0.
pub const PCI_MMIO_BASE: u64 = 0x1000_0000;
/// Size of the PCI MMIO window (256 MiB — vastly more than the handful of 16 KiB BARs we assign).
pub const PCI_MMIO_SIZE: u64 = 0x1000_0000;

/// Base IPA of the **64-bit prefetchable** PCI MMIO window that backs the virtio-gpu host-visible
/// shared-memory BAR (shmid 0). This is the guest-IPA window gfxstream blob maps
/// (`RESOURCE_MAP_BLOB`) land in: on a map the VMM `hv_vm_map`s the host pointer
/// `stream_renderer_resource_map` returns to `PCI_SHM_BASE + guest_offset`, so the guest's Mesa
/// `gfxstream_vk` ASG ring reads/writes host memory zero-copy.
///
/// **INVARIANT: `DRAM_BASE + ram_bytes <= PCI_SHM_BASE`.** RAM is mapped RWX at `DRAM_BASE` and this
/// window is advertised to the guest as 64-bit prefetchable PCI MMIO in the FDT (`boot.rs`). If they
/// overlap, Linux cannot assign PCI resources in the window and the whole PCI fleet degrades —
/// including virtio-blk, so `/dev/vda` never appears and Android dies in first-stage init with
/// "Failed to create devices required for first stage mount", then reboot-loops. Enforced at startup
/// in `machine.rs`; the failure mode is invisible from the guest console otherwise.
///
/// Was 8 GiB (`0x2_0000_0000`), documented as "well above the largest guest DRAM top (4 GiB RAM ends
/// at 0x1_8000_0000)". That assumption broke once profiles could ask for more: 6144 MiB ends exactly
/// AT the old base (safe by one byte), and 8192 MiB overlapped it by a full GiB. Now at 16 GiB,
/// which clears a 12288 MiB guest (top `0x3_8000_0000`) — the largest the Studio offers — and stays
/// inside HVF's IPA space on Apple Silicon.
pub const PCI_SHM_BASE: u64 = 0x4_0000_0000;
/// Size of the host-visible shared-memory window (1 GiB of guest IPA reserved for blob maps).
pub const PCI_SHM_SIZE: u64 = 0x4000_0000;

/// Per-device BAR0 size/stride (16 KiB — holds the four virtio-modern sub-regions at 4 KiB spacing).
const BAR_SIZE: u64 = 0x4000;

/// virtio shared-memory capability (`VIRTIO_PCI_CAP_SHARED_MEMORY_CFG`) shmid for the virtio-gpu
/// host-visible region. **Must be `1`** (`VIRTIO_GPU_SHM_ID_HOST_VISIBLE`, per Linux
/// `virtio_gpu.h` and crosvm `protocol.rs`); id `0` is `SHM_ID_NONE`/undefined, so a cap with id 0
/// is never matched by
/// the guest kernel's `virtio_get_shm_region(HOST_VISIBLE)` → no host-visible region → Mesa
/// gfxstream_vk fails "VirtGpuDevice must support host-visible memory / Failed to create ASG stream".
const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 1;
const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;

// virtio-pci-modern sub-region offsets inside BAR0.
const COMMON_CFG_OFF: u64 = 0x0000;
const NOTIFY_OFF: u64 = 0x1000;
const ISR_OFF: u64 = 0x2000;
const DEVICE_CFG_OFF: u64 = 0x3000;
const REGION_LEN: u32 = 0x1000;

const PCI_VENDOR_REDHAT: u16 = 0x1af4;
const VIRTIO_PCI_CAP_VNDR: u8 = 0x09;
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_ISR_QUEUE: u8 = 0x1;

/// The virtio-modern device-id offset (`0x1040 + virtio_device_type`).
const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040;

/// One virtio device exposed over PCI. Owns its split-virtqueue state + the modern register block.
pub struct PciDevice {
    pub backend: Backend,
    /// Absolute GIC INTID this device raises for its INTA line.
    pub intid: u32,
    /// PCI device (slot) number on bus 0. Function is always 0.
    pub dev_num: u8,
    queues: Vec<Queue>,

    /// Pre-assigned BAR0 base IPA (updated if the guest ever reprograms it).
    bar0: u32,
    /// Set while the guest is BAR-sizing (wrote all-ones; next read returns the size mask).
    bar0_sizing: bool,

    /// Optional 64-bit prefetchable **host-visible shared-memory BAR** (BAR2/BAR3), present only on
    /// the virtio-gpu device when the gfxstream renderer + `VIRTIO_GPU_F_RESOURCE_BLOB` are active.
    /// `(base_ipa, size)` — the guest's blob-map window (`PCI_SHM_BASE`, `PCI_SHM_SIZE`). It is NOT
    /// MMIO-trapped: sub-regions are backed on demand by `hv_vm_map` of gfxstream host pointers, so
    /// guest reads/writes to a mapped blob hit real host memory with no VM exit. `None` = no BAR2.
    shm_bar: Option<(u64, u64)>,
    /// Sizing latch for the low/high halves of the 64-bit BAR2 (all-ones written → next read = mask).
    bar2_sizing: bool,
    bar3_sizing: bool,

    // ---- common-cfg register state (virtio spec §4.1.4.3) ----
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    device_status: u8,
    queue_select: u16,
    config_generation: u8,
    isr: u8,

    // Cached 256-byte config header (vendor/device/class + the virtio capability chain). BAR0 and a
    // couple of RW registers are patched live; everything else is served straight from here.
    cfg: [u8; 256],
    /// PCI command register (bit1 = mem-space enable, bit2 = bus-master).
    command: u16,
}

impl PciDevice {
    pub fn new(backend: Backend, intid: u32, dev_num: u8, bar0: u32) -> Self {
        let n = backend.num_queues();
        let queues = (0..n)
            .map(|_| Queue::new(QUEUE_SIZE).expect("QUEUE_SIZE is a valid power of two"))
            .collect();
        let mut d = Self {
            backend,
            intid,
            dev_num,
            queues,
            bar0,
            bar0_sizing: false,
            shm_bar: None,
            bar2_sizing: false,
            bar3_sizing: false,
            device_feature_select: 0,
            driver_feature_select: 0,
            driver_features: 0,
            device_status: 0,
            queue_select: 0,
            config_generation: 0,
            isr: 0,
            cfg: [0u8; 256],
            command: 0,
        };
        d.build_config_header();
        d
    }

    /// Attach a 64-bit prefetchable host-visible shared-memory BAR (BAR2/BAR3) at `base`/`size` and
    /// rebuild the config header so it carries BAR2 + the virtio shared-memory capability (shmid 0)
    /// the guest's virtio-gpu driver reads to find its host-visible blob-map region. Only the GPU
    /// device (with a gfxstream renderer) calls this.
    pub fn attach_shm_bar(&mut self, base: u64, size: u64) {
        self.shm_bar = Some((base, size));
        self.build_config_header();
    }

    fn virtio_type(&self) -> u32 {
        self.backend.device_type()
    }

    /// The virtio device-type id of the backend (for logging).
    pub fn backend_type(&self) -> u32 {
        self.virtio_type()
    }

    /// PCI class code (24-bit: base<<16 | sub<<8 | prog). Only cosmetic — Linux virtio-pci binds by
    /// vendor/device id, not class.
    fn class_code(&self) -> u32 {
        match self.virtio_type() {
            super::VIRTIO_ID_NET => 0x02_0000,   // network controller
            super::VIRTIO_ID_BLOCK => 0x01_8000, // mass storage / other
            super::VIRTIO_ID_GPU => 0x03_8000,   // display controller / other
            _ => 0xff_0000,                      // unclassified
        }
    }

    fn build_config_header(&mut self) {
        let vtype = self.virtio_type() as u16;
        let device_id = VIRTIO_PCI_DEVICE_ID_BASE + vtype;
        let class = self.class_code();
        let bar0 = self.bar0;
        let put16 = |cfg: &mut [u8; 256], off: usize, v: u16| {
            cfg[off..off + 2].copy_from_slice(&v.to_le_bytes());
        };
        let put32 = |cfg: &mut [u8; 256], off: usize, v: u32| {
            cfg[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        self.cfg = [0u8; 256];
        put16(&mut self.cfg, 0x00, PCI_VENDOR_REDHAT); // vendor id
        put16(&mut self.cfg, 0x02, device_id); // device id (modern)
        put16(&mut self.cfg, 0x04, 0x0000); // command (guest programs this)
        put16(&mut self.cfg, 0x06, 0x0010); // status: bit4 = capabilities list
                                            // revision (0x01 = modern) + class code.
        put32(&mut self.cfg, 0x08, (class << 8) | 0x01);
        self.cfg[0x0c] = 0; // cache line size
        self.cfg[0x0e] = 0x00; // header type 0 (single function)
                               // BAR0 (patched live in config_read).
        put32(&mut self.cfg, 0x10, bar0);
        // BAR2/BAR3: the 64-bit prefetchable host-visible shared-memory window, when present.
        // Memory BAR encoding: bit0=0 (memory), bits2:1=10 (64-bit), bit3=1 (prefetchable) → 0x0C.
        if let Some((base, _size)) = self.shm_bar {
            put32(&mut self.cfg, 0x18, (base as u32 & 0xffff_fff0) | 0x0c);
            put32(&mut self.cfg, 0x1c, (base >> 32) as u32);
        }
        // Subsystem vendor / id. Subsystem id = virtio device type (legacy identification aid).
        put16(&mut self.cfg, 0x2c, PCI_VENDOR_REDHAT);
        put16(&mut self.cfg, 0x2e, vtype);
        self.cfg[0x34] = 0x40; // capabilities pointer
        self.cfg[0x3d] = 0x01; // interrupt pin = INTA
        self.cfg[0x3c] = 0x00; // interrupt line (informational)

        // Capability chain (virtio_pci_cap = 16 bytes; notify cap = 20 bytes).
        // 0x40 common, 0x50 notify, 0x64 isr, 0x74 device, [0x84 shared-memory (shmid 0) if a shm BAR].
        self.write_virtio_cap(
            0x40,
            VIRTIO_PCI_CAP_COMMON_CFG,
            COMMON_CFG_OFF as u32,
            REGION_LEN,
            0x50,
            None,
        );
        self.write_virtio_cap(
            0x50,
            VIRTIO_PCI_CAP_NOTIFY_CFG,
            NOTIFY_OFF as u32,
            REGION_LEN,
            0x64,
            Some(0),
        );
        self.write_virtio_cap(
            0x64,
            VIRTIO_PCI_CAP_ISR_CFG,
            ISR_OFF as u32,
            REGION_LEN,
            0x74,
            None,
        );
        let device_next = if self.shm_bar.is_some() { 0x84 } else { 0x00 };
        self.write_virtio_cap(
            0x74,
            VIRTIO_PCI_CAP_DEVICE_CFG,
            DEVICE_CFG_OFF as u32,
            REGION_LEN,
            device_next,
            None,
        );
        // The virtio shared-memory capability (virtio_pci_cap64, cfg_type=8) pointing at BAR2, shmid
        // 0 (host-visible), offset 0, length = the whole window. The guest's virtio-gpu driver reads
        // this to reserve its blob-map drm_mm allocator over `PCI_SHM_BASE`.
        if let Some((_base, size)) = self.shm_bar {
            self.write_shm_cap(0x84, 2, VIRTIO_GPU_SHM_ID_HOST_VISIBLE, 0, size);
        }
    }

    /// Write a `virtio_pci_cap64` (24 bytes) describing a shared-memory region: cfg_type =
    /// `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG`, in `bar`, with 64-bit `offset`/`length`. `next` = 0 (last).
    fn write_shm_cap(&mut self, at: usize, bar: u8, id: u8, offset: u64, length: u64) {
        self.cfg[at] = VIRTIO_PCI_CAP_VNDR;
        self.cfg[at + 1] = 0x00; // cap_next (last)
        self.cfg[at + 2] = 24; // cap_len (virtio_pci_cap64)
        self.cfg[at + 3] = VIRTIO_PCI_CAP_SHARED_MEMORY_CFG;
        self.cfg[at + 4] = bar;
        self.cfg[at + 5] = id;
        self.cfg[at + 6] = 0;
        self.cfg[at + 7] = 0;
        self.cfg[at + 8..at + 12].copy_from_slice(&(offset as u32).to_le_bytes());
        self.cfg[at + 12..at + 16].copy_from_slice(&(length as u32).to_le_bytes());
        self.cfg[at + 16..at + 20].copy_from_slice(&((offset >> 32) as u32).to_le_bytes());
        self.cfg[at + 20..at + 24].copy_from_slice(&((length >> 32) as u32).to_le_bytes());
    }

    /// Write one virtio_pci_cap structure at `at`. `notify_mult` = Some for the notify cap (adds the
    /// trailing `notify_off_multiplier` u32 → cap_len 20).
    fn write_virtio_cap(
        &mut self,
        at: usize,
        cfg_type: u8,
        offset: u32,
        length: u32,
        next: u8,
        notify_mult: Option<u32>,
    ) {
        let cap_len: u8 = if notify_mult.is_some() { 20 } else { 16 };
        self.cfg[at] = VIRTIO_PCI_CAP_VNDR; // cap_vndr
        self.cfg[at + 1] = next; // cap_next
        self.cfg[at + 2] = cap_len; // cap_len
        self.cfg[at + 3] = cfg_type; // cfg_type
        self.cfg[at + 4] = 0; // bar (BAR0)
        self.cfg[at + 5] = 0; // id
        self.cfg[at + 6] = 0; // padding
        self.cfg[at + 7] = 0; // padding
        self.cfg[at + 8..at + 12].copy_from_slice(&offset.to_le_bytes());
        self.cfg[at + 12..at + 16].copy_from_slice(&length.to_le_bytes());
        if let Some(m) = notify_mult {
            self.cfg[at + 16..at + 20].copy_from_slice(&m.to_le_bytes());
        }
    }

    // ---- PCI config space (256-byte header) ----

    fn config_read(&self, reg: u16, len: usize) -> u32 {
        let reg = reg as usize;
        // BAR0 special-case: return size mask while sizing, else the assigned/patched base.
        if reg == 0x10 {
            let v = if self.bar0_sizing {
                // 32-bit memory BAR, size = BAR_SIZE → mask off the low size bits, keep type bits 0.
                (!(BAR_SIZE as u32 - 1)) & 0xffff_fff0
            } else {
                self.bar0 & 0xffff_fff0
            };
            return read_le(&v.to_le_bytes(), 0, len);
        }
        if reg == 0x04 {
            // command | status<<16
            let v = (self.command as u32) | (0x0010u32 << 16);
            return read_le(&v.to_le_bytes(), 0, len);
        }
        // BAR2/BAR3: the 64-bit prefetchable host-visible window. Return the size mask while sizing,
        // else the assigned base (with the 64-bit-prefetchable type bits in the low half).
        if let Some((base, size)) = self.shm_bar {
            if reg == 0x18 {
                let v = if self.bar2_sizing {
                    (!(size as u32).wrapping_sub(1) & 0xffff_fff0) | 0x0c
                } else {
                    (base as u32 & 0xffff_fff0) | 0x0c
                };
                return read_le(&v.to_le_bytes(), 0, len);
            }
            if reg == 0x1c {
                let mask_hi = (!((size.wrapping_sub(1)) >> 32)) as u32;
                let v = if self.bar3_sizing {
                    mask_hi
                } else {
                    (base >> 32) as u32
                };
                return read_le(&v.to_le_bytes(), 0, len);
            }
        }
        if reg + len <= 256 {
            read_le(&self.cfg, reg, len)
        } else {
            0xffff_ffff
        }
    }

    fn config_write(&mut self, reg: u16, val: u32, len: usize) {
        let reg = reg as usize;
        match reg {
            0x04 => self.command = val as u16,
            0x10 => {
                if val == 0xffff_ffff {
                    self.bar0_sizing = true;
                } else if val != 0 {
                    self.bar0_sizing = false;
                    self.bar0 = val & 0xffff_fff0;
                }
            }
            0x18 if self.shm_bar.is_some() => {
                if val == 0xffff_ffff {
                    self.bar2_sizing = true;
                } else if val != 0 {
                    // probe-only: the guest restores the original base after sizing.
                    self.bar2_sizing = false;
                    if let Some((base, size)) = self.shm_bar {
                        let base = (base & !0xffff_ffff) | (val as u64 & 0xffff_fff0);
                        self.shm_bar = Some((base, size));
                    }
                }
            }
            0x1c if self.shm_bar.is_some() => {
                if val == 0xffff_ffff {
                    self.bar3_sizing = true;
                } else {
                    self.bar3_sizing = false;
                    if val != 0 {
                        if let Some((base, size)) = self.shm_bar {
                            let base = (base & 0xffff_ffff) | ((val as u64) << 32);
                            self.shm_bar = Some((base, size));
                        }
                    }
                }
            }
            0x14 | 0x18 | 0x1c | 0x20 | 0x24 => { /* BAR1..5 unused → stay 0 */ }
            _ => { /* caps + ids are read-only */ }
        }
        let _ = len;
    }

    // ---- virtio-modern BAR MMIO ----

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    /// Read from BAR0 at `off` (relative to BAR0 base). `len` ∈ {1,2,4,8}.
    fn bar_read(&self, off: u64, len: usize) -> u64 {
        let region = off & 0xf000;
        let ro = off & 0x0fff;
        match region {
            COMMON_CFG_OFF => self.common_cfg_read(ro, len),
            ISR_OFF => {
                // ISR is read-to-clear; but read() is &self, so the clear happens in the write path
                // is not right — instead we clear on read via a cell. Simplicity: report and let the
                // mmio dispatcher clear (see PciBus::mmio_read).
                self.isr as u64
            }
            DEVICE_CFG_OFF => self.backend.config_read(ro, len),
            NOTIFY_OFF => 0,
            _ => 0,
        }
    }

    fn common_cfg_read(&self, off: u64, _len: usize) -> u64 {
        (match off {
            0x00 => self.device_feature_select,
            0x04 => {
                let f = self.backend.features();
                if self.device_feature_select == 0 {
                    f as u32
                } else {
                    (f >> 32) as u32
                }
            }
            0x08 => self.driver_feature_select,
            0x0c => {
                if self.driver_feature_select == 0 {
                    self.driver_features as u32
                } else {
                    (self.driver_features >> 32) as u32
                }
            }
            0x10 => 0xffff, // config msix vector = NO_VECTOR
            0x12 => self.queues.len() as u32, // num_queues
            0x14 => self.device_status as u32, // + config_generation<<8
            0x16 => self.queue_select as u32,
            0x18 => self
                .queues
                .get(self.queue_select as usize)
                .map(|q| q.size() as u32)
                .unwrap_or(0),
            0x1a => 0xffff, // queue msix vector = NO_VECTOR
            0x1c => self
                .queues
                .get(self.queue_select as usize)
                .map(|q| q.ready() as u32)
                .unwrap_or(0),
            0x1e => 0, // queue_notify_off (multiplier 0 → all queues share one notify address)
            _ => 0,
        }) as u64
            // config_generation packed above status when a 2-byte read hits 0x14.
            | if off == 0x14 { (self.config_generation as u64) << 8 } else { 0 }
    }

    /// Write to BAR0 at `off`. Returns `true` when a queue notify produced used buffers (→ raise IRQ).
    fn bar_write(&mut self, mem: &Mem, off: u64, val: u64, len: usize) -> bool {
        let region = off & 0xf000;
        let ro = off & 0x0fff;
        match region {
            COMMON_CFG_OFF => {
                self.common_cfg_write(ro, val, len);
                false
            }
            NOTIFY_OFF => {
                // notify_off_multiplier = 0 → the written value is the virtqueue index.
                self.notify(val as u16, mem)
            }
            ISR_OFF => false,
            DEVICE_CFG_OFF => {
                self.backend.config_write(ro, val, len);
                false
            }
            _ => false,
        }
    }

    fn common_cfg_write(&mut self, off: u64, val: u64, len: usize) {
        let v = val as u32;
        match off {
            0x00 => self.device_feature_select = v,
            0x08 => self.driver_feature_select = v,
            0x0c => {
                if self.driver_feature_select == 0 {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | v as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | ((v as u64) << 32);
                }
            }
            0x14 => self.device_status = v as u8, // low byte; config_generation is RO
            0x16 => self.queue_select = v as u16,
            0x18 => {
                if let Some(q) = self.selected_queue() {
                    q.set_size(v as u16);
                }
            }
            0x1c => {
                if let Some(q) = self.selected_queue() {
                    q.set_ready(v & 1 == 1);
                }
            }
            // queue_desc (0x20 u64), queue_driver/avail (0x28 u64), queue_device/used (0x30 u64).
            0x20 => self.set_q_addr(QAddr::Desc, Half::Lo, v),
            0x24 => self.set_q_addr(QAddr::Desc, Half::Hi, v),
            0x28 => self.set_q_addr(QAddr::Avail, Half::Lo, v),
            0x2c => self.set_q_addr(QAddr::Avail, Half::Hi, v),
            0x30 => self.set_q_addr(QAddr::Used, Half::Lo, v),
            0x34 => self.set_q_addr(QAddr::Used, Half::Hi, v),
            _ => {}
        }
        // Handle a full 8-byte write to a queue address register (lo+hi in one access).
        if len == 8 {
            let hi = (val >> 32) as u32;
            match off {
                0x20 => self.set_q_addr(QAddr::Desc, Half::Hi, hi),
                0x28 => self.set_q_addr(QAddr::Avail, Half::Hi, hi),
                0x30 => self.set_q_addr(QAddr::Used, Half::Hi, hi),
                _ => {}
            }
        }
    }

    fn set_q_addr(&mut self, which: QAddr, half: Half, v: u32) {
        let Some(q) = self.selected_queue() else {
            return;
        };
        let (lo, hi) = match half {
            Half::Lo => (Some(v), None),
            Half::Hi => (None, Some(v)),
        };
        match which {
            QAddr::Desc => q.set_desc_table_address(lo, hi),
            QAddr::Avail => q.set_avail_ring_address(lo, hi),
            QAddr::Used => q.set_used_ring_address(lo, hi),
        }
    }

    fn notify(&mut self, idx: u16, mem: &Mem) -> bool {
        let ready = self
            .queues
            .get(idx as usize)
            .map(|q| q.ready())
            .unwrap_or(false);
        if !ready {
            return false;
        }
        // A console needs BOTH queues in one notify (a secure HAL channel writes its RX response as
        // it drains the TX request), so route it through `on_notify(&mut [Queue])`.
        let produced = if let Backend::Console(c) = &mut self.backend {
            c.on_notify(idx, mem, &mut self.queues)
        } else {
            let q = &mut self.queues[idx as usize];
            self.backend.process_queue(idx, mem, q)
        };
        if produced {
            self.isr |= VIRTIO_ISR_QUEUE;
        }
        produced
    }
}

enum QAddr {
    Desc,
    Avail,
    Used,
}
enum Half {
    Lo,
    Hi,
}

/// Read `len` little-endian bytes from `buf` at `off`, zero-extended into a u32.
fn read_le(buf: &[u8], off: usize, len: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..len.min(4) {
        if let Some(&b) = buf.get(off + i) {
            v |= (b as u32) << (8 * i);
        }
    }
    v
}

/// A single-bus PCI host bridge owning the virtio-PCI device fleet.
pub struct PciBus {
    pub devices: Vec<PciDevice>,
    /// dev_num → index into `devices`, for O(1) config decode.
    slot_to_idx: [Option<usize>; 32],
}

impl PciBus {
    pub fn new(devices: Vec<PciDevice>) -> Self {
        let mut slot_to_idx = [None; 32];
        for (i, d) in devices.iter().enumerate() {
            slot_to_idx[(d.dev_num & 0x1f) as usize] = Some(i);
        }
        Self {
            devices,
            slot_to_idx,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn cam_contains(ipa: u64) -> bool {
        (PCI_CAM_BASE..PCI_CAM_BASE + PCI_CAM_SIZE).contains(&ipa)
    }

    pub fn mmio_contains(ipa: u64) -> bool {
        (PCI_MMIO_BASE..PCI_MMIO_BASE + PCI_MMIO_SIZE).contains(&ipa)
    }

    /// Decode a CAM access → (bus, dev, func, reg).
    fn decode(ipa: u64) -> (u8, u8, u8, u16) {
        let off = ipa - PCI_CAM_BASE;
        let bus = ((off >> 16) & 0xff) as u8;
        let dev = ((off >> 11) & 0x1f) as u8;
        let func = ((off >> 8) & 0x07) as u8;
        let reg = (off & 0xff) as u16;
        (bus, dev, func, reg)
    }

    pub fn cfg_read(&self, ipa: u64, len: usize) -> u64 {
        let (bus, dev, func, reg) = Self::decode(ipa);
        if bus != 0 || func != 0 {
            return 0xffff_ffff;
        }
        match self.slot_to_idx.get(dev as usize).copied().flatten() {
            Some(i) => self.devices[i].config_read(reg, len) as u64,
            None => 0xffff_ffff, // absent device → all-ones so the guest stops probing
        }
    }

    pub fn cfg_write(&mut self, ipa: u64, val: u64, len: usize) {
        let (bus, dev, func, reg) = Self::decode(ipa);
        if bus != 0 || func != 0 {
            return;
        }
        if let Some(i) = self.slot_to_idx.get(dev as usize).copied().flatten() {
            self.devices[i].config_write(reg, val as u32, len);
        }
    }

    /// Find the device whose BAR0 covers `ipa`, plus the BAR-relative offset.
    fn locate_bar(&self, ipa: u64) -> Option<(usize, u64)> {
        for (i, d) in self.devices.iter().enumerate() {
            let base = d.bar0 as u64;
            if base != 0 && ipa >= base && ipa < base + BAR_SIZE {
                return Some((i, ipa - base));
            }
        }
        None
    }

    pub fn mmio_read(&mut self, ipa: u64, len: usize) -> u64 {
        let Some((i, off)) = self.locate_bar(ipa) else {
            return 0;
        };
        let v = self.devices[i].bar_read(off, len);
        // ISR is read-to-clear.
        if (off & 0xf000) == ISR_OFF {
            self.devices[i].isr = 0;
        }
        v
    }

    /// Returns `Some(intid)` when the write completed used buffers (caller raises the SPI).
    pub fn mmio_write(&mut self, mem: &Mem, ipa: u64, val: u64, len: usize) -> Option<u32> {
        let (i, off) = self.locate_bar(ipa)?;
        if self.devices[i].bar_write(mem, off, val, len) {
            Some(self.devices[i].intid)
        } else {
            None
        }
    }

    /// (dev_num, spi) pairs for the FDT `interrupt-map`.
    pub fn irq_map(&self) -> Vec<(u8, u32)> {
        self.devices
            .iter()
            .map(|d| (d.dev_num, d.intid - 32))
            .collect()
    }

    /// INTID of the virtio-gpu device on this bus, if present (for the gfxstream fence-signal thread).
    pub fn gpu_intid(&self) -> Option<u32> {
        self.devices
            .iter()
            .find(|d| matches!(d.backend, Backend::Gpu(_)))
            .map(|d| d.intid)
    }

    /// True if the bus carries a virtio-input device (so the input-inject thread knows to route here
    /// rather than to the — empty on the Android path — virtio-mmio fleet).
    pub fn has_input(&self) -> bool {
        self.devices
            .iter()
            .any(|d| matches!(d.backend, Backend::Input(_)))
    }

    /// Inject a frame of host input events (a coherent set + a trailing `EV_SYN`) into the virtio-
    /// input device's eventq. Returns `Some(intid)` when a guest buffer was completed (caller raises
    /// the device SPI + kicks the vCPU). Mirrors `MmioTransport::inject_input_events` for the PCI
    /// fleet — without this the guest never observes injected touch/keyboard events on the Android
    /// path (its virtio-input lives on the PCI bus, and the inject thread only searched the MMIO
    /// transports), so the keyguard could never be swiped away.
    pub fn inject_input_events(&mut self, events: &[(u16, u16, i32)], mem: &Mem) -> Option<u32> {
        for d in &mut self.devices {
            if let Backend::Input(inp) = &mut d.backend {
                inp.queue_frame(events);
                let produced = match d.queues.get_mut(super::input::EVENTQ as usize) {
                    Some(evq) => inp.flush_to_eventq(mem, evq),
                    None => false,
                };
                if produced {
                    d.isr |= VIRTIO_ISR_QUEUE;
                    return Some(d.intid);
                }
                return None;
            }
        }
        None
    }

    /// Clone the NAT backend handle of the bus's virtio-net device, if present (for the poll thread).
    pub fn net_slirp(&self) -> Option<std::sync::Arc<crate::net::SlirpNet>> {
        self.devices.iter().find_map(|d| match &d.backend {
            Backend::Net(net) => net.slirp(),
            _ => None,
        })
    }

    /// Flush any pending host->guest frames from the NAT backend into the virtio-net receiveq.
    /// Returns `Some(intid)` when a receive buffer was completed (caller raises the SPI + kicks the
    /// vCPU). Called from the slirp poll thread. Mirrors `feed_console_input`/`inject_input_events`.
    pub fn flush_net_rx(&mut self, mem: &Mem) -> Option<u32> {
        for d in &mut self.devices {
            if let Backend::Net(net) = &mut d.backend {
                let produced = match d.queues.get_mut(super::net::RECEIVEQ as usize) {
                    Some(rxq) => net.flush_rx(mem, rxq),
                    None => false,
                };
                if produced {
                    d.isr |= VIRTIO_ISR_QUEUE;
                    return Some(d.intid);
                }
                return None;
            }
        }
        None
    }

    /// Feed host-typed bytes into a console device's receiveq. Returns `Some(intid)` if a receive
    /// buffer completed. Called on the stdin thread.
    pub fn feed_console_input(&mut self, bytes: &[u8], mem: &Mem) -> Option<u32> {
        for d in &mut self.devices {
            if let Backend::Console(c) = &mut d.backend {
                if !c.is_terminal() {
                    continue; // HAL channels (hvc3/4/10/11, logcat, …) never take host keystrokes
                }
                let produced = c.push_input(bytes, mem, &mut d.queues);
                if produced {
                    d.isr |= VIRTIO_ISR_QUEUE;
                    return Some(d.intid);
                }
                return None;
            }
        }
        None
    }
}

/// IPA of the `i`-th PCI device's BAR0.
pub fn device_bar0(index: usize) -> u32 {
    (PCI_MMIO_BASE + index as u64 * BAR_SIZE) as u32
}

/// PCI slot (device number) assigned to the `i`-th device — starts at 1 (slot 0 reserved).
pub fn device_slot(index: usize) -> u8 {
    (index + 1) as u8
}

/// Host-visible shared-memory BAR + virtio shared-memory capability tests (gfxstream blob path).
#[cfg(test)]
mod tests_shm {
    use super::*;
    use crate::virtio::gpu::Gpu;

    fn gpu_pci_dev_with_shm() -> PciDevice {
        // A GPU device (no renderer needed for the BAR/cap layout) with the shmem BAR attached.
        let gpu = Gpu::new(64, 64);
        let mut d = PciDevice::new(Backend::Gpu(gpu), 48, 4, device_bar0(4));
        d.attach_shm_bar(PCI_SHM_BASE, PCI_SHM_SIZE);
        d
    }

    #[test]
    fn shm_bar2_reports_64bit_prefetchable_base_and_size() {
        let d = gpu_pci_dev_with_shm();
        // BAR2 low: base low | 0x0C (64-bit prefetchable memory BAR).
        assert_eq!(
            d.config_read(0x18, 4),
            (PCI_SHM_BASE as u32 & 0xffff_fff0) | 0x0c
        );
        // BAR3 high: base high.
        assert_eq!(d.config_read(0x1c, 4), (PCI_SHM_BASE >> 32) as u32);
    }

    #[test]
    fn shm_bar2_sizing_returns_region_length_mask() {
        let mut d = gpu_pci_dev_with_shm();
        // 64-bit BAR sizing: write all-ones to low+high, read the size mask, then restore.
        d.config_write(0x18, 0xffff_ffff, 4);
        d.config_write(0x1c, 0xffff_ffff, 4);
        let lo = d.config_read(0x18, 4);
        let hi = d.config_read(0x1c, 4);
        // Reconstruct the size from the 64-bit mask (clear the low type bits).
        let mask = ((hi as u64) << 32) | (lo as u64 & 0xffff_fff0);
        let size = (!mask).wrapping_add(1);
        assert_eq!(size, PCI_SHM_SIZE, "BAR2 must size to the shmem window");
        // Restore the original base; sizing latch clears.
        d.config_write(0x18, (PCI_SHM_BASE as u32 & 0xffff_fff0) | 0x0c, 4);
        d.config_write(0x1c, (PCI_SHM_BASE >> 32) as u32, 4);
        assert_eq!(
            d.config_read(0x18, 4),
            (PCI_SHM_BASE as u32 & 0xffff_fff0) | 0x0c
        );
        assert_eq!(d.config_read(0x1c, 4), (PCI_SHM_BASE >> 32) as u32);
    }

    #[test]
    fn shared_memory_capability_present_with_host_visible_shmid() {
        let d = gpu_pci_dev_with_shm();
        // Walk the capability chain from the caps pointer (0x34) looking for cfg_type 8.
        let mut at = d.config_read(0x34, 1) as usize;
        let mut found = None;
        for _ in 0..8 {
            if at == 0 || at + 4 > 256 {
                break;
            }
            let cap_vndr = d.config_read(at as u16, 1) as u8;
            let cfg_type = d.config_read((at + 3) as u16, 1) as u8;
            if cap_vndr == VIRTIO_PCI_CAP_VNDR && cfg_type == VIRTIO_PCI_CAP_SHARED_MEMORY_CFG {
                found = Some(at);
                break;
            }
            at = d.config_read((at + 1) as u16, 1) as usize;
        }
        let at = found.expect("a virtio shared-memory capability (cfg_type 8) must be present");
        assert_eq!(
            d.config_read((at + 4) as u16, 1) as u8,
            2,
            "shm cap must reference BAR2"
        );
        assert_eq!(
            d.config_read((at + 5) as u16, 1) as u8,
            VIRTIO_GPU_SHM_ID_HOST_VISIBLE,
            "shmid must be 1 (host-visible)"
        );
        // length (offset_lo=0, length_lo @ +12, length_hi @ +20) covers the full window.
        let len_lo = d.config_read((at + 12) as u16, 4) as u64;
        let len_hi = d.config_read((at + 20) as u16, 4) as u64;
        assert_eq!((len_hi << 32) | len_lo, PCI_SHM_SIZE);
    }
}
