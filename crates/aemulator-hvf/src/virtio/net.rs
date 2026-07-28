//! virtio-net (device type 1): a modern MMIO/PCI virtio-net backend.
//!
//! **Two modes, selected at build time:**
//!
//! * **Default build (no `net` feature)** — an *enumerate-only stub*. The guest negotiates features,
//!   sets up the RX/TX virtqueues, reads the MAC, and brings `eth0` up with the link **UP**; TX
//!   frames are drained-and-dropped and no RX frame is ever delivered. Enough for the guest to
//!   discover a NIC and for Android's `netd`/`ip` stack to probe it. This keeps the pure build
//!   hermetic (no libslirp link).
//!
//! * **`--features net`** — a *real user-mode NAT datapath* backed by libslirp
//!   ([`crate::net::SlirpNet`]). Guest TX frames have their 12-byte `virtio_net_hdr_v1` stripped and
//!   the ethernet frame is handed to `slirp_input`; inbound frames slirp produces (DHCP/ARP/DNS
//!   replies + NAT traffic) are pulled from the backend and written into guest-posted receiveq
//!   buffers with a fresh 12-byte header, raising the device IRQ. The guest gets 10.0.2.15/24, a
//!   gateway/NAT at 10.0.2.2, DNS at 10.0.2.3, DHCP, and internet — like `qemu -netdev user`.
//!
//! Two virtqueues (one queue pair, no `VIRTIO_NET_F_MQ`, no control vq). Config space is the
//! `struct virtio_net_config` prefix — `mac[6]`, `status`, `max_virtqueue_pairs`, `mtu` — read
//! because we advertise `VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS`.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_NET};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

/// Receiveq: guest posts write-only buffers to be filled with inbound frames.
pub const RECEIVEQ: u16 = 0;
/// Transmitq: guest posts read-only buffers holding frames to send.
pub const TRANSMITQ: u16 = 1;

// virtio-net feature bits (virtio spec §5.1.3).
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;

// virtio_net_config.status bits (virtio spec §5.1.4).
const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Default MTU we report to the guest (bytes).
const NET_MTU: u16 = 1500;

/// Length of `struct virtio_net_hdr_v1` — always 12 bytes for a modern (VERSION_1) device, whether
/// or not `VIRTIO_NET_F_MRG_RXBUF` is negotiated (the `num_buffers` field is always present).
const NET_HDR_LEN: usize = 12;

pub struct Net {
    /// The MAC we present in config space (locally-administered; QEMU `52:54:00` OUI).
    mac: [u8; 6],
    /// Count of TX packets drained — surfaced for diagnostics only.
    tx_packets: u64,
    /// The libslirp NAT backend. `None` = enumerate-only stub (TX dropped, no RX). Only ever `Some`
    /// on the `net` build.
    slirp: Option<std::sync::Arc<crate::net::SlirpNet>>,
}

impl Default for Net {
    fn default() -> Self {
        Self::new([0x52, 0x54, 0x00, 0xae, 0x00, 0x01])
    }
}

impl Net {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            tx_packets: 0,
            slirp: None,
        }
    }

    /// Build a NAT-backed NIC bridged to `slirp`.
    pub fn with_slirp(mac: [u8; 6], slirp: std::sync::Arc<crate::net::SlirpNet>) -> Self {
        Self {
            mac,
            tx_packets: 0,
            slirp: Some(slirp),
        }
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_NET
    }

    /// Clone the attached NAT backend handle, if any (for the poll thread).
    pub fn slirp(&self) -> Option<std::sync::Arc<crate::net::SlirpNet>> {
        self.slirp.clone()
    }

    /// Modern device (`VERSION_1`) advertising a fixed MAC and a link-status field. No MQ, no
    /// control vq, no checksum/GSO offloads — so the guest hands us fully-formed ethernet frames and
    /// we hand it fully-formed ones back (header `gso_type = NONE`).
    pub fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
    }

    /// One RX/TX queue pair.
    pub fn num_queues(&self) -> usize {
        2
    }

    /// `struct virtio_net_config`: `mac[6]`, `le16 status`, `le16 max_virtqueue_pairs`, `le16 mtu`.
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        let mut cfg = [0u8; 12];
        cfg[0..6].copy_from_slice(&self.mac);
        cfg[6..8].copy_from_slice(&VIRTIO_NET_S_LINK_UP.to_le_bytes());
        cfg[8..10].copy_from_slice(&1u16.to_le_bytes()); // max_virtqueue_pairs
        cfg[10..12].copy_from_slice(&NET_MTU.to_le_bytes());
        let mut v = 0u64;
        for i in 0..len {
            let idx = off as usize + i;
            let b = cfg.get(idx).copied().unwrap_or(0);
            v |= (b as u64) << (8 * i);
        }
        v
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    /// Queue notify: TX → drain (forward to slirp); RX → flush any pending inbound frames into the
    /// buffers the guest just posted.
    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match idx {
            TRANSMITQ => self.drain_tx(mem, queue),
            RECEIVEQ => self.flush_rx(mem, queue),
            _ => false,
        }
    }

    /// Consume every posted TX chain and mark it used. On the `net` build the ethernet frame (after
    /// the 12-byte virtio_net_hdr) is forwarded into libslirp; otherwise the bytes are dropped.
    /// Returns whether any buffer was completed (→ raise the device interrupt).
    fn drain_tx(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();

            if self.slirp.is_some() {
                // Gather the readable descriptors into one buffer, strip the 12-byte header, forward.
                let mut frame: Vec<u8> = Vec::with_capacity(crate::net::slirp::MAX_FRAME);
                for d in chain.into_iter() {
                    if d.is_write_only() {
                        continue;
                    }
                    let len = d.len() as usize;
                    let mut tmp = vec![0u8; len];
                    if mem.read_slice(&mut tmp, GuestAddress(d.addr().0)).is_ok() {
                        frame.extend_from_slice(&tmp);
                    }
                    if frame.len() > crate::net::slirp::MAX_FRAME + NET_HDR_LEN {
                        break;
                    }
                }
                if frame.len() > NET_HDR_LEN {
                    if let Some(slirp) = &self.slirp {
                        slirp.input(&frame[NET_HDR_LEN..]);
                    }
                }
            }

            self.tx_packets += 1;
            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        used
    }

    /// Fill guest-posted receiveq buffers with inbound frames from the NAT backend. Each frame gets a
    /// fresh 12-byte `virtio_net_hdr_v1` (`num_buffers = 1`) prepended, scatter-written across the
    /// chain's writable descriptors. Returns whether any buffer was completed (→ raise the IRQ).
    /// No-op (returns `false`) on the stub build or before a backend is attached.
    pub fn flush_rx(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let Some(slirp) = self.slirp.clone() else {
            return false;
        };
        let mut used = false;
        loop {
            if !slirp.has_rx() {
                break;
            }
            let Some(chain) = queue.pop_descriptor_chain(mem) else {
                break; // guest has no free RX buffer; leave frames queued
            };
            let head = chain.head_index();
            // Collect this chain's writable descriptors (scatter targets).
            let targets: Vec<(u64, usize)> = chain
                .into_iter()
                .filter(|d| d.is_write_only())
                .map(|d| (d.addr().0, d.len() as usize))
                .collect();

            // Now that a buffer is secured, take a frame.
            let Some(frame) = slirp.pop_rx() else {
                let _ = queue.add_used(mem, head, 0);
                used = true;
                break;
            };

            // Assemble header + frame.
            let mut hdr = [0u8; NET_HDR_LEN];
            hdr[10..12].copy_from_slice(&1u16.to_le_bytes()); // num_buffers = 1
            let written = scatter_write(mem, &targets, &hdr, &frame);
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

}

/// Scatter-write `hdr` followed by `frame` across the writable descriptor `targets` (addr, len) of a
/// receiveq chain. Returns the total number of bytes written (the used-ring length the guest reads).
/// Bytes that don't fit are dropped (an MTU-sized frame always fits a standard 1526-byte RX buffer).
fn scatter_write(mem: &Mem, targets: &[(u64, usize)], hdr: &[u8], frame: &[u8]) -> u32 {
    let mut written = 0u32;
    let mut src_hdr = hdr;
    let mut src_frame = frame;
    for &(addr, len) in targets {
        if src_hdr.is_empty() && src_frame.is_empty() {
            break;
        }
        let mut off = 0usize;
        // header bytes first
        if !src_hdr.is_empty() {
            let n = src_hdr.len().min(len);
            if mem.write_slice(&src_hdr[..n], GuestAddress(addr)).is_ok() {
                written += n as u32;
            }
            off += n;
            src_hdr = &src_hdr[n..];
        }
        // then frame bytes into the remaining space of this descriptor
        if off < len && !src_frame.is_empty() {
            let n = src_frame.len().min(len - off);
            if mem
                .write_slice(&src_frame[..n], GuestAddress(addr + off as u64))
                .is_ok()
            {
                written += n as u32;
            }
            src_frame = &src_frame[n..];
        }
    }
    written
}
