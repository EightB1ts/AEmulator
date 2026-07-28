//! `aemu-display-selftest` — Phase 3b proof (built only under `--features gfxstream`).
//!
//! Two modes:
//!
//!   * **metal** (default) — open a winit window, attach a CAMetalLayer to its NSView, and present a
//!     host-generated test pattern (gradient + checker) into the layer's drawable via the Metal
//!     blit → `presentDrawable` path. Then `screencapture -x` the window region to a PNG. This
//!     proves the exact CAMetalLayer presentation path our gfxstream frames will use — in isolation,
//!     no guest and no Vulkan driver needed (this mode talks to Metal directly). This is the
//!     load-bearing 3b proof.
//!
//!   * **--gfxstream** — additionally bring up the gfxstream host renderer windowed:
//!     `stream_renderer_init(USE_VK_BIT[|USE_GLES_BIT], WIN0=w×h, fence_cb)` (NOT surfaceless), then
//!     `gfxstream_backend_setup_window(NSWindow, …)`. With no guest posting a ColorBuffer there is
//!     nothing for gfxstream to scan out, so this proves the *wiring* (init + window hand-off
//!     succeed on KosmicKrisp/Metal without crashing), not a rendered guest frame. Needs the Vulkan
//!     loader staged + `VK_DRIVER_FILES` pointed at the KosmicKrisp ICD — see
//!     `run-display-selftest.sh`.
//!
//! Usage:
//!   aemu-display-selftest [--gfxstream] [--gles] [--out <png>] [--width N] [--height N]

use std::process::Command;
use std::time::{Duration, Instant};

use aemulator_hvf::display::metal;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::WindowBuilder;

fn main() {
    let mut width: u32 = 640;
    let mut height: u32 = 480;
    let mut out = String::from("/tmp/aemu-display-selftest.png");
    let mut use_gfxstream = false;
    let mut use_gles = false;
    // gfxstream_backend_setup_window() is KNOWN to abort on macOS with this renderer build
    // (host/vulkan/DisplaySurfaceVk.cpp is `#ifdef _WIN32 … #else abort("Unimplemented")`, and the
    // GLES display path isn't compiled under GuestVulkanOnly). It is therefore OFF by default; pass
    // --setup-window to empirically reproduce the abort.
    let mut call_setup_window = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--gfxstream" => use_gfxstream = true,
            "--gles" => use_gles = true,
            "--setup-window" => call_setup_window = true,
            "--out" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    out = v.clone();
                }
            }
            "--width" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|v| v.parse().ok()) {
                    width = v;
                }
            }
            "--height" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|v| v.parse().ok()) {
                    height = v;
                }
            }
            other => eprintln!("[selftest] ignoring unknown arg: {other}"),
        }
        i += 1;
    }

    println!(
        "[selftest] mode={} gles={use_gles} size={width}x{height} out={out}",
        if use_gfxstream { "gfxstream" } else { "metal" }
    );

    let event_loop = EventLoop::new().expect("event loop");
    let window = WindowBuilder::new()
        .with_title("aemulator-hvf — Phase 3b CAMetalLayer presenter")
        .with_inner_size(LogicalSize::new(width as f64, height as f64))
        .with_position(PhysicalPosition::new(120, 120))
        .build(&event_loop)
        .expect("window");

    // Bring the app + window forward so the screenshot captures it un-occluded.
    window.focus_window();

    let mut gfxstream_ready = false;
    let mut gfxstream_init_rc: i32 = i32::MIN;
    let mut presented = 0u32;
    let start = Instant::now();
    let mut shot_taken = false;

    let _ = event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));

        match event {
            Event::Resumed => {
                // One-time bring-up on the main thread once the window is live.
                if use_gfxstream && !gfxstream_ready {
                    gfxstream_ready = true;
                    println!("[selftest] stream_renderer_init (VK{})…", if use_gles { "+GLES" } else { "" });
                    gfxstream_init_rc = metal::init_gfxstream(width as i32, height as i32, use_gles);
                    println!("[selftest] stream_renderer_init returned {gfxstream_init_rc}");
                    if gfxstream_init_rc == 0 {
                        let attached = metal::attach_layer(&window, width, height);
                        println!("[selftest] CAMetalLayer attached to winit NSView={attached}; gfxstream host renderer live in-process");
                        if call_setup_window {
                            eprintln!("[selftest] --setup-window: calling gfxstream_backend_setup_window (expected to abort on macOS: DisplaySurfaceVk is Win32-only)…");
                            let window_ok = metal::setup_gfxstream_window(&window, width as i32, height as i32);
                            println!("[selftest] gfxstream_backend_setup_window resolved NSWindow={window_ok} (if you see this, it did NOT abort)");
                        }
                    } else {
                        eprintln!("[selftest] gfxstream init failed — skipping setup_window; continuing with the pure-Metal presenter proof");
                    }
                }
                window.request_redraw();
            }

            Event::WindowEvent { event: WindowEvent::RedrawRequested, .. } => {
                // Always drive the Metal presenter: it proves the CAMetalLayer path on screen. In
                // gfxstream mode this also demonstrates the layer is live alongside gfxstream's own
                // subview (there is no guest frame to show).
                let rc = metal::present_test_pattern(&window, width, height);
                if presented == 0 {
                    println!(
                        "[selftest] first present rc={rc}; CAMetalLayer attached={}",
                        metal::metal_layer_attached(&window)
                    );
                }
                presented += 1;
            }

            Event::AboutToWait => {
                let elapsed = start.elapsed();
                if !shot_taken && presented >= 3 && elapsed >= Duration::from_millis(900) {
                    shot_taken = true;
                    capture_window(&window, &out);
                    if use_gfxstream && gfxstream_init_rc == 0 {
                        metal::teardown_gfxstream();
                        println!("[selftest] gfxstream teardown complete");
                    }
                    println!("[selftest] DONE — screenshot: {out}");
                    elwt.exit();
                } else {
                    window.request_redraw();
                }
            }

            _ => {}
        }
    });
}

/// `screencapture -x -R<x,y,w,h>` the window's on-screen region (global display points) to `out`.
/// Falls back to a full-screen capture if the region can't be resolved.
fn capture_window(window: &winit::window::Window, out: &str) {
    let scale = window.scale_factor().max(1.0);
    let region = window.outer_position().ok().map(|pos| {
        let size = window.outer_size();
        let x = (pos.x as f64 / scale).round() as i64;
        let y = (pos.y as f64 / scale).round() as i64;
        let w = (size.width as f64 / scale).round() as i64;
        let h = (size.height as f64 / scale).round() as i64;
        format!("{x},{y},{w},{h}")
    });

    // Small settle so the presented frame is definitely on the glass before the grab.
    std::thread::sleep(Duration::from_millis(150));

    let status = if let Some(r) = region.as_deref() {
        println!("[selftest] screencapture -x -R{r} {out}");
        Command::new("screencapture")
            .args(["-x", &format!("-R{r}"), out])
            .status()
    } else {
        println!("[selftest] screencapture -x {out} (full screen — no window region)");
        Command::new("screencapture").args(["-x", out]).status()
    };

    match status {
        Ok(s) if s.success() => println!("[selftest] screenshot written: {out}"),
        Ok(s) => eprintln!("[selftest] screencapture exited {s}"),
        Err(e) => eprintln!("[selftest] screencapture failed to spawn: {e}"),
    }
}
