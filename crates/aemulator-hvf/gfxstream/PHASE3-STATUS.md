# Phase 3 — gfxstream 3D on Apple Silicon: STATUS

Scope of this doc: the full Phase 3 arc — **3a** host renderer, **3b** CAMetalLayer presenter,
**3c** virtio-gpu 3D transport, **3d** Linux-guest end-to-end attempt + integration. 3a has its own
detailed writeup in `PHASE3A-STATUS.md`; this file is the consolidated status + the honest 3d
outcome + the guest-driver decision.

**One-line result:** the host pipeline is real and proven on an Apple M2 — gfxstream initializes on
MoltenVK/Metal in-process with HVF, the CAMetalLayer presenter shows a frame on the glass, and a
**real Alpine aarch64 Linux guest negotiates our gfxstream-backed VIRGL device end-to-end at the
kernel level** (VIRGL + CONTEXT_INIT features, capset queried straight out of the live gfxstream host
renderer, `/dev/dri/renderD128` created). The only missing rung — a guest **3D application** drawing
a triangle — needs a guest userspace that ships Mesa's `gfxstream_vk`, which no general-purpose Linux
distro does; that proof is **deferred to Phase 4** (Android/Cuttlefish ships `gfxstream_vk`
natively). `endToEnd3d = partial`.

---

## Build / regression discipline (all green)

| Build | Result |
|---|---|
| `cargo build` (default, no feature) | ✅ green, warning-clean |
| `cargo build --features gfxstream` | ✅ green, warning-clean; **links** `libgfxstream_backend.dylib` + Metal + Hypervisor |
| `cargo test` (default) | ✅ 18 passed |
| `cargo test --features gfxstream` | ✅ 21 passed |
| clippy (both) | new code clean; remaining lints are pre-existing (Console/Rng `Default`, midr-probe) |

**Default build stays hermetic.** `otool -L` on the default `aemulator-hvf` shows **no**
gfxstream / Vulkan / MoltenVK / Metal link — only `Hypervisor.framework`. All gfxstream/Metal/MoltenVK
is behind `--features gfxstream` **and** `#[cfg(target_os = "macos")]` in `build.rs`.

### Regression — Phase 1 (headless boot to shell)
`./run-linux.sh` (default build), pipe `echo P3_$((8*8))` into the shell → guest prints **`P3_64`**.
Boot-to-interactive-shell path unchanged. (`/tmp/p1_regress.log`.)

### Regression — Phase 2 (`--display`, 2D console)
`./run-linux.sh --display` (default build) renders the guest framebuffer console in the winit window.
Screenshot: `gfxstream/evidence/phase2-display-regression.png`. Crucially the guest dmesg there reads
`[drm] features: -virgl … number of cap sets: 0` — i.e. **VIRGL OFF, pure 2D**, exactly Phase 2. The
`--gpu3d` upgrade is fully opt-in and invisible to the default path. **`defaultBuildUnaffected = true`.**

---

## 3b — CAMetalLayer presenter (PROVEN on-screen)

`aemu-display-selftest` (built only under `--features gfxstream`) attaches a `CAMetalLayer` to the
winit `NSView` and blits a host-generated test pattern into the drawable → `presentDrawable`. Captured
on the Apple M2 GPU: **`gfxstream/evidence/3b-metal-presenter.png`** (gradient + checker, first present
`rc=0`, `CAMetalLayer attached=true`). This is the exact surface a gfxstream ColorBuffer readback is
presented through. `presenterProven = true`.

**Definitive finding (unchanged from 3b):** `gfxstream_backend_setup_window` **cannot** present on
macOS with this renderer build — its Vulkan display surface (`host/vulkan/DisplaySurfaceVk.cpp`) is
`#ifdef _WIN32 … #else abort("Unimplemented")`, and the GLES display path isn't compiled under
`GuestVulkanOnly`. So we do **not** use gfxstream's own subwindow. Instead the transport reads a
flushed 3D ColorBuffer back (`stream_renderer_transfer_read_iov`) into our 2D scanout, which the
CAMetalLayer presenter shows. That decoupling is what makes the Mac path viable.

---

## 3c + 3d integration — virtio-gpu 3D transport, wired into the VMM

The virtio-gpu device (`src/virtio/gpu.rs`) upgrades to a `VIRTIO_GPU_F_VIRGL` + `F_CONTEXT_INIT`
device when a `Renderer3d` is attached, routing the guest 3D control stream into the gfxstream FFI
(`src/virtio/gpu3d.rs`): `CTX_CREATE`, `RESOURCE_CREATE_3D`, `*_ATTACH_BACKING`, `TRANSFER_*_3D`,
`SUBMIT_3D`, `CREATE_BLOB`, fences, `GET_CAPSET_INFO`/`GET_CAPSET`. Two implementations:
`GfxstreamRenderer` (real forwarder) and `MockRenderer` (recording double for the hermetic routing
tests).

**New in this phase — the real forwarder is now wired into the boot path** behind a `--gpu3d` flag
(`machine.rs::build_gpu`, `#[cfg(feature = "gfxstream")]`):
1. `stream_renderer_init` (VK/MoltenVK, no GLES) runs in-process alongside HVF.
2. On success, virtio-gpu is built with `Gpu::with_renderer(GfxstreamRenderer)`.
3. On failure (e.g. MoltenVK not staged) it logs and **falls back to the 2D device** so the guest
   still boots. `--gpu3d` on the default build is a warned no-op.

Runner: `run-linux-gpu3d.sh` (stages the MoltenVK loader/ICD in `target/debug/lib64/vulkan/` exactly
like `gfxstream/run.sh`, signs, boots the Alpine guest with `--gpu3d`).

### 3d empirical result — a REAL guest kernel drives the pipeline (observed)

`./run-linux-gpu3d.sh` (headless), Apple M2. Host renderer + guest dmesg, verbatim key lines
(`gfxstream/evidence/gpu3d-guest-negotiation.txt`):

```
[gpu3d] stream_renderer_init returned 0
VkCommonOperations.cpp:1230] Selecting Vulkan device: Apple M2, Version: 1.1.357
[ok] virtio-gpu 3D (VIRGL+CONTEXT_INIT) 1024x768 → gfxstream host renderer, /dev/dri/renderD128
...
[drm] features: +virgl -edid -resource_blob -host_visible
[drm] features: +context_init
[drm] number of scanouts: 1
[drm] number of cap sets: 2
[drm] cap set 0: id 3, max-version 0, max-size 104      <- GFXSTREAM_VULKAN, size from the LIVE renderer
[drm] cap set 1: id 1, max-version 0, max-size 0        <- VIRGL compat
[drm] Initialized virtio_gpu 0.1.0 0 for a000600.virtio_mmio on minor 0
[drm] fb0: virtio_gpudrmfb frame buffer device
# ls -la /dev/dri/
crw-rw----  1 root video 226,   0  card0
crw-rw----  1 root video 226, 128  renderD128
```

How far the 3D pipeline goes — **proven against a real guest, not the mock:**
- ✅ Device advertises `VIRTIO_GPU_F_VIRGL` + `VIRTIO_GPU_F_CONTEXT_INIT`; the guest **accepts** both
  (`+virgl +context_init`).
- ✅ The guest DRM `virtio-gpu` driver loads and initializes.
- ✅ **Capset negotiated out of the live gfxstream host renderer**: the guest issued
  `GET_CAPSET_INFO`, which routed `GfxstreamRenderer::capset_info → stream_renderer_get_cap_set(3)`
  and returned the **real** `GFXSTREAM_VULKAN` capset `max-size 104`. (The mock returns 8 — so 104 is
  hard proof the bytes came from `libgfxstream_backend.dylib`, not the double.)
- ✅ The guest created both DRM nodes: `card0` **and the render node `renderD128`**.
- ✅ `stream_renderer_init` returned 0 selecting **Apple M2 / MoltenVK**, running **in the same
  process as HVF**, no abort/crash; clean teardown.

What does **not** happen (honest): no guest **3D application** runs, so no `SUBMIT_3D` gfxstream
command stream is generated and no rendered triangle is read back to the window. The guest **kernel**
sets the device up perfectly, but the actual 3D command stream only comes from a guest **userspace**
GL/VK driver — and this Alpine userspace has none that speaks gfxstream (see next section).

`transportWired = true`, `endToEnd3d = partial`.

---

## The guest-driver decision (why end-to-end 3D is deferred to Phase 4)

**Our host renderer is gfxstream, whose guest protocol is Mesa's `gfxstream_vk` Vulkan driver — a
different wire protocol than virglrenderer's.** A stock Linux guest's Mesa ships the **`virtio`/virgl**
Vulkan+GL driver, which encodes the *virgl* protocol; it does not drive gfxstream. So booting any
off-the-shelf distro gets you exactly what we observed — kernel-level VIRGL negotiation — but no app
can render, because there is no `gfxstream_vk` in userspace to emit the gfxstream command stream.

Routes assessed for getting a gfxstream-speaking guest, cheapest-first:

- **(a) Prebuilt aarch64 Linux rootfs shipping `gfxstream_vk` — not available.** The userspaces that
  ship `gfxstream_vk` are **Android** (Cuttlefish/goldfish) and **ChromeOS** (crosvm termina/borealis).
  Neither is a "download a rootfs and boot it" artifact: Cuttlefish is an Android target (that's
  Phase 4), and the ChromeOS guest images aren't distributed standalone. No Debian/Ubuntu/Alpine/Arch
  aarch64 image carries `gfxstream_vk`. The gfxstream repo's own `guest/` tree here holds only the
  **legacy GLES encoders** (`GLESv1_enc`, `GLESv2_enc`, …), not the modern Mesa Vulkan driver.
- **(b) Cross-build Mesa `-Dvulkan-drivers=gfxstream` for aarch64 + a busybox rootfs — a multi-hour
  sub-project, and impractical from this host.** There is no aarch64-linux cross-toolchain installed
  (`aarch64-linux-gnu-gcc` absent), and cross-compiling Mesa from **macOS** to Linux/aarch64 needs a
  full Linux sysroot (libdrm + `linux/virtio_gpu.h` + the Vulkan runtime), i.e. a Linux builder
  container. Even natively that's a 30-min+ Mesa build plus assembling a matching initramfs with a
  `CONTEXT_INIT`-capable virtio-gpu. Not justified to prove one triangle when Phase 4 gets it for free.
- **(c) Defer the 3D-app proof to Phase 4 — chosen.** **Android ships `gfxstream_vk` natively.** The
  Cuttlefish/AOSP guest is the intended, first-class gfxstream client; the transport we just proved a
  real Linux kernel negotiates is the same transport the Android guest will drive with a real command
  stream. **`guestDriverPath`: defer to Phase 4 (Android Cuttlefish `gfxstream_vk`).**

---

## Reproduce

```bash
cd crates/aemulator-hvf

# Default (Phase-1/2) — must stay green + hermetic:
cargo build --manifest-path Cargo.toml
./run-linux.sh              # headless boot → shell; `echo P3_$((8*8))` prints P3_64
./run-linux.sh --display    # 2D console in a window (VIRGL off, cap sets: 0)

# 3b presenter proof (screenshot):
OUT=/tmp/3b.png ./run-display-selftest.sh          # gradient+checker on the Apple M2 GPU

# 3d integration — real guest negotiates the gfxstream VIRGL device:
cargo build --manifest-path Cargo.toml --features gfxstream
./run-linux-gpu3d.sh        # boots Alpine with --gpu3d; dmesg shows +virgl, cap set id 3 size 104, renderD128
```

Evidence artifacts: `gfxstream/evidence/{3b-metal-presenter.png, phase2-display-regression.png,
gpu3d-guest-negotiation.txt}`.

---

## Next steps

1. **Phase 4 — Android/Cuttlefish guest** brings a `gfxstream_vk` userspace; run vkcube/an app and
   read the ColorBuffer back into the CAMetalLayer presenter to close the end-to-end triangle.
2. **Fence signalling to the guest ring.** `build_gpu` installs a no-op fence callback (kernel-only
   negotiation produces no guest fences). A real 3D app needs the `FENCE_CALLBACK` to signal the
   guest virtio-gpu fence ring (Phase 2 already models the ring) — wire `create_fence` completions
   back through the GIC SPI. Do this alongside the first app that submits real 3D work.
3. **`USE_EXTERNAL_BLOB` / zero-copy.** Current path is guest-blob + `transfer_*_iov` copies. For
   scanout-sized frames, external-blob + host-visible memory avoids the readback copy.
4. **Upstream durability.** The `emugl_config.cpp` MoltenVK ICD patch (see `PHASE3A-STATUS.md`) is a
   local, real macOS-correctness fix — keep it as a tracked patch against the vendored gfxstream tree
   and re-apply on re-sync.
