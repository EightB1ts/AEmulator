> **SUPERSEDED IN PART — see [`PHASE-KOSMICKRISP-STATUS.md`](PHASE-KOSMICKRISP-STATUS.md).**
> The host Vulkan driver is no longer MoltenVK; it is Mesa/LunarG KosmicKrisp, and **the
> `MTLTexture` zero-copy present this document describes no longer exists**. KosmicKrisp implements
> no `VK_EXT_metal_objects` and its `VK_EXT_external_memory_metal` exports only `MTLHeap` handles, so
> `vkExportMetalObjectsEXT` → `id<MTLTexture>` → `CAMetalLayer` cannot be built on it at all. It was
> replaced by an in-Vulkan present (`vkCreateMetalSurfaceEXT` + `VK_KHR_swapchain` +
> `vkCmdBlitImage` + `vkQueuePresentKHR`) inside the renderer's own `VkDevice`. Everything else here
> — the resource-lifecycle protocol, the blob/ASG machinery, the cereal codegen gap and the
> `VK_EXT_memory_budget` analysis, the guest capability ceilings — still stands, and the
> guest-visible `cmd gpu vkjson` capability table below has since been **re-measured on KosmicKrisp**
> (87 guest extensions vs the 85 recorded here, device id `0x00000064` not `0x1B000208`, 3DMark
> verdicts byte-identical) — see the new document for the current numbers. History below is left
> unedited.

# Phase Metal (resource lifecycle) — the virtio-gpu ↔ gfxstream resource protocol is now complete: `RESOURCE_UNREF` / `DETACH_BACKING` reach the host renderer, `ATTACH_BACKING` no longer hands it a dangling pointer, and `SET_SCANOUT_BLOB` / `ASSIGN_UUID` are answered

Rendering already worked (previous sections). What did not was **resource lifecycle**: the device
accepted the guest's create/attach/unref traffic but only ever applied it to its own bookkeeping, so
the host renderer's view drifted from the guest's the longer a session ran. Four defects, all on the
paths a real app exercises continuously:

| Defect | Was | Now |
|---|---|---|
| `RESOURCE_ATTACH_BACKING` (3D) | Gathered the guest pages into a temporary `Vec`, handed gfxstream `bytes.as_mut_ptr()`, then dropped the `Vec`. `VirtioGpuResource::AttachIov` **stores** `iov_base` in `mIovs` and dereferences it on later transfers (`host/VirtioGpuResource.cpp:240`) — so the renderer was left holding a pointer into freed host memory. | Resolves each guest mem entry to its **stable host address** via `host_addr` (exactly as `RESOURCE_CREATE_BLOB` already did) and forwards one iovec per entry. Failure now answers `ERR_UNSPEC` instead of a silent `OK`. |
| `RESOURCE_UNREF` | Removed the id from the device's 2D map only. `stream_renderer_resource_unref` was never called from anywhere. Every texture, buffer and ColorBuffer an app ever created stayed live in gfxstream for the life of the VM. | Routes through `release_3d_resource`: retract the published MTLTexture → drop any live blob mapping → detach iovecs → `stream_renderer_resource_unref`. |
| `RESOURCE_DETACH_BACKING` | Cleared the 2D `backing` vec; `stream_renderer_resource_detach_iov` was never called. The guest revoked the memory while gfxstream kept pointing at it. | Routes into the renderer and clears `res3d_backing`. |
| `SET_SCANOUT_BLOB` (0x010d), `RESOURCE_ASSIGN_UUID` (0x010b) | Fell through to `ERR_UNSPEC` — while the device advertises `F_RESOURCE_BLOB`, which is what makes a guest use blob scanout in the first place. | Implemented. `SET_SCANOUT_BLOB` adopts the blob's own width/height (they arrive in the command, not from a prior `RESOURCE_CREATE_2D/_3D`); `ASSIGN_UUID` returns a stable per-resource `OK_RESOURCE_UUID`. |

**New hazard introduced by the unref fix, and closed.** Calling `stream_renderer_resource_unref` for
real means the scanout ColorBuffer's `id<MTLTexture>` — published to the display thread in
`scanout.metal` and blitted there without holding any device lock — can now be freed underneath that
thread. `Gpu` therefore tracks `metal_scanout_res` (which resource the published texture belongs to)
and retracts the handle before releasing that resource, so the display falls back to the CPU
framebuffer for that frame rather than blitting a freed texture.

Also: the 3D commands that discarded the renderer's status (`resource_create_3d`, both `transfer_*`,
`submit_cmd`) now report it through a rate-limited `warn_rc`. They still answer `OK_NODATA` — the
guest's `gfxstream_vk` carries its own error signalling over the ASG ring, and converting a host
hiccup into a mid-stream virtio `ERR_UNSPEC` is a larger behavioural change than the failure it
reports — but a host renderer that has started failing no longer looks identical to a healthy one.

## Verified (live guest, Apple M2)

Boot with the fixed device, `./run-control64.sh --gpu3d`:

- `[gpu3d] zero-copy present ACTIVE: scanout ColorBuffer res=15 MTLTexture=0x7a232e0000 1024x768` —
  the zero-copy present path is **unregressed** by the lifecycle changes.
- `sys.boot_completed=1`; `dumpsys SurfaceFlinger` →
  `GLES: Google Inc. (Apple), ANGLE (Apple, Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x1B000208)), Apple-26.0.99)`
  — still the real host-GPU path, not SwiftShader.
- **Real workload**: 3DMark (`com.futuremark.dmandroid.application`) installed and run through
  4 launch/force-stop cycles — the churn that exercises create/attach/unref hardest. Its full UI
  renders through the Metal path (screenshot: `screenshots/metal-3dmark-app.png`, captured with
  `src=metal`, i.e. read off the presented MTLTexture). Guest log:
  `I/ANGLE: Renderer (Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x1B000208)))`.
  Zero gfxstream aborts, zero `[gpu3d]` failures, VMM alive, scanout generation still advancing
  (`gen=459`). (The 7 `Runtime aborting` lines in that log are the Bluetooth HAL crash-loop —
  a vsock-HAL issue, unrelated to graphics.)
- Tests: 54 pass in `aemulator-hvf` (5 new lifecycle/command tests in `virtio::gpu::tests_3d`),
  26 in the Studio crate. Clippy clean on the changed files.

## Configuration drift fixed (same pass)

The Studio's `GPU3D_EXTRA_CMDLINE` claimed to be "mirrored from run-android64.sh" but was missing
`hwcomposer.display_finder_mode=drm`, `hwcomposer.mode=client` and `gltransport.drawFlushInterval=800`
— i.e. the app shipped a graphics configuration nobody validates end-to-end, and the ranchu composer
was left looking for a Cuttlefish vsock config server we do not run. Now identical, and pinned by
`spawn::tests::gpu3d_cmdline_matches_the_validated_harness_configuration`.

`run-android.sh` and `run-control64.sh` still defaulted to `egl=emulation`, the pairing documented in
three places as an immediate VMM abort on macOS (no host OpenGL → `FATAL in getEmulationGl`). Both now
default to the same validated `egl=angle` set as `run-android64.sh`.

## Measured guest GPU capability (`cmd gpu vkjson`, this build)

`Virtio-GPU GFXStream (Apple M2)`, **Vulkan 1.3.0**, `INTEGRATED_GPU`, 85 device extensions.

| Capability | State | Consequence |
|---|---|---|
| `VK_ANDROID_external_memory_android_hardware_buffer` | ✅ | AHardwareBuffer import works — the path camera / video / `SurfaceTexture` need. |
| `VK_KHR_sampler_ycbcr_conversion` | ✅ | YUV sampling, i.e. video frames as textures. |
| `VK_EXT_descriptor_indexing`, `VK_KHR_shader_float16_int8` | ✅ | Modern shader feature set present. |
| `tessellationShader`, `textureCompressionASTC_LDR` / `ETC2`, `shaderInt64`, `multiViewport` | ✅ | Mobile texture formats and geometry pipeline covered. |
| `geometryShader` | ❌ | Apple GPUs have none. **GLES 3.2 is unreachable** (it requires geometry + tessellation), so `androidboot.opengles.version=196609` (3.1) is the honest ceiling, not a shortfall to raise. |
| `VK_KHR_ray_query` / `ray_tracing_pipeline` / `acceleration_structure` | ❌ | No Vulkan ray tracing. Metal 3 has RT, but MoltenVK does not expose the Vulkan RT extensions and gfxstream does not forward them. Structural, not a wiring bug. |

3DMark's own verdicts on this device: **Sling Shot Extreme (OpenGL ES 3.1) — compatible**. Its Vulkan
tests (Wild Life, Solar Bay, Steel Nomad Light, Opacity Micromaps) all report incompatible, but do
**not** read that as a measured Vulkan gap: Solar Bay and Opacity Micromaps need ray tracing (absent,
confirmed above), Steel Nomad Light logs `memory requirement: 8.0` GB against this harness's 4 GB
guest, and 3DMark's device-recognition lookup itself failed offline
(`MyDeviceNotRecognizedException`), which gates its compatibility matrix. Each remaining test needs
its own root-cause before claiming a cause.

## Known, still open

- **`DRM_IOCTL_VIRTGPU_GET_CAPS failed with Invalid argument`** — logged once per app process by the
  guest's Mesa, which treats it as non-fatal and falls back to compiled-in defaults. The kernel
  negotiated our capset fine (`cap set 0: id 3, max-version 0, max-size 104`), so the guest must be
  asking for a capset id we do not advertise (`GFXSTREAM_CAPSETS = &[3]`) — gfxstream's host
  `fillCaps` also implements MAGMA(7), GLES(8) and COMPOSER(9). Advertising them was **not** done
  speculatively: capset 8 tells the guest a host-GLES transport exists, and this host build is
  `GuestVulkanOnly`, where a GLES client aborts the renderer. The decisive next step is to log the
  requested `cap_set_id` — which needs a guest-side Mesa build or a kernel trace, since an ioctl the
  kernel rejects never reaches the device.
- **`HWUI: Device claims wide gamut support, cannot find matching config`** + `Failed to initialize
  101010-2 format` — the display advertises wide colour but ANGLE-on-gfxstream exposes no matching
  FP16/RGBA1010102 EGLConfig. HWUI falls back to 8888 cleanly; cosmetic, but it is a genuine
  capability mismatch worth reconciling.
- **Depth formats `D24X` / `D24S`** are skipped by minigbm (`virgl_add_combination`) because
  `VK_FORMAT_D24_UNORM_S8_UINT` does not exist on Apple GPUs (they have `D32_SFLOAT_S8_UINT`). This is
  hardware, not a gap in the port — the gralloc combination list is correct to omit them.
- `cf_lock::tests::second_acquire_is_refused_while_first_is_held` is a **pre-existing flake** (12/12
  passes alone, ~5/8 under the parallel suite; nothing else in the suite touches that lock). Unrelated
  to graphics, but it should not stay flaky — it guards the single-writer lock the Studio depends on.

---

# Phase Metal — ZERO-COPY present ACTIVE: the scanout ColorBuffer's own `id<MTLTexture>` is presented straight into the CAMetalLayer, GPU→screen, NO CPU readback — Android home screenshot-verified

**SUCCESS — end-to-end zero-copy Metal present is now ACTIVE.** Follow-up #2 below (the readback→zero-copy
optimization) is done. Android boots to the launcher home surface, and every present is the gfxstream
ColorBuffer MTLTexture blitted directly into the window's CAMetalLayer drawable — the per-frame
`stream_renderer_transfer_read` readback is gone from the hot path (it survives only as a forced fallback).

Screenshot: `screenshots/metal-zerocopy-home.png` — the Android launcher home surface (all-apps drawer:
Calendar / Camera / Clock / Contacts / Files / Gallery / Messaging / Phone / Search / Settings / … app
grid, "Search apps" bar, status-bar clock 3:23), keyguard dismissed by the scripted swipe, rendered
GPU→screen off the zero-copy Metal texture with correct colors.

## Proof (observed in the display run `/tmp/metal-zerocopy-run2.log`, `AEMU_MTL_DUMP` PPM)

Both ACTIVE markers fire, and NO readback of any kind runs on the active present path:

```
[gpu3d] zero-copy present ACTIVE: scanout ColorBuffer res=12 MTLTexture=0x77b39ca580 1024x768 — no stream_renderer_transfer_read
[metal-present] zero-copy present ACTIVE: src MTLTexture 1024x768 pixelFormat=70 device=Apple M2
```

- `stream_renderer_transfer_read readback` markers on the active path: **0**
- `[gpu3d] zero-copy present unavailable` fallbacks: **0**
- `[metal-present] … unsupported by CAMetalLayer` (CPU-readback) fallbacks: **0**
- `AEMU_MTL_DUMP … ok=true` (dumped straight off the presented MTLTexture): **32**
- Presented-texture PPM (`screenshots/metal-zerocopy-home.ppm`, dumped FROM the Metal texture): **1024×768, 99% non-black**, correct channel order (matches the on-screen launcher).
- Guest renderer string: `Renderer (Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x1B000208)))` — the gfxstream_vk path on Apple M2, **NOT SwiftShader** (SwiftShader count: 0).
- Readback safety net intact: with `AEMU_PRESENT_READBACK=1` the zero-copy markers vanish (0/0), `flush_3d` fills the CPU scanout (`non_zero_fb_bytes=3145728`, fully painted), and the frame still renders (`screenshots/metal-readback-fallback.ppm`, 99% non-black) — the fallback is preserved.

## The two fixes that activated zero-copy

**1. Host (gfxstream, Apache-2.0) — return a real `id<MTLTexture>` from the ColorBuffer's image memory.**
Previously `stream_renderer_get_color_buffer_mtl_texture` → `getColorBufferMetalTexture` called
`vkGetMemoryMetalHandleEXT(MTLTEXTURE)`, which returned `VK_SUCCESS` with a **NULL** handle because the
ColorBuffer image memory is exported as an `MTLHeap` (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT`) —
MoltenVK exposes no standalone texture for a heap-suballocated image (247× `vkGetMemoryMetalHandleEXT(MTLTEXTURE) failed`
in the prior run). Fixed in `SampleRepos/platform_hardware_google_gfxstream/host/vulkan/VkCommonOperations.cpp`:
  - `createVkColorBufferLocked` (~:2478) chains `VkExportMetalObjectCreateInfoEXT{ .exportObjectType = VK_EXPORT_METAL_OBJECT_TYPE_METAL_TEXTURE_BIT_EXT }` into the ColorBuffer image's `pNext` **on MoltenVK only**, ahead of the existing `VkExternalMemoryImageCreateInfo` — so the MTLHeap external-memory sharing the boot baseline depends on is **preserved** and the image is *additionally* Metal-texture-exportable.
  - `getColorBufferMetalTexture` (~:3359) rewritten to call `vkExportMetalObjectsEXT(device, &VkExportMetalObjectsInfoEXT{ .pNext = &VkExportMetalTextureInfoEXT{ .image = infoPtr->image, .plane = VK_IMAGE_ASPECT_COLOR_BIT } })` and return the resulting `.mtlTexture` (the device-level `vkExportMetalObjectsEXT` dispatch pointer is loaded in `goldfish_vk_dispatch.cpp`). `VK_EXT_metal_objects` was already enabled on the device.
  - Rebuilt the dylib ONLY: `ninja -C SampleRepos/platform_hardware_google_gfxstream/build-host libgfxstream_backend.dylib` (no VMM rebuild needed — the Rust binary loads it by `@rpath`). The MTLHeap external-memory + metal-texture-export **co-exist**: a full boot-to-launcher succeeded, so the baseline is not regressed.

**2. VMM (`src/display/metal_shim.m`) — present the RGBA8Unorm ColorBuffer without a CPU readback.**
The imported ColorBuffer is `MTLPixelFormatRGBA8Unorm` (fmt **70**) — SurfaceFlinger's client-composition
output. The shim previously only accepted `BGRA8Unorm` and bailed (`-6`) to a CPU-readback present for
anything else. Now `aemu_metal_present_texture` also accepts `RGBA8Unorm(_sRGB)` + `RGBA16Float`, sets the
CAMetalLayer's pixelFormat to the source's own format (macOS CAMetalLayer scans out RGBA8Unorm), and does the
matched-format GPU blit into the drawable — GPU→screen, no readback. `aemu_metal_texture_dump` now honors the
source pixel format for channel order (RGBA vs BGRA) so the PPM colors match the screen. (The default,
non-gfxstream build never compiles this file, so the hermetic baseline is unchanged.)

## Regression (re-verified this pass)

- **Default build hermetic:** `otool -L target/debug/aemulator-hvf` = Hypervisor + winit frameworks
  (AppKit / QuartzCore / CoreGraphics / CoreFoundation / Foundation / CoreVideo) only — **no**
  Vulkan / MoltenVK / Metal / gfxstream.
- **`cargo test`: 32 passed (default) / 39 passed (gfxstream)** — unchanged.
- **Phase-1 Alpine** (`./run-linux.sh`, default build): MIDR spoof `real 0x…610f0000 → 0x511f8041` (impl **0x51**,
  readback-confirmed), `Booting Linux on physical CPU [0x511f8041]`, `virtio_blk virtio2: [vda] 131072
  512-byte logical blocks (67.1 MB)`, **0** kernel panics. (The later userspace "Mounting boot media: failed"
  is baseline harness behavior — the default build is byte-identical to baseline since only the gfxstream-only
  `metal_shim.m` changed.)

---

# Phase Metal (RenderPresent) — Android renders through gfxstream → MoltenVK → Metal and is presented in our window: the HOME SCREEN, screenshot-verified

**SUCCESS.** With the host-visible blob memory + ASG ring from Phase-4 pass 12 already live, the one
remaining wall — the host gfxstream renderer aborting mid-render — is fixed, and Android now boots all
the way to **`sys.boot_completed`** and the **launcher home screen**, rendered by SurfaceFlinger's
RenderEngine on **ANGLE → gfxstream_vk → our virtio-gpu ASG ring → host MoltenVK → Metal (Apple M2)**,
with the composited ColorBuffer read back into our scanout and presented in the window.

- Screenshot: `screenshots/metal-android-home.png` — the Android home screen (status-bar clock 3:22 +
  battery; Camera / Android / Messaging / Phone hotseat; Gallery on the grid; wallpaper), captured from
  the guest framebuffer (`AEMU_FB_DUMP`) after the scripted keyguard swipe.
- Renderer-string proof (guest logcat, `/tmp/metal-display1.log`):
  `SurfaceFlinger: renderer : ANGLE (Apple, Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x1B000208)), Apple-26.0.99)`
  — the gfxstream_vk path on Apple M2, **NOT SwiftShader** (SwiftShader/pastel string count in the whole
  run: **0**).
- Host in-process renderer proof: `VkCommonOperations.cpp: MoltenVK is supported, enabling Vulkan
  portability` + `Selecting Vulkan device: Apple M2, Version: 1.1.357` + `stream_renderer_init
  (VK/MoltenVK)`.

`triangleClosed = true` by the strict definition: the frame on screen was produced via
gfxstream → MoltenVK → Metal (Apple GPU), not the guest CPU.

---

## The one blocker that was in the way, and the fix (host gfxstream, Apache-2.0 — not GPL)

Phase-4 pass 12 had the blob/ASG/present machinery fully working but the run aborted mid-render on:

```
reservedunmarshal_extension_struct, Unhandled Vulkan structure type
  VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT [1000237000], aborting.
```

**Root cause (proven from the source):** the vendored gfxstream host's cereal codegen is *internally
inconsistent* for `VkPhysicalDeviceMemoryBudgetPropertiesEXT`. The size table
(`goldfish_vk_extension_structs.cpp` → `goldfish_vk_extension_struct_size_with_stream_features`) has a
case returning `sizeof(VkPhysicalDeviceMemoryBudgetPropertiesEXT)` (nonzero → the struct is treated as
"known"), but `reservedunmarshal_extension_struct` (`goldfish_vk_reserved_marshaling.cpp`) has **no
case** for it, so decoding the guest's pNext chain falls to the `default:` arm → `abort()` → the whole
VMM process dies. The guest's in-guest ANGLE enables `VK_EXT_memory_budget` (because host MoltenVK
advertises it) and chains this struct into `vkGetPhysicalDeviceMemoryProperties2` during device
bring-up, tripping the abort.

**Fix chosen — stop advertising the extension to the guest (no wire-format guessing).** In
`host/vulkan/VkDecoderGlobalState.cpp`, `on_vkEnumerateDeviceExtensionProperties` now strips
`VK_EXT_memory_budget` from the device-extension list returned to the guest (a 15-line
`std::remove_if`). Because the extension is a device-level extension, the Vulkan spec forbids ANGLE from
using `VkPhysicalDeviceMemoryBudgetPropertiesEXT` unless the extension is enabled at device creation —
and it can't enable an extension the host doesn't advertise. So the struct is never sent → the decoder
never hits the unhandled case → no abort. This is symmetric (the host write-back path also never sees
it) and needs no guessing of the reserved-marshaling wire layout.

- Why not add the missing unmarshal case instead: that requires hand-writing matching
  reserved-unmarshal + marshal (write-back) + deepcopy + handlemap helpers whose serialized wire format
  must byte-match the Mesa guest encoder in the Android image — high risk for a query the guest only
  uses for opportunistic memory budgeting. The extension is fully optional; dropping it is lossless for
  correctness.
- Why not zero the size-table entry: the guest's own (matching) size table still returns nonzero, so it
  writes the struct body onto the wire; a host that returns 0 would skip without advancing the read
  pointer → stream desync. Rejected.

Rebuild: `ninja -C build-host libgfxstream_backend.dylib` (3 files recompiled). The Rust VMM binary
links the dylib by `@rpath` and loads it at run time, so no VMM rebuild was required to pick up the fix.

**No VMM/Rust source changes were needed this phase.** The virtio-gpu host-visible blob memory,
`hv_vm_map` ASG ring, fence callback, and the ColorBuffer present path (`Gpu::flush_3d` →
`Renderer3d::read_pixels` = `stream_renderer_transfer_read` → scanout framebuffer) were all already in
place from Phase-4 pass 12. The host decoder abort was the sole thing stopping a frame from completing.

---

## The present path, observed working end-to-end

- SurfaceFlinger runs `hwcomposer=ranchu display_finder_mode=drm mode=client`: it client-composites the
  whole screen into one output ColorBuffer on the host GPU (gfxstream_vk → MoltenVK → Metal), then
  presents it to our virtio-gpu KMS scanout via `SET_SCANOUT` + `RESOURCE_FLUSH`.
- On `RESOURCE_FLUSH`, `Gpu` detects the flushed/bound resource is a gfxstream 3D resource and calls
  `flush_3d(rid)` → `read_pixels(rid, w, h, …)` (host `stream_renderer_transfer_read`) to copy the
  rendered ColorBuffer into the `ScanoutHandle` framebuffer, then bumps the scanout generation.
- The display thread (`src/display/mod.rs`) blits that framebuffer into the window on each generation
  change (and, with `AEMU_FB_DUMP`, writes it as a PPM for headless-independent capture).

Observed in the display run (`/tmp/metal-display1.log`):

```
[display] window open: 1024x768 — presenting guest scanout
[display] scanout gen=8   non_zero_fb_bytes=836805     <- first real gfxstream frame lands in the scanout
[display] scanout gen=244 non_zero_fb_bytes=3145728    <- fully painted (lock screen)
KeyguardTransitionRepository: FINISHED … (from=LOCKSCREEN, to=GONE, value=1.0)   <- swipe dismissed keyguard
[display] scanout gen=309 non_zero_fb_bytes=3145728    <- HOME SCREEN painted → screenshot captured
```

21+ host-visible blob maps succeeded (`[gpu3d] map_blob res=N -> ipa … map_info 0x1 (host-visible ASG
ring live)`, sizes 0x4000…0x800000), 0 `hv_vm_map` failures, 0 alignment failures — the ASG ring is
healthy. Host gfxstream created real VkDevices on MoltenVK for surfaceflinger, launcher3, systemui,
bootanimation, etc. (`Created VkDevice … engine:ANGLE`, seen in the headless run
`/tmp/metal-boot1.log`).

---

## Boot result (both runs, `--features gfxstream` + `--gpu3d`)

- Headless (`./run-android64.sh --gpu3d`, `/tmp/metal-boot1.log`): reached **`sys.boot_completed=1`**;
  no host decoder abort; SurfaceFlinger `Threaded RenderEngine with SkiaGL Backend (Ganesh)`,
  `BootAnimationShownTiming start`. (Non-fatal Cuttlefish service noise — bluetooth/com.android.phone
  restart churn, media.resource_manager lazy-start — is unrelated to graphics and does not block boot.)
- Display (`… AEMU_FB_DUMP=… AEMU_SWIPE_UNLOCK=115 ./run-android64.sh --gpu3d --display`,
  `/tmp/metal-display1.log`): window opened, scanout filled with real gfxstream frames, keyguard
  dismissed by the scripted swipe, **home screen rendered** → `screenshots/metal-android-home.png`.

The kernel cmdline carries `audit=0 loglevel=3` (SELinux-permissive avc-denied flood silenced), added in
pass 12.

---

## Regression — default build hermetic + Phase-1 green

- **Default build hermetic:** `cargo build` links **no** gfxstream/MoltenVK/Vulkan/`Metal.framework`
  (`otool -L`: Hypervisor + the pre-existing winit window frameworks — AppKit / QuartzCore /
  CoreGraphics / Foundation / CoreVideo — only). The `--features gfxstream` build additionally links
  `@rpath/libgfxstream_backend.dylib` + `Metal.framework`, as expected.
- **`cargo test`: 32 passed (default) / 39 passed (gfxstream)** — unchanged (no test churn; the only
  change is in the vendored host dylib).
- **Phase-1 Linux boot** (`./run-linux.sh`, default build): Alpine 6.6.134 boots over virtio-**mmio**,
  spoofed MIDR `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)` — no
  panic.

No git commits made.

---

## Exact repro

```
# 1. Rebuild the host gfxstream dylib with the memory_budget fix (once):
ninja -C "$TP_GFXSTREAM_BUILD_HOST" libgfxstream_backend.dylib

# 2. Headless — verify boot_completed + no host abort:
rm -f testdata/cf64/img/userdata.img
SKIP_PREP_SUPER=1 SKIP_PREP_32BIT=1 SKIP_PREP_APEX=1 SKIP_PREP_VSOCK_HALS=1 SKIP_PREP_DEBRAND=1 \
  ./run-android64.sh --gpu3d
#   grep for: "Created VkDevice … surfaceflinger engine:ANGLE", "sys.boot_completed=1",
#   and the ABSENCE of "Unhandled Vulkan structure type … MEMORY_BUDGET … aborting".

# 3. Display + home-screen screenshot (reuses the userdata from step 2):
SKIP_PREP_SUPER=1 SKIP_PREP_32BIT=1 SKIP_PREP_APEX=1 SKIP_PREP_VSOCK_HALS=1 SKIP_PREP_DEBRAND=1 \
  AEMU_FB_DUMP=/tmp/aemu-fb-metal.ppm AEMU_SWIPE_UNLOCK=115 ./run-android64.sh --gpu3d --display
#   after "from=LOCKSCREEN, to=GONE, value=1.0":
sips -s format png /tmp/aemu-fb-metal.ppm --out screenshots/metal-android-home.png
#   renderer proof (hvc2 logcat): "renderer : ANGLE (Apple, Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2)…"
```

Only one VMM may hold the `testdata/cf64` disk images at a time — a second concurrent instance corrupts
the shared `userdata` ext4. Kill any stale VMM (`pkill -f "aemulator-hvf run-android"`) before a run.

---

## Zero-copy Metal present — SHIPPED end-to-end (follow-up #2 below is now DONE)

The follow-up #2 optimization — remove the `stream_renderer_transfer_read` readback + CPU blit and put
the gfxstream ColorBuffer on screen GPU→GPU — is **live and screenshot-verified**. The full path is:
host gfxstream exports the scanout ColorBuffer as a real `MTLTexture`
(`stream_renderer_get_color_buffer_mtl_texture` via `vkExportMetalObjectsEXT`) → the display thread
hands that texture straight to the CAMetalLayer presenter (`aemu_metal_present_texture`) → on screen.
**No GPU→CPU→GPU roundtrip.**

The last hop that was still falling back — the CAMetalLayer pixel-format mismatch — is fixed by a
**format bridge** in `src/display/metal_shim.m`. The gfxstream ColorBuffer is RGBA8Unorm (fmt 70), but
macOS CAMetalLayer only scans out BGRA8Unorm. So the drawable is ALWAYS BGRA8Unorm and a cached
render-pipeline (`aemu_bridge_pipeline`: fullscreen-triangle vertex + a fragment that samples the RGBA
source) writes the sampled `float4(R,G,B,A)` into the BGRA drawable — Metal stores it in BGRA byte
order automatically, so colors are correct with no manual swap, GPU-only. A BGRA source still takes the
1:1 blit fast path. The one-time log prints `path=blit` or `path=bridge`.

### Observed proof (run 2026-07-25, `--features gfxstream` + `--gpu3d --display`, log `/tmp/zc-bridge.log`)

```
[gpu3d] zero-copy present ACTIVE: scanout ColorBuffer res=15 MTLTexture=0x7626c4a580 1024x768 — no stream_renderer_transfer_read
[metal-present] zero-copy present ACTIVE: src MTLTexture 1024x768 pixelFormat=70 path=bridge device=Apple M2
[display] AEMU_MTL_DUMP 1024x768 texture=0x7626c4a580 ok=true
SurfaceFlinger: Boot is finished (6441 ms)
init: processing action (sys.boot_completed=1) ...
KeyguardTransitionRepository: FINISHED transition: (from=LOCKSCREEN, to=GONE, value=1.0)
```

- **`path=bridge` engaged; ZERO `"falling back to readback present"` lines** from the present shim.
- **Host export still working:** `[gpu3d] zero-copy present ACTIVE … — no stream_renderer_transfer_read`;
  **0** `zero-copy present unavailable / falling back to transfer_read` on the active path.
- **Presented Metal texture is real Android UI:** `AEMU_MTL_DUMP` → `screenshots/metal-bridge-home.ppm`
  → `screenshots/metal-bridge-home.png` — 1024×768, **100% non-black**, mean luminance 246.6/ch, 56
  distinct colors: the Android **Settings** screen (Search Settings, Location, Passwords & passkeys,
  Accounts and backup, Safety & emergency, Accessibility) with status-bar clock 3:22 + green battery.
  Correct colors (green battery, blue/red/purple icons) confirm the RGBA→BGRA bridge is right.
- **Guest renderer still real GPU:** `SurfaceFlinger: renderer : ANGLE (Apple, Vulkan 1.3.0 (Virtio-GPU
  GFXStream (Apple M2) (0x1B000208)), Apple-26.0.99)` — SwiftShader/pastel count in the whole run: **0**.
- On the zero-copy path the CPU softbuffer scanout is deliberately empty (`non_zero_fb_bytes=0`) — the
  frame never touches the CPU; the `AEMU_MTL_DUMP` GPU capture is what proves the on-screen content.

### Graceful-degradation safety net (`AEMU_PRESENT_READBACK=1`, log `/tmp/zc-readback.log`)

Forcing the legacy CPU-readback present still renders: `Boot is finished (14085 ms)`, scanout fills to
`non_zero_fb_bytes=3145728` (full 1024×768×4), **0** `metal-present` lines (zero-copy correctly bypassed
when forced off). The present path degrades cleanly.

### Regression (this pass)

- **Default build hermetic:** `otool -L target/debug/aemulator-hvf` → Hypervisor + winit frameworks
  (AppKit / QuartzCore / Foundation / CoreFoundation) **only** — no Vulkan/MoltenVK/gfxstream/Metal.framework.
- **`cargo test`: 32 passed (default) / 39 passed (gfxstream)** — green, unchanged.
- **Phase-1 Linux boot** (`./run-linux.sh`, default build): MIDR real 0x61 → spoof `0x511f8041`
  (impl 0x51) accepted, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)`, Linux
  6.6.134, **0 panics** (drops to the expected initramfs recovery shell — scratch disk has no rootfs).

No git commits made.

---

## Follow-ups (not blocking the Metal milestone)

1. The Cuttlefish service noise (bluetooth abort loop, com.android.phone restarts, lazy
   media.resource_manager) is cosmetic here but would matter for a fully-stable session — a later
   image-prep pass could neuter the vsock-dependent bits, as earlier passes did for light/sensors.
2. ~~Zero-copy present~~ **DONE** — see the "Zero-copy Metal present — SHIPPED end-to-end" section above.
   gfxstream's ColorBuffer MTLTexture is presented straight into the CAMetalLayer (blit for BGRA,
   render-pass bridge for RGBA); the readback + CPU blit are gone from the active path.
3. Other ANGLE-chained extension structs could surface the same codegen gap on different images/driver
   versions (e.g. another properties struct in the size table but missing from the unmarshal switch).
   The same enumerate-strip technique (or, properly, regenerating the host cereal from a complete
   registry) applies.
