//! virtio-vsock (device type 19): the virtqueue shell around [`aemulator_hal::vsock::VsockCore`].
//!
//! Everything protocol-shaped — headers, connection state, credit accounting, the service registry —
//! lives in `aemulator-hal` so it is testable without a VM and reusable by the Linux/KVM backend. This
//! file does only what needs guest memory: walk the rings, hand parsed packets to the core, and scatter
//! its replies back.
//!
//! Three queues, per the spec: `RXQ` (guest posts empty buffers, we fill them), `TXQ` (guest posts
//! packets for us), and `EVENTQ` (transport resets). We never populate the event queue — that is only
//! needed to tell a guest its CID changed under a live migration, which cannot happen here — but the
//! queue must exist or the guest driver's probe fails.
//!
//! ## Why this device exists
//!
//! Two Cuttlefish HALs dial `AF_VSOCK` rather than opening a console port, and with no vsock transport
//! at all they cannot even create the socket. The RIL's failure is the expensive one: it crash-loops,
//! init counts four failures in four minutes, and `flags_health_check` then resets every
//! server-configurable flag on the device.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_VSOCK};
use aemulator_hal::vsock::{OutPacket, VsockCore, VsockHdr, VsockRegistry, VSOCK_HDR_LEN};
use std::collections::VecDeque;
use std::sync::Arc;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

pub const RXQ: u16 = 0;
pub const TXQ: u16 = 1;
pub const EVENTQ: u16 = 2;

/// Cap on packets held for the guest when it has posted no receive buffers. Generous enough that a
/// burst is never lost, bounded so a guest that stops servicing its ring cannot grow host memory
/// without limit.
const MAX_PENDING: usize = 512;

pub struct Vsock {
    core: VsockCore,
    /// Packets ready for the guest's receive queue.
    pending: VecDeque<OutPacket>,
    /// Packets dropped because [`MAX_PENDING`] was reached — reported rather than hidden, since silent
    /// loss on a stream socket would look like an unexplained protocol hang.
    dropped: u64,
}

impl Vsock {
    pub fn new(registry: Arc<VsockRegistry>) -> Self {
        Self {
            core: VsockCore::new(registry),
            pending: VecDeque::new(),
            dropped: 0,
        }
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_VSOCK
    }

    pub fn features(&self) -> u64 {
        // No F_SEQPACKET: stream sockets only, which is all the Cuttlefish HALs use.
        VIRTIO_F_VERSION_1
    }

    /// rx, tx, event.
    pub fn num_queues(&self) -> usize {
        3
    }

    /// Config space is a single `u64 guest_cid` at offset 0.
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        let bytes = self.core.guest_cid().to_le_bytes();
        let mut v = 0u64;
        for i in 0..len.min(8) {
            let idx = off as usize + i;
            if idx < bytes.len() {
                v |= (bytes[idx] as u64) << (8 * i);
            }
        }
        v
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    /// Take the doorbell the delivery thread waits on. Available once.
    pub fn take_doorbell(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.core.take_doorbell()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn enqueue(&mut self, packets: Vec<OutPacket>) {
        for p in packets {
            if self.pending.len() >= MAX_PENDING {
                self.dropped += 1;
                if self.dropped == 1 || self.dropped.is_multiple_of(256) {
                    eprintln!(
                        "[vsock] receive backlog full ({MAX_PENDING}) — dropped {} packet(s); the \
                         guest is not servicing its rx queue",
                        self.dropped
                    );
                }
                continue;
            }
            self.pending.push_back(p);
        }
    }

    /// Drain the guest's transmit queue and dispatch each packet to the core.
    fn drain_tx(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            // Gather the chain: a packet's header and body may span descriptors.
            let mut buf: Vec<u8> = Vec::new();
            for desc in chain {
                if !desc.is_write_only() {
                    let mut d = vec![0u8; desc.len() as usize];
                    if mem.read_slice(&mut d, GuestAddress(desc.addr().0)).is_ok() {
                        buf.extend_from_slice(&d);
                    }
                }
            }
            let _ = queue.add_used(mem, head, 0);
            used = true;
            // A chain shorter than a header is malformed; consume it rather than stalling the ring.
            if let Some(hdr) = VsockHdr::parse(&buf) {
                let body = &buf[VSOCK_HDR_LEN.min(buf.len())..];
                let out = self.core.on_guest_packet(hdr, body);
                self.enqueue(out);
            }
        }
        used
    }

    /// Fill the guest's posted receive buffers from `pending`, one packet per descriptor chain.
    ///
    /// A packet is only emitted if the chain can hold it whole: vsock has no fragmentation, so a
    /// partial write would corrupt the stream. An undersized chain is completed empty and the packet
    /// stays queued for a larger one.
    fn fill_rx(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while !self.pending.is_empty() {
            let Some(chain) = queue.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            // Collect the writable descriptors so capacity is known before committing to a packet.
            let mut slots: Vec<(u64, usize)> = Vec::new();
            let mut capacity = 0usize;
            for desc in chain {
                if desc.is_write_only() {
                    let len = desc.len() as usize;
                    slots.push((desc.addr().0, len));
                    capacity += len;
                }
            }
            let wire = match self.pending.front() {
                Some(p) if VSOCK_HDR_LEN + p.body.len() <= capacity => {
                    // Safe to pop: it fits.
                    self.pending.pop_front().map(|p| p.to_bytes())
                }
                _ => None,
            };
            let Some(wire) = wire else {
                let _ = queue.add_used(mem, head, 0);
                used = true;
                break; // wait for a chain big enough
            };
            let mut off = 0usize;
            let mut written = 0u32;
            for (addr, len) in slots {
                if off >= wire.len() {
                    break;
                }
                let n = len.min(wire.len() - off);
                if mem
                    .write_slice(&wire[off..off + n], GuestAddress(addr))
                    .is_ok()
                {
                    written += n as u32;
                }
                off += n;
            }
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    /// Cross-queue notify: a guest packet may need an immediate reply, so a TX drain must be able to
    /// fill RX in the same interrupt cycle. Mirrors the console's arrangement.
    pub fn on_notify(&mut self, idx: u16, mem: &Mem, queues: &mut [Queue]) -> bool {
        let mut produced = false;
        if idx == TXQ {
            if let Some(txq) = queues.get_mut(TXQ as usize) {
                produced |= self.drain_tx(mem, txq);
            }
        }
        if let Some(rxq) = queues.get_mut(RXQ as usize) {
            produced |= self.fill_rx(mem, rxq);
        }
        produced
    }

    /// Push whatever host services have produced asynchronously. Called by the delivery thread when the
    /// doorbell rings or its poll expires.
    pub fn flush_pending(&mut self, mem: &Mem, queues: &mut [Queue]) -> bool {
        let out = self.core.flush_all();
        self.enqueue(out);
        if self.pending.is_empty() {
            return false;
        }
        match queues.get_mut(RXQ as usize) {
            Some(rxq) => self.fill_rx(mem, rxq),
            None => false,
        }
    }

    /// Single-queue entry required by the shared `Backend::process_queue` match. The transports call
    /// [`Vsock::on_notify`] instead; this covers the TX-only path for completeness.
    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match idx {
            TXQ => self.drain_tx(mem, queue),
            RXQ => self.fill_rx(mem, queue),
            // The event queue is never populated; a notify on it just reclaims nothing.
            _ => false,
        }
    }
}
