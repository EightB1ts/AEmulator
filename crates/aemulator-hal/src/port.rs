//! The seam between the VMM's virtio-console plumbing and a guest-HAL protocol handler.
//!
//! A [`HalPort`] is handed complete, already-de-framed guest messages and returns zero or more
//! replies; the embedding VMM owns descriptor walking, guest memory, framing (via
//! [`crate::Reassembler`]), interrupt raising and threads. That split is what keeps every protocol in
//! this crate testable without a VM and reusable across the HVF and crosvm backends.

use crate::framing::Framing;

/// A host-side protocol handler bound to one guest-HAL channel.
///
/// ## Execution model — this is a hard constraint, not advice
///
/// In the `aemulator-hvf` embedding, [`HalPort::on_message`] is invoked **synchronously on the vCPU
/// thread that took the MMIO trap**, while the virtual PCI bus mutex is held. Therefore an
/// implementation must never block, sleep, perform I/O, or panic: a panic poisons that mutex for the
/// entire PCI bus and kills the VM. Parse defensively — `get()`, `try_into()`, no raw indexing or
/// `unwrap()` on anything derived from guest bytes.
///
/// Unsolicited host→guest traffic (a sensor sample, an HCI event, an NMEA fix) does **not** go
/// through this trait. It is pushed from a VMM-side pump thread, so a `HalPort` only ever handles the
/// request/response direction.
///
/// ## Confirming [`HalPort::framing`] against a live guest
///
/// Framing is a property of the guest binary, so it must be observed rather than assumed. The VMM
/// hex-dumps the first bytes seen on each port at boot. Read that dump: leading printable ASCII means
/// [`Framing::Newline`] (or [`Framing::Bare`]); a small integer followed by a plausible little-endian
/// length means [`Framing::RawMessage`]. Switching a port is a one-value change here.
pub trait HalPort: Send {
    /// Short label for logs, e.g. `"nfc(hvc12)"`. Used to tag the first-bytes dump and per-port
    /// diagnostics, so make it identify both the protocol and the port.
    fn label(&self) -> &'static str;

    /// The envelope this channel's guest peer uses, in both directions. Queried once, at construction.
    fn framing(&self) -> Framing;

    /// Handle one complete guest→host message; return the replies to send, in order.
    ///
    /// Returning an empty vector is normal and means "no reply" (a fire-and-forget command such as
    /// the sensors `set-delay:`). Each returned message is framed independently by the caller.
    fn on_message(&mut self, msg: &[u8]) -> Vec<Vec<u8>>;
}
