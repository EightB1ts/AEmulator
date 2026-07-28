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
//!   * **--init-teardown-cycles N** — headless: no window, no event loop, no Metal. Runs
//!     `stream_renderer_init` → `stream_renderer_teardown` N times and exits non-zero if any cycle
//!     fails. This is the guard for the shutdown path (plan items 1.2, 1.3, 1.4):
//!     `stream_renderer_teardown()` runs `~FrameBuffer()`, which runs any surviving
//!     process-cleanup callback, which unboxes an already-deleted `VkInstance` box — producing a
//!     `Failed to unbox` log and then handing `VK_NULL_HANDLE` to `vkDestroyInstanceImpl`, whose
//!     null path used to reach `node_type::key()` on an empty node and null-deref. So the *pass*
//!     condition is both a zero exit AND no `Failed to unbox` on stderr; `ci/hvf-doctor.sh`
//!     enforces both. N ≥ 2 is what proves the second init sees a genuinely clean host.
//!     Needs the Vulkan loader + KosmicKrisp ICD staged, exactly like `--gfxstream`.
//!
//!   * **--exit-without-teardown** — headless: `stream_renderer_init` and then return, WITHOUT
//!     calling `stream_renderer_teardown()`. The renderer is deliberately left running so that
//!     `exit()` drives gfxstream's own C++ static destructors, and the pass condition is simply a
//!     clean exit. See [`exit_without_teardown`] for why that is the single sharpest check in this
//!     binary: it is the only mode that reproduces the real `--control` shutdown crash, and it does
//!     so in about a second with no guest, no window and no VM.
//!
//! Usage:
//!   aemu-display-selftest [--gfxstream] [--gles] [--out <png>] [--width N] [--height N]
//!   aemu-display-selftest --init-teardown-cycles N [--gles] [--width N] [--height N]
//!   aemu-display-selftest --exit-without-teardown [--gles] [--width N] [--height N]

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
    let mut cycles: u32 = 0;
    let mut exit_without_teardown = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--gfxstream" => use_gfxstream = true,
            "--gles" => use_gles = true,
            "--setup-window" => call_setup_window = true,
            "--exit-without-teardown" => exit_without_teardown = true,
            "--init-teardown-cycles" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) if n > 0 => cycles = n,
                    _ => {
                        eprintln!("[selftest] --init-teardown-cycles needs a positive integer");
                        std::process::exit(2);
                    }
                }
            }
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

    if exit_without_teardown {
        std::process::exit(exit_without_teardown_check(
            width as i32,
            height as i32,
            use_gles,
        ));
    }

    if cycles > 0 {
        std::process::exit(init_teardown_cycles(
            cycles,
            width as i32,
            height as i32,
            use_gles,
        ));
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

/// Bring the gfxstream host renderer up and down `cycles` times, with no window and no event loop.
/// Returns the process exit code (0 = every cycle succeeded).
///
/// Deliberately headless. The failure this guards is on the **shutdown** path
/// (`stream_renderer_teardown` → `~FrameBuffer()` → a surviving process-cleanup callback →
/// `unbox_VkInstance` of a deleted box → `VK_NULL_HANDLE` into `vkDestroyInstanceImpl`), and none of
/// it needs a CAMetalLayer, a drawable or a guest frame. Keeping it window-free means it runs under
/// `ci/hvf-doctor.sh` on a machine with no session attached and cannot be confused by AppKit.
///
/// The pass condition has a second half this function cannot check: `Failed to unbox` must not
/// appear on stderr. gfxstream logs it and carries on, so it costs nothing here but is the exact
/// signature of the surviving-callback bug. `hvf-doctor` greps for it.
fn init_teardown_cycles(cycles: u32, w: i32, h: i32, use_gles: bool) -> i32 {
    println!("[selftest] init/teardown cycles={cycles} size={w}x{h} gles={use_gles}");
    for n in 1..=cycles {
        let rc = metal::init_gfxstream(w, h, use_gles);
        println!("[selftest] cycle {n}/{cycles}: stream_renderer_init returned {rc}");
        if rc != 0 {
            eprintln!(
                "[selftest] FAIL: cycle {n}/{cycles} stream_renderer_init returned {rc} — the \
                 Vulkan loader / KosmicKrisp ICD are probably not staged (see \
                 run-display-selftest.sh)"
            );
            return 1;
        }
        metal::teardown_gfxstream();
        println!("[selftest] cycle {n}/{cycles}: stream_renderer_teardown complete");
        // A second teardown must be a no-op, not a second trip through the FFI. Cheap to assert
        // here and it is the only place the idempotence wrapper is exercised against a renderer
        // that really did initialize.
        metal::teardown_gfxstream();
    }
    println!("[selftest] DONE — {cycles} init/teardown cycle(s), no crash");
    0
}

/// Bring the host renderer up and then **exit with it still running** — no
/// `stream_renderer_teardown()`. Returns 0 if `stream_renderer_init` succeeded; the check that
/// matters happens *after* this returns, inside `exit()`.
///
/// WHY THIS EXISTS, and why it is a different test from `--init-teardown-cycles`.
///
/// `init_teardown_cycles` above only ever exercises the ORDERED shutdown, and it therefore cannot
/// see the failure mode that actually crashed a real `--control` Android boot: nobody called
/// `stream_renderer_teardown()` at all, and libc's `exit()` ran the gfxstream dylib's C++ static
/// destructors instead. Those are not an equivalent teardown — they run in reverse
/// initializer order, which for `libgfxstream_backend.dylib` is:
///
///   `__TEXT,__init_offsets` idx  3 = `opengles.cpp`             (`static RendererPtr sRenderer`)
///   `__TEXT,__init_offsets` idx 14 = `MetalLayerPresenterVk.cpp` (`static std::mutex sPresentersMutex`)
///
/// `__cxa_atexit` runs LIFO, so idx 14 is destroyed FIRST. Destroying `sRenderer` (idx 3) then
/// drives `~RendererImpl` → `~RenderWindow` → `CMD_FINALIZE` → `FrameBuffer::finalize()` →
/// `~FrameBuffer()` → `vk::teardownGlobalVkEmulation()` → `teardownMetalLayerPresenters()`, which
/// locks `sPresentersMutex` — a `std::mutex` whose `pthread_mutex_destroy` has already run. Apple's
/// pthread returns `EINVAL` for that, libc++'s `std::mutex::lock()` turns a non-zero return into
/// `throw std::system_error(ec, "mutex lock failed")`, nothing catches it, and `libc++abi` aborts
/// the process. Exit status 134, at the very end of a successful boot.
///
/// So: this mode needs no guest, no `VkInstance`, no window and no VM, and it reproduces that abort
/// in about a second. It stays valuable even now that the VMM always reaches the ordered teardown,
/// because "the process exited without tearing the renderer down" is reachable from every panic,
/// error return and signal path there is — this asserts it is survivable rather than assuming no
/// caller will ever do it.
fn exit_without_teardown_check(w: i32, h: i32, use_gles: bool) -> i32 {
    println!("[selftest] exit-without-teardown size={w}x{h} gles={use_gles}");
    let rc = metal::init_gfxstream(w, h, use_gles);
    println!("[selftest] stream_renderer_init returned {rc}");
    if rc != 0 {
        eprintln!(
            "[selftest] FAIL: stream_renderer_init returned {rc} — the Vulkan loader / KosmicKrisp \
             ICD are probably not staged (see run-display-selftest.sh)"
        );
        return 1;
    }
    // Deliberately NO teardown_gfxstream() here. Returning from here reaches `exit()` with the
    // renderer still up, which is the whole point: the pass condition is that the process exits 0
    // rather than aborting inside the dylib's static destructors.
    println!("[selftest] DONE — exiting with the renderer still up (no stream_renderer_teardown)");
    0
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
