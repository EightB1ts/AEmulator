//! virtio-vsock: the packet layer plus a host-side service registry.
//!
//! ## Why the guest needs this
//!
//! Two Cuttlefish HALs do not use a virtio-console port — they open an `AF_VSOCK` socket to a host
//! port named by a `ro.boot.*` property:
//!
//! * **RIL / modem.** `vendor.ril-daemon` reads `ro.boot.modem_simulator_ports` and dies with
//!   `'ro.boot.modem_simulator_ports' must be an integer vsock port for the modem simulator`. That
//!   crash is not merely noisy: init counts it, `vendor.ril-daemon exited 4 times in 4 minutes` sets
//!   `sys.init.updatable_crashing=1`, and `flags_health_check` then **resets every
//!   server-configurable flag** on the device. It is the one symptom in the original report that is a
//!   correctness bug rather than log spam.
//! * **Lights.** The Rust lights HAL reads `ro.boot.vsock_lights_port` and connects to it.
//!
//! ## Scope: guest-initiated connections only
//!
//! Every Cuttlefish HAL *dials out*; none of them listens for the host to connect in. So this
//! implements the guest→host direction and deliberately does not implement host-initiated connect: no
//! outbound `REQUEST`, and the event queue exists only so the guest's driver probe succeeds. That
//! removes the need for a guest-side listening-port table and an accept path. Adding dial-in later is
//! additive (`registry.dial(port)`), not a redesign.
//!
//! ## Flow control, honestly described
//!
//! Each side reports its own `buf_alloc` and `fwd_cnt` in every header. We advertise a fixed
//! [`HOST_BUF_ALLOC`] and track how much we have handed to the local service. Before sending we
//! compute the peer's remaining window and send at most that much, holding the rest in the
//! connection's out-queue until a later `RW`/`CREDIT_UPDATE` refreshes it. Nothing is ever dropped,
//! but this is *delay*, not true backpressure — a service that produced data faster than the guest
//! consumed it forever would grow that queue. For AT-command and lights traffic against a 256 KiB
//! window it never binds; if a future service streams bulk data, that is the thing to revisit.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// `VMADDR_CID_HOST` — the well-known CID a guest dials to reach its host.
pub const HOST_CID: u64 = 2;
/// The guest's CID. Cuttlefish derives this per instance; we run one VM.
pub const GUEST_CID: u64 = 3;

/// Size of `virtio_vsock_hdr`.
pub const VSOCK_HDR_LEN: usize = 44;

/// Receive window we advertise, in bytes. 256 KiB is Linux's own default for a vsock socket, so
/// nothing about our window looks unusual to the guest.
pub const HOST_BUF_ALLOC: u32 = 262_144;

/// The only socket type we accept.
pub const VSOCK_TYPE_STREAM: u16 = 1;

/// Operations, per the virtio spec.
pub mod op {
    pub const INVALID: u16 = 0;
    pub const REQUEST: u16 = 1;
    pub const RESPONSE: u16 = 2;
    pub const RST: u16 = 3;
    pub const SHUTDOWN: u16 = 4;
    pub const RW: u16 = 5;
    pub const CREDIT_UPDATE: u16 = 6;
    pub const CREDIT_REQUEST: u16 = 7;
}

/// `SHUTDOWN` flags.
pub const SHUTDOWN_F_RECEIVE: u32 = 1;
pub const SHUTDOWN_F_SEND: u32 = 2;

/// A `virtio_vsock_hdr`. All fields little-endian on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct VsockHdr {
    pub src_cid: u64,
    pub dst_cid: u64,
    pub src_port: u32,
    pub dst_port: u32,
    pub len: u32,
    pub ty: u16,
    pub op: u16,
    pub flags: u32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
}

impl VsockHdr {
    /// Decode a header, or `None` if the slice is too short. Never panics on guest input.
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < VSOCK_HDR_LEN {
            return None;
        }
        let u32at = |o: usize| -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) };
        let u64at = |o: usize| -> u64 {
            u64::from_le_bytes([
                b[o],
                b[o + 1],
                b[o + 2],
                b[o + 3],
                b[o + 4],
                b[o + 5],
                b[o + 6],
                b[o + 7],
            ])
        };
        Some(Self {
            src_cid: u64at(0),
            dst_cid: u64at(8),
            src_port: u32at(16),
            dst_port: u32at(20),
            len: u32at(24),
            ty: u16::from_le_bytes([b[28], b[29]]),
            op: u16::from_le_bytes([b[30], b[31]]),
            flags: u32at(32),
            buf_alloc: u32at(36),
            fwd_cnt: u32at(40),
        })
    }

    pub fn to_bytes(self) -> [u8; VSOCK_HDR_LEN] {
        let mut b = [0u8; VSOCK_HDR_LEN];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.ty.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

/// The host end of one guest connection, as handed to a [`VsockService`].
///
/// Cloneable and thread-safe so a service can hold it and push data from its own thread (an unsolicited
/// AT unsolicited-result-code, for example) without touching any VM state.
#[derive(Clone)]
pub struct ConnHandle {
    out: Arc<Mutex<VecDeque<u8>>>,
    /// Best-effort wake for the VMM's delivery thread. An unbounded `Sender::send` never blocks, and a
    /// disconnected channel is not an error: the delivery thread also polls on a timeout, so a lost
    /// nudge only delays a flush rather than stranding data.
    doorbell: std::sync::mpsc::Sender<()>,
}

impl ConnHandle {
    /// Queue bytes for delivery to the guest.
    pub fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut q) = self.out.lock() {
            q.extend(bytes.iter().copied());
        }
        let _ = self.doorbell.send(());
    }

    /// Queue a string. Convenience for the line-oriented services.
    pub fn send_str(&self, s: &str) {
        self.send(s.as_bytes());
    }
}

/// A host service reachable at one vsock port.
///
/// One instance is created per guest connection, so per-connection state (a line buffer, say) can live
/// in `self` without any sharing discipline. Cross-connection state belongs in an `Arc<Mutex<…>>` the
/// factory captures.
pub trait VsockService: Send {
    /// The guest has connected. The handle is how to push data back, including later, from another
    /// thread.
    fn on_connect(&mut self, conn: ConnHandle);
    /// The guest sent bytes. Must not block.
    fn on_data(&mut self, data: &[u8]);
    /// The connection is gone (guest shutdown/reset). Default: nothing to do.
    fn on_shutdown(&mut self) {}
}

/// Builds a fresh service for each incoming connection.
pub type ServiceFactory = Box<dyn Fn() -> Box<dyn VsockService> + Send + Sync>;

/// Maps vsock port → service factory.
#[derive(Default)]
pub struct VsockRegistry {
    services: Mutex<HashMap<u32, ServiceFactory>>,
}

impl VsockRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a service on `port`. Replaces any previous registration for that port.
    pub fn register(&self, port: u32, factory: ServiceFactory) {
        if let Ok(mut m) = self.services.lock() {
            m.insert(port, factory);
        }
    }

    pub fn ports(&self) -> Vec<u32> {
        match self.services.lock() {
            Ok(m) => {
                let mut v: Vec<u32> = m.keys().copied().collect();
                v.sort_unstable();
                v
            }
            Err(_) => Vec::new(),
        }
    }

    /// Instantiate the service for `port`, if one is registered.
    pub fn create(&self, port: u32) -> Option<Box<dyn VsockService>> {
        let m = self.services.lock().ok()?;
        Some(m.get(&port)?())
    }
}

/// One live guest→host connection.
struct Conn {
    service: Box<dyn VsockService>,
    out: Arc<Mutex<VecDeque<u8>>>,
    /// The peer's advertised window and how much it has consumed.
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    /// Bytes we have transmitted, for the window calculation.
    tx_cnt: u32,
    /// Bytes we have delivered to the local service — our own `fwd_cnt`.
    rx_cnt: u32,
}

/// A packet the device should place on the guest's receive queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutPacket {
    pub hdr: VsockHdr,
    pub body: Vec<u8>,
}

impl OutPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(VSOCK_HDR_LEN + self.body.len());
        v.extend_from_slice(&self.hdr.to_bytes());
        v.extend_from_slice(&self.body);
        v
    }
}

/// Largest `RW` body we will emit in one packet. Keeps a single packet inside a typical guest receive
/// buffer so the device never has to fragment.
const MAX_RW_BODY: usize = 4096;

/// The transport-independent vsock state machine: guest packets in, host packets out.
///
/// Deliberately knows nothing about virtqueues or guest memory — the device shell in the VMM feeds it
/// parsed packets and transmits what it returns, which is what makes all of this testable without a VM.
pub struct VsockCore {
    registry: Arc<VsockRegistry>,
    conns: HashMap<(u32, u32), Conn>,
    doorbell_tx: std::sync::mpsc::Sender<()>,
    doorbell_rx: Option<std::sync::mpsc::Receiver<()>>,
    guest_cid: u64,
    /// Connections refused because no service was registered — surfaced for diagnostics.
    refused: u64,
}

impl VsockCore {
    pub fn new(registry: Arc<VsockRegistry>) -> Self {
        // An unbounded channel: the doorbell carries no data, only "there may be work", so the queue
        // depth is irrelevant and a never-blocking send keeps service threads off the VM's critical
        // path. The delivery thread drains it and also polls on a timeout, so a coalesced or lost
        // wake-up costs latency, never data.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        Self {
            registry,
            conns: HashMap::new(),
            doorbell_tx: tx,
            doorbell_rx: Some(rx),
            guest_cid: GUEST_CID,
            refused: 0,
        }
    }

    /// Take the doorbell receiver — the VMM's delivery thread waits on it. Available once.
    pub fn take_doorbell(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.doorbell_rx.take()
    }

    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    pub fn refused_count(&self) -> u64 {
        self.refused
    }

    /// Header template for a packet we send back on an established connection: our CID/port as source,
    /// the guest's as destination, carrying our current window state.
    fn reply_hdr(&self, key: (u32, u32), op: u16, len: u32, conn: Option<&Conn>) -> VsockHdr {
        let (guest_port, host_port) = key;
        VsockHdr {
            src_cid: HOST_CID,
            dst_cid: self.guest_cid,
            src_port: host_port,
            dst_port: guest_port,
            len,
            ty: VSOCK_TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: HOST_BUF_ALLOC,
            fwd_cnt: conn.map(|c| c.rx_cnt).unwrap_or(0),
        }
    }

    /// Handle one packet from the guest; return packets to send back.
    pub fn on_guest_packet(&mut self, hdr: VsockHdr, body: &[u8]) -> Vec<OutPacket> {
        // Track the guest's CID from whatever it actually uses, rather than assuming, so a differently
        // configured guest still works.
        if hdr.src_cid != 0 {
            self.guest_cid = hdr.src_cid;
        }
        let key = (hdr.src_port, hdr.dst_port);

        // Only stream sockets exist in this implementation. Reset anything else rather than
        // half-supporting it.
        if hdr.ty != VSOCK_TYPE_STREAM && hdr.op != op::RST {
            return vec![OutPacket {
                hdr: self.reply_hdr(key, op::RST, 0, None),
                body: Vec::new(),
            }];
        }

        match hdr.op {
            op::REQUEST => {
                let Some(service) = self.registry.create(hdr.dst_port) else {
                    // No service on that port: connection refused, which is exactly what RST means.
                    self.refused += 1;
                    return vec![OutPacket {
                        hdr: self.reply_hdr(key, op::RST, 0, None),
                        body: Vec::new(),
                    }];
                };
                let out = Arc::new(Mutex::new(VecDeque::new()));
                let mut conn = Conn {
                    service,
                    out: out.clone(),
                    peer_buf_alloc: hdr.buf_alloc,
                    peer_fwd_cnt: hdr.fwd_cnt,
                    tx_cnt: 0,
                    rx_cnt: 0,
                };
                conn.service.on_connect(ConnHandle {
                    out,
                    doorbell: self.doorbell_tx.clone(),
                });
                self.conns.insert(key, conn);
                let hdr = self.reply_hdr(key, op::RESPONSE, 0, self.conns.get(&key));
                vec![OutPacket {
                    hdr,
                    body: Vec::new(),
                }]
            }

            op::RW => {
                let Some(conn) = self.conns.get_mut(&key) else {
                    // Data on a connection we do not have: tell the guest to give up on it.
                    return vec![OutPacket {
                        hdr: self.reply_hdr(key, op::RST, 0, None),
                        body: Vec::new(),
                    }];
                };
                conn.peer_buf_alloc = hdr.buf_alloc;
                conn.peer_fwd_cnt = hdr.fwd_cnt;
                // The declared length is advisory; trust the body we actually received.
                let n = (hdr.len as usize).min(body.len());
                conn.rx_cnt = conn.rx_cnt.wrapping_add(n as u32);
                conn.service.on_data(&body[..n]);
                // A reply may already be queued by on_data; flush it now so a request/response service
                // answers within the same notify.
                self.flush_conn(key)
            }

            op::SHUTDOWN => {
                // Either direction closing is treated as end-of-connection: these are simple
                // request/response services with no half-open use case.
                if let Some(mut conn) = self.conns.remove(&key) {
                    conn.service.on_shutdown();
                }
                let _ = hdr.flags & (SHUTDOWN_F_RECEIVE | SHUTDOWN_F_SEND);
                vec![OutPacket {
                    hdr: self.reply_hdr(key, op::RST, 0, None),
                    body: Vec::new(),
                }]
            }

            op::RST => {
                if let Some(mut conn) = self.conns.remove(&key) {
                    conn.service.on_shutdown();
                }
                // Never answer a reset with a reset.
                Vec::new()
            }

            op::CREDIT_UPDATE => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.peer_buf_alloc = hdr.buf_alloc;
                    conn.peer_fwd_cnt = hdr.fwd_cnt;
                }
                // A refreshed window may unblock data we were holding.
                self.flush_conn(key)
            }

            op::CREDIT_REQUEST => {
                if let Some(conn) = self.conns.get_mut(&key) {
                    conn.peer_buf_alloc = hdr.buf_alloc;
                    conn.peer_fwd_cnt = hdr.fwd_cnt;
                }
                let hdr = self.reply_hdr(key, op::CREDIT_UPDATE, 0, self.conns.get(&key));
                vec![OutPacket {
                    hdr,
                    body: Vec::new(),
                }]
            }

            // RESPONSE is never expected (we do not dial out). Anything else is unknown. Reset rather
            // than silently ignore, so the guest is not left waiting.
            _ => vec![OutPacket {
                hdr: self.reply_hdr(key, op::RST, 0, None),
                body: Vec::new(),
            }],
        }
    }

    /// Emit as much of one connection's queued output as the peer's window allows.
    fn flush_conn(&mut self, key: (u32, u32)) -> Vec<OutPacket> {
        let mut out = Vec::new();
        let Some(conn) = self.conns.get_mut(&key) else {
            return out;
        };
        loop {
            // The peer's remaining window: what it advertised, minus what we have sent that it has not
            // yet reported forwarding. `wrapping_sub` because both counters wrap at u32 by design.
            let in_flight = conn.tx_cnt.wrapping_sub(conn.peer_fwd_cnt);
            let window = conn.peer_buf_alloc.saturating_sub(in_flight) as usize;
            if window == 0 {
                break; // hold the rest until a CREDIT_UPDATE arrives
            }
            let take = {
                let Ok(q) = conn.out.lock() else { break };
                q.len().min(window).min(MAX_RW_BODY)
            };
            if take == 0 {
                break;
            }
            let body: Vec<u8> = {
                let Ok(mut q) = conn.out.lock() else { break };
                q.drain(0..take).collect()
            };
            conn.tx_cnt = conn.tx_cnt.wrapping_add(body.len() as u32);
            let (guest_port, host_port) = key;
            out.push(OutPacket {
                hdr: VsockHdr {
                    src_cid: HOST_CID,
                    dst_cid: self.guest_cid,
                    src_port: host_port,
                    dst_port: guest_port,
                    len: body.len() as u32,
                    ty: VSOCK_TYPE_STREAM,
                    op: op::RW,
                    flags: 0,
                    buf_alloc: HOST_BUF_ALLOC,
                    fwd_cnt: conn.rx_cnt,
                },
                body,
            });
        }
        out
    }

    /// Emit queued output for every connection. The delivery thread calls this when the doorbell rings
    /// (or on its poll timeout), which is how data a service produced asynchronously reaches the guest.
    pub fn flush_all(&mut self) -> Vec<OutPacket> {
        let keys: Vec<(u32, u32)> = self.conns.keys().copied().collect();
        let mut out = Vec::new();
        for k in keys {
            out.extend(self.flush_conn(k));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A service that echoes what it receives, prefixed, and records everything.
    struct Echo {
        conn: Option<ConnHandle>,
        seen: Arc<Mutex<Vec<u8>>>,
        shutdowns: Arc<Mutex<u32>>,
    }
    impl VsockService for Echo {
        fn on_connect(&mut self, conn: ConnHandle) {
            conn.send_str("HELLO\r\n");
            self.conn = Some(conn);
        }
        fn on_data(&mut self, data: &[u8]) {
            if let Ok(mut s) = self.seen.lock() {
                s.extend_from_slice(data);
            }
            if let Some(c) = &self.conn {
                c.send(data); // echo
            }
        }
        fn on_shutdown(&mut self) {
            if let Ok(mut n) = self.shutdowns.lock() {
                *n += 1;
            }
        }
    }

    const HOST_PORT: u32 = 9600;
    const GUEST_PORT: u32 = 1024;

    /// (core, bytes the service received, shutdown count) — named so the signature stays readable.
    type Harness = (VsockCore, Arc<Mutex<Vec<u8>>>, Arc<Mutex<u32>>);

    fn setup() -> Harness {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shutdowns = Arc::new(Mutex::new(0u32));
        let reg = VsockRegistry::new();
        let s2 = seen.clone();
        let d2 = shutdowns.clone();
        reg.register(
            HOST_PORT,
            Box::new(move || {
                Box::new(Echo {
                    conn: None,
                    seen: s2.clone(),
                    shutdowns: d2.clone(),
                })
            }),
        );
        (VsockCore::new(reg), seen, shutdowns)
    }

    fn guest_hdr(op: u16, len: u32) -> VsockHdr {
        VsockHdr {
            src_cid: GUEST_CID,
            dst_cid: HOST_CID,
            src_port: GUEST_PORT,
            dst_port: HOST_PORT,
            len,
            ty: VSOCK_TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 65536,
            fwd_cnt: 0,
        }
    }

    #[test]
    fn header_round_trips_and_is_44_bytes() {
        let h = VsockHdr {
            src_cid: 3,
            dst_cid: 2,
            src_port: 1024,
            dst_port: 9600,
            len: 7,
            ty: VSOCK_TYPE_STREAM,
            op: op::RW,
            flags: 3,
            buf_alloc: 262144,
            fwd_cnt: 99,
        };
        let b = h.to_bytes();
        assert_eq!(b.len(), VSOCK_HDR_LEN);
        assert_eq!(VsockHdr::parse(&b), Some(h));
    }

    #[test]
    fn short_header_is_rejected_not_panicked_on() {
        for n in 0..VSOCK_HDR_LEN {
            assert_eq!(VsockHdr::parse(&vec![0u8; n]), None, "len {n}");
        }
    }

    #[test]
    fn request_to_a_registered_port_is_accepted_and_the_service_can_greet() {
        let (mut core, _, _) = setup();
        let out = core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        assert_eq!(out[0].hdr.op, op::RESPONSE, "connection must be accepted");
        assert_eq!(out[0].hdr.src_cid, HOST_CID);
        assert_eq!(out[0].hdr.dst_cid, GUEST_CID);
        assert_eq!(out[0].hdr.src_port, HOST_PORT);
        assert_eq!(out[0].hdr.dst_port, GUEST_PORT);
        assert_eq!(core.connection_count(), 1);
        // The greeting queued during on_connect flushes on the next opportunity.
        let flushed = core.flush_all();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].hdr.op, op::RW);
        assert_eq!(flushed[0].body, b"HELLO\r\n");
        assert_eq!(flushed[0].hdr.len as usize, flushed[0].body.len());
    }

    /// A port with nothing listening must be refused, not left hanging — the guest needs to fail fast.
    #[test]
    fn request_to_an_unregistered_port_is_reset() {
        let (mut core, _, _) = setup();
        let mut h = guest_hdr(op::REQUEST, 0);
        h.dst_port = 4242;
        let out = core.on_guest_packet(h, &[]);
        assert_eq!(out[0].hdr.op, op::RST);
        assert_eq!(core.connection_count(), 0);
        assert_eq!(core.refused_count(), 1);
    }

    #[test]
    fn rw_reaches_the_service_and_its_reply_comes_back_in_the_same_call() {
        let (mut core, seen, _) = setup();
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        let _ = core.flush_all(); // drain the greeting
        let out = core.on_guest_packet(guest_hdr(op::RW, 4), b"AT\r\n");
        assert_eq!(seen.lock().unwrap().as_slice(), b"AT\r\n");
        assert_eq!(out.len(), 1, "the echo must return without a second notify");
        assert_eq!(out[0].body, b"AT\r\n");
    }

    /// The guest's declared length must not be trusted over the body we actually got, or a short read
    /// would let it make us index past the end.
    #[test]
    fn declared_length_longer_than_the_body_is_clamped() {
        let (mut core, seen, _) = setup();
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        let _ = core.flush_all();
        core.on_guest_packet(guest_hdr(op::RW, 9999), b"hi");
        assert_eq!(seen.lock().unwrap().as_slice(), b"hi");
    }

    #[test]
    fn rw_on_an_unknown_connection_is_reset() {
        let (mut core, _, _) = setup();
        let out = core.on_guest_packet(guest_hdr(op::RW, 2), b"hi");
        assert_eq!(out[0].hdr.op, op::RST);
    }

    #[test]
    fn shutdown_closes_the_connection_and_notifies_the_service() {
        let (mut core, _, shutdowns) = setup();
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        let mut h = guest_hdr(op::SHUTDOWN, 0);
        h.flags = SHUTDOWN_F_RECEIVE | SHUTDOWN_F_SEND;
        let out = core.on_guest_packet(h, &[]);
        assert_eq!(out[0].hdr.op, op::RST);
        assert_eq!(core.connection_count(), 0);
        assert_eq!(*shutdowns.lock().unwrap(), 1);
    }

    /// Answering a RST with a RST would loop forever.
    #[test]
    fn rst_closes_silently() {
        let (mut core, _, shutdowns) = setup();
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        let out = core.on_guest_packet(guest_hdr(op::RST, 0), &[]);
        assert!(out.is_empty(), "must not reply to a reset");
        assert_eq!(core.connection_count(), 0);
        assert_eq!(*shutdowns.lock().unwrap(), 1);
    }

    #[test]
    fn credit_request_is_answered_with_our_window() {
        let (mut core, _, _) = setup();
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);
        let out = core.on_guest_packet(guest_hdr(op::CREDIT_REQUEST, 0), &[]);
        assert_eq!(out[0].hdr.op, op::CREDIT_UPDATE);
        assert_eq!(out[0].hdr.buf_alloc, HOST_BUF_ALLOC);
    }

    /// A closed window must delay, never drop — and a credit update must release the held bytes.
    #[test]
    fn a_closed_peer_window_holds_data_until_credit_arrives() {
        let (mut core, _, _) = setup();
        // Advertise a window of zero from the start.
        let mut h = guest_hdr(op::REQUEST, 0);
        h.buf_alloc = 0;
        core.on_guest_packet(h, &[]);
        // on_connect queued "HELLO\r\n" but there is no window for it.
        assert!(
            core.flush_all().is_empty(),
            "must not send past a zero window"
        );

        let mut credit = guest_hdr(op::CREDIT_UPDATE, 0);
        credit.buf_alloc = 4;
        credit.fwd_cnt = 0;
        let out = core.on_guest_packet(credit, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].body, b"HELL", "only what the 4-byte window allows");

        // Widen the window; the remainder follows, nothing lost.
        let mut credit = guest_hdr(op::CREDIT_UPDATE, 0);
        credit.buf_alloc = 1000;
        credit.fwd_cnt = 4;
        let out = core.on_guest_packet(credit, &[]);
        assert_eq!(out[0].body, b"O\r\n");
    }

    #[test]
    fn non_stream_socket_types_are_reset() {
        let (mut core, _, _) = setup();
        let mut h = guest_hdr(op::REQUEST, 0);
        h.ty = 99;
        let out = core.on_guest_packet(h, &[]);
        assert_eq!(out[0].hdr.op, op::RST);
        assert_eq!(core.connection_count(), 0);
    }

    #[test]
    fn a_service_can_push_asynchronously_after_connecting() {
        let seen: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let grabbed: Arc<Mutex<Option<ConnHandle>>> = Arc::new(Mutex::new(None));
        let reg = VsockRegistry::new();
        struct Grabber(Arc<Mutex<Option<ConnHandle>>>);
        impl VsockService for Grabber {
            fn on_connect(&mut self, conn: ConnHandle) {
                *self.0.lock().unwrap() = Some(conn);
            }
            fn on_data(&mut self, _: &[u8]) {}
        }
        let g2 = grabbed.clone();
        reg.register(HOST_PORT, Box::new(move || Box::new(Grabber(g2.clone()))));
        let mut core = VsockCore::new(reg);
        let mut doorbell = core.take_doorbell().expect("doorbell available once");
        assert!(core.take_doorbell().is_none(), "only handed out once");
        core.on_guest_packet(guest_hdr(op::REQUEST, 0), &[]);

        // Push from "another thread" via the stored handle, as an unsolicited result code would.
        let handle = grabbed.lock().unwrap().clone().expect("handle stored");
        let t = std::thread::spawn(move || handle.send_str("+CREG: 1\r\n"));
        t.join().unwrap();
        assert!(doorbell.try_recv().is_ok(), "send must ring the doorbell");
        let out = core.flush_all();
        assert_eq!(out[0].body, b"+CREG: 1\r\n");
        let _ = &mut doorbell;
        let _ = seen;
    }

    /// Bodies larger than one packet must be split, and every packet's declared length must match.
    #[test]
    fn large_output_is_split_into_well_formed_packets() {
        let big: Vec<u8> = (0..(MAX_RW_BODY * 2 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        let payload = big.clone();
        let reg = VsockRegistry::new();
        struct Blaster(Vec<u8>);
        impl VsockService for Blaster {
            fn on_connect(&mut self, conn: ConnHandle) {
                conn.send(&self.0);
            }
            fn on_data(&mut self, _: &[u8]) {}
        }
        reg.register(
            HOST_PORT,
            Box::new(move || Box::new(Blaster(payload.clone()))),
        );
        let mut core = VsockCore::new(reg);
        let mut h = guest_hdr(op::REQUEST, 0);
        h.buf_alloc = 1 << 20;
        core.on_guest_packet(h, &[]);
        let out = core.flush_all();
        assert!(
            out.len() >= 3,
            "expected splitting, got {} packet(s)",
            out.len()
        );
        let mut rebuilt = Vec::new();
        for p in &out {
            assert_eq!(
                p.hdr.len as usize,
                p.body.len(),
                "declared length must match body"
            );
            assert!(p.body.len() <= MAX_RW_BODY);
            rebuilt.extend_from_slice(&p.body);
        }
        assert_eq!(rebuilt, big, "no bytes lost or reordered across the split");
    }

    #[test]
    fn registry_lists_its_ports() {
        let reg = VsockRegistry::new();
        struct Nop;
        impl VsockService for Nop {
            fn on_connect(&mut self, _: ConnHandle) {}
            fn on_data(&mut self, _: &[u8]) {}
        }
        reg.register(30, Box::new(|| Box::new(Nop)));
        reg.register(10, Box::new(|| Box::new(Nop)));
        assert_eq!(reg.ports(), vec![10, 30]);
        assert!(reg.create(10).is_some());
        assert!(reg.create(11).is_none());
    }

    #[test]
    fn fuzz_lite_never_panics() {
        let (mut core, _, _) = setup();
        let mut x: u32 = 0xfeed_face;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..20_000 {
            let mut raw = [0u8; VSOCK_HDR_LEN];
            for b in raw.iter_mut() {
                *b = (next() & 0xff) as u8;
            }
            if let Some(h) = VsockHdr::parse(&raw) {
                let blen = (next() % 32) as usize;
                let body: Vec<u8> = (0..blen).map(|_| (next() & 0xff) as u8).collect();
                let _ = core.on_guest_packet(h, &body);
            }
            let _ = core.flush_all();
        }
    }
}
