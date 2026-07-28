//! The virtio-mmio **modern (v2)** transport register block. One [`MmioTransport`] per device wraps
//! a [`Backend`] plus the split virtqueue state (rust-vmm `Queue`s) and the small pile of registers
//! the Linux `virtio_mmio` driver programs during probe + feature negotiation + queue setup.
//!
//! The run loop forwards a data abort in the virtio window here as [`MmioTransport::read`] /
//! [`MmioTransport::write`]; a write to `QueueNotify` runs the backend over the notified queue and
//! returns `true` when used buffers were produced, so the caller can raise the device's SPI.

use super::block::Block;
use super::block_composite::CompositeBlock;
use super::console::Console;
use super::gpu::Gpu;
use super::input::Input;
use super::net::Net;
use super::rng::Rng;
use super::{Mem, QUEUE_SIZE, VIRTIO_ID_CONSOLE};
use virtio_queue::{Queue, QueueT};

// virtio-mmio register offsets (virtio spec §4.2.2, MMIO v2).
const MAGIC_VALUE: u64 = 0x000;
const VERSION: u64 = 0x004;
const DEVICE_ID: u64 = 0x008;
const VENDOR_ID: u64 = 0x00c;
const DEVICE_FEATURES: u64 = 0x010;
const DEVICE_FEATURES_SEL: u64 = 0x014;
const DRIVER_FEATURES: u64 = 0x020;
const DRIVER_FEATURES_SEL: u64 = 0x024;
const QUEUE_SEL: u64 = 0x030;
const QUEUE_NUM_MAX: u64 = 0x034;
const QUEUE_NUM: u64 = 0x038;
const QUEUE_READY: u64 = 0x044;
const QUEUE_NOTIFY: u64 = 0x050;
const INTERRUPT_STATUS: u64 = 0x060;
const INTERRUPT_ACK: u64 = 0x064;
const STATUS: u64 = 0x070;
const QUEUE_DESC_LOW: u64 = 0x080;
const QUEUE_DESC_HIGH: u64 = 0x084;
const QUEUE_DRIVER_LOW: u64 = 0x090;
const QUEUE_DRIVER_HIGH: u64 = 0x094;
const QUEUE_DEVICE_LOW: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH: u64 = 0x0a4;
// Shared-memory region window (virtio-mmio v2 §4.2.2). The guest selects a region id via SHM_SEL
// then reads SHM_LEN_{LOW,HIGH}; a length of all-ones means "this region does not exist". We expose
// no SHM regions, so LEN reads back 0xffff_ffff on both halves (a length of 0 would instead be read
// as a real zero-length region — that is what made the virtio-gpu DRM driver's unconditional
// `virtio_get_shm_region` believe in a host-visible window at addr 0 and fail probe with -EBUSY).
const SHM_SEL: u64 = 0x0ac;
const SHM_LEN_LOW: u64 = 0x0b0;
const SHM_LEN_HIGH: u64 = 0x0b4;
const SHM_BASE_LOW: u64 = 0x0b8;
const SHM_BASE_HIGH: u64 = 0x0bc;
const CONFIG_GENERATION: u64 = 0x0fc;
const CONFIG: u64 = 0x100;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x1af4; // Red Hat / virtio
const VIRTIO_MMIO_INT_VRING: u32 = 1 << 0;

/// One of the three device backends. An enum (not `dyn`) so the stdin thread can reach the console
/// without downcasting.
pub enum Backend {
    Rng(Rng),
    Console(Console),
    Block(Block),
    CompositeBlock(CompositeBlock),
    Gpu(Gpu),
    Input(Input),
    Net(Net),
}

impl Backend {
    pub fn device_type(&self) -> u32 {
        match self {
            Backend::Rng(d) => d.device_type(),
            Backend::Console(d) => d.device_type(),
            Backend::Block(d) => d.device_type(),
            Backend::CompositeBlock(d) => d.device_type(),
            Backend::Gpu(d) => d.device_type(),
            Backend::Input(d) => d.device_type(),
            Backend::Net(d) => d.device_type(),
        }
    }
    pub fn features(&self) -> u64 {
        match self {
            Backend::Rng(d) => d.features(),
            Backend::Console(d) => d.features(),
            Backend::Block(d) => d.features(),
            Backend::CompositeBlock(d) => d.features(),
            Backend::Gpu(d) => d.features(),
            Backend::Input(d) => d.features(),
            Backend::Net(d) => d.features(),
        }
    }
    pub fn num_queues(&self) -> usize {
        match self {
            Backend::Rng(d) => d.num_queues(),
            Backend::Console(d) => d.num_queues(),
            Backend::Block(d) => d.num_queues(),
            Backend::CompositeBlock(d) => d.num_queues(),
            Backend::Gpu(d) => d.num_queues(),
            Backend::Input(d) => d.num_queues(),
            Backend::Net(d) => d.num_queues(),
        }
    }
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        match self {
            Backend::Rng(d) => d.config_read(off, len),
            Backend::Console(d) => d.config_read(off, len),
            Backend::Block(d) => d.config_read(off, len),
            Backend::CompositeBlock(d) => d.config_read(off, len),
            Backend::Gpu(d) => d.config_read(off, len),
            Backend::Input(d) => d.config_read(off, len),
            Backend::Net(d) => d.config_read(off, len),
        }
    }
    pub fn config_write(&mut self, off: u64, val: u64, len: usize) {
        match self {
            Backend::Rng(d) => d.config_write(off, val, len),
            Backend::Console(d) => d.config_write(off, val, len),
            Backend::Block(d) => d.config_write(off, val, len),
            Backend::CompositeBlock(d) => d.config_write(off, val, len),
            Backend::Gpu(d) => d.config_write(off, val, len),
            Backend::Input(d) => d.config_write(off, val, len),
            Backend::Net(d) => d.config_write(off, val, len),
        }
    }
    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match self {
            Backend::Rng(d) => d.process_queue(idx, mem, queue),
            Backend::Console(d) => d.process_queue(idx, mem, queue),
            Backend::Block(d) => d.process_queue(idx, mem, queue),
            Backend::CompositeBlock(d) => d.process_queue(idx, mem, queue),
            Backend::Gpu(d) => d.process_queue(idx, mem, queue),
            Backend::Input(d) => d.process_queue(idx, mem, queue),
            Backend::Net(d) => d.process_queue(idx, mem, queue),
        }
    }
}

pub struct MmioTransport {
    pub backend: Backend,
    /// Absolute GIC INTID this device raises on a used-buffer notification.
    pub intid: u32,
    queues: Vec<Queue>,
    queue_sel: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    status: u32,
    interrupt_status: u32,
}

impl MmioTransport {
    pub fn new(backend: Backend, intid: u32) -> Self {
        let n = backend.num_queues();
        let queues = (0..n)
            .map(|_| Queue::new(QUEUE_SIZE).expect("QUEUE_SIZE is a valid power of two"))
            .collect();
        Self {
            backend,
            intid,
            queues,
            queue_sel: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            status: 0,
            interrupt_status: 0,
        }
    }

    pub fn device_type(&self) -> u32 {
        self.backend.device_type()
    }

    pub fn is_console(&self) -> bool {
        self.device_type() == VIRTIO_ID_CONSOLE
    }

    fn selected_queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    /// Read a virtio-mmio register / config byte-range. `len` ∈ {1,2,4,8}.
    pub fn read(&self, off: u64, len: usize) -> u64 {
        if off >= CONFIG {
            return self.backend.config_read(off - CONFIG, len);
        }
        (match off {
            MAGIC_VALUE => MAGIC,
            VERSION => 2,
            DEVICE_ID => self.backend.device_type(),
            VENDOR_ID => VENDOR,
            DEVICE_FEATURES => {
                let f = self.backend.features();
                if self.device_features_sel == 0 {
                    f as u32
                } else {
                    (f >> 32) as u32
                }
            }
            QUEUE_NUM_MAX => {
                if (self.queue_sel as usize) < self.queues.len() {
                    QUEUE_SIZE as u32
                } else {
                    0
                }
            }
            QUEUE_READY => self
                .queues
                .get(self.queue_sel as usize)
                .map(|q| q.ready() as u32)
                .unwrap_or(0),
            INTERRUPT_STATUS => self.interrupt_status,
            STATUS => self.status,
            // No shared-memory regions: report length = all-ones ("region absent"), base = 0.
            SHM_LEN_LOW | SHM_LEN_HIGH => 0xffff_ffff,
            SHM_BASE_LOW | SHM_BASE_HIGH => 0,
            CONFIG_GENERATION => 0,
            _ => 0,
        }) as u64
    }

    /// Write a virtio-mmio register / config byte-range. Returns `true` when the write completed
    /// used buffers (the caller must raise `self.intid`).
    pub fn write(&mut self, mem: &Mem, off: u64, val: u64, len: usize) -> bool {
        if off >= CONFIG {
            self.backend.config_write(off - CONFIG, val, len);
            return false;
        }
        let v = val as u32;
        match off {
            DEVICE_FEATURES_SEL => self.device_features_sel = v,
            DRIVER_FEATURES_SEL => self.driver_features_sel = v,
            DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & !0xffff_ffff) | v as u64;
                } else {
                    self.driver_features =
                        (self.driver_features & 0xffff_ffff) | ((v as u64) << 32);
                }
            }
            QUEUE_SEL => self.queue_sel = v,
            QUEUE_NUM => {
                if let Some(q) = self.selected_queue() {
                    q.set_size(v as u16);
                }
            }
            QUEUE_READY => {
                if let Some(q) = self.selected_queue() {
                    q.set_ready(v & 1 == 1);
                }
            }
            QUEUE_DESC_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_desc_table_address(Some(v), None);
                }
            }
            QUEUE_DESC_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_desc_table_address(None, Some(v));
                }
            }
            QUEUE_DRIVER_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_avail_ring_address(Some(v), None);
                }
            }
            QUEUE_DRIVER_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_avail_ring_address(None, Some(v));
                }
            }
            QUEUE_DEVICE_LOW => {
                if let Some(q) = self.selected_queue() {
                    q.set_used_ring_address(Some(v), None);
                }
            }
            QUEUE_DEVICE_HIGH => {
                if let Some(q) = self.selected_queue() {
                    q.set_used_ring_address(None, Some(v));
                }
            }
            INTERRUPT_ACK => self.interrupt_status &= !v,
            STATUS => self.status = v,
            // We expose no SHM regions, so the selected id is irrelevant (LEN always reads -1).
            SHM_SEL => {}
            QUEUE_NOTIFY => return self.notify(v as u16, mem),
            _ => {}
        }
        false
    }

    /// Run the backend over queue `idx`. Returns whether an interrupt is owed.
    fn notify(&mut self, idx: u16, mem: &Mem) -> bool {
        let ready = self
            .queues
            .get(idx as usize)
            .map(|q| q.ready())
            .unwrap_or(false);
        if !ready {
            return false;
        }
        // Disjoint field borrows: `self.queues[idx]` and `self.backend` are distinct fields.
        // A console needs BOTH queues in one notify (a secure channel writes its RX response while
        // draining the TX request), so route it through `on_notify(&mut [Queue])`.
        let produced = if let Backend::Console(c) = &mut self.backend {
            c.on_notify(idx, mem, &mut self.queues)
        } else {
            let q = &mut self.queues[idx as usize];
            self.backend.process_queue(idx, mem, q)
        };
        if produced {
            self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
        }
        produced
    }

    /// Push host-typed bytes into a console's receiveq (no-op for non-console devices). Returns
    /// whether a receive buffer was completed (→ raise `self.intid`). Called on the stdin thread.
    pub fn feed_console_input(&mut self, bytes: &[u8], mem: &Mem) -> bool {
        if let Backend::Console(c) = &mut self.backend {
            if !c.is_terminal() {
                return false;
            }
            let produced = c.push_input(bytes, mem, &mut self.queues);
            if produced {
                self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
            }
            produced
        } else {
            false
        }
    }

    /// True if this transport backs a virtio-input device.
    pub fn is_input(&self) -> bool {
        matches!(self.backend, Backend::Input(_))
    }

    /// Clone the NAT backend handle if this transport backs a virtio-net device (for the poll thread).
    pub fn net_slirp(&self) -> Option<std::sync::Arc<crate::net::SlirpNet>> {
        match &self.backend {
            Backend::Net(net) => net.slirp(),
            _ => None,
        }
    }

    /// Flush pending host->guest frames from the NAT backend into the virtio-net receiveq. Returns
    /// whether a buffer was completed (→ raise `self.intid`). No-op for non-net devices. Called from
    /// the slirp poll thread (Linux `--net` MMIO path).
    pub fn flush_net_rx(&mut self, mem: &Mem) -> bool {
        if let Backend::Net(net) = &mut self.backend {
            let produced = match self.queues.get_mut(super::net::RECEIVEQ as usize) {
                Some(rxq) => net.flush_rx(mem, rxq),
                None => false,
            };
            if produced {
                self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
            }
            produced
        } else {
            false
        }
    }

    /// Inject a frame of host input events (a coherent set + a trailing `EV_SYN`) into a virtio-
    /// input device's eventq (no-op for non-input devices). Returns whether a buffer was completed
    /// (→ raise `self.intid`). Called from the display/control plane.
    pub fn inject_input_events(&mut self, events: &[(u16, u16, i32)], mem: &Mem) -> bool {
        if let Backend::Input(d) = &mut self.backend {
            d.queue_frame(events);
            let produced = if let Some(evq) = self.queues.get_mut(super::input::EVENTQ as usize) {
                d.flush_to_eventq(mem, evq)
            } else {
                false
            };
            if produced {
                self.interrupt_status |= VIRTIO_MMIO_INT_VRING;
            }
            produced
        } else {
            false
        }
    }
}
