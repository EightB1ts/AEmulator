//! `gfxstream` — raw FFI bindings to the gfxstream host renderer's C ABI (Phase 3a).
//!
//! These are **bindings only**. Nothing here is wired into the VMM yet — that is Phase 3c
//! (virtio-gpu 3D transport). The module exists behind the `gfxstream` cargo feature so the
//! default `aemulator-hvf` build is completely unaffected (and needs no libclang / dylib).
//!
//! # What was proven in Phase 3a
//! The host renderer dylib
//! (`third_party/gfxstream/build-host/libgfxstream_backend.dylib`, built by
//! `third_party/bootstrap.sh gfxstream`)
//! builds on this Apple M2 Mac, loads, and `stream_renderer_init` returns `0` selecting the
//! **Apple M2** GPU. The Vulkan backend is now **Mesa KosmicKrisp** (Vulkan-on-Metal), which reports
//! `Selecting Vulkan device: Apple M2, Version: 1.4.354` — the API version is the tell that the
//! right ICD loaded; a portability-subset-shaped device would mean it did not. See
//! `crates/aemulator-hvf/gfxstream/PHASE3A-STATUS.md` and the `probe.c` / `run.sh` harness.
//!
//! # Regenerating [`ffi`]
//! `crates/aemulator-hvf/gfxstream/gen-ffi.sh` runs `bindgen` over
//! `src/gfxstream/wrapper.h` (which includes `virtio-gpu-gfxstream-renderer.h`,
//! `-unstable.h`, and `host-common/opengles.h`). The output [`ffi`] module is committed so the
//! feature build does not require bindgen/libclang.
//!
//! # Linking (future — Phase 3c)
//! To actually call into these symbols, a consumer must link `libgfxstream_backend.dylib`
//! (e.g. `cargo:rustc-link-lib=dylib=gfxstream_backend` +
//! `cargo:rustc-link-search=native=<build-host>` + an rpath), and at runtime stage the Vulkan loader
//! + point `VK_DRIVER_FILES` at the KosmicKrisp ICD exactly as `run.sh` does. The current feature
//! build compiles the bindings without linking, so it stays hermetic.

pub mod ffi;

/// Hermetic guards pinning the host Vulkan driver to KosmicKrisp (see the module docs). Test-only:
/// it reads the repo's launch surfaces as text and adds nothing to the built crate.
#[cfg(test)]
mod driver_pin;
