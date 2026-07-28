//! A minimal host-side `secure_env` for the Cuttlefish HAL layer.
//!
//! Cuttlefish runs KeyMint / Gatekeeper / OemLock as *remote* HALs: the guest HAL service is a thin
//! shim that forwards each request over a dedicated `virtio-console` HVC channel to a host process
//! (`secure_env`, normally a Linux x86_64 binary in `cvd-host_package` — which CANNOT run on macOS).
//! This module is our from-scratch, macOS-native, **software** replacement for just enough of that
//! protocol to unblock the guest: keystore2 depends on the KeyMint HAL coming up, and the KeyMint
//! HAL aborts if it cannot open (and talk over) `/dev/hvc11`.
//!
//! Channel map (from the vendor image's `ueventd.rc` + the KeyMint HAL's observed
//! `open("/dev/hvc11")` abort):
//!   * hvc3  → keymaster (legacy C++ keymaster_channel, if present)
//!   * hvc4  → gatekeeper (remote)
//!   * hvc10 → oemlock
//!   * hvc11 → keymint / secureclock / sharedsecret (the Rust `kmr` HAL)  ← the boot blocker
//!
//! ## Transport framing
//! Every channel is length-prefixed. The Cuttlefish transport (`common/libs/transport`) frames each
//! message as an 8-byte little-endian header `[u32 command][u32 payload_size]` followed by
//! `payload_size` bytes. (The legacy `keymaster_channel` packs `command` as `cmd:31 | is_response:1`;
//! we preserve the low 31 bits and echo the command, which is compatible with both readers.) The
//! payload for the Rust KeyMint HAL is a `kmr_wire` **CBOR** blob.
//!
//! ## Honesty
//! This is NOT a TEE and NOT a complete KeyMint TA. It implements the transport + a CBOR codec + the
//! `kmr_wire` message *shape* (a `PerformOpResponse` = `[error_code, Option<PerformOpRsp>]`), and
//! answers the HAL-startup "set" operations (SetHalInfo / SetBootInfo / …) with well-formed
//! empty-success responses so the HAL can register its binder services. Every request is logged
//! (opcode + CBOR hex) so the exact next wall — the first op whose *response body* we do not yet
//! synthesize — is captured precisely. See PHASE4-STATUS.md.

use std::sync::{Arc, Mutex};

/// Which HAL a given HVC channel carries. Derived from the guest `ueventd.rc` map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecChannel {
    /// hvc11 — the Rust `kmr` KeyMint/SecureClock/SharedSecret HAL. The boot-critical one.
    Keymint,
    /// hvc3 — legacy keymaster (may be unused on android14; logged if it appears).
    Keymaster,
    /// hvc4 — remote gatekeeper.
    Gatekeeper,
    /// hvc10 — oemlock.
    Oemlock,
}

impl SecChannel {
    pub fn label(self) -> &'static str {
        match self {
            SecChannel::Keymint => "keymint(hvc11)",
            SecChannel::Keymaster => "keymaster(hvc3)",
            SecChannel::Gatekeeper => "gatekeeper(hvc4)",
            SecChannel::Oemlock => "oemlock(hvc10)",
        }
    }
}

// kmr_wire `KeyMintOperation` opcodes (reverse-engineered from the captured hvc11 requests +
// AOSP `system/keymint/wire`). Only the ones we model are named; the rest fall through to
// empty-success.
const OP_GET_HARDWARE_INFO: u64 = 0x11; // 17  — IKeyMintDevice.getHardwareInfo
const OP_GET_ROOT_OF_TRUST_CHALLENGE: u64 = 0x20; // 32
const OP_GET_SHARED_SECRET_PARAMETERS: u64 = 0x51; // 81 — ISharedSecret.getSharedSecretParameters
const OP_COMPUTE_SHARED_SECRET: u64 = 0x52; // 82 — ISharedSecret.computeSharedSecret

/// `SecurityLevel::TRUSTED_ENVIRONMENT` (keystore2's "default" instance must be a TEE).
const SECURITY_LEVEL_TEE: u64 = 1;
/// KeyMint HAL version we advertise (AIDL v3).
const KEYMINT_VERSION: u64 = 3;

/// A kmr_wire struct serializes as a CBOR array of its fields.
fn struct_of(fields: Vec<Cbor>) -> Cbor {
    Cbor::Array(fields)
}

/// Software secure-environment state shared across all secure HVC channels.
pub struct SecureEnv {
    /// Per-channel request counter (for logging).
    counts: [u32; 4],
    /// Stable 32-byte nonce reported by `getSharedSecretParameters` (must not change within a boot).
    shared_nonce: [u8; 32],
    /// Stable 32-byte sharing check returned by `computeSharedSecret`.
    sharing_check: [u8; 32],
}

impl Default for SecureEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl SecureEnv {
    pub fn new() -> Self {
        // Deterministic, non-secret constants — this is a software stand-in, not a TEE.
        let mut shared_nonce = [0u8; 32];
        for (i, b) in shared_nonce.iter_mut().enumerate() {
            *b = (0xA5u8).wrapping_add(i as u8);
        }
        let sharing_check = [0x5Au8; 32];
        SecureEnv {
            counts: [0; 4],
            shared_nonce,
            sharing_check,
        }
    }

    fn bump(&mut self, ch: SecChannel) -> u32 {
        let i = match ch {
            SecChannel::Keymint => 0,
            SecChannel::Keymaster => 1,
            SecChannel::Gatekeeper => 2,
            SecChannel::Oemlock => 3,
        };
        self.counts[i] += 1;
        self.counts[i]
    }

    /// Handle one framed request. `command` is the transport header's command word (low 31 bits);
    /// `payload` is the message body. Returns `Some((resp_command, resp_payload))` to send back, or
    /// `None` to send nothing.
    pub fn handle(
        &mut self,
        ch: SecChannel,
        command: u32,
        payload: &[u8],
    ) -> Option<(u32, Vec<u8>)> {
        let n = self.bump(ch);
        eprintln!(
            "[secure_env] {} req #{n}: cmd={:#x} len={} bytes={}",
            ch.label(),
            command,
            payload.len(),
            hex_head(payload, 48),
        );
        match ch {
            SecChannel::Keymint | SecChannel::Keymaster => self.keymint(command, payload),
            SecChannel::Gatekeeper => self.gatekeeper(command, payload),
            SecChannel::Oemlock => self.oemlock(command, payload),
        }
    }

    /// Rust `kmr` KeyMint responder. The wire payload is a CBOR `PerformOpReq` = `[opcode, req_body]`.
    /// We echo the transport `command`, decode the opcode for logging, and return a well-formed
    /// `PerformOpResponse`.
    fn keymint(&mut self, command: u32, payload: &[u8]) -> Option<(u32, Vec<u8>)> {
        let (opcode, body_summary) = match cbor_decode(payload) {
            Ok((Cbor::Array(items), _)) if !items.is_empty() => {
                let op = items[0].as_u64();
                let summary = format!("{:?}", &items[1..]);
                (op, summary)
            }
            Ok((other, _)) => {
                eprintln!("[secure_env] keymint: request not a CBOR array: {other:?}");
                (None, String::from("<non-array>"))
            }
            Err(e) => {
                eprintln!("[secure_env] keymint: CBOR decode failed: {e}");
                (None, String::from("<decode-error>"))
            }
        };
        eprintln!(
            "[secure_env] keymint op={:?} body={}",
            opcode,
            truncate(&body_summary, 120)
        );

        // Build a well-formed `PerformOpResponse` (reverse-engineered from the captured requests, all
        // kmr_wire CBOR):
        //   PerformOpResponse = [error_code, Option<PerformOpRsp>]  (Option: [] = None, [x] = Some(x))
        //   PerformOpRsp      = [opcode, response_struct]
        //   a struct         = an array of its fields (0 fields → [])
        //
        // For the HAL-startup "set/configure" ops (SetHalInfo/SetBootInfo/SetAttestationIds) and for
        // ops whose response carries no data (AddRngEntropy, EarlyBootEnded, …) the response struct is
        // empty → `[]`. Ops with a real response body are modeled explicitly below. Anything we don't
        // model yet falls through to empty-success (and is logged, so the next wall is the first op
        // whose non-empty response body we haven't synthesized).
        let op = opcode.unwrap_or(0);
        let body: Cbor = match op {
            OP_GET_HARDWARE_INFO => {
                // KeyMintHardwareInfo { version_number, security_level, key_mint_name,
                // key_mint_author_name, timestamp_token_required }. keystore2 REQUIRES the "default"
                // instance to report TRUSTED_ENVIRONMENT (security_level = 1); SOFTWARE (0) makes it
                // abort "Trying to construct mandatory security level TEE".
                struct_of(vec![Cbor::Array(vec![
                    Cbor::Uint(KEYMINT_VERSION),
                    Cbor::Uint(SECURITY_LEVEL_TEE),
                    Cbor::Text("aemulator-keymint".into()),
                    Cbor::Text("aemulator".into()),
                    Cbor::Bool(false),
                ])])
            }
            OP_GET_SHARED_SECRET_PARAMETERS => {
                // GetSharedSecretParametersResponse { params: SharedSecretParameters { seed, nonce } }.
                struct_of(vec![Cbor::Array(vec![
                    Cbor::Bytes(vec![]),                     // seed (empty)
                    Cbor::Bytes(self.shared_nonce.to_vec()), // nonce (32 stable bytes)
                ])])
            }
            OP_COMPUTE_SHARED_SECRET => {
                // ComputeSharedSecretResponse { sharing_check: Vec<u8> (32) }. Single instance → any
                // stable 32-byte value is accepted (keystore2 only cross-checks across instances).
                struct_of(vec![Cbor::Bytes(self.sharing_check.to_vec())])
            }
            OP_GET_ROOT_OF_TRUST_CHALLENGE => {
                // GetRootOfTrustChallengeResponse { challenge: [u8; 16] }.
                struct_of(vec![Cbor::Bytes(vec![0u8; 16])])
            }
            _ => Cbor::Array(vec![]), // empty-success for set/void ops + everything not yet modeled
        };
        let rsp = Cbor::Array(vec![
            Cbor::Uint(0), // error_code = 0 (ErrorCode::Ok)
            Cbor::Array(vec![Cbor::Array(vec![Cbor::Uint(op), body])]),
        ]);
        Some((command, cbor_encode(&rsp)))
    }

    /// Gatekeeper responder — logged only for now (not the boot blocker; keystore2 gates on KeyMint,
    /// not gatekeeper). A real software gatekeeper (HMAC password handles) is a follow-up.
    fn gatekeeper(&mut self, command: u32, payload: &[u8]) -> Option<(u32, Vec<u8>)> {
        eprintln!(
            "[secure_env] gatekeeper: unmodeled request (len {}) — no reply",
            payload.len()
        );
        let _ = command;
        None
    }

    fn oemlock(&mut self, command: u32, payload: &[u8]) -> Option<(u32, Vec<u8>)> {
        eprintln!(
            "[secure_env] oemlock: unmodeled request (len {}) — no reply",
            payload.len()
        );
        let _ = command;
        None
    }
}

/// Process-wide singleton so every secure HVC channel shares one env. Returns an `Arc` clone.
pub fn shared() -> Arc<Mutex<SecureEnv>> {
    use std::sync::OnceLock;
    static ENV: OnceLock<Arc<Mutex<SecureEnv>>> = OnceLock::new();
    ENV.get_or_init(|| Arc::new(Mutex::new(SecureEnv::new())))
        .clone()
}

// ------------------------------------------------------------------------------------------------
// Minimal CBOR (RFC 8949) codec — just the major types kmr_wire uses: uint, negint, byte/text
// string, array, bool/null. Enough to decode a `PerformOpReq` for logging and encode a
// `PerformOpResponse`.
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Cbor {
    Uint(u64),
    Neg(u64), // encodes -(n+1)
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Bool(bool),
    Null,
}

impl Cbor {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Cbor::Uint(v) => Some(*v),
            _ => None,
        }
    }
}

fn cbor_encode(c: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    enc(c, &mut out);
    out
}

fn enc_head(major: u8, val: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if val < 24 {
        out.push(m | val as u8);
    } else if val < 0x100 {
        out.push(m | 24);
        out.push(val as u8);
    } else if val < 0x1_0000 {
        out.push(m | 25);
        out.extend_from_slice(&(val as u16).to_be_bytes());
    } else if val < 0x1_0000_0000 {
        out.push(m | 26);
        out.extend_from_slice(&(val as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&val.to_be_bytes());
    }
}

fn enc(c: &Cbor, out: &mut Vec<u8>) {
    match c {
        Cbor::Uint(v) => enc_head(0, *v, out),
        Cbor::Neg(v) => enc_head(1, *v, out),
        Cbor::Bytes(b) => {
            enc_head(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Cbor::Text(s) => {
            enc_head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Cbor::Array(items) => {
            enc_head(4, items.len() as u64, out);
            for it in items {
                enc(it, out);
            }
        }
        Cbor::Bool(b) => out.push(0xf4 | (*b as u8)), // 0xf4 false, 0xf5 true
        Cbor::Null => out.push(0xf6),
    }
}

fn cbor_decode(data: &[u8]) -> Result<(Cbor, usize), String> {
    dec(data, 0, 0)
}

fn dec(data: &[u8], pos: usize, depth: usize) -> Result<(Cbor, usize), String> {
    if depth > 32 {
        return Err("cbor nesting too deep".into());
    }
    let ib = *data.get(pos).ok_or("cbor: truncated")?;
    let major = ib >> 5;
    let ai = ib & 0x1f;
    let mut p = pos + 1;
    let read_len = |p: &mut usize| -> Result<u64, String> {
        Ok(match ai {
            0..=23 => ai as u64,
            24 => {
                let v = *data.get(*p).ok_or("cbor: trunc u8")? as u64;
                *p += 1;
                v
            }
            25 => {
                let b = data.get(*p..*p + 2).ok_or("cbor: trunc u16")?;
                *p += 2;
                u16::from_be_bytes([b[0], b[1]]) as u64
            }
            26 => {
                let b = data.get(*p..*p + 4).ok_or("cbor: trunc u32")?;
                *p += 4;
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
            }
            27 => {
                let b = data.get(*p..*p + 8).ok_or("cbor: trunc u64")?;
                *p += 8;
                u64::from_be_bytes(b.try_into().unwrap())
            }
            _ => return Err(format!("cbor: bad addl-info {ai}")),
        })
    };
    match major {
        0 => {
            let v = read_len(&mut p)?;
            Ok((Cbor::Uint(v), p))
        }
        1 => {
            let v = read_len(&mut p)?;
            Ok((Cbor::Neg(v), p))
        }
        2 => {
            let n = read_len(&mut p)? as usize;
            let b = data.get(p..p + n).ok_or("cbor: trunc bytes")?.to_vec();
            Ok((Cbor::Bytes(b), p + n))
        }
        3 => {
            let n = read_len(&mut p)? as usize;
            let b = data.get(p..p + n).ok_or("cbor: trunc text")?;
            Ok((Cbor::Text(String::from_utf8_lossy(b).into_owned()), p + n))
        }
        4 => {
            let n = read_len(&mut p)? as usize;
            let mut items = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                let (c, np) = dec(data, p, depth + 1)?;
                items.push(c);
                p = np;
            }
            Ok((Cbor::Array(items), p))
        }
        7 => match ai {
            20 => Ok((Cbor::Bool(false), p)),
            21 => Ok((Cbor::Bool(true), p)),
            22 => Ok((Cbor::Null, p)),
            _ => Ok((Cbor::Null, p)),
        },
        _ => Err(format!("cbor: unsupported major {major}")),
    }
}

fn hex_head(b: &[u8], max: usize) -> String {
    let n = b.len().min(max);
    let mut s = String::with_capacity(n * 2 + 4);
    for &x in &b[..n] {
        s.push_str(&format!("{x:02x}"));
    }
    if b.len() > max {
        s.push_str("..");
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}..", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbor_roundtrip_uint_array() {
        // [0, [[17, []]]]
        let c = Cbor::Array(vec![
            Cbor::Uint(0),
            Cbor::Array(vec![Cbor::Array(vec![Cbor::Uint(17), Cbor::Array(vec![])])]),
        ]);
        let enc = cbor_encode(&c);
        let (dec, used) = cbor_decode(&enc).unwrap();
        assert_eq!(used, enc.len());
        if let Cbor::Array(items) = dec {
            assert_eq!(items[0].as_u64(), Some(0));
        } else {
            panic!("not array");
        }
    }

    #[test]
    fn cbor_decode_perform_op_req_shape() {
        // A PerformOpReq [opcode=18, []]
        let req = cbor_encode(&Cbor::Array(vec![Cbor::Uint(18), Cbor::Array(vec![])]));
        let (dec, _) = cbor_decode(&req).unwrap();
        if let Cbor::Array(items) = dec {
            assert_eq!(items[0].as_u64(), Some(18));
        } else {
            panic!();
        }
    }

    #[test]
    fn keymint_returns_wellformed_response() {
        let mut env = SecureEnv::new();
        let req = cbor_encode(&Cbor::Array(vec![Cbor::Uint(1), Cbor::Array(vec![])]));
        let (cmd, rsp) = env.handle(SecChannel::Keymint, 0, &req).unwrap();
        assert_eq!(cmd, 0);
        // response must decode as an array [0, [...]]
        let (dec, used) = cbor_decode(&rsp).unwrap();
        assert_eq!(used, rsp.len());
        matches!(dec, Cbor::Array(_));
    }
}
