//! In-process **virtqueue harness** for the device tests. `#[cfg(test)]` only.
//!
//! Every backend in this crate is driven through `process_queue(idx, mem, queue)`, but no test built
//! a real [`Queue`]: the virtio-gpu tests all called the device's private `dispatch` helper directly,
//! so the layer that actually talks to the guest — `pop_descriptor_chain`, `split_chain`,
//! `write_scatter`, `add_used` — had **no coverage at all**. That is also precisely the layer the
//! deferred-fence work has to modify, so it needs a harness before it can be changed safely.
//!
//! [`TestQueue`] owns a real `GuestMemoryMmap` plus a real [`Queue`], programmed through the *same*
//! setters the PCI transport calls when the guest writes its common-configuration registers — see
//! `pci::VirtioPciDevice::common_cfg_write` / `set_q_addr`:
//!
//! | guest register        | offset | call made here                 |
//! |----------------------|--------|--------------------------------|
//! | `queue_size`         | `0x18` | [`QueueT::set_size`]           |
//! | `queue_desc`         | `0x20` | `set_desc_table_address(lo,hi)` |
//! | `queue_driver/avail` | `0x28` | `set_avail_ring_address(lo,hi)` |
//! | `queue_device/used`  | `0x30` | `set_used_ring_address(lo,hi)`  |
//! | `queue_enable`       | `0x1c` | [`QueueT::set_ready`]          |
//!
//! The split-ring bytes this module writes by hand are the driver's half of the contract (virtio 1.2
//! §2.7): the descriptor table, the available ring, and reading back the used ring. They are laid out
//! exactly as `virtio-queue` reads them, so the *device* half stays entirely upstream code.

use super::{Mem, QUEUE_SIZE};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

/// `VIRTQ_DESC_F_NEXT` — the chain continues at this descriptor's `next` index.
pub const DESC_F_NEXT: u16 = 1;
/// `VIRTQ_DESC_F_WRITE` — a device-*writable* descriptor (the response side of a control chain).
pub const DESC_F_WRITE: u16 = 2;

/// `sizeof(struct virtq_desc)`: addr(u64) len(u32) flags(u16) next(u16).
const DESC_BYTES: u64 = 16;

// Guest layout, sized for the full `QUEUE_SIZE` ring the PCI transport creates and spaced so no two
// structures can overlap: descriptor table 256*16 = 0x1000 B at 0x1000, avail ring 6 + 2*256 =
// 0x206 B at 0x3000, used ring 6 + 8*256 = 0x806 B at 0x4000. Descriptor buffers are bump-allocated
// from 0x5000 up. Every base clears `virtio-queue`'s alignment rules (desc 16 B, avail 2 B, used 4 B).
//
// Note the avail ring may NOT sit at guest address 0: `QueueOwnedT::iter` treats `avail_ring == 0` as
// "queue not configured" and refuses to pop.
const DESC_BASE: u64 = 0x1000;
const AVAIL_BASE: u64 = 0x3000;
const USED_BASE: u64 = 0x4000;
const ARENA_BASE: u64 = 0x5000;

/// Smallest `mem_size` [`TestQueue::new`] accepts: the three rings plus a usable buffer arena.
pub const MIN_MEM_SIZE: usize = 0x8000;

/// A real split virtqueue over real guest memory, driven from the *driver* side.
pub struct TestQueue {
    /// Guest RAM, based at 0.
    pub mem: Mem,
    /// The device-side queue, ready and programmed.
    pub queue: Queue,
    /// Guest address of the descriptor table.
    pub desc_base: u64,
    /// Guest address of the available ring.
    pub avail_base: u64,
    /// Guest address of the used ring.
    pub used_base: u64,
    mem_size: u64,
    /// Next free descriptor-table index.
    next_desc: u16,
    /// The driver's `avail.idx`.
    next_avail: u16,
    /// Bump allocator for descriptor buffers.
    arena: u64,
}

impl TestQueue {
    /// Build `mem_size` bytes of guest RAM with one ready queue programmed over it.
    ///
    /// # Panics
    /// If `mem_size` is below [`MIN_MEM_SIZE`].
    pub fn new(mem_size: usize) -> Self {
        assert!(
            mem_size >= MIN_MEM_SIZE,
            "TestQueue needs at least {MIN_MEM_SIZE:#x} bytes of guest RAM for the rings + arena"
        );
        let mem = Mem::from_ranges(&[(GuestAddress(0), mem_size)])
            .expect("a single anonymous guest-memory range always maps");

        // Mirror pci.rs: create the queue at the advertised max size, then apply what the guest
        // writes — size, the three ring addresses as lo/hi u32 pairs, and finally queue_enable.
        let mut queue = Queue::new(QUEUE_SIZE).expect("QUEUE_SIZE is a valid power of two");
        queue.set_size(QUEUE_SIZE);
        queue.set_desc_table_address(Some(DESC_BASE as u32), Some((DESC_BASE >> 32) as u32));
        queue.set_avail_ring_address(Some(AVAIL_BASE as u32), Some((AVAIL_BASE >> 32) as u32));
        queue.set_used_ring_address(Some(USED_BASE as u32), Some((USED_BASE >> 32) as u32));
        queue.set_ready(true);

        Self {
            mem,
            queue,
            desc_base: DESC_BASE,
            avail_base: AVAIL_BASE,
            used_base: USED_BASE,
            mem_size: mem_size as u64,
            next_desc: 0,
            next_avail: 0,
            arena: ARENA_BASE,
        }
    }

    /// Publish one descriptor chain the way a guest driver would: `readable` becomes a single
    /// device-readable descriptor carrying the command bytes, then one device-writable descriptor per
    /// entry in `writable_lens` receives the response. Returns the chain's head index.
    ///
    /// Passing more than one writable length exercises the device's scatter path (`write_scatter`)
    /// rather than a single contiguous response buffer.
    ///
    /// # Panics
    /// If the descriptor table or the buffer arena would overflow.
    pub fn push_chain(&mut self, readable: &[u8], writable_lens: &[u32]) -> u16 {
        let head = self.next_desc;
        let count = usize::from(!readable.is_empty()) + writable_lens.len();
        assert!(count > 0, "a chain needs at least one descriptor");
        assert!(
            head as usize + count <= QUEUE_SIZE as usize,
            "TestQueue descriptor table exhausted"
        );

        // (addr, len, flags) per buffer, command first.
        let mut entries: Vec<(u64, u32, u16)> = Vec::with_capacity(count);
        if !readable.is_empty() {
            let addr = self.alloc(readable.len() as u64);
            self.write(addr, readable);
            entries.push((addr, readable.len() as u32, 0));
        }
        for &len in writable_lens {
            let addr = self.alloc(u64::from(len));
            // Pre-zero, so anything `resp_bytes` reports back was written by the device.
            self.write(addr, &vec![0u8; len as usize]);
            entries.push((addr, len, DESC_F_WRITE));
        }

        let last = entries.len() - 1;
        for (i, &(addr, len, flags)) in entries.iter().enumerate() {
            let idx = head + i as u16;
            let (flags, next) = if i == last {
                (flags, 0)
            } else {
                (flags | DESC_F_NEXT, idx + 1)
            };
            self.write_desc(idx, addr, len, flags, next);
        }
        self.next_desc += count as u16;

        // The driver's kick: publish the head, then bump avail.idx (release ordering is the device's
        // problem — `Queue::avail_idx` loads it with `Acquire`).
        let slot = self.next_avail % QUEUE_SIZE;
        self.store_u16(self.avail_base + 4 + u64::from(slot) * 2, head);
        self.next_avail = self.next_avail.wrapping_add(1);
        self.store_u16(self.avail_base + 2, self.next_avail);
        head
    }

    /// Every `(id, len)` the device has published in the used ring, oldest first — read straight out
    /// of guest RAM, not from the [`Queue`]'s own state. Valid until the ring wraps (65536 entries),
    /// which no test comes close to.
    pub fn used(&self) -> Vec<(u32, u32)> {
        let idx = self.load_u16(self.used_base + 2);
        (0..idx)
            .map(|i| {
                let off = self.used_base + 4 + u64::from(i % QUEUE_SIZE) * 8;
                (self.load_u32(off), self.load_u32(off + 4))
            })
            .collect()
    }

    /// The bytes now sitting in the device-writable descriptors of the chain at `head`, concatenated
    /// in chain order — i.e. the response the guest would read.
    pub fn resp_bytes(&self, head: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut idx = head;
        for _ in 0..QUEUE_SIZE {
            let (addr, len, flags, next) = self.read_desc(idx);
            if flags & DESC_F_WRITE != 0 {
                let mut buf = vec![0u8; len as usize];
                self.mem
                    .read_slice(&mut buf, GuestAddress(addr))
                    .expect("descriptor buffer is inside guest RAM");
                out.extend_from_slice(&buf);
            }
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        out
    }

    /// Reserve `size` bytes of guest RAM for a descriptor buffer (16-byte aligned).
    fn alloc(&mut self, size: u64) -> u64 {
        let addr = self.arena;
        self.arena = (addr + size.max(1) + 15) & !15;
        assert!(
            self.arena <= self.mem_size,
            "TestQueue buffer arena exhausted — pass a larger mem_size"
        );
        addr
    }

    /// Write one `struct virtq_desc` at descriptor-table index `idx`.
    fn write_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut d = [0u8; DESC_BYTES as usize];
        d[0..8].copy_from_slice(&addr.to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&flags.to_le_bytes());
        d[14..16].copy_from_slice(&next.to_le_bytes());
        self.write(self.desc_base + u64::from(idx) * DESC_BYTES, &d);
    }

    /// Read back one `struct virtq_desc` as `(addr, len, flags, next)`.
    fn read_desc(&self, idx: u16) -> (u64, u32, u16, u16) {
        let base = self.desc_base + u64::from(idx) * DESC_BYTES;
        let mut d = [0u8; DESC_BYTES as usize];
        self.mem
            .read_slice(&mut d, GuestAddress(base))
            .expect("descriptor table is inside guest RAM");
        (
            u64::from_le_bytes(d[0..8].try_into().unwrap()),
            u32::from_le_bytes(d[8..12].try_into().unwrap()),
            u16::from_le_bytes(d[12..14].try_into().unwrap()),
            u16::from_le_bytes(d[14..16].try_into().unwrap()),
        )
    }

    fn write(&self, addr: u64, bytes: &[u8]) {
        self.mem
            .write_slice(bytes, GuestAddress(addr))
            .expect("TestQueue writes stay inside guest RAM");
    }

    fn store_u16(&self, addr: u64, v: u16) {
        self.write(addr, &v.to_le_bytes());
    }

    fn load_u16(&self, addr: u64) -> u16 {
        let mut b = [0u8; 2];
        self.mem
            .read_slice(&mut b, GuestAddress(addr))
            .expect("ring is inside guest RAM");
        u16::from_le_bytes(b)
    }

    fn load_u32(&self, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        self.mem
            .read_slice(&mut b, GuestAddress(addr))
            .expect("ring is inside guest RAM");
        u32::from_le_bytes(b)
    }
}
