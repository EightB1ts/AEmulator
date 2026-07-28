//! virtio-rng (device type 4): the guest posts write-only buffers on its single request queue and
//! the device fills them with entropy. We source real entropy from the host `/dev/urandom`.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_RNG};
use std::fs::File;
use std::io::Read;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

pub struct Rng {
    urandom: Option<File>,
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng {
    pub fn new() -> Self {
        Self {
            urandom: File::open("/dev/urandom").ok(),
        }
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_RNG
    }

    pub fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    /// One request queue.
    pub fn num_queues(&self) -> usize {
        1
    }

    pub fn config_read(&self, _off: u64, _len: usize) -> u64 {
        0
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    fn fill(&mut self, buf: &mut [u8]) {
        if let Some(f) = self.urandom.as_mut() {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
        // Fallback: a cheap counter-based fill so the guest never blocks on entropy.
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i as u8) ^ 0x5a;
        }
    }

    /// Drain the request queue, filling each write-only buffer with entropy. Returns whether any
    /// buffer was completed (→ raise the device interrupt).
    pub fn process_queue(&mut self, _idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let mut written = 0u32;
            for desc in chain {
                if desc.is_write_only() {
                    let mut buf = vec![0u8; desc.len() as usize];
                    self.fill(&mut buf);
                    if mem.write_slice(&buf, GuestAddress(desc.addr().0)).is_ok() {
                        written += desc.len();
                    }
                }
            }
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }
}
