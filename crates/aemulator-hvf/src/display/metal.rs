//! `display::metal` — CAMetalLayer ownership + the gfxstream hand-off, behind `--features gfxstream`.
//!
//! Phase 2's window presents the guest's 2D scanout with a CPU blit through softbuffer. gfxstream 3D
//! frames are produced by the host renderer on the GPU and never come back to the CPU: the renderer
//! creates a `VkSurfaceKHR` on our window's [`CAMetalLayer`] (`VK_EXT_metal_surface`), owns a
//! `VK_KHR_swapchain` on it, and blits each scanout ColorBuffer straight in. This module's job is to
//! produce that layer and hand it over.
//!
//! Three responsibilities:
//!   * **Layer hand-off** — [`layer_for_window`] guarantees the window's `NSView` is backed by a
//!     correctly configured `CAMetalLayer` and returns its pointer. The vCPU thread publishes that
//!     pointer to `stream_renderer_present_to_metal_layer` on every flush; this process never
//!     acquires a drawable from the layer again.
//!   * **gfxstream hand-off** — [`init_gfxstream`] calls `stream_renderer_init` (VK[+GLES], NOT
//!     surfaceless) and [`setup_gfxstream_window`] hands gfxstream the `NSWindow` so it attaches its
//!     own presentation subview. `gfxstream_backend_setup_window` on macOS expects an `NSWindow*`;
//!     raw-window-handle 0.6 only gives us the `NSView`, so we derive the window in the ObjC shim.
//!   * **Self-test presenter** — [`present_test_pattern`] drives a full host-texture → drawable →
//!     present cycle with no guest and no Vulkan, which is what `aemu-display-selftest` proves the
//!     window/layer plumbing with.
//!
//! The Objective-C implementation lives in `src/display/metal_shim.m` (compiled by `build.rs`).

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::os::raw::c_void;
use winit::window::Window;

use crate::gfxstream::ffi;

extern "C" {
    /// Present a host-generated test pattern into the view's CAMetalLayer drawable. 0 == success.
    fn aemu_metal_present_test_pattern(ns_view: *mut c_void, w: i32, h: i32) -> i32;
    /// Attach (idempotent) a CAMetalLayer to the view. 1 == success.
    fn aemu_metal_attach_layer(ns_view: *mut c_void, w: i32, h: i32) -> i32;
    /// 1 iff the view's backing layer is a CAMetalLayer.
    fn aemu_metal_layer_attached(ns_view: *mut c_void) -> i32;
    /// Derive the owning NSWindow* from an NSView* (NULL if not installed in a window yet).
    fn aemu_nsview_window(ns_view: *mut c_void) -> *mut c_void;
    /// In-Vulkan present hand-off: ensure the view is backed by a CAMetalLayer sized for a
    /// `px_w`x`px_h` physical-pixel drawable and return it. NULL on failure.
    fn aemu_metal_layer_for_view(ns_view: *mut c_void, px_w: i32, px_h: i32) -> *mut c_void;
    fn aemu_metal_layer_set_visible(metal_layer: *mut c_void, visible: i32);
}

/// Extract the `NSView*` from a winit [`Window`] via raw-window-handle 0.6. `None` on any non-AppKit
/// handle (should never happen on macOS).
pub fn nsview_of(window: &Window) -> Option<*mut c_void> {
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr()),
        _ => None,
    }
}

/// Present a checker+gradient test pattern into `window`'s CAMetalLayer. Proves the Metal presenter
/// path (host texture → drawable → present) that gfxstream frames will reuse. Returns the shim's
/// status code (0 == success). Must be called on the main thread.
pub fn present_test_pattern(window: &Window, w: u32, h: u32) -> i32 {
    let Some(view) = nsview_of(window) else {
        return -100;
    };
    // Safety: `view` is a live NSView owned by the still-alive `window`; the shim only messages it
    // on the calling (main) thread.
    unsafe { aemu_metal_present_test_pattern(view, w as i32, h as i32) }
}

/// Ensure `window`'s NSView is backed by a CAMetalLayer sized for a `px_w`x`px_h` **physical-pixel**
/// drawable, and return the layer as a `usize` for publishing on [`ScanoutHandle::metal_layer`].
/// `0` on failure.
///
/// Idempotent, so this is both the attach call and the resize call. Must run on the main thread.
/// Once the pointer is published, gfxstream owns presentation into this layer
/// (`stream_renderer_present_to_metal_layer` → `vkCreateMetalSurfaceEXT` + `VK_KHR_swapchain`) and
/// this process must never acquire a drawable from it again.
///
/// [`ScanoutHandle::metal_layer`]: crate::virtio::ScanoutHandle::metal_layer
pub fn layer_for_window(window: &Window, px_w: u32, px_h: u32) -> usize {
    let Some(view) = nsview_of(window) else {
        return 0;
    };
    // Safety: `view` is a live NSView owned by the still-alive `window`; the shim only messages it
    // on the calling (main) thread and returns an unretained pointer the NSView owns.
    unsafe { aemu_metal_layer_for_view(view, px_w as i32, px_h as i32) as usize }
}

/// Show or hide the in-Vulkan present layer.
///
/// The layer is created hidden and sits as a *sublayer* above softbuffer's content, so hiding it is
/// what lets the CPU blit show through. Call this only once an in-Vulkan present has actually
/// succeeded — a layer exists for the whole of any windowed run, but a 2D-only run, a run whose host
/// renderer failed to initialize, and the boot-console phase of a healthy run all present through
/// softbuffer and would otherwise be blanked by a visible empty layer.
pub fn set_layer_visible(layer: usize, visible: bool) {
    if layer == 0 {
        return;
    }
    // Safety: `layer` came from `layer_for_window` and is owned by the window's live NSView; the
    // shim wraps the property set in a CATransaction so it is safe off the main thread.
    unsafe { aemu_metal_layer_set_visible(layer as *mut c_void, i32::from(visible)) }
}

/// Whether `window`'s NSView is backed by a CAMetalLayer (verification hook).
pub fn metal_layer_attached(window: &Window) -> bool {
    let Some(view) = nsview_of(window) else {
        return false;
    };
    unsafe { aemu_metal_layer_attached(view) == 1 }
}

/// Ensure a CAMetalLayer is attached to `window`'s NSView. Returns true on success.
pub fn attach_layer(window: &Window, w: u32, h: u32) -> bool {
    let Some(view) = nsview_of(window) else {
        return false;
    };
    unsafe { aemu_metal_attach_layer(view, w as i32, h as i32) == 1 }
}

/// Initialize the gfxstream host renderer for a windowed (non-surfaceless) present at `w`x`h`.
///
/// Mirrors the Phase 3a probe's `stream_renderer_init` but **drops** `USE_SURFACELESS_BIT` (so a
/// window can be attached) and adds `USE_GLES_BIT` alongside `USE_VK_BIT` per the Phase 3b spec.
/// Returns `stream_renderer_init`'s status (0 == success). The runtime still needs the Vulkan loader
/// + the KosmicKrisp ICD staged exactly as `gfxstream/run.sh` does (see `run-display-selftest.sh`).
///
/// # Safety
/// Links + calls into `libgfxstream_backend.dylib`; only valid on macOS/aarch64 with the dylib on
/// the rpath and the Vulkan loader + KosmicKrisp ICD staged.
pub fn init_gfxstream(w: i32, h: i32, use_gles: bool) -> i32 {
    // The async fence callback: gfxstream signals it from a renderer thread when a fence completes.
    // It records the completion + wakes the VMM's fence-signal thread (installed by `machine.rs`),
    // which raises the virtio-gpu SPI so the guest's `gfxstream_vk` unblocks. See gpu3d fence bridge.
    let on_fence = crate::virtio::gpu3d::on_fence_complete;

    static USER_DATA: u64 = 0xA3A3_A3A3_A3A3_A3A3;

    let mut flags = ffi::RendererFlags::STREAM_RENDERER_FLAGS_USE_VK_BIT as u64;
    if use_gles {
        flags |= ffi::RendererFlags::STREAM_RENDERER_FLAGS_USE_GLES_BIT as u64;
    }

    let mut params = [
        ffi::stream_renderer_param {
            key: ffi::STREAM_RENDERER_PARAM_USER_DATA as u64,
            value: &USER_DATA as *const u64 as u64,
        },
        ffi::stream_renderer_param {
            key: ffi::STREAM_RENDERER_PARAM_RENDERER_FLAGS as u64,
            value: flags,
        },
        ffi::stream_renderer_param {
            key: ffi::STREAM_RENDERER_PARAM_FENCE_CALLBACK as u64,
            value: on_fence as *const () as usize as u64,
        },
        ffi::stream_renderer_param {
            key: ffi::STREAM_RENDERER_PARAM_WIN0_WIDTH as u64,
            value: w as u64,
        },
        ffi::stream_renderer_param {
            key: ffi::STREAM_RENDERER_PARAM_WIN0_HEIGHT as u64,
            value: h as u64,
        },
    ];

    // Safety: params is a valid, correctly-sized array for the FFI call.
    unsafe { ffi::stream_renderer_init(params.as_mut_ptr(), params.len() as u64) }
}

/// Hand gfxstream the window to present into. Derives the `NSWindow*` from the winit `NSView` and
/// calls `gfxstream_backend_setup_window(nswindow, 0,0,w,h, w,h)`. Returns false if the window
/// pointer could not be resolved. Must be called after [`init_gfxstream`] succeeds.
pub fn setup_gfxstream_window(window: &Window, w: i32, h: i32) -> bool {
    let Some(view) = nsview_of(window) else {
        return false;
    };
    // gfxstream's cocoa subwindow code casts the handle to NSWindow* and calls -contentView/-frame.
    let nswindow = unsafe { aemu_nsview_window(view) };
    if nswindow.is_null() {
        return false;
    }
    // Safety: nswindow is a live NSWindow* owned by `window`; gfxstream reads it on this thread.
    unsafe {
        ffi::gfxstream_backend_setup_window(nswindow, 0, 0, w, h, w, h);
    }
    true
}

/// Tear down the gfxstream host renderer.
///
/// # Safety
/// Only valid after a successful [`init_gfxstream`].
pub fn teardown_gfxstream() {
    unsafe { ffi::stream_renderer_teardown() }
}
