//! virtio-console (device type 3): a bidirectional byte pipe. Two virtqueues in the base
//! (no-multiport) configuration:
//!   * queue 0 = **receiveq** (port RX): the guest posts write-only buffers; the device fills them.
//!   * queue 1 = **transmitq** (port TX): the guest posts read-only buffers; the device drains them.
//!
//! ## Multiple HVC channels (Cuttlefish)
//! The Cuttlefish android14 guest expects **twelve** independent consoles — `hvc0..hvc11` — one per
//! HAL/log channel (hvc0 console, hvc2 logcat, hvc3 keymaster, hvc4 gatekeeper, hvc10 oemlock,
//! hvc11 keymint, …). Each is a separate single-port virtio-console device on the PCI bus, and the
//! guest's `virtio_console` driver numbers them `hvc0..hvcN` in probe (slot) order. We therefore
//! build twelve of these, each carrying a [`ConsoleRole`] that decides what its bytes mean:
//!   * [`ConsoleRole::Terminal`] — hvc0: TX→host stdout, RX←host stdin (interactive/log).
//!   * [`ConsoleRole::Log`] — a passive sink that copies guest TX to host stdout with a prefix
//!     (e.g. logcat on hvc2), never sends RX.
//!   * [`ConsoleRole::Sink`] — silently drains guest TX (channels we don't surface).
//!   * [`ConsoleRole::Secure`] — a HAL channel bridged to the host [`crate::secure_env::SecureEnv`]:
//!     guest TX is a length-prefixed request stream; each complete frame is answered and the response
//!     is pushed back through RX. This is what lets the KeyMint HAL (hvc11) talk to a host TA and
//!     stops it aborting on `open("/dev/hvc11")`.
//!
//! ## RX/TX in one notify
//! For a Secure channel the response must reach the guest's *already-posted* RX buffers during the
//! same interrupt cycle as the TX request. [`Console::on_notify`] is therefore handed **both** queues
//! (`&mut [Queue]`) by the transport, so a TX drain can immediately fill RX. Returning `true` makes
//! the transport raise the device's single INTx/ISR line; the guest driver then services both rings.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_CONSOLE};
use crate::secure_env::{SecChannel, SecureEnv};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

pub const RECEIVEQ: u16 = 0;
pub const TRANSMITQ: u16 = 1;

/// What a console channel's bytes mean.
pub enum ConsoleRole {
    /// Interactive terminal: TX→stdout, RX←host stdin. The Linux `run-linux` console + Android hvc0.
    Terminal,
    /// Passive log tap: guest TX is copied to host stdout with `prefix`; no RX is ever generated.
    Log { prefix: String },
    /// Silent drain: guest TX is discarded; no RX.
    Sink,
    /// HAL channel bridged to the host secure_env.
    Secure {
        channel: SecChannel,
        env: Arc<Mutex<SecureEnv>>,
    },
}

pub struct Console {
    role: ConsoleRole,
    /// Bytes awaiting delivery into the guest's receiveq (host-typed input, or secure_env responses).
    input: VecDeque<u8>,
    /// Reassembly buffer for a Secure channel's length-prefixed request stream.
    rx_frame: Vec<u8>,
    /// Whether we have already hex-dumped the first bytes seen on this secure channel (once).
    dumped_raw: bool,
}

impl Console {
    /// The default interactive console (Linux `run-linux`, Android hvc0). TX→stdout, RX←stdin.
    pub fn new() -> Self {
        Self::with_role(ConsoleRole::Terminal)
    }

    pub fn with_role(role: ConsoleRole) -> Self {
        Self {
            role,
            input: VecDeque::new(),
            rx_frame: Vec::new(),
            dumped_raw: false,
        }
    }

    /// A HAL channel bound to the shared host secure_env.
    pub fn secure(channel: SecChannel, env: Arc<Mutex<SecureEnv>>) -> Self {
        Self::with_role(ConsoleRole::Secure { channel, env })
    }

    /// A passive stdout log tap (e.g. logcat on hvc2).
    pub fn log(prefix: impl Into<String>) -> Self {
        Self::with_role(ConsoleRole::Log {
            prefix: prefix.into(),
        })
    }

    /// A silent drain (channels we don't surface but must exist so the guest HAL can open them).
    pub fn sink() -> Self {
        Self::with_role(ConsoleRole::Sink)
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_CONSOLE
    }

    pub fn features(&self) -> u64 {
        // No F_SIZE / F_MULTIPORT: base console, exactly two queues.
        VIRTIO_F_VERSION_1
    }

    pub fn num_queues(&self) -> usize {
        2
    }

    pub fn config_read(&self, _off: u64, _len: usize) -> u64 {
        0
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    /// Legacy single-queue entry (used by the [`super::mmio::Backend::process_queue`] enum arm for
    /// completeness). The transports now call [`Console::on_notify`] instead, which sees both queues.
    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match idx {
            TRANSMITQ => self.drain_tx_terminal(mem, queue),
            RECEIVEQ => self.fill_rx(mem, queue),
            _ => false,
        }
    }

    /// Unified notify handler with access to **both** queues. Called by the PCI/MMIO transports on a
    /// queue notification. Returns whether any buffer was completed (→ raise the device interrupt).
    pub fn on_notify(&mut self, idx: u16, mem: &Mem, queues: &mut [Queue]) -> bool {
        match &self.role {
            ConsoleRole::Terminal => {
                // Same behaviour as before: TX→stdout on transmitq, RX←buffered input on receiveq.
                match idx {
                    TRANSMITQ => {
                        if let Some(q) = queues.get_mut(TRANSMITQ as usize) {
                            self.drain_tx_terminal(mem, q)
                        } else {
                            false
                        }
                    }
                    RECEIVEQ => {
                        if let Some(q) = queues.get_mut(RECEIVEQ as usize) {
                            self.fill_rx(mem, q)
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            }
            ConsoleRole::Log { .. } | ConsoleRole::Sink => {
                // Drain the transmitq (to stdout or the void). Receiveq notifies just reclaim nothing.
                if idx == TRANSMITQ {
                    if let Some(q) = queues.get_mut(TRANSMITQ as usize) {
                        return self.drain_tx_log(mem, q);
                    }
                }
                false
            }
            ConsoleRole::Secure { .. } => self.on_notify_secure(idx, mem, queues),
        }
    }

    /// Secure-channel notify: on a transmitq notify, drain the request bytes, reassemble complete
    /// `[u32 command][u32 payload_size][payload]` frames, run each through secure_env, buffer the
    /// framed responses, and immediately fill the guest's posted receiveq. On a receiveq notify, just
    /// flush any already-buffered response bytes into newly posted RX buffers.
    fn on_notify_secure(&mut self, idx: u16, mem: &Mem, queues: &mut [Queue]) -> bool {
        let mut produced = false;
        if idx == TRANSMITQ {
            // 1) Drain TX descriptors into the reassembly buffer.
            if let Some(txq) = queues.get_mut(TRANSMITQ as usize) {
                while let Some(chain) = txq.pop_descriptor_chain(mem) {
                    let head = chain.head_index();
                    for desc in chain {
                        if !desc.is_write_only() {
                            let mut buf = vec![0u8; desc.len() as usize];
                            if mem
                                .read_slice(&mut buf, GuestAddress(desc.addr().0))
                                .is_ok()
                            {
                                self.rx_frame.extend_from_slice(&buf);
                            }
                        }
                    }
                    let _ = txq.add_used(mem, head, 0);
                    produced = true;
                }
            }
            // 2) Parse + dispatch every complete frame; buffer responses into `input`.
            self.dispatch_secure_frames();
        }
        // 3) Flush buffered responses into the receiveq (in both the TX and the RX notify path).
        if let Some(rxq) = queues.get_mut(RECEIVEQ as usize) {
            if self.fill_rx(mem, rxq) {
                produced = true;
            }
        }
        produced
    }

    /// Parse as many length-prefixed frames as are fully present in `rx_frame`, hand each to
    /// secure_env, and append framed responses to `input`.
    ///
    /// Framing (Cuttlefish `common/libs/transport` / rust `kmr` HAL, confirmed from the wire): a
    /// 4-byte **big-endian** payload length followed by that many payload bytes. The first bytes seen
    /// on each channel are hex-dumped once so the exact wire format is captured in the boot log.
    fn dispatch_secure_frames(&mut self) {
        let (channel, env) = match &self.role {
            ConsoleRole::Secure { channel, env } => (*channel, env.clone()),
            _ => return,
        };
        if !self.dumped_raw && !self.rx_frame.is_empty() {
            self.dumped_raw = true;
            let n = self.rx_frame.len().min(64);
            let hex: String = self.rx_frame[..n]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            eprintln!(
                "[secure_env] {} RAW first {n}B (total {}): {hex}",
                channel.label(),
                self.rx_frame.len()
            );
        }
        loop {
            if self.rx_frame.len() < 4 {
                break;
            }
            let plen = u32::from_be_bytes(self.rx_frame[0..4].try_into().unwrap()) as usize;
            // Guard against a corrupt/huge length so we never allocate wildly or spin.
            if plen > 64 * 1024 {
                eprintln!(
                    "[secure_env] {} frame length {} implausible — resyncing (dropping stream)",
                    channel.label(),
                    plen
                );
                self.rx_frame.clear();
                break;
            }
            if self.rx_frame.len() < 4 + plen {
                break; // wait for the rest of the frame
            }
            let payload: Vec<u8> = self.rx_frame[4..4 + plen].to_vec();
            self.rx_frame.drain(0..4 + plen);
            let reply = env.lock().unwrap().handle(channel, 0, &payload);
            if let Some((_cmd, resp_payload)) = reply {
                // Response uses the same 4-byte big-endian length-prefix framing.
                let mut frame = Vec::with_capacity(4 + resp_payload.len());
                frame.extend_from_slice(&(resp_payload.len() as u32).to_be_bytes());
                frame.extend_from_slice(&resp_payload);
                self.input.extend(frame);
            }
        }
    }

    /// Read-only TX buffers → host stdout (terminal role).
    fn drain_tx_terminal(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            for desc in chain {
                if !desc.is_write_only() {
                    let mut buf = vec![0u8; desc.len() as usize];
                    if mem
                        .read_slice(&mut buf, GuestAddress(desc.addr().0))
                        .is_ok()
                    {
                        let _ = out.write_all(&buf);
                    }
                }
            }
            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        let _ = out.flush();
        used
    }

    /// TX drain for Log/Sink roles: Log copies to stdout with its prefix (line-oriented); Sink drops.
    fn drain_tx_log(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        let mut collected: Vec<u8> = Vec::new();
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            for desc in chain {
                if !desc.is_write_only() {
                    let mut buf = vec![0u8; desc.len() as usize];
                    if mem
                        .read_slice(&mut buf, GuestAddress(desc.addr().0))
                        .is_ok()
                    {
                        collected.extend_from_slice(&buf);
                    }
                }
            }
            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        if let ConsoleRole::Log { prefix } = &self.role {
            if !collected.is_empty() {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                for line in collected.split(|&b| b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    let _ = write!(out, "{prefix}");
                    let _ = out.write_all(line);
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
        }
        used
    }

    /// Fill posted receive buffers from `input`. Returns whether any buffer was completed.
    fn fill_rx(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while !self.input.is_empty() {
            let Some(chain) = queue.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            let mut written = 0u32;
            for desc in chain {
                if desc.is_write_only() && !self.input.is_empty() {
                    let cap = desc.len() as usize;
                    let mut buf = Vec::with_capacity(cap);
                    while buf.len() < cap {
                        match self.input.pop_front() {
                            Some(b) => buf.push(b),
                            None => break,
                        }
                    }
                    if mem.write_slice(&buf, GuestAddress(desc.addr().0)).is_ok() {
                        written += buf.len() as u32;
                    }
                }
            }
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    /// Push host-typed bytes and flush them into the guest's receiveq (Terminal role only). Returns
    /// whether any receive buffer was completed. Called from the stdin reader thread.
    pub fn push_input(&mut self, bytes: &[u8], mem: &Mem, queues: &mut [Queue]) -> bool {
        if !matches!(self.role, ConsoleRole::Terminal) {
            return false;
        }
        self.input.extend(bytes.iter().copied());
        if let Some(rxq) = queues.get_mut(RECEIVEQ as usize) {
            self.fill_rx(mem, rxq)
        } else {
            false
        }
    }

    /// True if this console accepts host stdin (the interactive terminal). Used by the transports so
    /// only hvc0 (not the HAL channels) is fed keystrokes.
    pub fn is_terminal(&self) -> bool {
        matches!(self.role, ConsoleRole::Terminal)
    }
}
