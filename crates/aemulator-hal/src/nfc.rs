//! NFC: an NCI responder for the Cuttlefish NFC HAL on `/dev/hvc12`.
//!
//! ## What the guest does
//!
//! The image starts
//! `nfc_hal_service /apex/com.google.cf.nfc/bin/hw/android.hardware.nfc-service.cuttlefish
//! --virtio-dev-path /dev/hvc12` and the HAL opens that port in raw termios mode. With only 12
//! console ports the node did not exist, so the HAL died on open with
//! `Failed to open virtio-console device: Os { code: 19, … "No such device" }`
//! (`device/google/cuttlefish/guest/hals/nfc/src/nfc.rs:154`) and init respawned it every ~5 s
//! forever. On real Cuttlefish the other end is `casimir`, whose packet model is
//! `casimir::packets::nci` — `ControlPacket` (with `CorePacket`/`RfPacket`/`NfceePacket`/
//! `AndroidPacket` variants) plus `DataPacket`, i.e. the standard NCI control/data split.
//!
//! ## Framing
//!
//! [`Framing::Bare`]: NCI is self-delimiting, so this module finds its own packet boundaries. Every
//! NCI packet is a 3-byte header plus a 1-byte payload length:
//!
//! ```text
//! byte 0   bits 7..5 MT (message type), bit 4 PBF (packet boundary flag), bits 3..0 GID
//! byte 1   bits 5..0 OID          (bits 7..6 reserved)
//! byte 2   payload length (0..255)
//! byte 3.. payload
//! ```
//!
//! ## Scope: a controller with no tag in the field is a complete implementation
//!
//! This is not a stub standing in for something richer. A real phone sitting on a desk has no NFC tag
//! present, and that is exactly what we model: the NFCC resets, initialises, accepts configuration and
//! discovery, and simply never reports a discovered tag. Everything the framework needs in order to
//! bring `INfc/default` up and keep it up is answered; nothing that would require a physical card is
//! fabricated. Tag emulation, were it ever wanted, is an additive change — `RF_INTF_ACTIVATED_NTF` on
//! demand — not a rewrite.

use crate::framing::Framing;
use crate::port::HalPort;

/// NCI header length: MT/PBF/GID, OID, payload-length.
const NCI_HEADER_LEN: usize = 3;

// Message types, in bits 7..5 of byte 0.
const MT_DATA: u8 = 0x00;
const MT_COMMAND: u8 = 0x20;
const MT_RESPONSE: u8 = 0x40;
const MT_NOTIFICATION: u8 = 0x60;
const MT_MASK: u8 = 0xe0;

/// Packet Boundary Flag — set when another segment of the same message follows.
const PBF: u8 = 0x10;
const GID_MASK: u8 = 0x0f;
const OID_MASK: u8 = 0x3f;

// Group identifiers.
const GID_CORE: u8 = 0x00;
const GID_RF: u8 = 0x01;

// CORE opcodes.
const OID_CORE_RESET: u8 = 0x00;
const OID_CORE_INIT: u8 = 0x01;
const OID_CORE_SET_CONFIG: u8 = 0x02;
const OID_CORE_GET_CONFIG: u8 = 0x03;
const OID_CORE_CONN_CREATE: u8 = 0x04;
const OID_CORE_CONN_CLOSE: u8 = 0x05;

// RF opcodes.
const OID_RF_DISCOVER_MAP: u8 = 0x00;
const OID_RF_DISCOVER: u8 = 0x03;
const OID_RF_DEACTIVATE: u8 = 0x06;

/// `STATUS_OK`.
const STATUS_OK: u8 = 0x00;

/// NCI version we report: 2.0. The guest HAL is `android.hardware.nfc` v2 and the framework's NCI
/// layer keys several behaviours off this, so reporting 1.0 would take a different code path.
const NCI_VERSION_2_0: u8 = 0x20;

/// Reset was NOT triggered by the host (i.e. it was commanded). Byte 1 of `CORE_RESET_NTF`.
const RESET_TRIGGER_COMMANDED: u8 = 0x00;
/// Configuration was reset to defaults (as opposed to kept). Byte 2 of `CORE_RESET_RSP`.
const CONFIG_STATUS_RESET: u8 = 0x00;

/// Largest NCI payload, bounded by the single length byte.
const MAX_PAYLOAD: usize = 255;

/// An NCI controller (NFCC) with no tag in its field.
pub struct NciPort {
    /// Bytes received but not yet forming a whole packet.
    buf: Vec<u8>,
    /// Accumulated payload of a segmented (PBF-set) message, and its header.
    segment: Vec<u8>,
    segment_hdr: Option<(u8, u8)>,
    /// Count of packets handled — surfaced for tests and diagnostics.
    packets: u64,
}

impl Default for NciPort {
    fn default() -> Self {
        Self::new()
    }
}

impl NciPort {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            segment: Vec::new(),
            segment_hdr: None,
            packets: 0,
        }
    }

    /// Number of complete NCI packets handled so far.
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Build a packet with the given message type, GID, OID and payload. The payload is truncated to
    /// [`MAX_PAYLOAD`] because the length field is a single byte — a longer one could not be described.
    fn packet(mt: u8, gid: u8, oid: u8, payload: &[u8]) -> Vec<u8> {
        let n = payload.len().min(MAX_PAYLOAD);
        let mut v = Vec::with_capacity(NCI_HEADER_LEN + n);
        v.push((mt & MT_MASK) | (gid & GID_MASK));
        v.push(oid & OID_MASK);
        v.push(n as u8);
        v.extend_from_slice(&payload[..n]);
        v
    }

    /// Handle one fully-reassembled command and produce its replies.
    fn handle(&mut self, gid: u8, oid: u8, payload: &[u8]) -> Vec<Vec<u8>> {
        self.packets += 1;
        match (gid, oid) {
            (GID_CORE, OID_CORE_RESET) => {
                // Reset config only when the command asks for it (payload byte 0: 0=keep, 1=reset).
                let reset_config = payload.first().copied().unwrap_or(0) == 1;
                // NCI 2.0 splits reset across RSP + NTF: the RSP is a bare status, and the version and
                // capability detail arrive in CORE_RESET_NTF. (NCI 1.0 put them in the RSP; reporting
                // 2.0 above is what selects this shape.)
                let rsp = Self::packet(MT_RESPONSE, GID_CORE, OID_CORE_RESET, &[STATUS_OK]);
                let ntf = Self::packet(
                    MT_NOTIFICATION,
                    GID_CORE,
                    OID_CORE_RESET,
                    &[
                        RESET_TRIGGER_COMMANDED,
                        if reset_config { CONFIG_STATUS_RESET } else { 1 },
                        NCI_VERSION_2_0,
                        0x01, // manufacturer ID (arbitrary, non-zero)
                        0x04, // length of the manufacturer-specific info that follows
                        0x00,
                        0x00,
                        0x00,
                        0x00,
                    ],
                );
                vec![rsp, ntf]
            }
            (GID_CORE, OID_CORE_INIT) => {
                // CORE_INIT_RSP advertises what the NFCC can do. The zero RF-interface count is the
                // honest answer for a controller with nothing in its field.
                let payload = [
                    STATUS_OK, 0x00, 0x00, 0x00,
                    0x00, // NFCC features (4 bytes, little-endian bitfield): none
                    0x00, // number of supported RF interfaces
                    0x01, // max logical connections
                    0x00, 0x00, // max routing-table size (2 bytes)
                    0xff, // max control-packet payload size
                    0x00, 0x00, // max size for large parameters (2 bytes)
                    0x00, // manufacturer ID
                    0x00, 0x00, 0x00, 0x00, // manufacturer-specific info
                ];
                vec![Self::packet(MT_RESPONSE, GID_CORE, OID_CORE_INIT, &payload)]
            }
            (GID_CORE, OID_CORE_CONN_CREATE) => {
                // status, initial credits, destination connection id.
                vec![Self::packet(
                    MT_RESPONSE,
                    GID_CORE,
                    OID_CORE_CONN_CREATE,
                    &[STATUS_OK, 0xff, 0x01],
                )]
            }
            (GID_CORE, OID_CORE_GET_CONFIG) => {
                // status + "zero parameters returned". Answering with an empty TLV list is valid and
                // avoids inventing values for parameters we were never configured with.
                vec![Self::packet(
                    MT_RESPONSE,
                    GID_CORE,
                    OID_CORE_GET_CONFIG,
                    &[STATUS_OK, 0x00],
                )]
            }
            (GID_CORE, OID_CORE_SET_CONFIG) => {
                // status + "zero parameters rejected".
                vec![Self::packet(
                    MT_RESPONSE,
                    GID_CORE,
                    OID_CORE_SET_CONFIG,
                    &[STATUS_OK, 0x00],
                )]
            }
            (GID_RF, OID_RF_DISCOVER) => {
                // Discovery starts successfully and then finds nothing — no RF_INTF_ACTIVATED_NTF and
                // no RF_DISCOVER_NTF ever follow. That is a desk with no card on it.
                vec![Self::packet(
                    MT_RESPONSE,
                    GID_RF,
                    OID_RF_DISCOVER,
                    &[STATUS_OK],
                )]
            }
            (GID_RF, OID_RF_DEACTIVATE) => {
                // RSP, then the NTF the framework waits for: type=idle, reason=DH_Request.
                vec![
                    Self::packet(MT_RESPONSE, GID_RF, OID_RF_DEACTIVATE, &[STATUS_OK]),
                    Self::packet(MT_NOTIFICATION, GID_RF, OID_RF_DEACTIVATE, &[0x00, 0x00]),
                ]
            }
            // Everything else — CORE_CONN_CLOSE, RF_DISCOVER_MAP, RF_SET_LISTEN_MODE_ROUTING, the
            // Android-proprietary GID, NFCEE management — gets a same-GID/OID success response. The
            // framework only needs these to not fail; fabricating richer answers would be inventing
            // hardware behaviour we have no basis for.
            (g, o) => {
                let _ = (OID_CORE_CONN_CLOSE, OID_RF_DISCOVER_MAP); // documented above; kept named
                vec![Self::packet(MT_RESPONSE, g, o, &[STATUS_OK])]
            }
        }
    }
}

impl HalPort for NciPort {
    fn label(&self) -> &'static str {
        "nfc(hvc12)"
    }

    fn framing(&self) -> Framing {
        Framing::Bare
    }

    fn on_message(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(msg);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < NCI_HEADER_LEN {
                break;
            }
            // Bounds already checked; use get() anyway so a future refactor can't make this panic.
            let (Some(&b0), Some(&b1), Some(&plen)) =
                (self.buf.first(), self.buf.get(1), self.buf.get(2))
            else {
                break;
            };
            let plen = plen as usize;
            if self.buf.len() < NCI_HEADER_LEN + plen {
                break; // wait for the rest of this packet
            }
            let payload: Vec<u8> = self.buf[NCI_HEADER_LEN..NCI_HEADER_LEN + plen].to_vec();
            self.buf.drain(0..NCI_HEADER_LEN + plen);

            let mt = b0 & MT_MASK;
            let gid = b0 & GID_MASK;
            let oid = b1 & OID_MASK;

            // Data packets carry no GID/OID and address a logical connection we never activate, so
            // there is nothing meaningful to answer. Consume and ignore.
            if mt == MT_DATA {
                self.packets += 1;
                continue;
            }
            // Only commands are answered. A guest sending us a RSP/NTF would be malformed; drop it
            // rather than reflecting something back and desynchronising the stream.
            if mt != MT_COMMAND {
                self.packets += 1;
                continue;
            }

            // Reassemble segmented messages (PBF set means "more to come").
            if b0 & PBF != 0 {
                if self.segment_hdr.is_none() {
                    self.segment_hdr = Some((gid, oid));
                }
                // Cap the reassembly buffer: a guest that never clears PBF must not grow it forever.
                if self.segment.len() + payload.len() <= MAX_PAYLOAD * 16 {
                    self.segment.extend_from_slice(&payload);
                }
                continue;
            }
            let (gid, oid, full) = match self.segment_hdr.take() {
                Some((g, o)) => {
                    let mut full = std::mem::take(&mut self.segment);
                    full.extend_from_slice(&payload);
                    (g, o, full)
                }
                None => (gid, oid, payload),
            };
            out.extend(self.handle(gid, oid, &full));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(gid: u8, oid: u8, payload: &[u8]) -> Vec<u8> {
        NciPort::packet(MT_COMMAND, gid, oid, payload)
    }

    /// Decode a reply's header so assertions read clearly.
    fn hdr(pkt: &[u8]) -> (u8, u8, u8, usize) {
        (
            pkt[0] & MT_MASK,
            pkt[0] & GID_MASK,
            pkt[1] & OID_MASK,
            pkt[2] as usize,
        )
    }

    #[test]
    fn core_reset_answers_rsp_then_ntf_with_nci_2_0() {
        let mut p = NciPort::new();
        let out = p.on_message(&cmd(GID_CORE, OID_CORE_RESET, &[0x01]));
        assert_eq!(out.len(), 2, "NCI 2.0 reset is RSP + NTF");

        let (mt, gid, oid, len) = hdr(&out[0]);
        assert_eq!((mt, gid, oid), (MT_RESPONSE, GID_CORE, OID_CORE_RESET));
        assert_eq!(len, 1);
        assert_eq!(out[0][3], STATUS_OK);

        let (mt, gid, oid, _) = hdr(&out[1]);
        assert_eq!((mt, gid, oid), (MT_NOTIFICATION, GID_CORE, OID_CORE_RESET));
        assert_eq!(
            out[1][5], NCI_VERSION_2_0,
            "NTF must report the NCI version the HAL keys off"
        );
        // reset_config=1 was requested, so the NTF must say config was reset.
        assert_eq!(out[1][4], CONFIG_STATUS_RESET);
    }

    #[test]
    fn core_init_reports_no_rf_interfaces_and_one_connection() {
        let mut p = NciPort::new();
        let _ = p.on_message(&cmd(GID_CORE, OID_CORE_RESET, &[0x01]));
        let out = p.on_message(&cmd(GID_CORE, OID_CORE_INIT, &[]));
        assert_eq!(out.len(), 1);
        let (mt, gid, oid, len) = hdr(&out[0]);
        assert_eq!((mt, gid, oid), (MT_RESPONSE, GID_CORE, OID_CORE_INIT));
        assert_eq!(out[0][3], STATUS_OK);
        assert!(len >= 10, "CORE_INIT_RSP is a long capability block");
        assert_eq!(
            out[0][8], 0x00,
            "no supported RF interfaces (no tag present)"
        );
        assert_eq!(out[0][9], 0x01, "exactly one max logical connection");
    }

    #[test]
    fn rf_discover_succeeds_but_never_activates_an_interface() {
        let mut p = NciPort::new();
        let out = p.on_message(&cmd(GID_RF, OID_RF_DISCOVER, &[0x01, 0x00, 0x01]));
        assert_eq!(out.len(), 1, "discovery must NOT emit an activation NTF");
        assert_eq!(out[0][3], STATUS_OK);
    }

    #[test]
    fn unmodelled_command_gets_a_matching_success_response() {
        let mut p = NciPort::new();
        // RF_SET_LISTEN_MODE_ROUTING (GID 1, OID 0x21) is not modelled explicitly.
        let out = p.on_message(&cmd(GID_RF, 0x21, &[0x00]));
        assert_eq!(out.len(), 1);
        let (mt, gid, oid, _) = hdr(&out[0]);
        assert_eq!(
            (mt, gid, oid),
            (MT_RESPONSE, GID_RF, 0x21),
            "the fallback must echo the same GID/OID or the HAL cannot match it to its request"
        );
        assert_eq!(out[0][3], STATUS_OK);
    }

    #[test]
    fn reassembles_a_packet_split_across_byte_at_a_time_writes() {
        let mut p = NciPort::new();
        let wire = cmd(GID_CORE, OID_CORE_RESET, &[0x01]);
        for (i, b) in wire.iter().enumerate() {
            let got = p.on_message(&[*b]);
            if i + 1 == wire.len() {
                assert_eq!(got.len(), 2, "replies only once the packet is whole");
            } else {
                assert!(got.is_empty(), "replied early at byte {i}");
            }
        }
    }

    #[test]
    fn segmented_message_is_reassembled_before_dispatch() {
        let mut p = NciPort::new();
        // First segment with PBF set, then the final segment.
        let mut first = NciPort::packet(MT_COMMAND, GID_CORE, OID_CORE_SET_CONFIG, &[0xaa]);
        first[0] |= PBF;
        assert!(
            p.on_message(&first).is_empty(),
            "PBF segment must not reply"
        );
        let out = p.on_message(&NciPort::packet(
            MT_COMMAND,
            GID_CORE,
            OID_CORE_SET_CONFIG,
            &[0xbb],
        ));
        assert_eq!(out.len(), 1, "the completed message gets exactly one reply");
        let (_, gid, oid, _) = hdr(&out[0]);
        assert_eq!((gid, oid), (GID_CORE, OID_CORE_SET_CONFIG));
    }

    #[test]
    fn data_packets_and_stray_responses_are_consumed_without_reply() {
        let mut p = NciPort::new();
        assert!(p
            .on_message(&NciPort::packet(MT_DATA, 0, 0, &[1, 2, 3]))
            .is_empty());
        assert!(p
            .on_message(&NciPort::packet(MT_RESPONSE, GID_CORE, OID_CORE_INIT, &[0]))
            .is_empty());
        assert_eq!(p.packets(), 2, "both were consumed, not left buffered");
    }

    #[test]
    fn fuzz_lite_never_panics() {
        let mut x: u32 = 0x9e37_79b9;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut p = NciPort::new();
        for _ in 0..200_000 {
            let b = (next() & 0xff) as u8;
            let _ = p.on_message(&[b]);
        }
        // Truncated packets must never be dispatched, only buffered.
        let mut q = NciPort::new();
        for len in 0..3usize {
            let junk = vec![0x20u8; len];
            assert!(q.on_message(&junk).is_empty());
        }
        // A packet claiming 255 bytes but delivering none must stay buffered, not panic.
        let mut r = NciPort::new();
        assert!(r.on_message(&[0x20, 0x00, 0xff]).is_empty());
    }
}
