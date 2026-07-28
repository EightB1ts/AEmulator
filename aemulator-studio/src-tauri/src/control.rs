//! Unix-socket control-plane client. The backend speaks the one-line text
//! protocol DIRECTLY (open the `UnixStream`, write `"<verb> args\n"`, read one
//! `OK …` / `ERR …` line) rather than shelling out to the one-shot client verbs.
//! This mirrors `aemulator_hvf::control::client_send`.
//!
//! Wire recap (from `crates/aemulator-hvf/src/control/mod.rs`):
//!   * request = one line, response = one line, `OK …` on success / `ERR …`.
//!   * `screenshot [path]` writes a PNG to `path` and replies
//!     `OK <path> WxH gen=N nonzero=M bytes=B` — the bytes are NOT on the socket,
//!     so callers read the file afterward.
//!   * `info` replies `OK WxH abs_max=A gen=N nonzero=M hash=HHHH`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A control verb, rendered to its exact one-line wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    Tap {
        x: u32,
        y: u32,
    },
    Swipe {
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        ms: Option<u32>,
    },
    Text(String),
    Key(String),
    /// Press-and-hold / release halves, for live keyboard forwarding + held modifiers.
    KeyDown(String),
    KeyUp(String),
    Launch(String),
    Screenshot(PathBuf),
    WaitStable {
        timeout_ms: Option<u32>,
    },
    Info,
    Quit,
}

impl Verb {
    /// The exact line sent over the socket (no trailing newline; `send` adds it).
    pub fn wire(&self) -> String {
        match self {
            Verb::Tap { x, y } => format!("tap {x} {y}"),
            Verb::Swipe { x1, y1, x2, y2, ms } => match ms {
                Some(ms) => format!("swipe {x1} {y1} {x2} {y2} {ms}"),
                None => format!("swipe {x1} {y1} {x2} {y2}"),
            },
            Verb::Text(s) => format!("text {s}"),
            Verb::Key(k) => format!("key {k}"),
            Verb::KeyDown(k) => format!("keydown {k}"),
            Verb::KeyUp(k) => format!("keyup {k}"),
            Verb::Launch(p) => format!("launch {p}"),
            Verb::Screenshot(path) => format!("screenshot {}", path.display()),
            Verb::WaitStable { timeout_ms } => match timeout_ms {
                Some(t) => format!("wait_stable {t}"),
                None => "wait_stable".into(),
            },
            Verb::Info => "info".into(),
            Verb::Quit => "quit".into(),
        }
    }
}

/// Send one raw command line to the socket and return the single-line reply
/// (trimmed). Times out fast so a wedged VM never blocks the IPC thread.
pub fn send_raw(socket: &Path, command: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect {}: {e}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    writeln!(stream, "{command}").map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader
        .read_line(&mut resp)
        .map_err(|e| format!("read: {e}"))?;
    Ok(resp.trim_end().to_string())
}

/// Send a typed verb; map an `ERR …` reply to `Err`.
pub fn send(socket: &Path, verb: &Verb) -> Result<String, String> {
    let reply = send_raw(socket, &verb.wire())?;
    if reply.starts_with("ERR") {
        Err(reply)
    } else {
        Ok(reply)
    }
}

/// Parsed `info` reply: framebuffer geometry + the change-detector counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub gen: u64,
    pub nonzero: u64,
}

/// Query `info` and parse `OK WxH abs_max=A gen=N nonzero=M hash=H`.
pub fn info(socket: &Path) -> Result<Info, String> {
    let reply = send(socket, &Verb::Info)?;
    parse_info(&reply).ok_or_else(|| format!("unparseable info reply: {reply}"))
}

fn parse_info(reply: &str) -> Option<Info> {
    // "OK 1024x768 abs_max=32767 gen=12 nonzero=345 hash=abcd..."
    let mut it = reply.split_whitespace();
    if it.next()? != "OK" {
        return None;
    }
    let dims = it.next()?;
    let (w, h) = dims.split_once('x')?;
    let mut info = Info {
        width: w.parse().ok()?,
        height: h.parse().ok()?,
        ..Default::default()
    };
    for tok in it {
        if let Some(v) = tok.strip_prefix("gen=") {
            info.gen = v.parse().unwrap_or(0);
        } else if let Some(v) = tok.strip_prefix("nonzero=") {
            info.nonzero = v.parse().unwrap_or(0);
        }
    }
    Some(info)
}

/// One raw RGBA frame: geometry, the generation it was captured at, and tightly-packed
/// `w*h*4` opaque-alpha pixels ready for a canvas `ImageData`.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub gen: u64,
    pub pixels: Vec<u8>,
}

/// A PERSISTENT control connection for the live-view frame loop.
///
/// Every other call in this module connects, sends one line, and closes. That is fine at human
/// cadence but wrong at frame cadence: the VMM's accept loop costs a fresh connection real latency,
/// and at 60fps we would open 60 sockets a second to ask the same question. The control protocol is
/// explicitly persistent ("the connection can send multiple commands, closes on EOF"), so the frame
/// loop holds ONE connection and pipelines requests down it.
pub struct FrameStream {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl FrameStream {
    pub fn connect(socket: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(socket)
            .map_err(|e| format!("connect {}: {e}", socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        // (No TCP_NODELAY concern here — Nagle is a TCP algorithm; unix-domain sockets
        // deliver immediately.)
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("clone stream: {e}"))?,
        );
        Ok(Self { stream, reader })
    }

    /// One request/reply on the held connection.
    fn request(&mut self, line: &str) -> Result<String, String> {
        writeln!(self.stream, "{line}").map_err(|e| format!("write: {e}"))?;
        self.stream.flush().map_err(|e| format!("flush: {e}"))?;
        let mut resp = String::new();
        self.reader
            .read_line(&mut resp)
            .map_err(|e| format!("read: {e}"))?;
        if resp.is_empty() {
            return Err("control connection closed".into());
        }
        Ok(resp.trim_end().to_string())
    }

    /// Fetch the next frame, or `Ok(None)` when the guest hasn't presented since `since`.
    pub fn next_frame(
        &mut self,
        path: &Path,
        since: Option<u64>,
    ) -> Result<Option<RawFrame>, String> {
        let cmd = match since {
            Some(g) => format!("frame {} since={}", path.display(), g),
            None => format!("frame {}", path.display()),
        };
        let reply = self.request(&cmd)?;
        parse_frame_reply(&reply, path)
    }
}

/// Shared reply parser for both the one-shot and persistent frame paths.
fn parse_frame_reply(reply: &str, path: &Path) -> Result<Option<RawFrame>, String> {
    if reply.starts_with("ERR") {
        return Err(reply.to_string());
    }
    if reply.contains("unchanged") {
        return Ok(None);
    }
    // "OK size=WxH gen=N bytes=B src=metal|cpu" — every field keyed, no echoed path. An earlier
    // version echoed the path and scanned for a bare `WxH` token by looking for an 'x'; both broke
    // on real paths, which contain spaces (and can contain an 'x').
    let mut width = 0u32;
    let mut height = 0u32;
    let mut gen = 0u64;
    for tok in reply.split_whitespace() {
        if let Some(size) = tok.strip_prefix("size=") {
            if let Some((w, h)) = size.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                    width = w;
                    height = h;
                }
            }
        } else if let Some(v) = tok.strip_prefix("gen=") {
            gen = v.parse().unwrap_or(0);
        }
    }
    if width == 0 || height == 0 {
        return Err(format!("unparseable frame reply: {reply}"));
    }
    let pixels = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let need = width as usize * height as usize * 4;
    if pixels.len() < need {
        return Err(format!(
            "short frame: {} bytes for {width}x{height} (want {need})",
            pixels.len()
        ));
    }
    Ok(Some(RawFrame {
        width,
        height,
        gen,
        pixels,
    }))
}

/// Ask the VM to capture a screenshot to `path`, then read the PNG bytes back.
/// Returns the raw PNG file contents.
pub fn screenshot_bytes(socket: &Path, path: &Path) -> Result<Vec<u8>, String> {
    let reply = send(socket, &Verb::Screenshot(path.to_path_buf()))?;
    if !reply.starts_with("OK") {
        return Err(reply);
    }
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Is the control plane answering yet? (convenience predicate).
#[allow(dead_code)]
pub fn is_ready(socket: &Path) -> bool {
    socket.exists() && info(socket).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_info_reply() {
        let i = parse_info("OK 1024x768 abs_max=32767 gen=12 nonzero=345 hash=deadbeef").unwrap();
        assert_eq!((i.width, i.height, i.gen, i.nonzero), (1024, 768, 12, 345));
    }

    #[test]
    fn parses_keyed_frame_reply() {
        // Keyed `size=`, no echoed path. The previous positional form was parsed by scanning
        // for a token containing 'x', which broke the moment the reply carried a path (paths
        // contain spaces AND can contain 'x') — the live-view stall.
        let dir = std::env::temp_dir().join("aemu-frame-reply-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.raw");
        std::fs::write(&p, vec![0u8; 4 * 4 * 4]).unwrap();
        let f = parse_frame_reply("OK size=4x4 gen=9 bytes=64 src=metal", &p)
            .unwrap()
            .expect("a keyed reply must parse");
        assert_eq!((f.width, f.height, f.gen), (4, 4, 9));
        assert_eq!(f.pixels.len(), 64);

        // `unchanged` short-circuits without touching the file.
        assert!(parse_frame_reply("OK unchanged gen=9", &p)
            .unwrap()
            .is_none());
        // A short file is an error, not a silently-accepted partial frame.
        std::fs::write(&p, vec![0u8; 8]).unwrap();
        assert!(parse_frame_reply("OK size=4x4 gen=9 bytes=64 src=cpu", &p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end against a LIVE VM: exercises the real `FrameStream` client — connect, request a
    /// frame at a SPACE-BEARING path, parse the reply, read the file, verify the length.
    ///
    /// Skipped unless `AEMU_TEST_SOCKET=<control socket>` is set, so it never breaks a normal
    /// `cargo test`. This is the path that stalled for an hour with no error; a smoke test that can
    /// be pointed at a running VM is how it gets caught next time.
    #[test]
    fn live_frame_stream_smoke() {
        let Some(sock) = std::env::var_os("AEMU_TEST_SOCKET") else {
            eprintln!("skipping: set AEMU_TEST_SOCKET=<control socket> to run");
            return;
        };
        let sock = std::path::PathBuf::from(sock);
        // Deliberately a path WITH SPACES, mirroring the real instance dir.
        let dir = std::env::temp_dir().join("aemu live frame test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.raw");
        let _ = std::fs::remove_file(&path);

        let mut stream = FrameStream::connect(&sock).expect("connect to control socket");
        let f = stream
            .next_frame(&path, None)
            .expect("frame request must succeed")
            .expect("a first request (since=None) must return a frame");
        assert!(
            f.width > 0 && f.height > 0,
            "geometry must parse: {f:?}",
            f = (f.width, f.height)
        );
        assert_eq!(
            f.pixels.len(),
            f.width as usize * f.height as usize * 4,
            "pixel buffer must be exactly w*h*4 RGBA"
        );
        assert!(
            path.exists(),
            "the frame must land at the REAL (spaced) path"
        );

        // Asking again at the same generation must short-circuit.
        assert!(
            stream.next_frame(&path, Some(f.gen)).unwrap().is_none(),
            "since=<current gen> must report unchanged"
        );
        eprintln!(
            "live_frame_stream_smoke OK: {}x{} gen={} {} bytes",
            f.width,
            f.height,
            f.gen,
            f.pixels.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verb_wire_forms() {
        assert_eq!(Verb::Tap { x: 5, y: 9 }.wire(), "tap 5 9");
        assert_eq!(
            Verb::Swipe {
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4,
                ms: Some(300)
            }
            .wire(),
            "swipe 1 2 3 4 300"
        );
        assert_eq!(Verb::Key("ENTER".into()).wire(), "key ENTER");
        // The live keyboard forwards physical keys by evdev number.
        assert_eq!(Verb::KeyDown("125".into()).wire(), "keydown 125");
        assert_eq!(Verb::KeyUp("125".into()).wire(), "keyup 125");
    }
}
