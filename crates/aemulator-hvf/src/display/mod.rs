//! The `--display` window layer: a real macOS window that presents the guest's virtio-gpu 2D
//! scanout and forwards keyboard + pointer input back into the guest's virtio-input device.
//!
//! **macOS main-thread rule.** AppKit requires the window + event loop on the process main thread.
//! So the VMM (boot vCPU + secondaries + input-inject thread) runs on spawned threads while THIS
//! module owns the main thread inside [`run_event_loop`]. The two sides meet at a [`DisplayBridge`]
//! (built in `machine.rs`, handed to the main thread over a channel):
//!   * **VMM → UI**: the virtio-gpu scanout [`ScanoutHandle`] — an `Arc<Mutex<Framebuffer>>` plus a
//!     generation counter bumped on every `RESOURCE_FLUSH`. Each redraw copies the framebuffer
//!     (BGRA/XRGB → softbuffer `0RGB`) into the window surface and presents it.
//!   * **UI → VMM**: winit keyboard / pointer events are translated to Linux evdev codes and pushed
//!     over an `mpsc::Sender` frame channel; the VMM's input-inject thread delivers them to the
//!     virtio-input eventq and raises the device SPI.
//!
//! The softbuffer blit is a plain CPU path (nearest-neighbour scaled to the window), which is all a
//! 2D dumb-framebuffer scanout needs. **gfxstream 3D frames never take it**: this thread publishes
//! the window's `CAMetalLayer*` onto the scanout handle once at startup, and from then on the host
//! renderer presents into that layer itself from the vCPU thread (`vkCreateMetalSurfaceEXT` +
//! `VK_KHR_swapchain` + `vkCmdBlitImage` + `vkQueuePresentKHR`), so `RedrawRequested` has nothing to
//! do. After handover this thread owes the layer exactly two things: a `drawableSize` re-sync on
//! resize (which is what makes the renderer rebuild its swapchain), and an unpublish on the way out,
//! since the layer dies with the window while the vCPU thread is still reading the pointer — see
//! [`retire_metal_layer`].

/// CAMetalLayer ownership + the gfxstream hand-off: the layer this window hands the host renderer
/// to present into, plus `stream_renderer_init` / `gfxstream_backend_setup_window`.
pub mod metal;

use crate::machine::DisplayBridge;
use crate::virtio::input::{ABS_MAX, ABS_X, ABS_Y, BTN_LEFT, BTN_TOUCH, EV_ABS, EV_KEY};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Event, MouseButton, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowBuilder};

/// The concrete softbuffer surface type for our window (both display + window handle are the `Rc`).
type WindowSurface = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// Target redraw cadence (~60fps). We only actually re-blit when the scanout generation changed.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Own the main thread: open a window sized to the guest scanout, present its frames, and forward
/// input into the guest. Returns when the window is closed or the VMM signals it has stopped.
pub fn run_event_loop(bridge: DisplayBridge) {
    let DisplayBridge {
        scanout,
        input_tx,
        console_tx: _, // control-plane only; the window feeds the guest console from stdin
        width,
        height,
        vmm_done,
    } = bridge;

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("[display] failed to create event loop: {e}");
            return;
        }
    };

    let window = match WindowBuilder::new()
        .with_title("aemulator-hvf — guest display")
        .with_inner_size(LogicalSize::new(width as f64, height as f64))
        // Pin to a visible spot on the primary display; winit's default placement can land the
        // window off-screen (top/right) when the process is launched outside a normal GUI session.
        .with_position(LogicalPosition::new(80.0, 80.0))
        .build(&event_loop)
    {
        Ok(w) => Rc::new(w),
        Err(e) => {
            eprintln!("[display] failed to create window: {e}");
            return;
        }
    };

    // softbuffer context + surface (both keyed on the window handle). `context` is kept alive on
    // this stack frame for the whole `run` call; the surface owns its own display handle copy.
    let context = match softbuffer::Context::new(window.clone()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display] softbuffer context: {e}");
            return;
        }
    };
    let mut surface = match softbuffer::Surface::new(&context, window.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[display] softbuffer surface: {e}");
            return;
        }
    };

    println!("[display] window open: {width}x{height} — presenting guest scanout");

    // In-Vulkan present hand-off: create the CAMetalLayer now and publish its pointer so the vCPU
    // thread's `flush_3d` can hand it to gfxstream, which creates a VkSurfaceKHR on it
    // (VK_EXT_metal_surface), owns a VK_KHR_swapchain, and blits each scanout ColorBuffer in with
    // vkCmdBlitImage. Publishing once here is the whole protocol; this thread never touches the
    // layer again except to re-sync its drawableSize on resize.
    //
    // `AEMU_PRESENT_READBACK=1` deliberately publishes nothing: with `metal_layer == 0`, `flush_3d`
    // keeps filling the CPU framebuffer and this thread presents it through softbuffer below —
    // exactly the legacy readback semantics the flag has always had.
    if !crate::virtio::gpu::present_readback_forced() {
        let size = window.inner_size();
        let layer = metal::layer_for_window(&window, size.width, size.height);
        if layer != 0 {
            scanout.metal_layer.store(layer, Ordering::Release);
            println!(
                "[display] CAMetalLayer {layer:#x} published for in-Vulkan present ({}x{} px)",
                size.width, size.height
            );
        } else {
            eprintln!("[display] no CAMetalLayer available — falling back to CPU readback present");
        }
    } else {
        println!("[display] AEMU_PRESENT_READBACK=1 — CPU readback present (no CAMetalLayer)");
    }

    // Mirrors the Metal sublayer's `hidden` state so we only cross the FFI on an actual transition,
    // not once per redraw. Starts false: the layer is created hidden.
    let mut metal_layer_visible = false;

    // One-shot guard for the `AEMU_FB_DUMP`-has-no-source notice below (the diagnostic tick fires
    // once a second and the condition is steady-state, so it must not repeat).
    let mut fb_dump_unavailable_logged = false;

    // Latest pointer position in window (physical) pixels, for BTN events (which don't carry pos).
    let mut cursor_x = 0f64;
    let mut cursor_y = 0f64;
    let mut last_gen = u64::MAX; // force an initial paint
    let mut next_frame = Instant::now();
    let mut next_diag = Instant::now();

    // Optional deterministic input self-test: when `AEMU_INPUT_SELFTEST` is set, a background thread
    // pushes a scripted key sequence ("avnir" + Enter) through the SAME `input_tx` channel a real
    // winit keystroke uses — proving the window→virtio-input→guest evdev path without depending on
    // macOS delivering synthetic events to the window (which needs Accessibility permission the
    // headless test context lacks). A real physical keystroke traverses the identical path below.
    if std::env::var_os("AEMU_INPUT_SELFTEST").is_some() {
        let tx = input_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(18));
            // Linux KEY_ codes: a=30 v=47 n=49 i=23 r=19 ENTER=28.
            for code in [30u16, 47, 49, 23, 19, 28] {
                let _ = tx.send(vec![(EV_KEY, code, 1)]);
                std::thread::sleep(Duration::from_millis(40));
                let _ = tx.send(vec![(EV_KEY, code, 0)]);
                std::thread::sleep(Duration::from_millis(120));
            }
            eprintln!("[display] input self-test: injected key sequence a,v,n,i,r,ENTER");
        });
    }

    // Optional scripted swipe-up to dismiss the (insecure, swipe-only) lock screen after boot, so a
    // headless run can reach the HOME launcher. `AEMU_SWIPE_UNLOCK` = start delay in seconds (default
    // 80). Injects several bottom→top touch drags through the SAME `input_tx` a real pointer uses
    // (virtio-input absolute touchscreen: ABS_X/ABS_Y + BTN_TOUCH), spaced a few seconds apart so it
    // lands once the keyguard is showing. Android-diagnostic only.
    if let Some(val) = std::env::var_os("AEMU_SWIPE_UNLOCK") {
        let delay: u64 = val.to_str().and_then(|s| s.parse().ok()).unwrap_or(80);
        let tx = input_tx.clone();
        std::thread::spawn(move || {
            let cx = (ABS_MAX as i32) / 2;
            // A single well-formed DIRECT-touchscreen vertical drag from (x0,y0)→(x1,y1): touch-down,
            // `steps` interpolated moves, touch-up. Each `send` is one evdev frame (the device appends
            // an EV_SYN), so InputReader sees a coherent down→move→up gesture.
            let drag = |y0: i32, y1: i32, steps: i32, step_ms: u64| {
                let _ = tx.send(vec![
                    (EV_ABS, ABS_X, cx),
                    (EV_ABS, ABS_Y, y0),
                    (EV_KEY, BTN_TOUCH, 1),
                ]);
                std::thread::sleep(Duration::from_millis(step_ms));
                for step in 1..=steps {
                    let y = y0 + (y1 - y0) * step / steps;
                    let _ = tx.send(vec![(EV_ABS, ABS_X, cx), (EV_ABS, ABS_Y, y)]);
                    std::thread::sleep(Duration::from_millis(step_ms));
                }
                let _ = tx.send(vec![(EV_KEY, BTN_TOUCH, 0)]);
            };

            std::thread::sleep(Duration::from_secs(delay));
            let y_bottom = (ABS_MAX as i32) * 90 / 100;
            let y_top = (ABS_MAX as i32) * 8 / 100;
            // Phase 1 — dismiss the swipe keyguard: two bottom→top drags, spaced, to catch it once the
            // keyguard is showing.
            for attempt in 0..3 {
                drag(y_bottom, y_top, 16, 14);
                eprintln!("[display] swipe-unlock: keyguard drag {}", attempt + 1);
                std::thread::sleep(Duration::from_secs(3));
            }
            // Phase 2 — prove touch on the running launcher: swipe up from the hotseat to open the
            // all-apps drawer (a visible UI change distinct from the home screen).
            std::thread::sleep(Duration::from_secs(4));
            drag(
                (ABS_MAX as i32) * 97 / 100,
                (ABS_MAX as i32) * 20 / 100,
                20,
                12,
            );
            eprintln!("[display] touch-prove: app-drawer swipe-up injected");
        });
    }

    let send_frame = |frame: Vec<(u16, u16, i32)>| {
        // A dead receiver (VMM stopped) just means the guest is gone; drop the event silently.
        let _ = input_tx.send(frame);
    };

    let _ = event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::WaitUntil(next_frame));
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    // Unpublish before the window starts going away — see `retire_metal_layer`. The
                    // guest is usually still running at this point, so this is the one exit route
                    // where the vCPU thread can genuinely still be flushing frames.
                    retire_metal_layer(&scanout);
                    elwt.exit();
                }

                WindowEvent::RedrawRequested => {
                    // Gate on whether a frame was actually PRESENTED, not on whether a layer exists.
                    // A layer exists for the whole of any windowed run, but a 2D-only run
                    // (`--display` without `--gpu3d`) has no renderer, a run whose
                    // `stream_renderer_init` failed falls back to the 2D device, and even a healthy
                    // `--gpu3d` run paints its boot console through 2D scanout long before the first
                    // 3D flush. Gating on the layer blanked the window in all of those.
                    //
                    // Once gfxstream has put a frame on the drawable there is nothing for this
                    // thread to do, so reveal the (initially hidden) Metal sublayer and stop
                    // blitting; a hard present failure clears the flag and hands the window back.
                    let presented = scanout.presented.load(Ordering::Acquire);
                    if presented != metal_layer_visible {
                        metal::set_layer_visible(scanout.metal_layer.load(Ordering::Acquire), presented);
                        metal_layer_visible = presented;
                    }
                    if !presented {
                        present(&mut surface, &scanout, &window);
                    }
                }

                WindowEvent::KeyboardInput { event: key, .. } => {
                    if let PhysicalKey::Code(code) = key.physical_key {
                        if let Some(linux_code) = keycode_to_linux(code) {
                            let value = match key.state {
                                ElementState::Pressed => {
                                    if key.repeat {
                                        2
                                    } else {
                                        1
                                    }
                                }
                                ElementState::Released => 0,
                            };
                            send_frame(vec![(EV_KEY, linux_code, value)]);
                        }
                    }
                }

                WindowEvent::CursorMoved { position, .. } => {
                    cursor_x = position.x;
                    cursor_y = position.y;
                    let size = window.inner_size();
                    let (ax, ay) = scale_abs(cursor_x, cursor_y, size.width, size.height);
                    send_frame(vec![(EV_ABS, ABS_X, ax), (EV_ABS, ABS_Y, ay)]);
                }

                WindowEvent::MouseInput { state, button, .. } => {
                    if button == MouseButton::Left {
                        let value = matches!(state, ElementState::Pressed) as i32;
                        let size = window.inner_size();
                        let (ax, ay) = scale_abs(cursor_x, cursor_y, size.width, size.height);
                        // Re-assert position with the button so the guest gets a coherent click.
                        send_frame(vec![
                            (EV_ABS, ABS_X, ax),
                            (EV_ABS, ABS_Y, ay),
                            (EV_KEY, BTN_LEFT, value),
                            (EV_KEY, BTN_TOUCH, value),
                        ]);
                    }
                }

                WindowEvent::Resized(size) => {
                    // Re-sync the layer's drawableSize to the new physical size. gfxstream's next
                    // vkAcquireNextImageKHR/vkQueuePresentKHR then reports VK_ERROR_OUT_OF_DATE_KHR
                    // (or VK_SUBOPTIMAL_KHR) and it rebuilds its swapchain. `layer_for_window` is
                    // idempotent and returns the same pointer, so nothing needs republishing.
                    if scanout.metal_layer.load(Ordering::Acquire) != 0 {
                        let _ = metal::layer_for_window(&window, size.width, size.height);
                    }
                    window.request_redraw();
                }

                WindowEvent::ScaleFactorChanged { .. } => {
                    if scanout.metal_layer.load(Ordering::Acquire) != 0 {
                        let size = window.inner_size();
                        let _ = metal::layer_for_window(&window, size.width, size.height);
                    }
                    window.request_redraw();
                }
                _ => {}
            },

            Event::AboutToWait => {
                if vmm_done.load(Ordering::Acquire) {
                    println!("[display] VMM stopped — closing window");
                    elwt.exit();
                    return;
                }
                let now = Instant::now();
                if now >= next_frame {
                    next_frame = now + FRAME_INTERVAL;
                    let gen = scanout.generation();
                    if gen != last_gen {
                        last_gen = gen;
                        window.request_redraw();
                    }
                }
                if now >= next_diag {
                    next_diag = now + Duration::from_secs(1);
                    let gen = scanout.generation();
                    // Report the ACTIVE present mode rather than an unconditional byte count. Under
                    // in-Vulkan present the ColorBuffer goes GPU-side straight into the swapchain and
                    // `flush_3d` deliberately never writes the CPU framebuffer, so counting its bytes
                    // yields a permanent `non_zero_fb_bytes=0` that reads as "the display is black"
                    // while the window is demonstrably rendering — the exact false alarm
                    // `control::Controller::fb_hash` warns in-tree not to reintroduce.
                    //
                    // `presented` is the same predicate that gates the softbuffer blit, so the count
                    // is printed on precisely the ticks where something is filling that buffer.
                    let in_vulkan = scanout.presented.load(Ordering::Acquire);
                    if in_vulkan {
                        eprintln!(
                            "[display] scanout gen={gen} present=in-vulkan (CPU framebuffer unwritten by design)"
                        );
                    } else {
                        let nz = scanout.fb.lock().unwrap().non_zero_bytes();
                        eprintln!("[display] scanout gen={gen} present=cpu non_zero_fb_bytes={nz}");
                    }
                    // Env-gated headless framebuffer capture: when `AEMU_FB_DUMP=<path.ppm>` is set,
                    // write the current guest scanout as a binary PPM (P6) every diagnostic tick,
                    // overwriting so the file always holds the latest frame. This captures the guest's
                    // OWN pixels directly (independent of the host screen being asleep/locked, which
                    // makes `screencapture` return black), so a booted launcher can be screenshot on a
                    // headless/remote session. Android-diagnostic only; adds no deps (std::fs), so the
                    // default build stays hermetic.
                    //
                    // Readback mode only, for the same reason: writing the file under in-Vulkan
                    // present would emit an all-black PPM every second and read as a dead guest.
                    // Refuse and say why once, naming the flag that restores a CPU-side frame source.
                    if let Some(path) = std::env::var_os("AEMU_FB_DUMP") {
                        if in_vulkan {
                            if !fb_dump_unavailable_logged {
                                fb_dump_unavailable_logged = true;
                                eprintln!(
                                    "[display] AEMU_FB_DUMP has no pixel source under in-Vulkan present \
                                     (guest frame goes ColorBuffer -> swapchain, never through the CPU \
                                     framebuffer) — not writing {}. Re-run with AEMU_PRESENT_READBACK=1 \
                                     to force the readback path, which costs a full host transfer per \
                                     guest frame.",
                                    path.to_string_lossy()
                                );
                            }
                        } else {
                            let fb = scanout.fb.lock().unwrap();
                            let (w, h) = (fb.width.max(1), fb.height.max(1));
                            let mut out = Vec::with_capacity(15 + (w * h * 3) as usize);
                            out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
                            for i in 0..(w * h) as usize {
                                let idx = i * 4;
                                if idx + 2 < fb.bytes.len() {
                                    // Guest fb is BGRA little-endian: byte0=B byte1=G byte2=R.
                                    out.push(fb.bytes[idx + 2]); // R
                                    out.push(fb.bytes[idx + 1]); // G
                                    out.push(fb.bytes[idx]); // B
                                } else {
                                    out.extend_from_slice(&[0, 0, 0]);
                                }
                            }
                            drop(fb);
                            let _ = std::fs::write(&path, &out);
                        }
                    }
                }
                elwt.set_control_flow(ControlFlow::WaitUntil(next_frame));
            }

            // Guaranteed-last event, so it catches every exit route (including ones that never send
            // `CloseRequested`, e.g. Cmd-Q / app termination). Idempotent with the calls above.
            Event::LoopExiting => retire_metal_layer(&scanout),
            _ => {}
        }
    });
}

/// Unpublish the `CAMetalLayer*` handed to the host renderer, before the window that owns it dies.
///
/// The layer is a sublayer of the window's `NSView` and is released with it — and the window is
/// released as soon as `run` returns and this closure is dropped. But `metal_layer` is read on the
/// **vCPU thread** by `flush_3d` on every 3D flush, so a stale non-zero pointer left published means
/// the next flush hands a freed `CALayer` to `stream_renderer_present_to_metal_layer`. Storing 0
/// routes `flush_3d` down its readback branch instead, which is safe with no window (it is the same
/// branch headless `--control` runs on permanently).
///
/// This narrows the hazard to the interval between `flush_3d`'s Acquire load of `metal_layer` and
/// its present call; it does not close it. Closing it needs the present itself serialized against
/// teardown, i.e. a lock on the vCPU hot path, which is not worth paying on every frame to harden a
/// shutdown that is followed by process exit within milliseconds.
///
/// `presented` is cleared for the same reason it exists: it means "an in-Vulkan frame is on the
/// drawable". Once the drawable is gone that is no longer true, and leaving it set would tell any
/// future reader to keep skipping the CPU blit.
///
/// **Unpublishing is not enough on its own.** The host renderer caches a `VkSurfaceKHR` +
/// `VK_KHR_swapchain` per layer pointer, and that cache outlives our `store(0)`: it is otherwise
/// only emptied by `stream_renderer_teardown()`, by which point the `CAMetalLayer` has been freed
/// with its `NSView` and destroying the surface is a use-after-free inside Metal. So we hand the
/// pointer to [`metal::release_metal_layer`] first, while the layer is still alive and the
/// `VkDevice` still is too. The `swap` (rather than `store`) makes that exactly-once: this function
/// runs on both `CloseRequested` and the guaranteed-last `LoopExiting`, and the second call sees 0.
fn retire_metal_layer(scanout: &crate::virtio::ScanoutHandle) {
    let layer = scanout.metal_layer.swap(0, Ordering::AcqRel);
    scanout.presented.store(false, Ordering::Release);
    metal::release_metal_layer(layer);
}

/// Blit the current scanout framebuffer into the window surface and present it. Nearest-neighbour
/// scales the fixed guest resolution to the window's physical size (handles retina + user resize).
fn present(surface: &mut WindowSurface, scanout: &crate::virtio::ScanoutHandle, window: &Window) {
    let size = window.inner_size();
    let (Some(sw), Some(sh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
        return; // minimized / zero-sized
    };
    if surface.resize(sw, sh).is_err() {
        return;
    }
    let mut buf = match surface.buffer_mut() {
        Ok(b) => b,
        Err(_) => return,
    };
    let (sw, sh) = (sw.get(), sh.get());

    let fb = scanout.fb.lock().unwrap();
    let gw = fb.width.max(1);
    let gh = fb.height.max(1);
    for y in 0..sh {
        let gy = (y * gh / sh).min(gh - 1);
        let row = (gy * gw) as usize * 4;
        let out_row = (y * sw) as usize;
        for x in 0..sw {
            let gx = (x * gw / sw).min(gw - 1);
            let idx = row + gx as usize * 4;
            // Guest fb is BGRA/XRGB little-endian: byte0=B byte1=G byte2=R. softbuffer wants 0RGB.
            let px = if idx + 2 < fb.bytes.len() {
                let b = fb.bytes[idx] as u32;
                let g = fb.bytes[idx + 1] as u32;
                let r = fb.bytes[idx + 2] as u32;
                (r << 16) | (g << 8) | b
            } else {
                0
            };
            buf[out_row + x as usize] = px;
        }
    }
    drop(fb);
    let _ = buf.present();
}

/// Map a window (physical) pixel coordinate into the virtio-input absolute range `0..=ABS_MAX`.
fn scale_abs(x: f64, y: f64, w: u32, h: u32) -> (i32, i32) {
    let w = w.max(1) as f64;
    let h = h.max(1) as f64;
    let ax = (x.clamp(0.0, w - 1.0) / (w - 1.0).max(1.0) * ABS_MAX as f64).round() as i32;
    let ay = (y.clamp(0.0, h - 1.0) / (h - 1.0).max(1.0) * ABS_MAX as f64).round() as i32;
    (ax.clamp(0, ABS_MAX as i32), ay.clamp(0, ABS_MAX as i32))
}

/// Translate a winit physical `KeyCode` to a Linux `input-event-codes.h` `KEY_*` scancode. Covers
/// the main typing block (letters/digits/punctuation), modifiers, navigation, and function keys —
/// enough to drive a busybox shell. Unmapped keys return `None` (dropped).
fn keycode_to_linux(code: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match code {
        // Letters (QWERTY positional, not alphabetical — evdev is by physical position).
        KeyA => 30,
        KeyB => 48,
        KeyC => 46,
        KeyD => 32,
        KeyE => 18,
        KeyF => 33,
        KeyG => 34,
        KeyH => 35,
        KeyI => 23,
        KeyJ => 36,
        KeyK => 37,
        KeyL => 38,
        KeyM => 50,
        KeyN => 49,
        KeyO => 24,
        KeyP => 25,
        KeyQ => 16,
        KeyR => 19,
        KeyS => 31,
        KeyT => 20,
        KeyU => 22,
        KeyV => 47,
        KeyW => 17,
        KeyX => 45,
        KeyY => 21,
        KeyZ => 44,
        // Digit row.
        Digit1 => 2,
        Digit2 => 3,
        Digit3 => 4,
        Digit4 => 5,
        Digit5 => 6,
        Digit6 => 7,
        Digit7 => 8,
        Digit8 => 9,
        Digit9 => 10,
        Digit0 => 11,
        // Punctuation / whitespace.
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        BracketLeft => 26,
        BracketRight => 27,
        Enter => 28,
        Semicolon => 39,
        Quote => 40,
        Backquote => 41,
        Backslash => 43,
        Comma => 51,
        Period => 52,
        Slash => 53,
        Space => 57,
        Escape => 1,
        // Modifiers.
        ControlLeft => 29,
        ShiftLeft => 42,
        ShiftRight => 54,
        AltLeft => 56,
        CapsLock => 58,
        ControlRight => 97,
        AltRight => 100,
        SuperLeft => 125,
        SuperRight => 126,
        // Function keys.
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        F11 => 87,
        F12 => 88,
        // Navigation cluster.
        Home => 102,
        ArrowUp => 103,
        PageUp => 104,
        ArrowLeft => 105,
        ArrowRight => 106,
        End => 107,
        ArrowDown => 108,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        _ => return None,
    })
}
