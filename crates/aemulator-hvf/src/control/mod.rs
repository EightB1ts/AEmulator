//! The **control plane** (Phase 6): turn the existing host-side framebuffer capture + virtio-input
//! injection into a clean, scriptable command interface an LLM (or a thin MCP server) can drive —
//! with NO adb and NO in-guest accessibility service. Everything runs host-side against the guest's
//! own scanout pixels and its virtio-input touchscreen/keyboard.
//!
//! ## Transports (all speak the SAME one-line text protocol)
//!   * **Unix-domain socket** (default `/tmp/aemulator-hvf-control.sock`): the persistent command
//!     channel. A running `aemulator-hvf control …` server listens; any number of one-shot clients
//!     (`aemulator-hvf tap 500 400`, or the deferred MCP server) connect, send one request line, read
//!     one response line, and disconnect. This is the seam a thin MCP server wraps.
//!   * **stdin REPL** (`--repl`): read request lines from stdin, write responses to stdout — an
//!     interactive session in one terminal. (Control mode frees stdin from the guest console; guest
//!     console OUTPUT still streams to stdout, and `launch` injects into the guest console over the
//!     bridge's `console_tx`.)
//!   * **`--script <file>`**: execute newline-delimited commands from a file at startup, print each
//!     result, then keep serving the socket (unless `--exit-after-script`).
//!
//! ## Protocol
//! Request  = one line: `<verb> [args…]`. Response = one line: `OK …` or `ERR …`.
//! Verbs: `screenshot [path]`, `tap X Y`, `swipe X1 Y1 X2 Y2 [MS]`, `text <string…>`, `key <code|name>`,
//! `keydown <code|name>`, `keyup <code|name>`, `launch <pkg>`, `wait_stable [TIMEOUT_MS] [INTERVAL_MS]`,
//! `hash`, `info`, `help`, `quit`.
//!
//! `key` is an atomic press+release; `keydown`/`keyup` are the separate edges, which is what a live
//! keyboard (Studio's Display tab) and any held-modifier chord (Cmd+A, Ctrl+C, shift-arrow) need.
//! Whoever sends a `keydown` owns the matching `keyup` — a dropped release is a stuck modifier.
//! Coordinates are in **display space** (`width`×`height`, default 1024×768); they are mapped into the
//! virtio-input absolute range `0..=ABS_MAX` before injection.

pub mod png;

use crate::machine::{DisplayBridge, InputFrame};
use crate::virtio::input::{ABS_MAX, ABS_X, ABS_Y, BTN_TOUCH, EV_ABS, EV_KEY};
use crate::virtio::ScanoutHandle;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default Unix-domain socket path the control server listens on and one-shot clients connect to.
pub const DEFAULT_SOCKET: &str = "/tmp/aemulator-hvf-control.sock";

/// How long the accept loop blocks in `poll()` before re-checking the shutdown flags.
/// This bounds teardown responsiveness ONLY — a pending connection wakes `poll()`
/// immediately, so it costs an arriving request nothing.
const ACCEPT_POLL_TIMEOUT_MS: libc::c_int = 100;

/// Options controlling how [`run_control`] serves commands.
#[derive(Clone, Debug, Default)]
pub struct ControlOpts {
    /// Unix-domain socket path (falls back to [`DEFAULT_SOCKET`]).
    pub socket: Option<PathBuf>,
    /// Optional script file of newline commands executed at startup.
    pub script: Option<PathBuf>,
    /// Read interactive commands from stdin.
    pub repl: bool,
    /// Exit after the startup script finishes (do not keep serving the socket).
    pub exit_after_script: bool,
}

/// The command executor shared across the socket/REPL handler threads. All senders live behind one
/// mutex so a multi-frame gesture (a swipe, a text string) is injected atomically — two concurrent
/// clients can never interleave their events mid-gesture.
pub struct Controller {
    inner: Mutex<Exec>,
    scanout: ScanoutHandle,
    width: u32,
    height: u32,
    /// Flipped by `quit`/`shutdown` so the server loops can wind down.
    stop: Arc<AtomicBool>,
}

struct Exec {
    input_tx: Sender<InputFrame>,
    console_tx: Option<Sender<Vec<u8>>>,
}

impl Controller {
    fn map_abs(&self, x: i64, y: i64) -> (i32, i32) {
        let w = self.width.max(1) as i64;
        let h = self.height.max(1) as i64;
        let cx = x.clamp(0, w - 1);
        let cy = y.clamp(0, h - 1);
        let ax = (cx * ABS_MAX as i64 / (w - 1).max(1)) as i32;
        let ay = (cy * ABS_MAX as i64 / (h - 1).max(1)) as i32;
        (ax.clamp(0, ABS_MAX as i32), ay.clamp(0, ABS_MAX as i32))
    }

    fn send(&self, exec: &Exec, frame: InputFrame) {
        let _ = exec.input_tx.send(frame);
    }

    /// A single tap: touch-down at (x,y), brief hold, touch-up.
    fn tap(&self, exec: &Exec, x: i64, y: i64) {
        let (ax, ay) = self.map_abs(x, y);
        self.send(
            exec,
            vec![
                (EV_ABS, ABS_X, ax),
                (EV_ABS, ABS_Y, ay),
                (EV_KEY, BTN_TOUCH, 1),
            ],
        );
        std::thread::sleep(Duration::from_millis(60));
        self.send(
            exec,
            vec![
                (EV_ABS, ABS_X, ax),
                (EV_ABS, ABS_Y, ay),
                (EV_KEY, BTN_TOUCH, 0),
            ],
        );
    }

    /// A directional drag from (x1,y1) to (x2,y2) over `ms` milliseconds (interpolated moves).
    fn swipe(&self, exec: &Exec, x1: i64, y1: i64, x2: i64, y2: i64, ms: u64) {
        let (ax1, ay1) = self.map_abs(x1, y1);
        let (ax2, ay2) = self.map_abs(x2, y2);
        let steps: i64 = 24;
        let step_ms = (ms / steps as u64).max(4);
        self.send(
            exec,
            vec![
                (EV_ABS, ABS_X, ax1),
                (EV_ABS, ABS_Y, ay1),
                (EV_KEY, BTN_TOUCH, 1),
            ],
        );
        std::thread::sleep(Duration::from_millis(step_ms));
        for s in 1..=steps {
            let ax = ax1 + ((ax2 - ax1) as i64 * s / steps) as i32;
            let ay = ay1 + ((ay2 - ay1) as i64 * s / steps) as i32;
            self.send(exec, vec![(EV_ABS, ABS_X, ax), (EV_ABS, ABS_Y, ay)]);
            std::thread::sleep(Duration::from_millis(step_ms));
        }
        self.send(
            exec,
            vec![
                (EV_ABS, ABS_X, ax2),
                (EV_ABS, ABS_Y, ay2),
                (EV_KEY, BTN_TOUCH, 0),
            ],
        );
    }

    /// Inject a single evdev key (press then release).
    fn key(&self, exec: &Exec, code: u16) {
        self.send(exec, vec![(EV_KEY, code, 1)]);
        std::thread::sleep(Duration::from_millis(30));
        self.send(exec, vec![(EV_KEY, code, 0)]);
    }

    /// Press a key and LEAVE IT HELD. The other half of a chord: a live keyboard
    /// forwards its own down/up edges, and a modifier (shift/ctrl/alt/meta) is only
    /// a modifier while it is down — `key`'s atomic press+release can never express
    /// that, so Cmd+A / Ctrl+C / shift-arrow selection are impossible without this.
    /// The caller owns the matching `key_up`; a dropped release leaves the guest
    /// with a stuck modifier (the studio releases everything on blur for exactly
    /// this reason).
    fn key_down(&self, exec: &Exec, code: u16) {
        self.send(exec, vec![(EV_KEY, code, 1)]);
    }

    /// Release a held key. Safe to send for a key that is not down (the guest's
    /// input reader ignores a 0-value for an already-released key).
    fn key_up(&self, exec: &Exec, code: u16) {
        self.send(exec, vec![(EV_KEY, code, 0)]);
    }

    /// Type a string as key events (with SHIFT for uppercase / shifted symbols).
    fn text(&self, exec: &Exec, s: &str) {
        const KEY_LEFTSHIFT: u16 = 42;
        for ch in s.chars() {
            let Some((code, shift)) = char_to_key(ch) else {
                continue;
            };
            if shift {
                self.send(exec, vec![(EV_KEY, KEY_LEFTSHIFT, 1)]);
            }
            self.send(exec, vec![(EV_KEY, code, 1)]);
            std::thread::sleep(Duration::from_millis(12));
            self.send(exec, vec![(EV_KEY, code, 0)]);
            if shift {
                self.send(exec, vec![(EV_KEY, KEY_LEFTSHIFT, 0)]);
            }
            std::thread::sleep(Duration::from_millis(18));
        }
    }

    /// A stable 64-bit hash of the current scanout, plus its flush generation.
    /// Content hash + flush generation + nonzero-byte count of **what is actually on screen**.
    ///
    /// In headless `--control` mode there is no CAMetalLayer, so `flush_3d` takes the readback path
    /// and writes the CPU framebuffer on every guest flush. Reading it here is therefore accurate
    /// again — which is the point: `nonzero` is the headline number in the Studio's live-view status
    /// line, and when the present path stopped writing this buffer it reported a working display as
    /// a black screen (verified: 836,805 nonzero bytes in the same frame that `info` said
    /// `nonzero=0`). Do not reintroduce a present mode that leaves this buffer unwritten in a mode
    /// where these verbs are the only viewer.
    fn fb_hash(&self) -> (u64, u64, usize) {
        let gen = self.scanout.generation();
        let fb = self.scanout.fb.lock().unwrap();
        (
            fnv1a(&fb.bytes),
            gen,
            fb.bytes.iter().filter(|b| **b != 0).count(),
        )
    }

    /// Poll the framebuffer until its content-hash holds steady for `stable_ticks` consecutive
    /// intervals, or `timeout` elapses. Returns `Ok(gen)` when stable, `Err(())` on timeout.
    fn wait_stable(&self, timeout: Duration, interval: Duration) -> Result<u64, u64> {
        let deadline = Instant::now() + timeout;
        let stable_ticks = 3u32;
        let (mut last_hash, mut last_gen, _) = self.fb_hash();
        let mut steady = 0u32;
        loop {
            std::thread::sleep(interval);
            let (h, gen, _) = self.fb_hash();
            // Require BOTH the content hash AND the flush generation to hold still. Hash alone is
            // wrong whenever the CPU scanout isn't the live surface: under zero-copy present the
            // framebuffer is never written, so its hash is permanently constant and a hash-only
            // check reports "stable" while the guest is actively animating. `gen` advances on every
            // present regardless of path, so the pair is correct for both.
            if h == last_hash && gen == last_gen {
                steady += 1;
                if steady >= stable_ticks {
                    return Ok(gen);
                }
            } else {
                steady = 0;
                last_hash = h;
                last_gen = gen;
            }
            if Instant::now() >= deadline {
                return Err(last_gen);
            }
        }
    }

    /// Capture the current scanout to a PNG file. Returns the response line.
    fn screenshot(&self, path: &Path) -> String {
        let (w, h, rgb) = {
            let fb = self.scanout.fb.lock().unwrap();
            let (w, h) = (fb.width.max(1), fb.height.max(1));
            let mut rgb = Vec::with_capacity((w * h * 3) as usize);
            for i in 0..(w * h) as usize {
                let idx = i * 4;
                if idx + 2 < fb.bytes.len() {
                    // Guest fb is BGRA little-endian: byte0=B byte1=G byte2=R.
                    rgb.push(fb.bytes[idx + 2]); // R
                    rgb.push(fb.bytes[idx + 1]); // G
                    rgb.push(fb.bytes[idx]); // B
                } else {
                    rgb.extend_from_slice(&[0, 0, 0]);
                }
            }
            (w, h, rgb)
        };
        let png = png::encode_rgb(w, h, &rgb);
        match std::fs::write(path, &png) {
            Ok(()) => {
                let gen = self.scanout.generation();
                let nz = rgb.iter().filter(|b| **b != 0).count();
                format!(
                    "OK {} {}x{} gen={} nonzero={} bytes={}",
                    path.display(),
                    w,
                    h,
                    gen,
                    nz,
                    png.len()
                )
            }
            Err(e) => format!("ERR write {}: {e}", path.display()),
        }
    }

    /// Capture the current display to `path` as tightly-packed **RGBA8**, no container.
    ///
    /// This is the live-view hot path, and it differs from `screenshot` in three ways that together
    /// are what make 60fps viable:
    ///  1. **No PNG encode.** Encoding ~2.3 MB per frame dominated the old path; raw bytes are a
    ///     `write` into the page cache.
    ///  2. **Reads the CPU scanout framebuffer.** In headless mode `flush_3d` fills it on every
    ///     guest flush (there is no CAMetalLayer to present into), so it is the live surface.
    ///  3. **`since=<gen>` short-circuit.** A still screen costs one generation read and nothing
    ///     else: no blit, no transfer, no write. Polling fast is therefore cheap when idle.
    ///
    /// RGBA (not BGRA) with opaque alpha is deliberate: it drops into a canvas `ImageData` with no
    /// per-pixel work on the viewer side.
    fn frame(&self, path: &Path, since: Option<u64>) -> String {
        let gen = self.scanout.generation();
        if since == Some(gen) {
            return format!("OK unchanged gen={gen}");
        }

        let (w, h, rgba) = {
            let fb = self.scanout.fb.lock().unwrap();
            let (w, h) = (fb.width.max(1), fb.height.max(1));
            let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
            for i in 0..(w * h) as usize {
                let src = i * 4;
                let dst = i * 4;
                if src + 2 < fb.bytes.len() {
                    // Guest fb is BGRA little-endian: byte0=B byte1=G byte2=R.
                    rgba[dst] = fb.bytes[src + 2];
                    rgba[dst + 1] = fb.bytes[src + 1];
                    rgba[dst + 2] = fb.bytes[src];
                }
                rgba[dst + 3] = 255;
            }
            (w, h, rgba)
        };
        // The reply deliberately does NOT echo the path back. Paths contain spaces, and a
        // space-bearing path in a whitespace-delimited reply is unparseable — the caller passed the
        // path in, so repeating it buys nothing and breaks parsing. Geometry is keyed (`size=`)
        // rather than positional for the same reason: a bare `1024x768` token was found by scanning
        // for an 'x', which a path segment can also contain.
        match write_frame_atomic(path, &rgba) {
            Ok(()) => format!("OK size={w}x{h} gen={gen} bytes={} src=cpu", rgba.len()),
            Err(e) => format!("ERR write {}: {e}", path.display()),
        }
    }

    /// Execute one request line and return the (single-line) response.
    pub fn execute(&self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() {
            return "OK".to_string();
        }
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (line, ""),
        };
        match verb {
            "screenshot" | "shot" => {
                let path = if rest.is_empty() {
                    default_shot_path()
                } else {
                    PathBuf::from(rest)
                };
                self.screenshot(&path)
            }
            // `frame <path> [since=<gen>]` — raw RGBA capture for the live view. See `Self::frame`.
            //
            // The path MUST NOT be tokenized: real paths contain spaces (every macOS instance dir
            // is under "Application Support"). Tokenizing truncated the path at the first space,
            // silently wrote the frame to `/Users/<u>/Library/Application`, and left the caller
            // reading a stale file forever. So: peel an OPTIONAL trailing `since=<gen>` off the
            // end, and take the whole remainder as the path verbatim — exactly how `screenshot`
            // has always treated `rest`.
            "frame" => {
                let (path_part, since) = parse_frame_args(rest);
                if path_part.is_empty() {
                    "ERR usage: frame <path> [since=<gen>]".to_string()
                } else {
                    self.frame(Path::new(path_part), since)
                }
            }
            "tap" => {
                let n = parse_ints(rest);
                if n.len() != 2 {
                    return "ERR usage: tap X Y".to_string();
                }
                let exec = self.inner.lock().unwrap();
                self.tap(&exec, n[0], n[1]);
                format!("OK tap {} {}", n[0], n[1])
            }
            "swipe" => {
                let n = parse_ints(rest);
                if n.len() < 4 {
                    return "ERR usage: swipe X1 Y1 X2 Y2 [MS]".to_string();
                }
                let ms = if n.len() >= 5 { n[4].max(0) as u64 } else { 300 };
                let exec = self.inner.lock().unwrap();
                self.swipe(&exec, n[0], n[1], n[2], n[3], ms);
                format!("OK swipe {} {} -> {} {} ({}ms)", n[0], n[1], n[2], n[3], ms)
            }
            "text" => {
                if rest.is_empty() {
                    return "ERR usage: text <string>".to_string();
                }
                let exec = self.inner.lock().unwrap();
                self.text(&exec, rest);
                format!("OK text ({} chars)", rest.chars().count())
            }
            "key" => {
                let code = match parse_key(rest) {
                    Some(c) => c,
                    None => return format!("ERR unknown key: {rest}"),
                };
                let exec = self.inner.lock().unwrap();
                self.key(&exec, code);
                format!("OK key {code}")
            }
            "keydown" | "key_down" => {
                let code = match parse_key(rest) {
                    Some(c) => c,
                    None => return format!("ERR unknown key: {rest}"),
                };
                let exec = self.inner.lock().unwrap();
                self.key_down(&exec, code);
                format!("OK keydown {code}")
            }
            "keyup" | "key_up" => {
                let code = match parse_key(rest) {
                    Some(c) => c,
                    None => return format!("ERR unknown key: {rest}"),
                };
                let exec = self.inner.lock().unwrap();
                self.key_up(&exec, code);
                format!("OK keyup {code}")
            }
            "launch" => {
                if rest.is_empty() {
                    return "ERR usage: launch <package>".to_string();
                }
                let exec = self.inner.lock().unwrap();
                match &exec.console_tx {
                    Some(tx) => {
                        // Best-effort: drive `am start` over the guest serial console (requires a
                        // console shell in the booted image; input-driven navigation is the primary,
                        // adb-free control path).
                        let cmd = format!("am start -n {rest} || monkey -p {rest} -c android.intent.category.LAUNCHER 1\n");
                        let _ = tx.send(cmd.into_bytes());
                        format!("OK launch {rest} (am/monkey over console; best-effort)")
                    }
                    None => "ERR launch: no console channel wired".to_string(),
                }
            }
            "sh" => {
                // Run an arbitrary command on the guest serial console (root shell on ttyAMA0).
                // Output appears in the VMM's streamed console log. Requires a console shell in the
                // booted image. Used for host-driven diagnostics (e.g. networking bring-up/proof).
                if rest.is_empty() {
                    return "ERR usage: sh <command>".to_string();
                }
                let exec = self.inner.lock().unwrap();
                match &exec.console_tx {
                    Some(tx) => {
                        let cmd = format!("{rest}\n");
                        let _ = tx.send(cmd.into_bytes());
                        format!("OK sh: {rest}")
                    }
                    None => "ERR sh: no console channel wired".to_string(),
                }
            }
            "wait_stable" | "wait" => {
                let n = parse_ints(rest);
                let timeout = Duration::from_millis(if !n.is_empty() { n[0].max(0) as u64 } else { 8000 });
                let interval = Duration::from_millis(if n.len() >= 2 { n[1].max(1) as u64 } else { 250 });
                match self.wait_stable(timeout, interval) {
                    Ok(gen) => format!("OK stable gen={gen}"),
                    Err(gen) => format!("OK timeout gen={gen}"),
                }
            }
            "hash" => {
                let (h, gen, nz) = self.fb_hash();
                format!("OK hash={h:016x} gen={gen} nonzero={nz}")
            }
            "info" => {
                let (h, gen, nz) = self.fb_hash();
                format!(
                    "OK {}x{} abs_max={} gen={} nonzero={} hash={:016x}",
                    self.width, self.height, ABS_MAX, gen, nz, h
                )
            }
            "help" => "OK verbs: screenshot|shot [path], frame <path> [since=<gen>], tap X Y, swipe X1 Y1 X2 Y2 [ms], text <s>, key <code|name>, keydown <code|name>, keyup <code|name>, launch <pkg>, wait_stable [timeout_ms] [interval_ms], hash, info, quit".to_string(),
            "quit" | "shutdown" | "exit" => {
                self.stop.store(true, Ordering::Release);
                "OK bye".to_string()
            }
            other => format!("ERR unknown verb: {other} (try `help`)"),
        }
    }
}

/// Own the main thread after boot: serve the control protocol over the Unix socket (+ optional
/// startup script + optional stdin REPL) until the guest stops or a `quit` is received.
pub fn run_control(bridge: DisplayBridge, opts: ControlOpts) {
    let DisplayBridge {
        scanout,
        input_tx,
        console_tx,
        width,
        height,
        vmm_done,
    } = bridge;

    let stop = Arc::new(AtomicBool::new(false));
    let controller = Arc::new(Controller {
        inner: Mutex::new(Exec {
            input_tx,
            console_tx,
        }),
        scanout,
        width,
        height,
        stop: stop.clone(),
    });

    let socket_path = opts
        .socket
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));
    // Fresh socket each run.
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => {
            l.set_nonblocking(true).ok();
            println!("[control] listening on unix:{}", socket_path.display());
            Some(l)
        }
        Err(e) => {
            eprintln!(
                "[control] could not bind {}: {e} (socket transport disabled)",
                socket_path.display()
            );
            None
        }
    };

    println!(
        "[control] ready — {}x{} scanout; verbs: screenshot|tap|swipe|text|key|launch|wait_stable|hash|info|quit",
        width, height
    );

    // Startup script.
    if let Some(script) = &opts.script {
        match std::fs::read_to_string(script) {
            Ok(body) => {
                println!("[control] running script {}", script.display());
                for raw in body.lines() {
                    let cmd = raw.trim();
                    if cmd.is_empty() || cmd.starts_with('#') {
                        continue;
                    }
                    let resp = controller.execute(cmd);
                    println!("[script] {cmd} => {resp}");
                }
            }
            Err(e) => eprintln!("[control] script {}: {e}", script.display()),
        }
        if opts.exit_after_script {
            stop.store(true, Ordering::Release);
        }
    }

    // stdin REPL thread (optional).
    if opts.repl {
        let c = controller.clone();
        let stop_repl = stop.clone();
        std::thread::Builder::new()
            .name("control-repl".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                let mut line = String::new();
                loop {
                    line.clear();
                    match stdin.lock().read_line(&mut line) {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let resp = c.execute(&line);
                            println!("{resp}");
                            let _ = std::io::stdout().flush();
                            if stop_repl.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();
    }

    // Accept loop (main thread). Poll the listener + the stop/vmm_done flags.
    loop {
        if stop.load(Ordering::Acquire) {
            println!("[control] stop requested — shutting down");
            break;
        }
        if vmm_done.load(Ordering::Acquire) {
            println!("[control] guest stopped — shutting down");
            break;
        }
        if let Some(l) = &listener {
            match l.accept() {
                Ok((stream, _addr)) => {
                    let c = controller.clone();
                    std::thread::spawn(move || handle_conn(stream, c));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Block in poll() until a connection is actually pending, rather than
                    // sleeping a fixed interval.
                    //
                    // This used to `sleep(30ms)`, which charged EVERY request up to 30 ms of
                    // pure accept latency — capping the control plane near 30 requests/sec
                    // and making a 60fps frame poll impossible no matter how cheap the frames
                    // were. It was invisible in profiling because the cost is in the accept
                    // loop, not the work: a no-op `info` measured the same ~33 ms as a full
                    // Metal readback plus a 3 MB write.
                    //
                    // The timeout only bounds how long shutdown can take to notice the
                    // stop/vmm_done flags; arrivals wake poll() immediately.
                    let mut pfd = libc::pollfd {
                        fd: l.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // Safety: `pfd` is a single valid pollfd for the listener's fd, which
                    // outlives the call (we hold `l`).
                    unsafe {
                        libc::poll(&mut pfd, 1, ACCEPT_POLL_TIMEOUT_MS);
                    }
                }
                Err(e) => {
                    eprintln!("[control] accept error: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let _ = std::fs::remove_file(&socket_path);
}

/// Serve one client connection: each line is a request; each reply is one line. The connection can
/// send multiple commands (persistent) and closes on EOF.
fn handle_conn(stream: UnixStream, controller: Arc<Controller>) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // client closed
            Ok(_) => {
                let resp = controller.execute(&line);
                if writeln!(writer, "{resp}").is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            Err(_) => break,
        }
    }
}

/// One-shot client: connect to `socket`, send `command`, print the single-line response. Used by the
/// `aemulator-hvf tap/swipe/shot/…` client verbs and by any external MCP wrapper.
pub fn client_send(socket: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    writeln!(stream, "{command}")?;
    stream.flush()?;
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(resp.trim_end().to_string())
}

fn parse_ints(s: &str) -> Vec<i64> {
    s.split_whitespace()
        .filter_map(|t| t.parse::<i64>().ok())
        .collect()
}

/// Write a frame so a concurrent reader can NEVER observe a partial one.
///
/// `std::fs::write` opens with `O_TRUNC`: the file becomes 0 bytes, then 3 MB is written into it.
/// Any reader in that window sees a truncated file — which is exactly the failure that made the live
/// view report `short frame: 0 bytes for 1024x768` while the file on disk was a perfectly good
/// 3,145,728 bytes a moment later. The reader wasn't wrong and the writer wasn't wrong; the two just
/// have no ordering between them.
///
/// Writing to a sibling temp file and `rename`-ing it into place fixes it structurally: POSIX
/// `rename` is atomic, so a reader opens either the previous complete frame or the new complete
/// frame, never a half-written one. This holds even with several writers, which the Studio can
/// legitimately have (React re-mounts its Display tab, and each mount starts a poll).
/// FNV-1a over a byte slice — cheap, and good enough as a change detector.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn write_frame_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Keep the temp file in the SAME directory — `rename` is only atomic within a filesystem.
    //
    // The temp name must be unique **per call**, not per process. A pid-only suffix
    // (`live.tmp<pid>`) looked sufficient but was not: the control server runs a thread per
    // connection, so two concurrent writers inside ONE process computed the SAME temp path, and
    //
    //   A: write tmp │ B: write tmp (same file — torn bytes) │ A: rename tmp→dst │ B: rename → ENOENT
    //
    // surfaced in the live view as `ERR write …/live.raw: No such file or directory (os error 2)`.
    // `with_extension` made it worse than it looks: `live.raw` and `live.png` both reduce to
    // `live.tmp<pid>`, so the frame stream and the one-shot screenshot collided even though their
    // DESTINATIONS were deliberately separated — and a screenshot's bytes could be renamed onto the
    // frame path. The counter makes every in-flight write its own file, so concurrent writers can
    // neither see nor steal each other's temp.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);

    // Append rather than replace the extension: `live.raw` → `live.raw.tmp<pid>-<seq>` keeps the
    // destination's identity in the temp name, so a stray temp is attributable.
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}-{seq}", std::process::id()));
    let tmp = path.with_file_name(name);

    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Split a `frame` argument string into `(path, since)`.
///
/// The path is taken VERBATIM (minus an optional trailing `since=<gen>`) because real capture paths
/// contain spaces — every macOS instance dir lives under "Application Support". Tokenizing this was
/// a live bug: the path truncated at the first space, frames were written to
/// `/Users/<user>/Library/Application`, and the viewer read a stale file forever.
fn parse_frame_args(rest: &str) -> (&str, Option<u64>) {
    match rest.rsplit_once(char::is_whitespace) {
        Some((head, last)) if last.starts_with("since=") => {
            (head.trim_end(), last["since=".len()..].parse::<u64>().ok())
        }
        // A bare `since=N` with no path, or no `since=` at all.
        _ => match rest.strip_prefix("since=") {
            Some(v) => ("", v.parse::<u64>().ok()),
            None => (rest, None),
        },
    }
}

fn default_shot_path() -> PathBuf {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/aemulator-shot-{ms}.png"))
}

/// Parse a `key` argument: a bare evdev number, or one of a few named keys. Only codes < 256 are
/// deliverable (the virtio-input keyboard advertises EV_KEY 0..255).
fn parse_key(s: &str) -> Option<u16> {
    if let Ok(n) = s.trim().parse::<u16>() {
        return (n < 256).then_some(n);
    }
    // Android-relevant named keys (evdev KEY_* codes < 256). The modifier + navigation half of this
    // table exists for `keydown`/`keyup`: a live keyboard forwards physical keys by NUMBER, but a
    // script wants `keydown META` / `key LEFT` to be legible.
    Some(match s.trim().to_ascii_uppercase().as_str() {
        "ENTER" => 28,
        "BACK" => 158,              // KEY_BACK
        "HOME" | "HOMEPAGE" => 172, // KEY_HOMEPAGE — Android's Home, NOT KEY_HOME (see MOVEHOME)
        "MENU" => 139,              // KEY_MENU
        "POWER" => 116,             // KEY_POWER
        "VOLUP" | "VOLUMEUP" => 115,
        "VOLDOWN" | "VOLUMEDOWN" => 114,
        "MUTE" => 113,
        "TAB" => 15,
        "ESC" | "ESCAPE" => 1,
        "SPACE" => 57,
        "DEL" | "BACKSPACE" => 14,
        "FORWARDDEL" | "DELETE" => 111, // KEY_DELETE
        "SEARCH" => 217,
        // Modifiers — only meaningful held, i.e. via keydown/keyup.
        "SHIFT" | "LSHIFT" | "SHIFTLEFT" => 42,
        "RSHIFT" | "SHIFTRIGHT" => 54,
        "CTRL" | "LCTRL" | "CTRLLEFT" | "CONTROL" => 29,
        "RCTRL" | "CTRLRIGHT" => 97,
        "ALT" | "LALT" | "ALTLEFT" | "OPTION" => 56,
        "RALT" | "ALTRIGHT" => 100,
        // KEY_LEFTMETA — the Mac Command key. Android reads META as its own modifier
        // (Meta+chord shortcuts); it is NOT a second Ctrl.
        "META" | "LMETA" | "CMD" | "COMMAND" | "SUPER" | "WIN" => 125,
        "RMETA" | "RCMD" => 126,
        "CAPSLOCK" => 58,
        // Navigation.
        "UP" => 103,
        "DOWN" => 108,
        "LEFT" => 105,
        "RIGHT" => 106,
        "MOVEHOME" => 102, // KEY_HOME — line start, not Android Home
        "END" => 107,
        "PAGEUP" => 104,
        "PAGEDOWN" => 109,
        "INSERT" => 110,
        // Function row.
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "F11" => 87,
        "F12" => 88,
        _ => return None,
    })
}

/// Map a character to an evdev `(KEY_* code, needs_shift)`. Positional QWERTY (evdev is by physical
/// key position). Returns `None` for characters we don't have a key for.
fn char_to_key(ch: char) -> Option<(u16, bool)> {
    let lower = ch.to_ascii_lowercase();
    let shift = ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch);
    let code: u16 = match lower {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        '1' | '!' => 2,
        '2' | '@' => 3,
        '3' | '#' => 4,
        '4' | '$' => 5,
        '5' | '%' => 6,
        '6' | '^' => 7,
        '7' | '&' => 8,
        '8' | '*' => 9,
        '9' | '(' => 10,
        '0' | ')' => 11,
        '-' | '_' => 12,
        '=' | '+' => 13,
        '[' | '{' => 26,
        ']' | '}' => 27,
        '\\' | '|' => 43,
        ';' | ':' => 39,
        '\'' | '"' => 40,
        '`' | '~' => 41,
        ',' | '<' => 51,
        '.' | '>' => 52,
        '/' | '?' => 53,
        ' ' => 57,
        '\n' => 28,
        '\t' => 15,
        _ => return None,
    };
    Some((code, shift))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_path_with_spaces_survives_parsing() {
        // The real capture path. Splitting on whitespace truncated this to
        // "/Users/cam/Library/Application", wrote frames there, and left the viewer reading a
        // stale file forever with no error anywhere.
        let p = "/Users/cam/Library/Application Support/com.avnir.aemulator-studio/instances/x/live.raw";
        assert_eq!(parse_frame_args(p), (p, None));
        assert_eq!(parse_frame_args(&format!("{p} since=42")), (p, Some(42)));
    }

    /// The `ERR write …/live.raw: No such file or directory (os error 2)` regression, reproduced
    /// without a VM.
    ///
    /// Two things are asserted, and BOTH failed with a pid-only temp name:
    ///   1. Concurrent writers in ONE process (the control server runs a thread per connection) must
    ///      not share a temp path — sharing means one thread renames the other's temp away, and the
    ///      loser's `rename` fails with ENOENT.
    ///   2. Writers to DIFFERENT destinations that share a stem (`live.raw` / `live.png`) must not
    ///      collide either. `with_extension` reduced both to `live.tmp<pid>`, so the frame stream and
    ///      the one-shot screenshot fought over one temp file — which could also rename a PNG's bytes
    ///      onto the raw-frame path.
    ///
    /// Every reader must additionally observe a COMPLETE file, never a partial one.
    #[test]
    fn concurrent_writers_never_collide_on_the_temp_file() {
        let dir = std::env::temp_dir().join(format!("aemu-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("live.raw");
        let png = dir.join("live.png");

        // Distinct payload sizes so a torn write is detectable by length alone.
        let big = vec![0xABu8; 512 * 1024];
        let small = vec![0xCDu8; 256 * 1024];

        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut handles = Vec::new();
        for t in 0..6 {
            // Half the threads target `live.raw`, half `live.png` — the stem-collision case.
            let (path, payload) = if t % 2 == 0 {
                (raw.clone(), big.clone())
            } else {
                (png.clone(), small.clone())
            };
            let (raw_len, png_len) = (big.len(), small.len());
            let errors = errors.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..40 {
                    if let Err(e) = write_frame_atomic(&path, &payload) {
                        errors
                            .lock()
                            .unwrap()
                            .push(format!("{}: {e}", path.display()));
                    }
                    // Any successful read must see one writer's complete payload, never a mix.
                    if let Ok(got) = std::fs::read(&path) {
                        let expected = if path.extension().unwrap() == "raw" {
                            raw_len
                        } else {
                            png_len
                        };
                        if got.len() != expected {
                            errors.lock().unwrap().push(format!(
                                "{}: torn read {} != {expected}",
                                path.display(),
                                got.len()
                            ));
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let errs = errors.lock().unwrap();
        assert!(
            errs.is_empty(),
            "concurrent writes must not collide, got {} error(s): {:?}",
            errs.len(),
            &errs[..errs.len().min(5)]
        );

        // No temp files may survive a clean run.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv1a_distinguishes_content() {
        assert_eq!(
            fnv1a(b""),
            0xcbf2_9ce4_8422_2325,
            "empty input is the FNV offset basis"
        );
        assert_ne!(fnv1a(b"a"), fnv1a(b"b"));
    }

    #[test]
    fn frame_args_edge_cases() {
        assert_eq!(parse_frame_args("/tmp/a.raw"), ("/tmp/a.raw", None));
        assert_eq!(
            parse_frame_args("/tmp/a.raw since=7"),
            ("/tmp/a.raw", Some(7))
        );
        // A trailing token that merely looks like a flag is part of the path.
        assert_eq!(
            parse_frame_args("/tmp/since=7.raw"),
            ("/tmp/since=7.raw", None)
        );
        // Malformed generation → treated as absent, path still intact.
        assert_eq!(
            parse_frame_args("/tmp/a.raw since=xx"),
            ("/tmp/a.raw", None)
        );
        // No path at all → caller must reject.
        assert_eq!(parse_frame_args("since=3"), ("", Some(3)));
        assert_eq!(parse_frame_args(""), ("", None));
    }

    #[test]
    fn parse_key_numeric_and_named() {
        assert_eq!(parse_key("28"), Some(28));
        assert_eq!(parse_key("ENTER"), Some(28));
        assert_eq!(parse_key("back"), Some(158));
        assert_eq!(parse_key("300"), None); // out of advertised range
        assert_eq!(parse_key("nope"), None);
    }

    #[test]
    fn parse_key_modifiers_and_nav() {
        // The Mac Command key is KEY_LEFTMETA (125) — a distinct modifier from Ctrl (29).
        assert_eq!(parse_key("cmd"), Some(125));
        assert_eq!(parse_key("COMMAND"), Some(125));
        assert_eq!(parse_key("meta"), Some(125));
        assert_eq!(parse_key("rcmd"), Some(126));
        assert_eq!(parse_key("ctrl"), Some(29));
        assert_eq!(parse_key("option"), Some(56));
        assert_eq!(parse_key("shift"), Some(42));
        assert_eq!(parse_key("up"), Some(103));
        assert_eq!(parse_key("f12"), Some(88));
        // Android's Home is KEY_HOMEPAGE; KEY_HOME (line start) is spelled MOVEHOME.
        assert_eq!(parse_key("home"), Some(172));
        assert_eq!(parse_key("movehome"), Some(102));
    }

    #[test]
    fn char_to_key_shift_flags() {
        assert_eq!(char_to_key('a'), Some((30, false)));
        assert_eq!(char_to_key('A'), Some((30, true)));
        assert_eq!(char_to_key('!'), Some((2, true)));
        assert_eq!(char_to_key('1'), Some((2, false)));
        assert_eq!(char_to_key(' '), Some((57, false)));
    }

    #[test]
    fn parse_ints_skips_junk() {
        assert_eq!(parse_ints("10 20 30"), vec![10, 20, 30]);
        assert_eq!(parse_ints("  5   x  9 "), vec![5, 9]);
    }
}
