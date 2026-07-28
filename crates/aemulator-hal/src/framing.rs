//! Wire framing for a guest-HAL byte channel.
//!
//! Three shapes cover every Cuttlefish HAL channel we serve, and which one a port uses is a property
//! of the *guest binary on the other end* — never a choice we get to make:
//!
//! * [`Framing::Bare`] — no envelope at all. Bluetooth H4 and NFC NCI both carry their own length
//!   field, so the protocol handler finds its own packet boundaries. Cuttlefish confirms this
//!   indirectly: on a real host the bytes reach `rootcanal`/`casimir` through `tcp_connector`, a dumb
//!   pump whose entire flag surface (`--fifo_in`, `--fifo_out`, `--data_port`) contains no framing
//!   logic whatsoever.
//! * [`Framing::Newline`] — `\n`-delimited text. The goldfish/qemud sensor protocol shape.
//! * [`Framing::RawMessage`] — Cuttlefish's own `common/libs/transport` envelope, used by every HAL
//!   that links `libcuttlefish_transport.so` (keymaster, gatekeeper, oemlock, keymint, and the
//!   sensors sub-HAL). Layout below.
//!
//! ## `RawMessage`
//!
//! `libcuttlefish_transport.so` exports `SharedFdChannel::ReceiveMessage`, which asserts
//! `read == sizeof(RawMessage)` and then `read == message->payload_size`, rebuilding via
//! `CreateMessage(message_header.command, message_header.is_response, message_header.payload_size)`.
//! That is a fixed-size POD header in **host-native (little) endian**:
//!
//! ```text
//! byte  0..4   u32  bits 0..30 = command, bit 31 = is_response
//! byte  4..8   u32  payload_size
//! byte  8..    payload (payload_size bytes)
//! ```
//!
//! `command` being a 31-bit field with `is_response` packed into the top bit is why the header is 8
//! bytes and not 12 — the two fields share one word.
//!
//! **This is not what `aemulator-hvf`'s `console.rs` historically implemented** (a 4-byte
//! *big-endian* payload length with no command word). The only live consumer of that code path is
//! oemlock on hvc10 — `prep-apex-dedup.sh` selects the pure-software `nonsecure` keymint/gatekeeper
//! APEXes, which never open hvc3/hvc4/hvc11 at all — so the divergence has been able to hide. Note
//! that `console.rs`'s own doc comment describes `[u32 command][u32 payload_size][payload]`, i.e. the
//! documentation was right and the implementation drifted. Confirm against a live hexdump before
//! trusting either (see `HalPort` docs), and switch a port by changing one [`Framing`] value.

/// Number of header bytes in a [`Framing::RawMessage`] envelope: two `u32`s.
pub const RAW_MESSAGE_HEADER_LEN: usize = 8;

/// Bit 31 of a `RawMessage`'s first word marks a response rather than a request.
const RAW_MESSAGE_IS_RESPONSE: u32 = 1 << 31;

/// Largest single message we will reassemble. A length beyond this means the stream is corrupt or we
/// are out of sync, so we drop and resync rather than allocate wildly or spin. Matches the guard
/// `aemulator-hvf`'s secure-channel path has always used.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// The envelope a HAL channel's bytes are wrapped in, in both directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// No envelope; the protocol self-delimits (HCI H4, NCI, UCI).
    Bare,
    /// `\n`-terminated text lines. A trailing partial line is buffered until its newline arrives.
    Newline,
    /// Cuttlefish `common/libs/transport` `RawMessage`: `[u32 command|is_response][u32 len][payload]`,
    /// little-endian. See the module docs for the layout and the caveat.
    RawMessage,
}

/// Incremental reassembler for one channel's inbound byte stream.
///
/// Guest writes arrive in arbitrary chunks — a single virtqueue descriptor may hold half a message,
/// or three and a half. `feed` absorbs whatever arrived and returns only the messages that are now
/// complete; `frame_out` applies the same framing to a reply.
///
/// For [`Framing::RawMessage`] the reassembler also remembers the `command` of the most recent
/// request so that `frame_out` can echo it with the response bit set — that pairing is the whole
/// point of the envelope, and keeping it here is what lets [`crate::HalPort::on_message`] stay a
/// plain `&[u8] -> Vec<Vec<u8>>` with no envelope awareness in any protocol module.
pub struct Reassembler {
    kind: Framing,
    buf: Vec<u8>,
    /// `command` of the last request decoded on this channel (RawMessage only).
    last_command: u32,
    /// Set once a corrupt length has forced a resync, so we only log the event a single time.
    desynced: bool,
}

impl Reassembler {
    pub fn new(kind: Framing) -> Self {
        Self {
            kind,
            buf: Vec::new(),
            last_command: 0,
            desynced: false,
        }
    }

    pub fn framing(&self) -> Framing {
        self.kind
    }

    /// `command` of the most recently decoded request. Meaningful for [`Framing::RawMessage`] only;
    /// `0` for the other framings.
    pub fn last_command(&self) -> u32 {
        self.last_command
    }

    /// True once a corrupt length forced this channel to resync.
    pub fn desynced(&self) -> bool {
        self.desynced
    }

    /// Absorb freshly received guest bytes; return every message that is now complete, in order.
    ///
    /// Never panics and never loops on malformed input: an implausible declared length clears the
    /// buffer and sets [`Reassembler::desynced`].
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        match self.kind {
            // Nothing to reassemble — the protocol handler owns its own boundaries. Hand the chunk
            // straight through (skipping empties so a bare receiveq notify doesn't fabricate a
            // zero-length "message").
            Framing::Bare => {
                if bytes.is_empty() {
                    Vec::new()
                } else {
                    vec![bytes.to_vec()]
                }
            }
            Framing::Newline => {
                self.buf.extend_from_slice(bytes);
                let mut out = Vec::new();
                while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
                    line.pop(); // drop the '\n'
                    if line.last() == Some(&b'\r') {
                        line.pop(); // tolerate CRLF
                    }
                    out.push(line);
                }
                // A partial line with no newline in sight and no plausible end is a desync; drop it
                // rather than growing without bound.
                if self.buf.len() > MAX_FRAME_LEN {
                    self.buf.clear();
                    self.desynced = true;
                }
                out
            }
            Framing::RawMessage => {
                self.buf.extend_from_slice(bytes);
                let mut out = Vec::new();
                loop {
                    if self.buf.len() < RAW_MESSAGE_HEADER_LEN {
                        break;
                    }
                    // Header reads are bounds-checked above; `try_into` on a 4-byte slice of an
                    // 8-byte-or-longer buffer cannot fail, but stay total anyway.
                    let Ok(w0) = <[u8; 4]>::try_from(&self.buf[0..4]) else {
                        break;
                    };
                    let Ok(w1) = <[u8; 4]>::try_from(&self.buf[4..8]) else {
                        break;
                    };
                    let word0 = u32::from_le_bytes(w0);
                    let plen = u32::from_le_bytes(w1) as usize;
                    if plen > MAX_FRAME_LEN {
                        self.buf.clear();
                        self.desynced = true;
                        break;
                    }
                    if self.buf.len() < RAW_MESSAGE_HEADER_LEN + plen {
                        break; // wait for the rest
                    }
                    self.last_command = word0 & !RAW_MESSAGE_IS_RESPONSE;
                    let payload: Vec<u8> =
                        self.buf[RAW_MESSAGE_HEADER_LEN..RAW_MESSAGE_HEADER_LEN + plen].to_vec();
                    self.buf.drain(0..RAW_MESSAGE_HEADER_LEN + plen);
                    out.push(payload);
                }
                out
            }
        }
    }

    /// Wrap an outbound message in this channel's framing.
    ///
    /// For [`Framing::RawMessage`] the reply carries [`Reassembler::last_command`] with the response
    /// bit set, matching what the guest's `SharedFdChannel::SendResponse` expects.
    pub fn frame_out(&self, msg: &[u8]) -> Vec<u8> {
        match self.kind {
            Framing::Bare => msg.to_vec(),
            Framing::Newline => {
                let mut v = Vec::with_capacity(msg.len() + 1);
                v.extend_from_slice(msg);
                v.push(b'\n');
                v
            }
            Framing::RawMessage => {
                let mut v = Vec::with_capacity(RAW_MESSAGE_HEADER_LEN + msg.len());
                v.extend_from_slice(&(self.last_command | RAW_MESSAGE_IS_RESPONSE).to_le_bytes());
                v.extend_from_slice(&(msg.len() as u32).to_le_bytes());
                v.extend_from_slice(msg);
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a RawMessage request the way the guest's `SendRequest` would.
    fn raw_request(command: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&command.to_le_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn bare_passes_through_and_skips_empty() {
        let mut r = Reassembler::new(Framing::Bare);
        assert_eq!(r.feed(&[]), Vec::<Vec<u8>>::new());
        assert_eq!(r.feed(&[1, 2, 3]), vec![vec![1, 2, 3]]);
        assert_eq!(r.frame_out(&[9, 9]), vec![9, 9]);
    }

    #[test]
    fn newline_splits_and_buffers_partial_lines() {
        let mut r = Reassembler::new(Framing::Newline);
        assert!(r.feed(b"list-sen").is_empty(), "partial line must buffer");
        let msgs = r.feed(b"sors\nset:light:1\n");
        assert_eq!(
            msgs,
            vec![b"list-sensors".to_vec(), b"set:light:1".to_vec()]
        );
        // CRLF tolerated.
        assert_eq!(r.feed(b"time:5\r\n"), vec![b"time:5".to_vec()]);
        assert_eq!(r.frame_out(b"42"), b"42\n".to_vec());
    }

    #[test]
    fn newline_resyncs_on_unbounded_garbage() {
        let mut r = Reassembler::new(Framing::Newline);
        let junk = vec![b'x'; MAX_FRAME_LEN + 1];
        assert!(r.feed(&junk).is_empty());
        assert!(r.desynced(), "an unterminated oversize line must resync");
    }

    #[test]
    fn raw_message_round_trip() {
        let mut r = Reassembler::new(Framing::RawMessage);
        let msgs = r.feed(&raw_request(7, b"list-sensors"));
        assert_eq!(msgs, vec![b"list-sensors".to_vec()]);
        assert_eq!(r.last_command(), 7);

        // The reply echoes the command with bit 31 set, and a little-endian length.
        let out = r.frame_out(b"1023");
        assert_eq!(&out[0..4], &(7u32 | 1 << 31).to_le_bytes());
        assert_eq!(&out[4..8], &4u32.to_le_bytes());
        assert_eq!(&out[8..], b"1023");
    }

    #[test]
    fn raw_message_reassembles_across_chunk_boundaries() {
        let mut r = Reassembler::new(Framing::RawMessage);
        let wire = raw_request(3, b"hello world");
        // Feed one byte at a time: nothing may emerge until the very last byte.
        for (i, b) in wire.iter().enumerate() {
            let got = r.feed(&[*b]);
            if i + 1 == wire.len() {
                assert_eq!(got, vec![b"hello world".to_vec()]);
            } else {
                assert!(got.is_empty(), "emitted early at byte {i}");
            }
        }
    }

    #[test]
    fn raw_message_handles_several_messages_in_one_chunk() {
        let mut r = Reassembler::new(Framing::RawMessage);
        let mut wire = raw_request(1, b"a");
        wire.extend(raw_request(2, b"bb"));
        wire.extend(raw_request(3, b"ccc"));
        assert_eq!(
            r.feed(&wire),
            vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]
        );
        assert_eq!(
            r.last_command(),
            3,
            "last_command tracks the newest request"
        );
    }

    #[test]
    fn raw_message_resyncs_on_implausible_length() {
        let mut r = Reassembler::new(Framing::RawMessage);
        let mut wire = 0u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_le_bytes());
        assert!(r.feed(&wire).is_empty());
        assert!(r.desynced());
    }

    #[test]
    fn fuzz_lite_never_panics() {
        // Deterministic xorshift — no rand dependency, and a failure is reproducible.
        let mut x: u32 = 0x1234_5678;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for kind in [Framing::Bare, Framing::Newline, Framing::RawMessage] {
            let mut r = Reassembler::new(kind);
            for _ in 0..20_000 {
                let b = (next() & 0xff) as u8;
                let _ = r.feed(&[b]);
                let _ = r.frame_out(&[b]);
            }
        }
    }
}
