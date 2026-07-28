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
use std::sync::atomic::{AtomicBool, Ordering};
use winit::window::Window;

use crate::gfxstream::ffi;

/// Whether [`init_gfxstream`] has brought the host renderer up and [`teardown_gfxstream`] has not
/// yet taken it down. Gates the teardown FFI call.
///
/// `stream_renderer_teardown()` is not defensive on the C++ side: it dereferences the frontend
/// singleton, tears down the Vulkan emulation, and calls `android_finishOpenglesRenderer()`. Calling
/// it without a successful init, or twice, is undefined behaviour rather than a no-op — and it now
/// has two callers (`machine.rs`'s common shutdown path and `aemu-display-selftest`), so "the one
/// caller knows" is no longer a safe premise.
static INITED: AtomicBool = AtomicBool::new(false);

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

/// Install the GPU identity the host renderer reports to the **guest**, and return the FFI status
/// (`0` == installed).
///
/// Takes explicit values on purpose: this function is the *mechanism*, and it deliberately contains
/// no opinion about what a GPU should claim to be. That policy belongs in the identity crate, where
/// it is a plain struct whose tests run on every platform, rather than in a macOS-only display
/// module that lives in a workspace-excluded crate its CI never compiles.
///
/// **Sentinel convention, and it is the whole contract:** `0` for an integer, `""` for the name and
/// an all-zero `pipeline_cache_uuid` each mean *keep the host driver's own value*. So calling this
/// with all sentinels is exactly equivalent to not calling it at all, and both are byte-identical to
/// an unpatched renderer. That equivalence is load-bearing: it is what lets the identity policy be
/// wired in as a separate, revertable change without this call being the thing that changes
/// behaviour.
///
/// **Must be called before [`init_gfxstream`].** The renderer reads these values from its first
/// `vkEnumeratePhysicalDevices` onward, on threads that `stream_renderer_init` creates; installing
/// before init is what makes the unsynchronized read on those threads sound. Installing *after* would
/// mean a guest that already read one answer starts seeing a different one.
///
/// `flags` is a bitmask of `ffi::STREAM_RENDERER_GPU_IDENTITY_*`:
/// `CLAMP_MOBILE_FEATURES` (bit 0) reports the mobile-plausible subset of the host feature set, and
/// `STRIP_PLATFORM_EXTS` (bit 1) hides the host-platform device extensions. Neither ever *adds* a
/// capability — the renderer cannot report a feature the host lacks without breaking
/// `vkCreateDevice` outright.
///
/// `device_name` is truncated to fit `VK_MAX_PHYSICAL_DEVICE_NAME_SIZE` (256 bytes including the
/// NUL) and any interior NUL ends it, matching C string semantics. A name that does not fit is
/// truncated rather than rejected: a slightly short name is a cosmetic problem, a failed install is
/// a silent fallback to leaking the host's identity.
///
/// Returns the renderer's status: `0` installed, `-1` null pointer (not reachable from here),
/// `-2` ABI mismatch — the dylib was built from a different version of
/// `virtio-gpu-gfxstream-renderer-unstable.h` than `ffi.rs` was generated from. `-2` is not fatal:
/// the renderer keeps exact passthrough, so graphics still work and only the identity is unchanged.
#[allow(clippy::too_many_arguments)]
pub fn set_gpu_identity(
    vendor_id: u32,
    device_id: u32,
    device_name: &str,
    pipeline_cache_uuid: [u8; 16],
    max_viewports: u32,
    subgroup_size: u32,
    flags: u32,
) -> i32 {
    let mut identity = ffi::stream_renderer_gpu_identity {
        // From `size_of`, never a literal: this field exists precisely so that a struct laid out by
        // a stale header is refused instead of misread.
        struct_size: std::mem::size_of::<ffi::stream_renderer_gpu_identity>() as u32,
        vendor_id,
        device_id,
        flags,
        max_viewports,
        subgroup_size,
        device_name: [0; 256],
        pipeline_cache_uuid,
    };
    // Copy the name in, stopping at the first NUL and leaving room for the terminator. `as i8` is
    // the c_char cast on aarch64-apple-darwin; the bytes are whatever the caller passed, and the
    // renderer re-terminates on its side as well.
    let bytes = device_name.as_bytes();
    let name_len = bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(bytes.len())
        .min(identity.device_name.len() - 1);
    for (dst, &src) in identity.device_name.iter_mut().zip(&bytes[..name_len]) {
        *dst = src as std::os::raw::c_char;
    }

    // Safety: `identity` is a fully initialized, correctly laid out
    // `stream_renderer_gpu_identity`; the renderer copies it and does not retain the pointer.
    unsafe { ffi::stream_renderer_set_gpu_identity(&identity) }
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

    // Install the guest-facing GPU identity BEFORE stream_renderer_init, which is the only point at
    // which it is legal (the renderer reads it from its first vkEnumeratePhysicalDevices, on threads
    // init creates). Keeping the call here rather than at each call site is what makes "the setter
    // always runs first" a property of the code instead of a convention.
    //
    // ALL SENTINELS TODAY, i.e. this is exact passthrough: the guest sees the host driver's own
    // vendorID, deviceID, deviceName, pipelineCacheUUID, feature set and extension list, byte for
    // byte identical to an unpatched renderer. That is deliberate. The mechanism lands here; the
    // policy — which vendor to claim, which UUID to derive, and the tests that justify each — lands
    // separately in `aemulator-identity`, and switching it on is then a change to the arguments of
    // this one call rather than a change to any of the machinery. Do not put a literal vendor id or
    // UUID here; that is the policy leaking back into the mechanism.
    let identity_rc = set_gpu_identity(
        0,         // vendor_id      — 0 = keep host
        0,         // device_id      — 0 = keep host
        "",        // device_name    — "" = keep host
        [0u8; 16], // cache UUID     — all-zero = keep host
        0,         // max_viewports  — 0 = keep host
        0,         // subgroup_size  — 0 = keep host
        0,         // flags          — no feature clamp, no extension strip
    );
    if identity_rc != 0 {
        // Never fatal: a rejected install means the renderer keeps reporting the host GPU, which is
        // exactly what it does today. But -2 specifically means ffi.rs and the dylib disagree about
        // a struct layout, and that is worth saying out loud before something subtler goes wrong.
        eprintln!(
            "[display] stream_renderer_set_gpu_identity returned {identity_rc} \
             (-2 = ABI mismatch between ffi.rs and libgfxstream_backend.dylib; \
             re-run crates/aemulator-hvf/gfxstream/gen-ffi.sh). \
             Continuing with the host GPU's own identity."
        );
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
    let rc = unsafe { ffi::stream_renderer_init(params.as_mut_ptr(), params.len() as u64) };
    if rc == 0 {
        // Only a SUCCESSFUL init arms teardown. A failed init leaves the frontend singleton in a
        // state `stream_renderer_teardown()` is not written to survive, and every caller already
        // falls back to a non-gfxstream path on a non-zero rc.
        INITED.store(true, Ordering::Release);
    }
    rc
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

/// Release the host renderer's cached `VkSurfaceKHR` + swapchain for one `CAMetalLayer`.
///
/// The renderer caches a presenter per layer on the first
/// `stream_renderer_present_to_metal_layer` call and keys it on the layer pointer. That cache is
/// otherwise only emptied by [`teardown_gfxstream`] — which is far too late, because the layer is a
/// sublayer of the window's `NSView` and dies with the window. Destroying a `VkSurfaceKHR` whose
/// `CAMetalLayer` has already been freed is a use-after-free inside Metal, so the release has to
/// happen while the layer is **still alive** and the renderer is **still running** (the `VkDevice`
/// must be live to destroy the swapchain).
///
/// The intended shutdown order is: vCPUs joined → `retire_metal_layer` (unpublish + this call) →
/// [`teardown_gfxstream`]. See `display::retire_metal_layer`.
///
/// **But those two run on different threads** — `retire_metal_layer` on the display thread as its
/// event loop exits, `teardown_gfxstream` on the VMM thread as `boot_and_run_common` returns — so on
/// a normal guest halt teardown can win the race. The `INITED` check below is what makes that
/// harmless rather than a second use-after-free: teardown clears `INITED` before it calls into the
/// FFI, and `stream_renderer_teardown()` starts by running `teardownMetalLayerPresenters()`, which
/// destroys *every* presenter while the `VkDevice` is still alive. So if teardown went first the
/// presenter is already gone and there is nothing here to do; if this went first, teardown finds an
/// empty map. Both orders are correct, and neither ever touches a presenter after device destruction.
///
/// A `layer` of 0 is a no-op, as is a layer the renderer never presented to. Idempotent.
pub fn release_metal_layer(layer: usize) {
    if layer == 0 {
        return;
    }
    if !INITED.load(Ordering::Acquire) {
        // No live renderer, so no presenter to release: either init never succeeded, or
        // teardown_gfxstream() already ran teardownMetalLayerPresenters() for us.
        return;
    }
    // Safety: `layer` came from `layer_for_window` and the NSView that owns it is still alive; the
    // renderer serializes this against its own present path under `sPresentersMutex`.
    unsafe { ffi::stream_renderer_release_metal_layer(layer as *mut c_void) }
}

/// Tear down the gfxstream host renderer. Idempotent, and a no-op if [`init_gfxstream`] never
/// succeeded — so every exit path can call it unconditionally.
///
/// The `compare_exchange` is what makes both true: exactly one caller wins the `true -> false`
/// transition and reaches the FFI, everybody else returns. Two callers race here in practice
/// (`machine.rs`'s shutdown path and `aemu-display-selftest`), and the FFI itself is not re-entrant.
///
/// **Call this only once no thread can still present.** In `machine.rs` that means after the
/// secondary-vCPU join loop, since `flush_3d` runs on a vCPU thread; and after
/// [`release_metal_layer`], since the swapchain must be destroyed before the layer it targets.
pub fn teardown_gfxstream() {
    if INITED
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    // Safety: the compare_exchange above proves a successful `stream_renderer_init` happened and
    // that this is the only call to reach the FFI for it.
    unsafe { ffi::stream_renderer_teardown() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests below deliberately exercise ONLY the `INITED` atomic: with `INITED` false the
    /// `compare_exchange` fails and `stream_renderer_teardown()` is never reached, so they are
    /// hermetic — no host renderer, no Vulkan loader, no ICD. (They still LINK against the dylib;
    /// that is what `ci/stub-gfxstream` exists for.) A live-renderer cycle is covered by
    /// `aemu-display-selftest --init-teardown-cycles N`, run from `ci/hvf-doctor.sh`.
    ///
    /// `INITED` is process-global and `cargo test` shares one process across threads, so neither
    /// test may ever set it to true — that would make them order-dependent on each other and on
    /// anything else that touches the renderer.
    #[test]
    fn teardown_is_a_noop_when_init_never_ran() {
        assert!(
            !INITED.load(Ordering::Acquire),
            "no test may arm INITED: teardown_gfxstream() would then call into the FFI"
        );
        teardown_gfxstream();
        assert!(!INITED.load(Ordering::Acquire));
    }

    #[test]
    fn teardown_is_idempotent() {
        assert!(!INITED.load(Ordering::Acquire));
        for _ in 0..4 {
            teardown_gfxstream();
        }
        assert!(!INITED.load(Ordering::Acquire));
        // Same guard for the per-layer release: without a live renderer it must not reach the FFI
        // either, since the presenter map it would touch does not exist yet.
        release_metal_layer(0);
        release_metal_layer(0xdead_beef);
    }
}
