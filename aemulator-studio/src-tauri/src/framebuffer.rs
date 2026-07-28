//! Live-framebuffer poll loop, targeting 60fps.
//!
//! The control plane hands frames over a FILE, not the socket (the socket is a
//! one-line request/reply protocol), so this is a poll loop — but everything that
//! made the old one slow is gone:
//!
//! | | before | now |
//! |---|---|---|
//! | cadence | 250 ms (4 fps ceiling) | `FRAME_INTERVAL` 16 ms |
//! | round trips per tick | 2 (`info`, then `screenshot`) | 1 (`frame … since=<gen>`) |
//! | encode | PNG (~2.3 MB deflate/frame) | none — raw RGBA |
//! | transport | base64 `data:` **String** (+33%) | `InvokeResponseBody::Raw` bytes |
//! | viewer | `<img src=data:…>` → PNG decode | `putImageData` straight from the buffer |
//! | present path | forced GPU→CPU readback every guest frame | zero-copy preserved; readback only per delivered frame |
//!
//! Still change-gated, and now the gate is server-side: `since=<gen>` makes an
//! idle screen cost a single generation read — no blit, no transfer, no write, no
//! IPC. So polling at frame cadence is cheap when nothing moves.
//!
//! Wire format per message: an 8-byte little-endian header (`w: u32, h: u32`)
//! followed by `w*h*4` RGBA bytes. The header travels with the pixels so a mid-
//! stream resolution change can never be applied to the wrong buffer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::ipc::{Channel, InvokeResponseBody};

use crate::control;

/// Target cadence — 16 ms ≈ 60fps. The `since=<gen>` gate keeps idle ticks nearly free.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Backoff while the VM isn't answering (still booting its control plane, or gone).
const RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Bytes of little-endian header prepended to each frame: `w: u32, h: u32`.
pub const FRAME_HEADER_BYTES: usize = 8;

/// Parameters for one VM's framebuffer poll thread.
pub struct FbPoll {
    pub socket: PathBuf,
    pub live_shot: PathBuf,
    pub stop: Arc<AtomicBool>,
    pub chan: Channel<InvokeResponseBody>,
    /// Used to emit `vm-error` so a stalled frame stream is visible IN THE APP.
    /// Printing to the Studio's stdout was not enough: the user is looking at the
    /// Display tab (or the guest's Logs tab), not the terminal that launched the
    /// dev server, so a silent retry loop reads as "the emulator is broken".
    pub app: tauri::AppHandle,
    pub id: String,
}

/// Spawn the poll thread. It runs until `stop` is set (by `stop_framebuffer` or
/// VM teardown). Sends an initial frame immediately, then gen-gated frames.
pub fn spawn(poll: FbPoll) {
    std::thread::Builder::new()
        .name("fb-poll".into())
        .spawn(move || run(poll))
        .ok();
}

fn run(poll: FbPoll) {
    let FbPoll { socket, live_shot, stop, chan, app, id } = poll;
    // `None` forces a full capture on the first tick regardless of the VM's generation.
    let mut since: Option<u64> = None;
    // ONE connection for the whole stream — see `control::FrameStream`. Reconnected lazily
    // below, so a VM whose control plane isn't up yet (or that restarted) recovers on its own.
    let mut conn: Option<control::FrameStream> = None;
    // Dedupes the error log so a persistent failure reports once, not 4×/second.
    let mut last_err: Option<String> = None;
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let stream = match conn.as_mut() {
            Some(s) => s,
            None => match control::FrameStream::connect(&socket) {
                Ok(s) => {
                    conn = Some(s);
                    // Re-request the current frame after a reconnect.
                    since = None;
                    conn.as_mut().expect("just assigned")
                }
                Err(_) => {
                    std::thread::sleep(RETRY_INTERVAL);
                    continue;
                }
            },
        };
        match stream.next_frame(&live_shot, since) {
            // Generation advanced — a real frame came back.
            Ok(Some(frame)) => {
                since = Some(frame.gen);
                let need = frame.width as usize * frame.height as usize * 4;
                let mut msg = Vec::with_capacity(FRAME_HEADER_BYTES + need);
                msg.extend_from_slice(&frame.width.to_le_bytes());
                msg.extend_from_slice(&frame.height.to_le_bytes());
                msg.extend_from_slice(&frame.pixels[..need]);
                // A send error means the frontend dropped the channel.
                if chan.send(InvokeResponseBody::Raw(msg)).is_err() {
                    break;
                }
            }
            // Nothing moved. The VM did no work and neither do we.
            Ok(None) => {}
            Err(e) => {
                // Connection died (VM stopped/restarted) or the control plane isn't up.
                // Drop it so the next iteration reconnects, and back off so a booting VM
                // isn't hammered 60×/sec.
                //
                // LOG IT (once per distinct message). Swallowing this made a real defect —
                // a truncated capture path, so every read returned a stale file and failed
                // the length check — look exactly like "the guest hasn't rendered yet",
                // for an hour. A failing frame loop must never be silent.
                if last_err.as_deref() != Some(e.as_str()) {
                    eprintln!("[studio] frame stream error (retrying): {e}");
                    // Surface it in the UI too — see the `app` field's note.
                    let _ = tauri::Emitter::emit(
                        &app,
                        "vm-error",
                        crate::types::VmErrorEvent {
                            id: id.clone(),
                            message: format!("live view: {e}"),
                        },
                    );
                    last_err = Some(e);
                }
                conn = None;
                std::thread::sleep(RETRY_INTERVAL);
                continue;
            }
        }
        // Pace to the target cadence, accounting for the time the capture itself took, so a
        // slow frame doesn't add its cost on top of a full interval.
        if let Some(rest) = FRAME_INTERVAL.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Wrap PNG bytes as a `data:image/png;base64,…` URL (shared with `screenshot`).
pub fn png_data_url(bytes: &[u8]) -> String {
    let mut s = String::from("data:image/png;base64,");
    base64_encode_into(bytes, &mut s);
    s
}

/// Minimal std-only base64 (standard alphabet, padded). No external crate.
fn base64_encode_into(input: &[u8], out: &mut String) {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { A[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        let mut s = String::new();
        base64_encode_into(b"", &mut s);
        assert_eq!(s, "");
        s.clear();
        base64_encode_into(b"f", &mut s);
        assert_eq!(s, "Zg==");
        s.clear();
        base64_encode_into(b"fo", &mut s);
        assert_eq!(s, "Zm8=");
        s.clear();
        base64_encode_into(b"foo", &mut s);
        assert_eq!(s, "Zm9v");
        s.clear();
        base64_encode_into(b"foobar", &mut s);
        assert_eq!(s, "Zm9vYmFy");
    }
}
