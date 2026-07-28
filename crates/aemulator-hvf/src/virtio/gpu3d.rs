//! virtio-gpu **3D / virgl transport** (Phase 3c) — the command-routing layer that turns the
//! Phase-2 2D dumb-framebuffer device into a `VIRTIO_GPU_F_VIRGL` device that forwards the guest's
//! 3D control stream into the gfxstream host renderer (`libgfxstream_backend.dylib`, proved live on
//! Apple-M2 in Phase 3a/3b, now over Mesa KosmicKrisp).
//!
//! **Entirely behind `--features gfxstream`.** The default build never compiles this module, so the
//! Phase-1/2 boot-to-shell path stays a pure 2D device with no gfxstream/Vulkan/Metal link.
//!
//! ## What this layer does
//! [`Gpu`](super::gpu::Gpu) parses each 3D control command (struct layouts per the canonical Linux
//! `virtio_gpu.h`) and calls the matching method on a [`Renderer3d`] trait object:
//!
//! | virtio-gpu command                | [`Renderer3d`] method            | gfxstream FFI                              |
//! |-----------------------------------|----------------------------------|--------------------------------------------|
//! | `CTX_CREATE` / `CTX_DESTROY`      | `context_create` / `_destroy`    | `stream_renderer_context_create/_destroy`  |
//! | `RESOURCE_CREATE_3D`              | `resource_create_3d`             | `stream_renderer_resource_create`          |
//! | `CTX_ATTACH/DETACH_RESOURCE`      | `ctx_attach/detach_resource`     | `stream_renderer_ctx_attach/detach_resource`|
//! | `RESOURCE_ATTACH_BACKING` (3D id) | `attach_backing`                 | `stream_renderer_resource_attach_iov`      |
//! | `TRANSFER_TO_HOST_3D`             | `transfer_to_host_3d`            | `stream_renderer_transfer_write_iov`       |
//! | `TRANSFER_FROM_HOST_3D`           | `transfer_from_host_3d`          | `stream_renderer_transfer_read_iov`        |
//! | `RESOURCE_CREATE_BLOB`            | `create_blob`                    | `stream_renderer_create_blob`              |
//! | `SUBMIT_3D`                       | `submit_cmd`                     | `stream_renderer_submit_cmd`               |
//! | `CTRL_HDR` fence flag / fences    | `create_fence`                   | `stream_renderer_create_fence`             |
//! | `GET_CAPSET_INFO` / `GET_CAPSET`  | `capset_info` / `fill_caps`      | `stream_renderer_get_cap_set/_fill_caps`   |
//!
//! ## Two implementations
//!   * [`GfxstreamRenderer`] — the real forwarder; each method is a thin `unsafe` call into
//!     [`crate::gfxstream::ffi`]. Requires a live host renderer (`stream_renderer_init`, done by
//!     `display::metal::init_gfxstream`) + the KosmicKrisp ICD staged. Wiring a real guest through
//!     it is deferred to Phase 4 (Android ships Mesa's `gfxstream_vk` guest driver natively).
//!   * [`MockRenderer`] — a recording double that logs every routed [`Call`] into a shared
//!     `Arc<Mutex<Vec<Call>>>`. This is what the Phase-3c smoke test drives: it proves the guest
//!     command bytes parse and route into the renderer trait **without** needing a live guest or GPU.
//!
//! ## Honest scope (Phase 3c)
//! There is no standalone Linux gfxstream guest driver here, so the end-to-end "a guest 3D app draws
//! a triangle on screen" proof is **deferred to Phase 4**. What 3c delivers + verifies: the device
//! advertises VIRGL, and the full 3D command set parses + routes into the renderer FFI boundary
//! (observed via the mock). Present coupling ([`Renderer3d::read_pixels`]) reads a flushed 3D
//! resource back into the 2D scanout [`Framebuffer`](super::gpu::Framebuffer) that the Phase-3b
//! CAMetalLayer presenter shows — the path a real guest frame will take.

use crate::gfxstream::ffi;
use std::os::raw::{c_char, c_int, c_uint, c_void};

// ---- virtio-gpu feature bits (virtio spec §5.7.3 / linux virtio_gpu.h) ----
/// `VIRTIO_GPU_F_VIRGL` (bit 0): the device supports the virgl 3D command set.
pub const VIRTIO_GPU_F_VIRGL: u64 = 1 << 0;
/// `VIRTIO_GPU_F_RESOURCE_BLOB` (bit 3): the device supports blob resources (`RESOURCE_CREATE_BLOB`)
/// + host-visible blob mapping (`RESOURCE_MAP_BLOB`/`UNMAP_BLOB`). This is what unlocks the gfxstream
/// **Address Space Graphics** (ASG) ring — Mesa's `gfxstream_vk` streams its command buffer through a
/// HOST3D blob mapped into both guest and host, so without this feature the guest falls back to
/// SwiftShader. Requires a host-visible PCI shared-memory region (BAR2, see `pci::PCI_SHM_BASE`).
pub const VIRTIO_GPU_F_RESOURCE_BLOB: u64 = 1 << 3;
/// `VIRTIO_GPU_F_CONTEXT_INIT` (bit 4): `CTX_CREATE` carries a `context_init` capset selector
/// (gfxstream selects its renderer — Vulkan/GLES — through this).
pub const VIRTIO_GPU_F_CONTEXT_INIT: u64 = 1 << 4;

// ---- 3D control command opcodes (0x02xx block) ----
pub const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
pub const VIRTIO_GPU_CMD_CTX_DESTROY: u32 = 0x0201;
pub const VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
pub const VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
pub const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
pub const VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
pub const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;
/// `RESOURCE_MAP_BLOB` (0x0208): map a host-visible blob into the guest's shared-memory window.
pub const VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
/// `RESOURCE_UNMAP_BLOB` (0x0209): tear down a blob map.
pub const VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;
// ---- capset queries (0x01xx block) ----
pub const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
pub const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;
pub const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
/// `RESOURCE_ASSIGN_UUID` (0x010b): the guest asks for a stable 16-byte UUID naming a resource, so a
/// Vulkan external-memory import in another process/context can refer to the same allocation.
pub const VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID: u32 = 0x010b;
/// `SET_SCANOUT_BLOB` (0x010d): bind a blob resource directly to a scanout, carrying its own format
/// / width / height / strides / offsets rather than reusing a `RESOURCE_CREATE_2D`-declared surface.
/// A device that advertises `F_RESOURCE_BLOB` must answer this — the guest's KMS/hwcomposer path
/// uses it whenever the framebuffer it wants to scan out is a blob.
pub const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x010d;

/// `VIRTIO_GPU_RESP_OK_RESOURCE_UUID` (0x1105): the response to `RESOURCE_ASSIGN_UUID`.
pub const VIRTIO_GPU_RESP_OK_RESOURCE_UUID: u32 = 0x1105;

// ---- 3D responses ----
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
/// `VIRTIO_GPU_RESP_OK_MAP_INFO` (0x1106): the response to `RESOURCE_MAP_BLOB`, carrying `map_info`
/// (the `STREAM_RENDERER_MAP_CACHE_*` caching flags the guest maps the region with).
pub const VIRTIO_GPU_RESP_OK_MAP_INFO: u32 = 0x1106;

/// Offset of the `ctx_id` field inside `virtio_gpu_ctrl_hdr` (type,flags,fence_id, **ctx_id**).
pub const CTRL_HDR_CTX_ID_OFF: usize = 16;

/// Parsed `virtio_gpu_resource_create_3d` payload (everything after the 24-byte ctrl header).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceCreate3d {
    pub resource_id: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// Parsed `virtio_gpu_transfer_host_3d` payload (box + offset + resource + strides).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transfer3d {
    /// `virtio_gpu_box`: x, y, z, w, h, d.
    pub box_: [u32; 6],
    pub offset: u64,
    pub resource_id: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// Parsed `virtio_gpu_resource_create_blob` header (mem entries follow but aren't parsed here).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CreateBlob {
    pub resource_id: u32,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub nr_entries: u32,
    pub blob_id: u64,
    pub size: u64,
}

/// A guest fence request (from the ctrl header's fence fields, or `CTX`-scoped ring fencing).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FenceReq {
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

/// The renderer boundary the virtio-gpu device routes 3D commands into. Object-safe + `Send` so a
/// `Box<dyn Renderer3d>` can live inside the [`Gpu`](super::gpu::Gpu) device (processed on the vCPU
/// thread). Every method returns the gfxstream-style status (`0` == success) where one exists.
pub trait Renderer3d: Send {
    fn context_create(&mut self, ctx_id: u32, context_init: u32, name: &[u8]) -> i32;
    fn context_destroy(&mut self, ctx_id: u32);
    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> i32;
    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32);
    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32);
    /// Release a 3D resource in the host renderer (`RESOURCE_UNREF`). Without this, every texture,
    /// buffer and ColorBuffer the guest ever creates stays live in gfxstream for the life of the VM.
    fn resource_unref(&mut self, resource_id: u32);

    /// Block until the guest's in-flight GPU work on this ColorBuffer has completed
    /// (`stream_renderer_wait_sync_resource` → `FrameBuffer::waitSyncColorBuffer`).
    ///
    /// **Required before any readback.** `stream_renderer_transfer_read_iov` records its own copy
    /// command and waits for *that* — it never waits on the ColorBuffer's `latestUse`, i.e. the work
    /// the guest submitted to render the frame in the first place. Reading without this settle
    /// samples a half-rendered image, which shows up as parts of the screen going black in a
    /// shifting, random-looking pattern while the rest of the frame is correct.
    /// `0` == settled (also returned when the renderer has nothing to wait on).
    fn wait_sync(&mut self, resource_id: u32) -> i32;
    /// Attach the resource's guest-memory backing as iovecs. `iovs` are `(host_addr, len)` pairs
    /// resolved from the guest mem entries — **stable host pointers into mapped guest RAM**, not a
    /// gathered copy: gfxstream stores these pointers in the resource (`VirtioGpuResource::AttachIov`
    /// → `mIovs`) and dereferences them on later transfers, so anything shorter-lived than the VM
    /// dangles.
    fn attach_backing(&mut self, resource_id: u32, iovs: &[(u64, usize)]) -> i32;
    /// Drop the iovecs previously attached to `resource_id` (`RESOURCE_DETACH_BACKING`). The guest is
    /// revoking that memory, so gfxstream must stop holding pointers into it.
    fn detach_backing(&mut self, resource_id: u32);
    fn transfer_to_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32;
    fn transfer_from_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32;
    /// Create a blob resource. `iovs` are `(host_addr, len)` pairs resolved from the guest mem
    /// entries (empty for a HOST3D blob — the gfxstream ASG ring — which the host allocates itself).
    fn create_blob(&mut self, ctx_id: u32, blob: &CreateBlob, iovs: &[(u64, usize)]) -> i32;
    /// Map a host-visible blob: returns `Some((host_ptr, size))` from `stream_renderer_resource_map`,
    /// or `None` on failure. The VMM then `hv_vm_map`s that host memory into the guest's shmem window.
    fn resource_map(&mut self, resource_id: u32) -> Option<(u64, u64)>;
    /// Tear down a blob map (`stream_renderer_resource_unmap`). `0` == success.
    fn resource_unmap(&mut self, resource_id: u32) -> i32;
    /// The `STREAM_RENDERER_MAP_CACHE_*` caching flags for a mapped blob (drives `OK_MAP_INFO`).
    fn resource_map_info(&mut self, resource_id: u32) -> u32;
    /// Forward the guest gfxstream command stream (`SUBMIT_3D` payload) verbatim.
    fn submit_cmd(&mut self, ctx_id: u32, cmd: &mut [u8]) -> i32;
    fn create_fence(&mut self, fence: &FenceReq) -> i32;
    /// Number of host capsets (drives `virtio_gpu_config.num_capsets`).
    fn num_capsets(&self) -> u32;
    /// Map a capset **index** → `(capset_id, max_version, max_size)` for `GET_CAPSET_INFO`.
    fn capset_info(&mut self, index: u32) -> (u32, u32, u32);
    /// Fill `out` (sized to the capset's `max_size`) with the capset blob for `GET_CAPSET`.
    fn fill_caps(&mut self, capset_id: u32, version: u32, out: &mut [u8]) -> i32;
    /// Present coupling: read a flushed 3D resource's pixels back into `dst` (BGRA, `w*h*4`) so the
    /// 2D scanout the CAMetalLayer presenter shows reflects the gfxstream ColorBuffer. `0` == success.
    fn read_pixels(&mut self, resource_id: u32, w: u32, h: u32, dst: &mut [u8]) -> i32;

    /// In-Vulkan present: ask the host renderer to present the flushed scanout ColorBuffer into the
    /// `CAMetalLayer` at `layer_ptr` (`stream_renderer_present_to_metal_layer`). The renderer owns
    /// the `VkSurfaceKHR` + `VK_KHR_swapchain` on that layer and does the whole thing on the GPU:
    /// `vkCmdBlitImage(ColorBuffer -> swapchain image)` then `vkQueuePresentKHR`. Nothing comes
    /// back — no texture handle, no pixels.
    ///
    /// Returns the host status: `0` presented, `> 0` frame intentionally dropped (swapchain busy or
    /// being recreated — the path is healthy, do NOT fall back), `< 0` hard failure (caller falls
    /// back to [`read_pixels`](Self::read_pixels)). Default: `-1`.
    fn present_to_layer(&mut self, _resource_id: u32, _layer_ptr: usize) -> i32 {
        -1
    }
}

// =====================================================================================
// Real forwarder — thin unsafe shims over crate::gfxstream::ffi.
// =====================================================================================

/// The capset ids gfxstream exposes, in the index order this device reports them to the guest.
/// gfxstream's `VirtioGpuFrontend::getCapset/fillCaps` implements ONLY
/// `GFXSTREAM_VULKAN(3)` / `GFXSTREAM_MAGMA(7)` / `GFXSTREAM_GLES(8)` / `GFXSTREAM_COMPOSER(9)` —
/// it does **not** implement `VIRGL(1)`. Advertising capset 1 made the guest's `GET_CAPSET(1)`
/// (minigbm's `DRM_IOCTL_VIRTGPU_GET_CAPS`, and Mesa's probe) hit gfxstream's
/// `fillCaps: "Incorrect capability set specified"` default and fail. Advertise ONLY the Vulkan
/// capset (id 3), which is the one the host driver backs and the one Android's Mesa `gfxstream_vk` +
/// minigbm actually consume. Verified against `host/virtgpu_gfxstream_protocol.h`.
const GFXSTREAM_CAPSETS: &[u32] = &[3];

/// Real gfxstream host-renderer forwarder. Assumes `stream_renderer_init` has already run
/// (`display::metal::init_gfxstream`). Constructing one does **not** re-init the renderer.
///
/// # Safety
/// Every method calls into `libgfxstream_backend.dylib`; only valid on macOS/aarch64 with the dylib
/// linked (build.rs) + the Vulkan loader/KosmicKrisp ICD staged. Because no standalone Linux guest
/// driver exists yet, the real forwarder is not wired into the boot path in Phase 3c — the mock is
/// what the smoke test exercises. See the module honesty note.
pub struct GfxstreamRenderer {
    _private: (),
}

impl GfxstreamRenderer {
    /// # Safety
    /// The caller must have already initialized the host renderer (`stream_renderer_init` returned
    /// 0) and be on a thread that owns the renderer.
    pub unsafe fn new() -> Self {
        Self { _private: () }
    }
}

impl Renderer3d for GfxstreamRenderer {
    fn context_create(&mut self, ctx_id: u32, context_init: u32, name: &[u8]) -> i32 {
        unsafe {
            ffi::stream_renderer_context_create(
                ctx_id,
                name.len() as u32,
                name.as_ptr() as *const c_char,
                context_init,
            )
        }
    }

    fn context_destroy(&mut self, ctx_id: u32) {
        unsafe { ffi::stream_renderer_context_destroy(ctx_id) }
    }

    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> i32 {
        let mut create = ffi::stream_renderer_resource_create_args {
            handle: args.resource_id,
            target: args.target,
            format: args.format,
            bind: args.bind,
            width: args.width,
            height: args.height,
            depth: args.depth,
            array_size: args.array_size,
            last_level: args.last_level,
            nr_samples: args.nr_samples,
            flags: args.flags,
        };
        // No initial iovec backing — a later RESOURCE_ATTACH_BACKING supplies it.
        unsafe { ffi::stream_renderer_resource_create(&mut create, std::ptr::null_mut(), 0) }
    }

    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        unsafe { ffi::stream_renderer_ctx_attach_resource(ctx_id as c_int, resource_id as c_int) }
    }

    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        unsafe { ffi::stream_renderer_ctx_detach_resource(ctx_id as c_int, resource_id as c_int) }
    }

    fn resource_unref(&mut self, resource_id: u32) {
        unsafe { ffi::stream_renderer_resource_unref(resource_id) }
    }

    fn wait_sync(&mut self, resource_id: u32) -> i32 {
        unsafe { ffi::stream_renderer_wait_sync_resource(resource_id) }
    }

    fn attach_backing(&mut self, resource_id: u32, iovs: &[(u64, usize)]) -> i32 {
        let mut iov: Vec<ffi::iovec> = iovs
            .iter()
            .map(|&(addr, len)| ffi::iovec {
                iov_base: addr as *mut c_void,
                iov_len: len,
            })
            .collect();
        if iov.is_empty() {
            return 0;
        }
        unsafe {
            ffi::stream_renderer_resource_attach_iov(
                resource_id as c_int,
                iov.as_mut_ptr(),
                iov.len() as c_int,
            )
        }
    }

    fn detach_backing(&mut self, resource_id: u32) {
        // gfxstream's out-params report the iovecs it dropped; we own none of that memory (it is
        // guest RAM), so both are discarded.
        let mut iov: *mut ffi::iovec = std::ptr::null_mut();
        let mut num: c_int = 0;
        unsafe {
            ffi::stream_renderer_resource_detach_iov(resource_id as c_int, &mut iov, &mut num)
        }
    }

    fn transfer_to_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32 {
        let mut b = to_ffi_box(xfer);
        let mut iov = ffi::iovec {
            iov_base: data.as_mut_ptr() as *mut c_void,
            iov_len: data.len(),
        };
        unsafe {
            ffi::stream_renderer_transfer_write_iov(
                xfer.resource_id,
                ctx_id,
                xfer.level as c_int,
                xfer.stride,
                xfer.layer_stride,
                &mut b,
                xfer.offset,
                &mut iov,
                1 as c_uint,
            )
        }
    }

    fn transfer_from_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32 {
        let mut b = to_ffi_box(xfer);
        let mut iov = ffi::iovec {
            iov_base: data.as_mut_ptr() as *mut c_void,
            iov_len: data.len(),
        };
        unsafe {
            ffi::stream_renderer_transfer_read_iov(
                xfer.resource_id,
                ctx_id,
                xfer.level,
                xfer.stride,
                xfer.layer_stride,
                &mut b,
                xfer.offset,
                &mut iov,
                1 as c_int,
            )
        }
    }

    fn create_blob(&mut self, ctx_id: u32, blob: &CreateBlob, iovs: &[(u64, usize)]) -> i32 {
        let create = ffi::stream_renderer_create_blob {
            blob_mem: blob.blob_mem,
            blob_flags: blob.blob_flags,
            blob_id: blob.blob_id,
            size: blob.size,
        };
        // A HOST3D blob (the ASG ring) carries no guest mem entries — the host allocates it. A
        // GUEST / HOST3D_GUEST blob passes the guest-backing iovecs so gfxstream registers them.
        let mut iov: Vec<ffi::iovec> = iovs
            .iter()
            .map(|&(addr, len)| ffi::iovec {
                iov_base: addr as *mut c_void,
                iov_len: len,
            })
            .collect();
        let (iov_ptr, num) = if iov.is_empty() {
            (std::ptr::null(), 0u32)
        } else {
            (iov.as_mut_ptr() as *const ffi::iovec, iov.len() as u32)
        };
        unsafe {
            ffi::stream_renderer_create_blob(
                ctx_id,
                blob.resource_id,
                &create,
                iov_ptr,
                num,
                std::ptr::null(),
            )
        }
    }

    fn resource_map(&mut self, resource_id: u32) -> Option<(u64, u64)> {
        let mut hva: *mut c_void = std::ptr::null_mut();
        let mut size: u64 = 0;
        let rc = unsafe { ffi::stream_renderer_resource_map(resource_id, &mut hva, &mut size) };
        if rc != 0 || hva.is_null() {
            return None;
        }
        Some((hva as u64, size))
    }

    fn resource_unmap(&mut self, resource_id: u32) -> i32 {
        unsafe { ffi::stream_renderer_resource_unmap(resource_id) }
    }

    fn resource_map_info(&mut self, resource_id: u32) -> u32 {
        let mut info: u32 = 0;
        let rc = unsafe { ffi::stream_renderer_resource_map_info(resource_id, &mut info) };
        if rc != 0 {
            // Default to write-combine cached — the common host-visible mapping the guest expects.
            ffi::STREAM_RENDERER_MAP_CACHE_CACHED
        } else {
            info
        }
    }

    fn submit_cmd(&mut self, ctx_id: u32, cmd: &mut [u8]) -> i32 {
        let mut command = ffi::stream_renderer_command {
            ctx_id,
            cmd_size: cmd.len() as u32,
            cmd: cmd.as_mut_ptr(),
            num_in_fences: 0,
            fences: std::ptr::null_mut(),
        };
        unsafe { ffi::stream_renderer_submit_cmd(&mut command) }
    }

    fn create_fence(&mut self, fence: &FenceReq) -> i32 {
        let f = ffi::stream_renderer_fence {
            flags: fence.flags,
            fence_id: fence.fence_id,
            ctx_id: fence.ctx_id,
            ring_idx: fence.ring_idx,
        };
        unsafe { ffi::stream_renderer_create_fence(&f) }
    }

    fn num_capsets(&self) -> u32 {
        GFXSTREAM_CAPSETS.len() as u32
    }

    fn capset_info(&mut self, index: u32) -> (u32, u32, u32) {
        let id = GFXSTREAM_CAPSETS.get(index as usize).copied().unwrap_or(0);
        let (mut max_ver, mut max_size) = (0u32, 0u32);
        unsafe { ffi::stream_renderer_get_cap_set(id, &mut max_ver, &mut max_size) };
        (id, max_ver, max_size)
    }

    fn fill_caps(&mut self, capset_id: u32, version: u32, out: &mut [u8]) -> i32 {
        unsafe {
            ffi::stream_renderer_fill_caps(capset_id, version, out.as_mut_ptr() as *mut c_void)
        };
        0
    }

    fn read_pixels(&mut self, resource_id: u32, w: u32, h: u32, dst: &mut [u8]) -> i32 {
        // Full-surface readback: box covers (0,0)->(w,h), tightly packed BGRA rows.
        let mut b = ffi::stream_renderer_box {
            x: 0,
            y: 0,
            z: 0,
            w,
            h,
            d: 1,
        };
        let mut iov = ffi::iovec {
            iov_base: dst.as_mut_ptr() as *mut c_void,
            iov_len: dst.len(),
        };
        let rc = unsafe {
            ffi::stream_renderer_transfer_read_iov(
                resource_id,
                0,
                0,
                w * 4,
                0,
                &mut b,
                0,
                &mut iov,
                1 as c_int,
            )
        };

        // gfxstream reads the ColorBuffer back in its OWN storage order, and that ColorBuffer is
        // RGBA8 (measured through the backing Metal texture as `MTLPixelFormatRGBA8Unorm`, 70 —
        // gfxstream picks that order itself, so swapping the host ICD does not move it). The CPU
        // scanout framebuffer, however, is BGRA by contract: the 2D path writes guest BGRA/XRGB into
        // it, and every consumer (`Controller::screenshot`, `display::present`, `AEMU_FB_DUMP`)
        // decodes byte0=B. Handing them RGBA silently swapped red and blue for the entire readback
        // present path — visible as a peach wallpaper rendering blue (and vice versa), which is easy
        // to miss because greys, whites and greens are unaffected.
        //
        // Swizzle here so the framebuffer invariant holds for ALL producers rather than teaching each
        // consumer about a second layout.
        if rc == 0 {
            for px in dst.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
        rc
    }

    fn present_to_layer(&mut self, resource_id: u32, layer_ptr: usize) -> i32 {
        if layer_ptr == 0 {
            return -1;
        }
        // Safety: FFI into libgfxstream_backend. `layer_ptr` is a live, unretained CAMetalLayer*
        // owned by the display thread's NSView, published once at window creation and valid for the
        // whole run; the renderer only reads it (vkCreateMetalSurfaceEXT) and caches the surface.
        unsafe {
            ffi::stream_renderer_present_to_metal_layer(resource_id, layer_ptr as *mut c_void)
        }
    }
}

fn to_ffi_box(xfer: &Transfer3d) -> ffi::stream_renderer_box {
    ffi::stream_renderer_box {
        x: xfer.box_[0],
        y: xfer.box_[1],
        z: xfer.box_[2],
        w: xfer.box_[3],
        h: xfer.box_[4],
        d: xfer.box_[5],
    }
}

// =====================================================================================
// Async fence bridge — the FENCE_CALLBACK → guest virtio-gpu IRQ path.
// =====================================================================================
//
// gfxstream signals fence completion **asynchronously** from a renderer thread via the
// `STREAM_RENDERER_PARAM_FENCE_CALLBACK`. Mesa's `gfxstream_vk` waits on these fences to know the
// host has drained the ASG ring, so a stalled fence signal deadlocks the guest driver. But the
// callback fires on gfxstream's thread, which must NOT touch HVF/GIC state. So the callback only
// records the completion + wakes a VMM-owned thread (installed by `machine.rs`) that raises the
// virtio-gpu device's SPI + kicks the vCPU, so the guest's virtio-gpu IRQ handler re-scans fence
// progress. The device also completes fenced commands synchronously (echoing the fence id in the
// used-ring response), so this async poke is the belt-and-suspenders wakeup for a fence that
// completes after the command response was already written.

use std::sync::mpsc::Sender;
use std::sync::OnceLock;

/// Count of fence completions observed from gfxstream (evidence; read at teardown).
static FENCE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Wakeup channel to the VMM's fence-signal thread. Installed by `machine.rs` before the vCPU runs.
static FENCE_WAKE: OnceLock<Sender<()>> = OnceLock::new();

/// Install the wakeup sender the fence callback pokes on each completion. Idempotent (first wins).
pub fn install_fence_wake(tx: Sender<()>) {
    let _ = FENCE_WAKE.set(tx);
}

/// The C `stream_renderer_fence_callback`: record the completion + wake the VMM fence thread. Called
/// from a gfxstream renderer thread — does no HVF/GIC work here.
pub extern "C" fn on_fence_complete(
    _user_data: *mut c_void,
    _fence: *mut ffi::stream_renderer_fence,
) {
    FENCE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(tx) = FENCE_WAKE.get() {
        let _ = tx.send(());
    }
}

/// Total fence completions gfxstream has signalled (evidence for the boot report).
pub fn fence_count() -> u64 {
    FENCE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

// =====================================================================================
// Mock forwarder — the recording double the Phase-3c smoke test drives.
// =====================================================================================

use std::sync::{Arc, Mutex};

/// One routed call recorded by [`MockRenderer`]. `PartialEq` so tests can assert exact routing.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    ContextCreate {
        ctx_id: u32,
        context_init: u32,
        name: Vec<u8>,
    },
    ContextDestroy {
        ctx_id: u32,
    },
    ResourceCreate3d(ResourceCreate3d),
    CtxAttach {
        ctx_id: u32,
        resource_id: u32,
    },
    CtxDetach {
        ctx_id: u32,
        resource_id: u32,
    },
    ResourceUnref {
        resource_id: u32,
    },
    WaitSync {
        resource_id: u32,
    },
    AttachBacking {
        resource_id: u32,
        len: usize,
    },
    DetachBacking {
        resource_id: u32,
    },
    TransferToHost {
        ctx_id: u32,
        xfer: Transfer3d,
        len: usize,
    },
    TransferFromHost {
        ctx_id: u32,
        xfer: Transfer3d,
        len: usize,
    },
    CreateBlob {
        ctx_id: u32,
        blob: CreateBlob,
    },
    SubmitCmd {
        ctx_id: u32,
        bytes: Vec<u8>,
    },
    CreateFence(FenceReq),
    CapsetInfo {
        index: u32,
    },
    FillCaps {
        capset_id: u32,
        version: u32,
    },
    ReadPixels {
        resource_id: u32,
        w: u32,
        h: u32,
    },
    PresentToLayer {
        resource_id: u32,
        layer_ptr: usize,
    },
}

/// Shared, cloneable log of routed [`Call`]s. Clone one before moving the [`MockRenderer`] into the
/// device, then inspect it after driving commands.
pub type CallLog = Arc<Mutex<Vec<Call>>>;

/// A [`Renderer3d`] double that records every routed call and returns success — no GPU, no gfxstream.
/// This is what proves the guest→device→renderer-FFI routing in the smoke test.
pub struct MockRenderer {
    log: CallLog,
    /// Optional byte the mock writes into every `read_pixels`/`transfer_from_host` dst, so present
    /// coupling can be observed (a non-zero framebuffer after a 3D flush).
    fill_byte: u8,
    /// What [`Renderer3d::present_to_layer`] reports back. `0` (presented) by default, so a test that
    /// publishes a layer pointer exercises the in-Vulkan present branch; [`Self::with_present_rc`]
    /// substitutes a dropped-frame (`1`) or hard-failure (negative) status to drive the other arms.
    present_rc: i32,
}

impl MockRenderer {
    pub fn new() -> (Self, CallLog) {
        Self::with_present_rc(0)
    }

    /// A mock whose `present_to_layer` reports `rc` — the knob that selects which `flush_3d` branch
    /// runs when a CAMetalLayer is published.
    pub fn with_present_rc(rc: i32) -> (Self, CallLog) {
        let log: CallLog = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                log: log.clone(),
                fill_byte: 0xAB,
                present_rc: rc,
            },
            log,
        )
    }

    fn push(&self, c: Call) {
        self.log.lock().unwrap().push(c);
    }
}

impl Renderer3d for MockRenderer {
    fn context_create(&mut self, ctx_id: u32, context_init: u32, name: &[u8]) -> i32 {
        self.push(Call::ContextCreate {
            ctx_id,
            context_init,
            name: name.to_vec(),
        });
        0
    }
    fn context_destroy(&mut self, ctx_id: u32) {
        self.push(Call::ContextDestroy { ctx_id });
    }
    fn resource_create_3d(&mut self, args: &ResourceCreate3d) -> i32 {
        self.push(Call::ResourceCreate3d(*args));
        0
    }
    fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.push(Call::CtxAttach {
            ctx_id,
            resource_id,
        });
    }
    fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) {
        self.push(Call::CtxDetach {
            ctx_id,
            resource_id,
        });
    }
    fn resource_unref(&mut self, resource_id: u32) {
        self.push(Call::ResourceUnref { resource_id });
    }
    fn wait_sync(&mut self, resource_id: u32) -> i32 {
        self.push(Call::WaitSync { resource_id });
        0
    }
    fn attach_backing(&mut self, resource_id: u32, iovs: &[(u64, usize)]) -> i32 {
        self.push(Call::AttachBacking {
            resource_id,
            len: iovs.iter().map(|&(_, l)| l).sum(),
        });
        0
    }
    fn detach_backing(&mut self, resource_id: u32) {
        self.push(Call::DetachBacking { resource_id });
    }
    fn transfer_to_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32 {
        self.push(Call::TransferToHost {
            ctx_id,
            xfer: *xfer,
            len: data.len(),
        });
        0
    }
    fn transfer_from_host_3d(&mut self, ctx_id: u32, xfer: &Transfer3d, data: &mut [u8]) -> i32 {
        for b in data.iter_mut() {
            *b = self.fill_byte;
        }
        self.push(Call::TransferFromHost {
            ctx_id,
            xfer: *xfer,
            len: data.len(),
        });
        0
    }
    fn create_blob(&mut self, ctx_id: u32, blob: &CreateBlob, _iovs: &[(u64, usize)]) -> i32 {
        self.push(Call::CreateBlob {
            ctx_id,
            blob: *blob,
        });
        0
    }
    fn resource_map(&mut self, _resource_id: u32) -> Option<(u64, u64)> {
        None
    }
    fn resource_unmap(&mut self, _resource_id: u32) -> i32 {
        0
    }
    fn resource_map_info(&mut self, _resource_id: u32) -> u32 {
        0
    }
    fn submit_cmd(&mut self, ctx_id: u32, cmd: &mut [u8]) -> i32 {
        self.push(Call::SubmitCmd {
            ctx_id,
            bytes: cmd.to_vec(),
        });
        0
    }
    fn create_fence(&mut self, fence: &FenceReq) -> i32 {
        self.push(Call::CreateFence(*fence));
        0
    }
    fn num_capsets(&self) -> u32 {
        1
    }
    fn capset_info(&mut self, index: u32) -> (u32, u32, u32) {
        self.push(Call::CapsetInfo { index });
        // Pretend index 0 = gfxstream-vulkan capset (id 3), v1, 8 bytes.
        (3, 1, 8)
    }
    fn fill_caps(&mut self, capset_id: u32, version: u32, out: &mut [u8]) -> i32 {
        self.push(Call::FillCaps { capset_id, version });
        for b in out.iter_mut() {
            *b = 0;
        }
        0
    }
    fn read_pixels(&mut self, resource_id: u32, w: u32, h: u32, dst: &mut [u8]) -> i32 {
        for b in dst.iter_mut() {
            *b = self.fill_byte;
        }
        self.push(Call::ReadPixels { resource_id, w, h });
        0
    }
    fn present_to_layer(&mut self, resource_id: u32, layer_ptr: usize) -> i32 {
        self.push(Call::PresentToLayer {
            resource_id,
            layer_ptr,
        });
        self.present_rc
    }
}
