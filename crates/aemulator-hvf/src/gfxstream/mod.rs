//! `gfxstream` — raw FFI bindings to the gfxstream host renderer's C ABI (Phase 3a).
//!
//! These are the raw bindings; `crate::virtio::gpu3d` is the safe forwarder built on them and
//! `crate::display::metal::init_gfxstream` is what starts the renderer. They are **not** behind a
//! cargo feature — there are none, deliberately, and `Cargo.toml` explains why: a feature-gated
//! renderer produced binaries that booted perfectly and were quietly crippled.
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
//! `-unstable.h`, and `host-common/opengles.h`). The output [`ffi`] module is committed so an
//! ordinary build needs neither bindgen nor libclang.
//!
//! # Linking
//! `build.rs` links `libgfxstream_backend.dylib` unconditionally
//! (`cargo:rustc-link-lib=dylib=gfxstream_backend`, `cargo:rustc-link-search=native=<build-host>`,
//! and an rpath). At runtime the Vulkan loader must be staged and `VK_DRIVER_FILES` pointed at the
//! KosmicKrisp ICD, exactly as `run.sh` does; `ci/hvf-doctor.sh` checks both, plus that every
//! `stream_renderer_*` symbol the binary imports is actually exported by the dylib — the guard for
//! the stale-binary crash recorded in `PHASE-KOSMICKRISP-STATUS.md`.

pub mod ffi;

/// Pure text analysis of gfxstream's *generated* cereal switches: which Vulkan extensions the host
/// can size but not decode, and therefore must never advertise to the guest. Backs the
/// `aemu-cereal-gap` binary, `host/vulkan/VkDecoderCerealGap.inc`, and the `ci/hvf-doctor.sh` drift
/// guard. Dependency-free and hermetically tested — it never links or calls the renderer.
pub mod cereal_gap;

/// Hermetic guards pinning the host Vulkan driver to KosmicKrisp (see the module docs). Test-only:
/// it reads the repo's launch surfaces as text and adds nothing to the built crate.
#[cfg(test)]
mod driver_pin;
