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
use aemulator_hal::{HalPort, Reassembler};
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
    /// **Diagnostic** drain for a port whose real backend has not landed yet: behaves as
    /// [`ConsoleRole::Sink`], but hex-dumps the first bytes the guest ever writes, tagged with the
    /// port number and role.
    ///
    /// This is how the wire framing of each HAL channel gets *observed* rather than assumed — the
    /// question that blocks writing any protocol handler. Read the dump: leading printable ASCII
    /// (`list-sensors`, `$GPGGA`) means a text/newline protocol; a small integer followed by a
    /// plausible little-endian length means Cuttlefish's `RawMessage` envelope; an NCI/H4 packet
    /// header means a self-delimiting binary protocol.
    ///
    /// It doubles as the check on the load-bearing assumption that the guest's `hvcN` numbering
    /// follows PCI slot order: NFC's first write must appear tagged `hvc12`, Bluetooth's on `hvc5`.
    /// Every real HAL opens its port and writes immediately, so no synthetic probe traffic is needed
    /// — and injecting any would be actively harmful on the NCI/H4 channels.
    ///
    /// Empirically this is exactly how the framings in use were established:
    /// ```text
    /// hvc18 sensors: 00 00 00 00 0c 00 00 00 6c 69 73 74 2d 73 65 6e 73 6f 72 73
    ///               |command=0    |len=12      |"list-sensors"      -> Framing::RawMessage
    /// hvc5  bt:      01 03 0c 00  -> H4 command, opcode 0x0c03 = HCI_Reset  -> Framing::Bare
    /// hvc12 nfc:     20 00 01 01  -> NCI CORE_RESET_CMD, plen 1             -> Framing::Bare
    /// hvc9  uwb:     20 00 00 01 00 -> UCI CORE_DEVICE_RESET                -> Framing::Bare
    /// ```
    Probe { hvc: usize, label: &'static str },
    /// A real host-side protocol backend: guest bytes are de-framed per the port's
    /// [`aemulator_hal::Framing`], each complete message is handed to the [`HalPort`], and its replies
    /// are framed and pushed back through RX in the same notify cycle.
    ///
    /// One variant covers every protocol (NFC, Bluetooth, sensors, …) because the queue mechanics are
    /// identical and only the state machine differs — the same reason `Gpu` holds a
    /// `Box<dyn Renderer3d>` rather than growing a variant per renderer.
    Hal {
        port: Box<dyn HalPort>,
        reasm: Reassembler,
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

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
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

    /// A drain that hex-dumps the first bytes the guest writes, so the channel's wire framing can be
    /// read off the boot log. See [`ConsoleRole::Probe`].
    pub fn probe(hvc: usize, label: &'static str) -> Self {
        Self::with_role(ConsoleRole::Probe { hvc, label })
    }

    /// A channel served by a real host-side protocol backend. The framing comes from the port itself,
    /// so a port is never wired to the wrong envelope.
    pub fn hal(port: Box<dyn HalPort>) -> Self {
        let reasm = Reassembler::new(port.framing());
        Self::with_role(ConsoleRole::Hal { port, reasm })
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
            ConsoleRole::Log { .. } | ConsoleRole::Sink | ConsoleRole::Probe { .. } => {
                // Drain the transmitq (to stdout or the void). Receiveq notifies just reclaim nothing.
                if idx == TRANSMITQ {
                    if let Some(q) = queues.get_mut(TRANSMITQ as usize) {
                        return self.drain_tx_log(mem, q);
                    }
                }
                false
            }
            ConsoleRole::Secure { .. } => self.on_notify_secure(idx, mem, queues),
            ConsoleRole::Hal { .. } => self.on_notify_hal(idx, mem, queues),
        }
    }

    /// HAL-channel notify: drain the TX ring, de-frame it, run each complete message through the
    /// [`HalPort`], frame the replies, and fill the guest's already-posted RX buffers — all within one
    /// notify, exactly as the secure channel does. A HAL that expects a synchronous answer (NCI, HCI)
    /// would otherwise time out waiting for a second interrupt.
    fn on_notify_hal(&mut self, idx: u16, mem: &Mem, queues: &mut [Queue]) -> bool {
        let mut produced = false;
        if idx == TRANSMITQ {
            if let Some(txq) = queues.get_mut(TRANSMITQ as usize) {
                let (bytes, used) = Self::drain_tx_bytes(mem, txq);
                produced |= used;
                if let ConsoleRole::Hal { port, reasm } = &mut self.role {
                    if !bytes.is_empty() {
                        let tag = port.label();
                        Self::dump_first_bytes(&mut self.dumped_raw, tag, &bytes);
                    }
                    for msg in reasm.feed(&bytes) {
                        for reply in port.on_message(&msg) {
                            self.input.extend(reasm.frame_out(&reply));
                        }
                    }
                    if reasm.desynced() {
                        eprintln!(
                            "[hal] {}: framing desync — stream dropped and resynced",
                            port.label()
                        );
                    }
                }
            }
        }
        if let Some(rxq) = queues.get_mut(RECEIVEQ as usize) {
            produced |= self.fill_rx(mem, rxq);
        }
        produced
    }

    /// Drain every read-only TX descriptor into one byte buffer, returning it plus whether any chain
    /// was completed. Factored out so the HAL path shares the exact descriptor walk the secure and
    /// log paths use.
    fn drain_tx_bytes(mem: &Mem, queue: &mut Queue) -> (Vec<u8>, bool) {
        let mut collected = Vec::new();
        let mut used = false;
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
        (collected, used)
    }

    /// Push unsolicited host bytes into the guest's receiveq, bypassing any request/reply cycle.
    ///
    /// This is the path for data the guest never asked for — a sensor sample, an NMEA fix, an HCI
    /// event — driven from a VMM pump thread rather than a vCPU trap. Bytes must already be framed by
    /// the caller (pump threads own the port's [`Reassembler`] choice), because the framing a push
    /// needs is the same one its port's [`HalPort`] declared.
    pub fn push_host_bytes(&mut self, bytes: &[u8], mem: &Mem, queues: &mut [Queue]) -> bool {
        self.input.extend(bytes.iter().copied());
        queues
            .get_mut(RECEIVEQ as usize)
            .map(|q| self.fill_rx(mem, q))
            .unwrap_or(false)
    }

    /// Hex-dump the first bytes ever seen on a channel, once, alongside a printable-ASCII rendering.
    ///
    /// The ASCII column is the point: it is what distinguishes a text protocol from Cuttlefish's
    /// binary `RawMessage` envelope at a glance, and therefore what settles which `Framing` a port's
    /// handler must use. `tag` identifies the port and role so the dump also proves the hvcN↔PCI-slot
    /// mapping.
    fn dump_first_bytes(dumped: &mut bool, tag: &str, bytes: &[u8]) {
        if *dumped || bytes.is_empty() {
            return;
        }
        *dumped = true;
        let n = bytes.len().min(64);
        let hex: String = bytes[..n]
            .iter()
            .map(|b| format!("{b:02x} "))
            .collect::<String>()
            .trim_end()
            .to_string();
        let ascii: String = bytes[..n]
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        eprintln!("[probe] {tag} first {n}B of {}: {hex}", bytes.len());
        eprintln!("[probe] {tag} ascii: |{ascii}|");
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
        // Dump the first request bytes once. This is the same probe the not-yet-implemented ports get
        // (see [`ConsoleRole::Probe`]) and it matters here too: `aemulator-hal`'s framing analysis of
        // `libcuttlefish_transport.so` says the envelope is an 8-byte little-endian
        // `[u32 command|is_response][u32 payload_size]` header, whereas the loop below parses a 4-byte
        // BIG-endian length with no command word. Only oemlock (hvc10) has a live guest peer in this
        // image — `prep-apex-dedup.sh` selects the software `nonsecure` keymint/gatekeeper APEXes,
        // which never open hvc3/hvc4/hvc11 — so this dump is what tells us whether the mismatch is
        // real or whether the path is simply dead.
        {
            let tag = format!("secure {}", channel.label());
            let frame = std::mem::take(&mut self.rx_frame);
            Self::dump_first_bytes(&mut self.dumped_raw, &tag, &frame);
            self.rx_frame = frame;
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
        if let ConsoleRole::Probe { hvc, label } = &self.role {
            let tag = format!("hvc{hvc} {label}");
            Self::dump_first_bytes(&mut self.dumped_raw, &tag, &collected);
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
