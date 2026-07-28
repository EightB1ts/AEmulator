> **HISTORICAL — the build recipe below no longer works.** It predates `third_party/`: the
> `SampleRepos/` paths it uses are gone, and it configures the renderer against **MoltenVK**,
> which has been removed entirely. The reproducible build is now a single command —
> `third_party/bootstrap.sh gfxstream` — which pins the revision, applies our tracked patch
> series, and verifies the dylib exports the present path. See `../PHASE-KOSMICKRISP-STATUS.md`.
> Everything below is kept as the record of how the host renderer was first brought up.

# Phase 3a — gfxstream host renderer on macOS/MoltenVK: STATUS = SUCCESS

**Goal:** produce a loadable gfxstream host library on this Apple M2 Mac and empirically confirm
`stream_renderer_init` succeeds using **MoltenVK** as the Vulkan backend — in isolation, no VMM /
guest wiring. This was the single biggest technical risk on the Mac path.

**Result:** ✅ Achieved.
`libgfxstream_backend.dylib` builds, loads, and `stream_renderer_init` returns **0**, selecting the
**Apple M2** GPU via **MoltenVK 1.4.2** (Vulkan 1.4.357). Clean teardown, probe exit 0.
A macOS-specific source bug (`ANDROID_EMU_VK_ICD` blanked on the host-vulkan path) was found and
patched to get there. The C ABI is bindgen'd into `src/gfxstream/ffi.rs` behind a `gfxstream`
cargo feature.

---

## 1. The dylib

| | |
|---|---|
| Path | `SampleRepos/platform_hardware_google_gfxstream/build-host/libgfxstream_backend.dylib` |
| Arch | Mach-O 64-bit arm64 (Apple Silicon native — not universal) |
| Size | ~8.0 MB |
| install_name | `@rpath/libgfxstream_backend.dylib` |
| Link deps | AppKit, IOSurface, QuartzCore, CoreFoundation, Foundation, libobjc, libc++, libSystem (all system frameworks — **no** link-time Vulkan; MoltenVK is loaded at runtime) |

**Exported C ABI entry points confirmed present** (`nm -gU`):
`stream_renderer_init`, `stream_renderer_teardown`, `stream_renderer_context_create`,
`stream_renderer_context_destroy`, `stream_renderer_submit_cmd`, `stream_renderer_create_fence`,
`stream_renderer_resource_create`, `stream_renderer_create_blob`, `stream_renderer_export_blob`,
`stream_renderer_fill_caps`, `stream_renderer_transfer_{read,write}_iov`,
`gfxstream_backend_setup_window`, `gfxstream_backend_set_screen_mask`, plus the full
`android_*Opengles*` surface from `opengles.h`.

---

## 2. Build recipe (reproducible)

Prereqs (all Homebrew): `cmake`, `ninja`, `pkg-config`, `molten-vk`, `vulkan-loader`,
`vulkan-headers`. Compiler = `/usr/bin/clang` (Xcode CLT). **meson is NOT used** (that meson.build
is the *guest* Mesa driver; x11/wayland/android only, no macOS).

### 2a. Build aemu (not vendored — cloned manually)

`aemu-base` + `aemu-host-common` are not in the tree and there are no meson `.wrap` files, so aemu
is cloned + built + installed first:

```bash
# git clone https://android.googlesource.com/platform/hardware/google/aemu \
#     SampleRepos/aemu     (or a GitHub mirror)
cd SampleRepos/aemu
cmake -B build -G Ninja \
    -DAEMU_COMMON_BUILD_CONFIG=gfxstream \
    -DCMAKE_INSTALL_PREFIX="$PWD/install"
ninja -C build && ninja -C build install
```

This produces `SampleRepos/aemu/install/{include,lib}` including the pkg-config `.pc` files
(`aemu_base.pc`, `aemu_host_common.pc`, `aemu_logging.pc`, `aemu_snapshot.pc`) that gfxstream's
`DEPENDENCY_RESOLUTION=SYSTEM` discovers via `PKG_CONFIG_PATH`.

### 2b. Configure + build the gfxstream host renderer

```bash
cd SampleRepos/platform_hardware_google_gfxstream
PKG_CONFIG_PATH="$PWD/../aemu/install/lib/pkgconfig" \
cmake -B build-host -G Ninja \
    -DBUILD_STANDALONE=ON \
    -DDEPENDENCY_RESOLUTION=SYSTEM \
    -DENABLE_VKCEREAL_TESTS=OFF \
    -DCMAKE_PREFIX_PATH="/opt/homebrew;/opt/homebrew/opt/vulkan-headers;/opt/homebrew/opt/vulkan-loader"
ninja -C build-host libgfxstream_backend.dylib
```

Key cache values that made it work (from `build-host/CMakeCache.txt`):
`BUILD_STANDALONE=ON`, `DEPENDENCY_RESOLUTION=SYSTEM`, `AEMU_COMMON_BUILD_CONFIG=gfxstream`,
aemu libs resolved to `SampleRepos/aemu/install/lib/lib{aemu-base,aemu-host-common,...}.a`.
The `APPLE` branch in `host/CMakeLists.txt` links AppKit/QuartzCore/IOSurface and compiles
`NativeSubWindow_cocoa.mm`.

### 2c. THE macOS SOURCE PATCH (required — without it init fails)

**File:** `host/gl/gl-host-common/opengl/emugl_config.cpp`, `emuglConfig_setupEnv()`.

`virtio-gpu-gfxstream-renderer.cpp:595` calls `emuglConfig_init(..., use_host_vulkan=true)`
(hardcoded). `emuglConfig_setupEnv()` then did, unconditionally:

```cpp
if (config->use_host_vulkan) {
    setEnvironmentVariable("ANDROID_EMU_VK_ICD", "");   // <-- blanks the ICD selection
}
```

On Linux/Windows "host vulkan" means the system loader finds the discrete-GPU ICD with no
override, so blanking is fine. **On macOS there is no native Vulkan ICD — host vulkan IS
MoltenVK.** Blanking `ANDROID_EMU_VK_ICD` makes `VkCommonOperations.cpp` compute
`moltenVKEnabled = (ANDROID_EMU_VK_ICD == "moltenvk") == false`, so it never sets
`VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR` nor enables `VK_KHR_portability_enumeration`.
`vkCreateInstance` against the MoltenVK (portability) ICD then fails:

```
[Vulkan Loader] ERROR | DRIVER: vkCreateInstance: Found drivers that contain devices which
support the portability subset, but the instance does not enumerate portability drivers! ...
VK_ERROR_INCOMPATIBLE_DRIVER
```

**Patch** (applied): under `#if defined(__APPLE__)`, on the `use_host_vulkan` path set the ICD to
`"moltenvk"` (unless the caller explicitly asked for `swiftshader`) instead of `""`. Rebuild is
incremental (`ninja libgfxstream_backend.dylib`). After the patch the log shows
`VkCommonOperations.cpp:866] MoltenVK is supported, enabling Vulkan portability.` and init
succeeds.

---

## 3. The probe (empirical proof)

Harness lives beside this doc:

- `probe.c` — dlopen-free: links `-lgfxstream_backend` with an rpath, sets up a fence callback,
  calls `stream_renderer_init` with `STREAM_RENDERER_FLAGS_USE_VK_BIT |
  STREAM_RENDERER_FLAGS_USE_SURFACELESS_BIT`, `USER_DATA`, `FENCE_CALLBACK`, `WIN0_WIDTH/HEIGHT`,
  then `stream_renderer_teardown`. Exit 0 iff init returned 0.
- `run.sh` — compiles + ad-hoc-signs the probe, **stages the Vulkan runtime** (see gotcha below),
  exports `ANDROID_EMU_VK_ICD=moltenvk` + `ANDROID_EMUGL_VERBOSE=1`, runs.
- `MoltenVK_icd.json` — ICD manifest with an absolute `library_path` to
  `/opt/homebrew/lib/libMoltenVK.dylib`.

**Runtime wiring gotcha (baked into `run.sh`):** with `ANDROID_EMU_VK_ICD=moltenvk`, gfxstream does
**not** honor a caller `VK_ICD_FILENAMES` or `DYLD` search for the loader. `VulkanDispatch.cpp`
hard-codes the search to **`<programDir>/lib64/vulkan/`** for BOTH `libvulkan.dylib` (the loader)
and `MoltenVK_icd.json` (which it rewrites into `VK_ICD_FILENAMES` itself). So `run.sh` creates
`./lib64/vulkan/`, symlinks the Homebrew `libvulkan.dylib` in, and copies the ICD json there.

### Observed output (verbatim, key lines)

```
[probe] starting stream_renderer_init on MoltenVK...
VkCommonOperations.cpp:866] MoltenVK is supported, enabling Vulkan portability.
[mvk-info] MoltenVK version 1.4.2, supporting Vulkan version 1.4.357.
[mvk-info] GPU device:
    model: Apple M2
    deviceID: 0x1b000208
    GPU memory available: 12124 MB
    GPU Family Metal 4 / GPU Family Apple 8 / GPU Family Mac 1 / GPU Family Mac 2
VkCommonOperations.cpp:1230] Selecting Vulkan device: Apple M2, Version: 1.1.357
[mvk-info] Created VkDevice to run on GPU Apple M2 with 8 Vulkan extensions enabled
FrameBuffer.cpp:367] Supports id properties, got a vulkan device UUID
virtio-gpu-gfxstream-renderer.cpp(993)] stream_renderer_init Gfxstream initialized successfully!
[probe] stream_renderer_init returned 0
[probe] SUCCESS — host renderer initialized.
[mvk-info] Destroyed VkDevice on GPU Apple M2 ...
[probe] teardown complete.       (process exit code 0)
```

Note: `stream_renderer_opengles_init GL strings; [N/A] [N/A] [N/A]` — GLES is intentionally off
(VK-only, surfaceless); expected and harmless for the 3c Vulkan transport path.

---

## 4. The Rust bindings

- `src/gfxstream/ffi.rs` — bindgen output (50 `extern "C"` fns, all structs/enums/consts).
  Committed so the feature build needs no libclang.
- `src/gfxstream/wrapper.h` — bindgen entry point (includes the 3 ABI headers; parsed as C so the
  `#ifdef __cplusplus` heavy C++ includes in `opengles.h` are skipped; defuses `bool` /
  `EMUGL_COMMON_API`).
- `src/gfxstream/mod.rs` — module docs; `pub mod ffi;`.
- `gfxstream/gen-ffi.sh` — regenerator (`LIBCLANG_PATH=/Library/Developer/CommandLineTools/usr/lib`;
  the Xcode-Beta toolchain omits `libclang.dylib`).
- Gating: `Cargo.toml` `[features] gfxstream = []`; `src/lib.rs` has
  `#[cfg(feature = "gfxstream")] pub mod gfxstream;`.

Verified: `cargo build` (default, feature OFF) **and** `cargo build --features gfxstream` (bindings
compile) both succeed. The feature build compiles the bindings only — it does **not** link the
dylib, so it stays hermetic. Bound symbols confirmed present: `stream_renderer_init`,
`stream_renderer_context_create`, `stream_renderer_submit_cmd`, `stream_renderer_create_fence`,
`gfxstream_backend_setup_window`; structs `stream_renderer_param`, `stream_renderer_fence`,
`stream_renderer_command`, `stream_renderer_resource_create_args`, `stream_renderer_create_blob`;
enum `RendererFlags` (`USE_VK_BIT = 32`, `USE_SURFACELESS_BIT = 8`).

---

## 5. Next steps

### 3b — CAMetalLayer presenter (scanout to a window)

- The probe ran **surfaceless**. To present, drop `USE_SURFACELESS_BIT` and call
  `gfxstream_backend_setup_window(native_window_handle, x, y, w, h, fb_w, fb_h)` with an
  `NSView*`/`CAMetalLayer`-backed handle. `host/NativeSubWindow_cocoa.mm` is already compiled in.
- Phase 2's winit window (`src/display.rs`) gives us an `NSView` via `raw-window-handle`; feed its
  pointer to `gfxstream_backend_setup_window`. MoltenVK will present through the view's
  `CAMetalLayer`. Alternatively wire `android_setPostCallback` + `android_getReadPixelsFunc` to
  blit into the existing softbuffer surface (no Metal swapchain) as a fallback.
- `VulkanNativeSwapchain` was **disabled** in this init; presenting via the native subwindow
  (Cocoa) is the path that's already built, not `STREAM_RENDERER_FLAGS_VULKAN_NATIVE_SWAPCHAIN_BIT`.

### 3c — virtio-gpu 3D transport (wire into the VMM)

- Replace/extend the Phase 2 virtio-gpu **2D** device to advertise `VIRTIO_GPU_F_VIRGL` (3D) and
  route `VIRTIO_GPU_CMD_SUBMIT_3D` + context/resource commands to the `ffi` entry points:
  `stream_renderer_context_create`, `stream_renderer_submit_cmd` (the guest gfxstream command
  stream), `stream_renderer_resource_create_*` / `stream_renderer_create_blob`,
  `stream_renderer_transfer_{read,write}_iov`, and `stream_renderer_create_fence` +
  the fence callback for guest-visible fencing.
- Provide the `FENCE_CALLBACK` to signal the guest's virtio-gpu fence ring (Phase 2 already models
  the ring). `USE_EXTERNAL_BLOB` will matter for zero-copy resource sharing with guest memory —
  start without it (guest blob).
- **Linking (3c build.rs):** the feature build currently compiles bindings without linking. When
  wiring the VMM, add `cargo:rustc-link-lib=dylib=gfxstream_backend` +
  `cargo:rustc-link-search=native=<build-host>` + an rpath, and stage the MoltenVK loader/ICD at
  runtime exactly as `run.sh` does (or set `ANDROID_EMU_VK_LOADER_PATH` to the Homebrew
  `libvulkan.dylib` to bypass the `lib64/vulkan` layout).

### Upstreaming / durability

- The `emugl_config.cpp` MoltenVK patch is local. Keep it as a tracked patch against the vendored
  gfxstream checkout (it is a real macOS-correctness fix). Re-apply if the gfxstream tree is
  re-synced.
