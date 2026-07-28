//! Rust side of the libslirp user-mode NAT backend.
//!
//! [`SlirpNet`] owns the C `AemuSlirp*` (behind a mutex — libslirp is not internally locked) plus a
//! bounded RX buffer of host->guest ethernet frames. Three actors touch it:
//!
//!   * **the vCPU TX path** (`virtio::net::Net::process_queue` on the transmitq) calls [`SlirpNet::input`]
//!     with each guest-emitted frame → `slirp_input`;
//!   * **the poll thread** ([`SlirpNet::run_poll_thread`]) pumps `slirp_pollfds_fill/poll` so slirp
//!     services its sockets/timers; inbound frames land in the RX buffer via the `send_packet`
//!     callback ([`aemu_slirp_recv_frame`]);
//!   * **the RX flush** (poll thread again, or the guest's receiveq notify) drains that buffer into
//!     the guest's posted RX descriptors (`virtio::net::Net::flush_rx`).
//!
//! Lock order (no cycles): `pci`/`virtio` transport lock → `SlirpNet.inner` (slirp) → `SlirpNet.rx`.
//! The blocking `poll()` syscall runs with NO lock held so a TX frame can wake the loop promptly.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::raw::{c_int, c_uint};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---- FFI to slirp_shim.c ----

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

extern "C" {
    fn aemu_slirp_new(rust_opaque: *mut c_void) -> *mut c_void;
    fn aemu_slirp_input(a: *mut c_void, buf: *const u8, len: usize);
    fn aemu_slirp_add_hostfwd(
        a: *mut c_void,
        is_udp: c_int,
        host_ip_be: u32,
        host_port: c_int,
        guest_ip_be: u32,
        guest_port: c_int,
    ) -> c_int;
    fn aemu_slirp_fill(a: *mut c_void, timeout: *mut c_uint);
    fn aemu_slirp_fds(a: *mut c_void) -> *mut PollFd;
    fn aemu_slirp_nfds(a: *mut c_void) -> c_int;
    fn aemu_slirp_poll_result(a: *mut c_void, poll_error: c_int);
    fn aemu_slirp_kick(a: *mut c_void);
    fn aemu_slirp_cleanup(a: *mut c_void);
}

extern "C" {
    // libc poll(2).
    fn poll(fds: *mut PollFd, nfds: c_uint, timeout: c_int) -> c_int;
}

/// Max frame we buffer/deliver (jumbo-free: 1500 MTU + 14 ethernet + slack).
pub const MAX_FRAME: usize = 2048;
/// Cap the RX backlog so a stalled guest (not draining its receiveq) can't grow memory unbounded.
const RX_BACKLOG_CAP: usize = 1024;

/// The raw `AemuSlirp*`. Not `Send`/`Sync` by default (raw pointer); we serialize every access
/// through [`SlirpNet::inner`] and assert it is safe to move across threads.
struct SlirpPtr(*mut c_void);
unsafe impl Send for SlirpPtr {}

/// One host port-forward request: (is_udp, host_port, guest_port). Host bind addr is 127.0.0.1,
/// guest addr is 10.0.2.15 (the DHCP lease).
#[derive(Clone, Copy, Debug)]
pub struct HostFwd {
    pub is_udp: bool,
    pub host_port: u16,
    pub guest_port: u16,
}

pub struct SlirpNet {
    /// The slirp instance. EVERY shim call except `kick` goes through this lock.
    inner: Mutex<SlirpPtr>,
    /// A lock-free copy of the handle for `kick` (writes only the wake pipe — touches no slirp state).
    kick_handle: usize,
    /// Host->guest frames produced by slirp, awaiting delivery into the guest receiveq.
    rx: Mutex<VecDeque<Vec<u8>>>,
    /// Diagnostics.
    pub tx_frames: Mutex<u64>,
    pub rx_frames: Mutex<u64>,
}

impl SlirpNet {
    /// Create the NAT backend and install the requested host port-forwards. Returns an `Arc` shared
    /// by the device (TX), the poll thread, and (as a raw opaque) the `send_packet` callback.
    pub fn new(hostfwds: &[HostFwd]) -> Result<Arc<Self>, String> {
        // Build the shared state first so its address is the stable `opaque` slirp hands back to the
        // recv callback. `Arc::as_ptr` does NOT bump the strong count, so the Arc stays uniquely
        // owned here and `Arc::get_mut` below succeeds.
        let mut net = Arc::new(SlirpNet {
            inner: Mutex::new(SlirpPtr(std::ptr::null_mut())),
            kick_handle: 0,
            rx: Mutex::new(VecDeque::new()),
            tx_frames: Mutex::new(0),
            rx_frames: Mutex::new(0),
        });
        let opaque = Arc::as_ptr(&net) as *mut c_void;
        let handle = unsafe { aemu_slirp_new(opaque) };
        if handle.is_null() {
            return Err("aemu_slirp_new returned NULL (slirp_new failed)".into());
        }
        // Publish the handle into the (still uniquely-owned) Arc.
        {
            let m = Arc::get_mut(&mut net).expect("SlirpNet uniquely owned at construction");
            m.inner.get_mut().unwrap().0 = handle;
            m.kick_handle = handle as usize;
        }

        // Install host port-forwards.
        {
            let g = net.inner.lock().unwrap();
            for f in hostfwds {
                let rc = unsafe {
                    aemu_slirp_add_hostfwd(
                        g.0,
                        if f.is_udp { 1 } else { 0 },
                        u32::from_ne_bytes([127, 0, 0, 1]), // 127.0.0.1 in network order
                        f.host_port as c_int,
                        // 10.0.2.15 in network byte order.
                        u32::from_ne_bytes([10, 0, 2, 15]),
                        f.guest_port as c_int,
                    )
                };
                if rc != 0 {
                    eprintln!(
                        "[slirp][warn] hostfwd {}:{}->guest:{} failed (rc={rc}) — port in use?",
                        if f.is_udp { "udp" } else { "tcp" },
                        f.host_port,
                        f.guest_port
                    );
                } else {
                    println!(
                        "[slirp] hostfwd {} 127.0.0.1:{} -> 10.0.2.15:{}",
                        if f.is_udp { "udp" } else { "tcp" },
                        f.host_port,
                        f.guest_port
                    );
                }
            }
        }
        Ok(net)
    }

    /// Guest -> host: forward a raw ethernet frame (virtio_net_hdr already stripped) into slirp.
    pub fn input(&self, frame: &[u8]) {
        if frame.is_empty() {
            return;
        }
        {
            let g = self.inner.lock().unwrap();
            if !g.0.is_null() {
                unsafe { aemu_slirp_input(g.0, frame.as_ptr(), frame.len()) };
            }
        }
        *self.tx_frames.lock().unwrap() += 1;
    }

    /// Pop the next host->guest frame awaiting delivery, if any.
    pub fn pop_rx(&self) -> Option<Vec<u8>> {
        self.rx.lock().unwrap().pop_front()
    }

    /// Whether any host->guest frame is queued.
    pub fn has_rx(&self) -> bool {
        !self.rx.lock().unwrap().is_empty()
    }

    /// Called by the C `send_packet` callback (from within a locked slirp call). Enqueue a
    /// host->guest frame. Drops the frame if the backlog is full (TCP will retransmit).
    fn on_recv(&self, buf: &[u8]) {
        if buf.is_empty() || buf.len() > MAX_FRAME {
            return;
        }
        let mut rx = self.rx.lock().unwrap();
        if rx.len() >= RX_BACKLOG_CAP {
            return;
        }
        rx.push_back(buf.to_vec());
        *self.rx_frames.lock().unwrap() += 1;
    }

    /// Wake the poll thread (e.g. after the guest posts fresh RX buffers). Lock-free.
    pub fn kick(&self) {
        if self.kick_handle != 0 {
            unsafe { aemu_slirp_kick(self.kick_handle as *mut c_void) };
        }
    }

    /// Run the slirp poll pump until `done` is set. `on_rx_ready` is invoked after each pump tick so
    /// the caller can flush any freshly-arrived RX frames into the guest receiveq + raise the IRQ.
    /// Blocks — call from a dedicated thread.
    pub fn run_poll_thread<F: FnMut()>(self: &Arc<Self>, done: Arc<AtomicBool>, mut on_rx_ready: F) {
        while !done.load(Ordering::Relaxed) {
            // Fill (locked) → capture fds ptr + count + timeout.
            let (fds_ptr, nfds, timeout_ms) = {
                let g = self.inner.lock().unwrap();
                if g.0.is_null() {
                    break;
                }
                let mut t: c_uint = 0;
                unsafe { aemu_slirp_fill(g.0, &mut t) };
                let fds = unsafe { aemu_slirp_fds(g.0) };
                let n = unsafe { aemu_slirp_nfds(g.0) };
                (fds, n, t)
            };

            // Clamp the timeout so the loop notices `done` and never sleeps forever.
            let timeout: c_int = if timeout_ms == u32::MAX || timeout_ms > 250 {
                250
            } else {
                timeout_ms as c_int
            };

            // poll() UNLOCKED so a TX frame / kick can wake us immediately.
            let ret = unsafe { poll(fds_ptr, nfds as c_uint, timeout) };

            // Report results (locked). This may synchronously enqueue RX via on_recv.
            {
                let g = self.inner.lock().unwrap();
                if g.0.is_null() {
                    break;
                }
                unsafe { aemu_slirp_poll_result(g.0, if ret < 0 { 1 } else { 0}) };
            }

            // Deliver whatever landed in the RX buffer.
            on_rx_ready();
        }
    }
}

impl Drop for SlirpNet {
    fn drop(&mut self) {
        let g = self.inner.lock().unwrap();
        if !g.0.is_null() {
            unsafe { aemu_slirp_cleanup(g.0) };
        }
    }
}

/// C callback (from slirp_shim.c): deliver a host->guest ethernet frame. `opaque` is the
/// `Arc<SlirpNet>` inner pointer passed to `aemu_slirp_new`. Called with the slirp lock held.
///
/// SAFETY: `opaque` points at the live `SlirpNet` (the Arc outlives slirp). We borrow it without
/// touching the refcount (no Arc reconstruction), so we never risk a double-free. `buf` must be
/// readable for `len` bytes — guaranteed by the shim, which passes slirp's own frame buffer.
///
/// Declared `unsafe` because it dereferences both raw pointers; the C ABI and symbol are unchanged,
/// so `slirp_shim.c` needs no edit. (This only became visible once `net` stopped being an optional
/// feature — clippy's deny-by-default `not_unsafe_ptr_arg_deref` had never run over this file.)
#[no_mangle]
pub unsafe extern "C" fn aemu_slirp_recv_frame(opaque: *mut c_void, buf: *const u8, len: usize) {
    if opaque.is_null() || buf.is_null() || len == 0 {
        return;
    }
    let net = unsafe { &*(opaque as *const SlirpNet) };
    let slice = unsafe { std::slice::from_raw_parts(buf, len) };
    net.on_recv(slice);
}

/// Parse `AEMU_HOSTFWD` (e.g. `tcp:8080:80,udp:5300:53`) into host-forward specs. Silently skips
/// malformed entries (logging a warning). Always returns at least the default adb-style tcp 5555.
pub fn parse_hostfwds(spec: Option<&str>) -> Vec<HostFwd> {
    let mut out = vec![HostFwd {
        is_udp: false,
        host_port: 5555,
        guest_port: 5555,
    }];
    let Some(spec) = spec else { return out };
    for entry in spec.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = entry.split(':').collect();
        let (is_udp, hp, gp) = match parts.as_slice() {
            [proto, h, g] => (
                match proto.to_ascii_lowercase().as_str() {
                    "tcp" => false,
                    "udp" => true,
                    _ => {
                        eprintln!("[slirp][warn] hostfwd '{entry}': proto must be tcp|udp");
                        continue;
                    }
                },
                h.parse::<u16>(),
                g.parse::<u16>(),
            ),
            _ => {
                eprintln!("[slirp][warn] hostfwd '{entry}': expected proto:hostport:guestport");
                continue;
            }
        };
        match (hp, gp) {
            (Ok(hp), Ok(gp)) => out.push(HostFwd {
                is_udp,
                host_port: hp,
                guest_port: gp,
            }),
            _ => eprintln!("[slirp][warn] hostfwd '{entry}': bad port number"),
        }
    }
    out
}
