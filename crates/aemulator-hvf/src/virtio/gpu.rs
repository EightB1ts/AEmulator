//! virtio-gpu (device type 16) in **2D mode** — a dumb-framebuffer scanout device. Enough of the
//! virtio-gpu 2D control protocol (virtio spec §5.7) for a Linux guest's `virtio-gpu` DRM driver to
//! create a scanout resource, attach a guest-memory backing, blit its framebuffer to the host
//! (`TRANSFER_TO_HOST_2D`), and flush it to the display (`RESOURCE_FLUSH`). No 3D / virgl / blob
//! resources — those features are not advertised, so the guest never issues them.
//!
//! Two virtqueues: **controlq** (0, the command/response stream) and **cursorq** (1, cursor
//! updates — drained + acked, no host cursor yet). The scanned-out image lives in a host
//! [`Framebuffer`] behind an `Arc<Mutex<…>>`; each `RESOURCE_FLUSH` copies the flushed resource
//! into it and bumps a generation counter, so a display layer (a future window) can poll for a new
//! frame via [`ScanoutHandle`]. Reference: crosvm `devices/src/virtio/gpu` 2D path (not copied).

use super::gpu3d::FenceReq;
use super::{Mem, VIRTIO_F_VERSION_1};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

/// virtio device id for GPU (virtio spec §5.7).
pub const VIRTIO_ID_GPU: u32 = 16;

// 2D control commands.
const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const VIRTIO_GPU_CMD_GET_EDID: u32 = 0x010a;
// cursor commands.
const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

// Responses.
const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_EDID: u32 = 0x1104;
const VIRTIO_GPU_RESP_ERR_UNSPEC: u32 = 0x1200;
const VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;

/// `VIRTIO_GPU_F_EDID` (feature bit 1): the device serves a per-scanout EDID blob via `GET_EDID`.
/// Advertising it (and answering with a valid EDID carrying a physical image size) makes the guest's
/// DRM connector report `width_mm`/`height_mm` > 0, so Android's SurfaceFlinger computes a real dpi
/// and builds a `DisplayViewport` with physical properties — the precondition for a DIRECT
/// touchscreen (`INPUT_PROP_DIRECT`) to associate with the internal display (without it InputReader
/// deactivates the touch device: "could not query the properties of its associated display").
const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

/// Target pixel density used to derive the EDID physical image size from the scanout resolution.
/// Any positive dpi fixes the "dpi -1 / viewport not found" gap; 320 matches Cuttlefish's default.
const EDID_DPI: u32 = 320;

/// `VIRTIO_GPU_FLAG_FENCE` (bit 0): the command carries a `fence_id` the device must signal when the
/// work it describes has completed.
const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;

/// `VIRTIO_GPU_FLAG_INFO_RING_IDX` (bit 1): the header's `ctx_id` + `ring_idx` name a **per-context
/// timeline** the fence belongs to, instead of the device's single global one.
///
/// The guest sets this whenever it fences on a non-default ring (Linux
/// `virtio_gpu_fence_emit`: `if (fence->ring_idx != 0)`), and Mesa's `gfxstream_vk` asks for
/// `VIRTGPU_CONTEXT_PARAM_NUM_RINGS = 2` and submits its real Vulkan queue work on ring 1. A device
/// that drops this bit from its response — as this one did, by masking `flags & FLAG_FENCE` and
/// zeroing bytes 16..21 — reports every completion as belonging to ring 0 of context 0, so per-ring
/// fence completion cannot be routed at all.
const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;

const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
const CTRL_HDR_LEN: usize = 24;
const BPP: u32 = 4;

/// How long a deferred (fenced) command may sit unpublished before [`Gpu::sweep_stale_fences`]
/// force-completes it.
///
/// **This watchdog is not optional.** Deferring a response until the host fence fires converts a
/// wedged renderer — a lost fence callback, a gfxstream thread stuck in a driver, a context torn
/// down with work outstanding — from "one stale frame" into "the guest's virtio-gpu ring never
/// advances again", which is strictly worse than the synchronous behaviour it replaces. 2 s is far
/// beyond any legitimate host fence (gfxstream's own present acquire is bounded at 100 ms) and far
/// below the guest's own DRM/dma-fence timeouts, so it only ever fires on a real fault, loudly.
const FENCE_DEADLINE: Duration = Duration::from_secs(2);

/// Ceiling on simultaneously deferred fenced commands. Past this, [`Gpu::dispatch`] stops deferring
/// and answers synchronously (the pre-deferral behaviour) instead of growing the pending map without
/// bound.
///
/// The second independent safety net beside [`FENCE_DEADLINE`]: the deadline bounds how *long* a
/// single wedged fence can stall one command, this bounds how *much* state a renderer that has
/// stopped signalling altogether can accumulate. 1024 is 4× the control ring's depth
/// ([`super::QUEUE_SIZE`] = 256), so a guest cannot reach it with in-flight chains alone — only a
/// host that has stopped completing can.
const MAX_PENDING_FENCES: usize = 1024;

// =====================================================================================
// Guest-controlled allocation limits.
//
// Every number below arrives from the guest inside a control command, and each one used to size a
// host allocation directly: `vec![0u8; len as usize]` per readable descriptor, one host pixel buffer
// per `RESOURCE_CREATE_2D`, one gathered copy per `TRANSFER_TO_HOST_*`. A malicious or simply broken
// guest could ask for ~4 GiB per command, on the vCPU thread, holding `ctx.pci`. Worse, the size
// arithmetic was `u32` (`(width * height * BPP) as usize`), which **panics** in a debug build — a
// guest-triggerable VMM abort — and silently **wraps** in a release build, yielding a buffer smaller
// than the code that fills it believes.
//
// So: bound every guest-sized allocation, do all size arithmetic in `u64`, and reject the command
// (with the rejected value logged) rather than clamping — a clamped resource is a resource whose
// dimensions the device and the guest disagree about, which is a silent-corruption bug instead of a
// loud one.
// =====================================================================================

/// Largest scanout/resource dimension accepted on either axis, in pixels. Two orders of magnitude
/// above any panel this device presents (1080×2400) and above the `maxImageDimension2D` of the host
/// Vulkan driver, so nothing legitimate is near it.
const MAX_DIM: u32 = 16384;

/// Cap on the bytes one resource's host pixel buffer (or one gathered guest backing) may occupy.
///
/// **Exclusive**: a resource must be strictly smaller than this, which puts the round 8192×8192 BGRA
/// image (exactly 256 MiB) just outside it. 256 MiB is ~25× a 1080×2400 BGRA frame and ~4× a 4096²
/// RGBA texture, so it bounds the DoS without being reachable by a real guest.
const MAX_RESOURCE_BYTES: u64 = 256 << 20;

/// Largest `nr_entries` accepted on `RESOURCE_ATTACH_BACKING` / `RESOURCE_CREATE_BLOB`. A 256 MiB
/// resource scattered into 4 KiB guest pages needs 65536 entries, but a real guest hands us far
/// larger physically-contiguous runs; 4096 covers every backing observed on the boot path.
const MAX_MEM_ENTRIES: usize = 4096;

/// `sizeof(struct virtio_gpu_mem_entry)`: addr(u64) length(u32) padding(u32).
const MEM_ENTRY_BYTES: usize = 16;

/// Cap on the assembled bytes of one control command (the device-readable half of a chain).
///
/// The largest legitimate control command is `RESOURCE_ATTACH_BACKING` with
/// [`MAX_MEM_ENTRIES`]×[`MEM_ENTRY_BYTES`] = 64 KiB of trailing mem entries, so this is that with 2×
/// headroom. The rejected size is logged, so a cap set too tight presents as a log line naming the
/// number to raise rather than as a black screen.
const MAX_CMD_BYTES: usize = 128 << 10;

/// Byte size of a `width`×`height` BGRA image, or `None` if the dimensions are unusable: zero, past
/// [`MAX_DIM`], or a total at/above [`MAX_RESOURCE_BYTES`].
///
/// All arithmetic in `u64`. The `u32` version of this expression is the overflow: 65536² × 4 is 2^36,
/// which does not fit, so a guest asking for a 65536×65536 resource either aborted the VMM (debug) or
/// got a 0-byte buffer described as a 65536×65536 image (release).
fn resource_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return None;
    }
    let bytes = u64::from(width) * u64::from(height) * u64::from(BPP);
    if bytes >= MAX_RESOURCE_BYTES {
        return None;
    }
    usize::try_from(bytes).ok()
}

pub const CONTROLQ: u16 = 0;
pub const CURSORQ: u16 = 1;

/// The host-side scanned-out image. `bytes` is `width*height*4` in the guest's chosen pixel layout
/// (virtio-gpu's default scanout format is BGRA/XRGB little-endian, i.e. a `0xAARRGGBB` u32).
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

impl Framebuffer {
    /// Allocate a `width`×`height` BGRA framebuffer, or `None` when the dimensions are outside the
    /// device limits (see [`resource_bytes`]).
    ///
    /// Fallible on purpose. This is reached from `RESOURCE_CREATE_2D` and from `SET_SCANOUT_BLOB`,
    /// i.e. from guest-supplied `width`/`height` — the previous infallible `new` computed its size in
    /// `u32` and allocated whatever came out, so a 65536² request was a VMM abort in a debug build.
    /// Every call site rejects the command instead of resizing to something the guest did not ask for.
    fn try_new(width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width,
            height,
            bytes: vec![0u8; resource_bytes(width, height)?],
        })
    }

    /// Count of non-zero bytes — a cheap "did the guest draw anything?" probe for headless runs.
    pub fn non_zero_bytes(&self) -> usize {
        self.bytes.iter().filter(|b| **b != 0).count()
    }
}

/// A cloneable handle onto the scanout framebuffer + a generation counter bumped on every flush.
/// Handed to the display layer so it can present the latest frame; `Arc` keeps it alive independent
/// of the device.
#[derive(Clone)]
pub struct ScanoutHandle {
    pub fb: Arc<Mutex<Framebuffer>>,
    pub generation: Arc<AtomicU64>,
    /// In-Vulkan present channel: the host `CAMetalLayer*` the gfxstream renderer presents into
    /// (`0` == none). Published ONCE by the display thread at window creation; read by `flush_3d`
    /// on the vCPU thread. This single value is what splits the two present modes:
    ///
    ///   * non-zero (windowed `--display`) — `flush_3d` calls
    ///     `stream_renderer_present_to_metal_layer`; the CPU framebuffer is left untouched.
    ///   * zero (headless `--control`, or `AEMU_PRESENT_READBACK=1`) — `flush_3d` reads the
    ///     ColorBuffer back into `fb`, which is what softbuffer and every control verb read.
    ///
    /// The two modes are mutually exclusive at the CLI (`--control` short-circuits `--display`), so
    /// the split is total: there is no configuration in which nobody produces pixels.
    pub metal_layer: Arc<AtomicUsize>,
    /// Set the first time an in-Vulkan present actually succeeds. This — **not** the mere existence
    /// of a layer — is what tells the display thread to stop doing the softbuffer CPU blit.
    ///
    /// The distinction is load-bearing. A layer exists for the whole of any windowed run, but plenty
    /// of frames never reach `present_to_layer`: a 2D-only run (`--display` without `--gpu3d`) has no
    /// renderer at all, a run whose `stream_renderer_init` failed falls back to the 2D device, and
    /// even a healthy `--gpu3d` run paints its boot console through 2D scanout long before the first
    /// 3D flush. Gating on the layer alone blanks the window in every one of those cases.
    pub presented: Arc<AtomicBool>,
}

impl ScanoutHandle {
    /// The current flush generation (monotonic; a display layer redraws when it changes).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// A 2D resource: a host pixel buffer plus the guest-memory backing iovec the guest transfers from.
struct Resource {
    width: u32,
    height: u32,
    #[allow(dead_code)]
    format: u32,
    /// Guest-memory backing as (addr, len) iovec entries.
    backing: Vec<(u64, u32)>,
    /// Host copy of the resource image (`width*height*4`).
    pixels: Vec<u8>,
}

/// A parsed control command's fixed rectangle (x, y, w, h).
#[derive(Clone, Copy, Default)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// The 24-byte `virtio_gpu_ctrl_hdr` every control command starts with, parsed **once**:
///
/// ```text
/// le32 type | le32 flags | le64 fence_id | le32 ctx_id | u8 ring_idx | u8 padding[3]
///    0..4       4..8          8..16          16..20         20            21..24
/// ```
///
/// It used to be re-read piecemeal — `cmd_type`/`flags`/`fence` in `dispatch`, `ctx_id` again in
/// `dispatch_3d`, `ring_idx` again in `route_header_fence`, and never at all when building a
/// response. Parsing it once and threading it through is what makes the response header able to echo
/// the fields it is required to echo, and what makes the fence key `(ctx_id, ring_idx, fence_id)`
/// available at every site that needs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReqHdr {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
}

impl ReqHdr {
    /// Parse the header, or `None` if the command is shorter than one.
    fn parse(cmd: &[u8]) -> Option<Self> {
        if cmd.len() < CTRL_HDR_LEN {
            return None;
        }
        Some(Self {
            cmd_type: read_le(cmd, 0, 4) as u32,
            flags: read_le(cmd, 4, 4) as u32,
            fence_id: read_le(cmd, 8, 8),
            ctx_id: read_le(cmd, super::gpu3d::CTRL_HDR_CTX_ID_OFF, 4) as u32,
            ring_idx: read_le(cmd, super::gpu3d::CTRL_HDR_RING_IDX_OFF, 1) as u8,
        })
    }

    /// Whether the guest asked for this command to be fenced.
    fn fenced(&self) -> bool {
        self.flags & VIRTIO_GPU_FLAG_FENCE != 0
    }

    /// The `(ctx_id, ring_idx, fence_id)` identity of this command's fence.
    fn fence_key(&self) -> FenceKey {
        FenceKey {
            ctx_id: self.ctx_id,
            ring_idx: self.ring_idx,
            fence_id: self.fence_id,
        }
    }
}

/// Identity of one in-flight fence. `ring_idx` is part of it, not an afterthought: two rings of the
/// same context carry independent, independently-ordered fence streams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct FenceKey {
    ctx_id: u32,
    ring_idx: u8,
    fence_id: u64,
}

impl FenceKey {
    fn of(fence: &FenceReq) -> Self {
        Self {
            ctx_id: fence.ctx_id,
            ring_idx: fence.ring_idx,
            fence_id: fence.fence_id,
        }
    }

    /// Same context + same ring — i.e. the same ordered fence timeline.
    fn same_ring(&self, other: &Self) -> bool {
        self.ctx_id == other.ctx_id && self.ring_idx == other.ring_idx
    }
}

/// A fenced command whose response is written but **not yet published**: it waits in
/// [`Gpu::pending_fences`] until the host fence fires (or [`FENCE_DEADLINE`] expires).
struct PendingFence {
    /// Descriptor-chain head, i.e. the `id` `add_used` must publish.
    head: u16,
    /// The chain's device-writable descriptors, in chain order — where the response goes.
    writable: Vec<(u64, u32)>,
    /// The already-built response bytes. Held rather than written immediately so the guest cannot
    /// observe a half-finished response, and written at the same moment the used entry is published.
    resp: Vec<u8>,
    /// When the command was dispatched — the watchdog's clock.
    since: Instant,
}

/// What [`Gpu::dispatch`] decided to do with a command.
///
/// The whole of item 2.5 is this enum existing. Before it, `process_controlq` wrote the response and
/// `add_used`'d it the instant `dispatch` returned, and `resp_hdr` echoed the guest's `fence_id` in
/// that same response — so **every guest fence signalled at submit time, not at completion**.
/// `vkWaitForFences` returned immediately, Mesa's `VirtioGpuAddressSpaceStream` `execBuffer` +
/// `blob->wait()` pair both returned instantly, and any host work still in flight was free to tear.
enum Disposition {
    /// Publish now: write `0` into the chain's writable descriptors and `add_used`.
    Complete(Vec<u8>),
    /// Hold until the host fence identified by `key` completes.
    Deferred { key: FenceKey, resp: Vec<u8> },
}

impl Disposition {
    /// The response bytes, whichever arm this is. Only the test shim `Gpu::dispatch_resp` needs to
    /// erase the distinction — production has to act on it.
    #[cfg(test)]
    fn into_resp(self) -> Vec<u8> {
        match self {
            Self::Complete(resp) | Self::Deferred { resp, .. } => resp,
        }
    }
}

/// The guest-address-space mapping boundary `RESOURCE_MAP_BLOB` / `RESOURCE_UNMAP_BLOB` cross.
///
/// This exists so the blob-map path is testable. The two `hv_vm_map`/`hv_vm_unmap` calls used to sit
/// inline in [`Gpu::map_blob`] / [`Gpu::unmap_blob`], which made the whole body of `map_blob` — the
/// window bounds check, the granule-alignment check, the mapping itself, the `blob_maps` bookkeeping
/// and the `OK_MAP_INFO` reply — unreachable from any test: an `hv_vm_map` in a unit test would need
/// a live `hv_vm_create`d VM, and the failure branch would need HVF to be *made* to fail.
///
/// `Send` because the `Box<dyn GuestMapper>` lives inside the [`Gpu`] device, which moves onto the
/// vCPU thread. `Err` carries the raw `hv_return_t` so the caller can name it in a log.
pub(crate) trait GuestMapper: Send {
    /// Map `size` bytes of host memory at `host_ptr` into the guest at IPA `ipa`, read-write.
    fn map(&mut self, host_ptr: u64, ipa: u64, size: u64) -> Result<(), i32>;
    /// Remove a mapping previously installed by [`Self::map`].
    fn unmap(&mut self, ipa: u64, size: u64) -> Result<(), i32>;
}

/// The production mapper: HVF second-stage translation via `hv_vm_map` / `hv_vm_unmap`.
struct HvfMapper;

impl GuestMapper for HvfMapper {
    fn map(&mut self, host_ptr: u64, ipa: u64, size: u64) -> Result<(), i32> {
        let rw = crate::HV_MEMORY_READ | crate::HV_MEMORY_WRITE;
        // Safety: `host_ptr`/`size` come from `stream_renderer_resource_map`, i.e. a live host
        // allocation owned by the renderer for as long as the blob is mapped; `unmap_blob` removes
        // the guest mapping before the renderer releases it.
        let rc = unsafe {
            crate::hv_vm_map(
                host_ptr as *mut std::os::raw::c_void,
                ipa,
                size as usize,
                rw,
            )
        };
        if rc == crate::HV_SUCCESS {
            Ok(())
        } else {
            Err(rc)
        }
    }

    fn unmap(&mut self, ipa: u64, size: u64) -> Result<(), i32> {
        // Safety: removing a mapping this device installed; HVF validates the range itself.
        let rc = unsafe { crate::hv_vm_unmap(ipa, size as usize) };
        if rc == crate::HV_SUCCESS {
            Ok(())
        } else {
            Err(rc)
        }
    }
}

/// One operation a [`RecordingMapper`] observed, in call order.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MapOp {
    Map { host_ptr: u64, ipa: u64, size: u64 },
    Unmap { ipa: u64, size: u64 },
}

/// Shared, cloneable log of [`MapOp`]s — clone the handle before moving the mapper into the device.
#[cfg(test)]
pub(crate) type MapLog = Arc<Mutex<Vec<MapOp>>>;

/// Test double for [`GuestMapper`]: records every attempted mapping (successful or not) and can be
/// made to fail the next `map`, which is the only way to reach `map_blob`'s "release the host map
/// again because the guest mapping failed" branch.
#[cfg(test)]
pub(crate) struct RecordingMapper {
    ops: MapLog,
    /// Consumed by the next [`GuestMapper::map`]; that call reports this `hv_return_t` and every
    /// later one succeeds.
    fail_next: Option<i32>,
}

#[cfg(test)]
impl RecordingMapper {
    /// A mapper where every operation succeeds.
    pub(crate) fn new() -> (Self, MapLog) {
        Self::build(None)
    }

    /// A mapper whose **next** `map` reports `rc` (an `hv_return_t`); later ones succeed.
    pub(crate) fn failing(rc: i32) -> (Self, MapLog) {
        Self::build(Some(rc))
    }

    fn build(fail_next: Option<i32>) -> (Self, MapLog) {
        let ops: MapLog = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                ops: ops.clone(),
                fail_next,
            },
            ops,
        )
    }
}

#[cfg(test)]
impl GuestMapper for RecordingMapper {
    fn map(&mut self, host_ptr: u64, ipa: u64, size: u64) -> Result<(), i32> {
        self.ops.lock().unwrap().push(MapOp::Map {
            host_ptr,
            ipa,
            size,
        });
        match self.fail_next.take() {
            Some(rc) => Err(rc),
            None => Ok(()),
        }
    }

    fn unmap(&mut self, ipa: u64, size: u64) -> Result<(), i32> {
        self.ops.lock().unwrap().push(MapOp::Unmap { ipa, size });
        Ok(())
    }
}

pub struct Gpu {
    default_width: u32,
    default_height: u32,
    resources: HashMap<u32, Resource>,
    /// resource id currently bound to scanout 0 (0 = none).
    scanout_resource: u32,
    scanout: ScanoutHandle,
    /// Phase 3c: the gfxstream 3D renderer this device forwards virgl commands into. `None` ⇒ pure
    /// 2D device (the default even under the feature, unless [`Gpu::with_renderer`] is used), so the
    /// working boot-to-shell 2D path is never disturbed. Feature-gated so the default build is 2D-only.
    renderer: Option<Box<dyn super::gpu3d::Renderer3d>>,
    /// Dimensions of 3D resources (id → (w, h)), so `SET_SCANOUT` / `RESOURCE_FLUSH` on a virgl
    /// resource (which never enters the 2D `resources` map) can size + read it back for presenting.
    res3d: HashMap<u32, (u32, u32)>,
    /// Guest-memory backing iovecs (addr, len) attached to a 3D resource via RESOURCE_ATTACH_BACKING,
    /// gathered at TRANSFER_TO_HOST_3D time to feed `stream_renderer_transfer_write_iov`.
    res3d_backing: HashMap<u32, Vec<(u64, u32)>>,
    /// Cache of capset id → max_size learned during `GET_CAPSET_INFO`, used to size `GET_CAPSET`.
    capset_size: HashMap<u32, u32>,
    /// Blob resources (`RESOURCE_CREATE_BLOB`): id → requested size. Used to validate maps.
    blobs: HashMap<u32, u64>,
    /// Live host-visible blob maps: resource id → (guest-offset within the shmem window, size). Set
    /// on `RESOURCE_MAP_BLOB` (after a successful `hv_vm_map`), cleared on `RESOURCE_UNMAP_BLOB`.
    blob_maps: HashMap<u32, (u64, u64)>,
    /// How host blob memory reaches the guest address space. [`HvfMapper`] in production; a
    /// [`RecordingMapper`] under test (see [`Gpu::with_mapper`]).
    mapper: Box<dyn GuestMapper>,
    /// Whether a 3D present has already run during the current [`Gpu::process_controlq`] batch.
    ///
    /// `process_controlq` drains the ring until it is empty, inside the MMIO exit, holding
    /// `ctx.pci` — and the expensive part of a control command is the present in [`Gpu::flush_3d`]
    /// (a swapchain acquire bounded at 100 ms, or a full GPU→CPU readback of the scanout). A raw
    /// iteration cap is the wrong bound: the ring is only 256 deep and stopping early risks stalling
    /// a guest that does not re-kick. Bounding the *present* costs nothing, because only the last
    /// frame of a batch can reach the drawable anyway.
    presented_this_batch: bool,
    /// Fenced commands whose response is built but withheld until the host fence fires, oldest first
    /// (see [`Disposition`] and [`Gpu::complete_fence`]). Insertion order is maintained so
    /// publication order follows submission order.
    ///
    /// A `Vec` rather than a `HashMap`: it is bounded at [`MAX_PENDING_FENCES`] and the common
    /// operation is "complete everything on this ring up to id N", which wants ordered iteration, not
    /// point lookup.
    ///
    /// **Known gap, deliberately not closed here:** these entries hold descriptor-chain heads and
    /// guest addresses from the ring as it was programmed when the command arrived. A guest device
    /// reset (`vp_reset` is the very first thing Linux writes at PCI probe) that reprogrammed the ring
    /// while entries were outstanding would publish them into the new one. This device has no reset
    /// hook at all today, so adding one is its own change — noted so it is not added casually. The
    /// exposure is bounded by [`FENCE_DEADLINE`]: after 2 s the map is empty again either way.
    pending_fences: Vec<(FenceKey, PendingFence)>,
    /// The [`MAX_PENDING_FENCES`] cap, as a **value** rather than a constant reference.
    ///
    /// Only so the fallback is reachable from a test: the control ring holds 256 descriptors, i.e. at
    /// most 128 two-descriptor chains for the whole life of a `TestQueue`, so no test can push 1025
    /// real chains to provoke it. A test lowers this; production sets it from the constant, which the
    /// cap test also pins.
    max_pending_fences: usize,
    /// Whether `AEMU_GPU_SYNC_FENCES` forced the pre-deferral behaviour (every fenced command
    /// completed at submit time). Sampled at construction so the whole of item 2.5 is revertable at
    /// runtime without a rebuild.
    sync_fences: bool,
}

impl Gpu {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            default_width: width,
            default_height: height,
            resources: HashMap::new(),
            scanout_resource: 0,
            scanout: ScanoutHandle {
                // Not guest input: this is the VMM's own configured scanout size, so a value outside
                // the device limits is a host misconfiguration and must be loud at construction.
                fb: Arc::new(Mutex::new(Framebuffer::try_new(width, height).expect(
                    "the VMM's default scanout size must be within the device limits",
                ))),
                generation: Arc::new(AtomicU64::new(0)),
                metal_layer: Arc::new(AtomicUsize::new(0)),
                presented: Arc::new(AtomicBool::new(false)),
            },
            renderer: None,
            res3d: HashMap::new(),
            res3d_backing: HashMap::new(),
            capset_size: HashMap::new(),
            blobs: HashMap::new(),
            blob_maps: HashMap::new(),
            mapper: Box::new(HvfMapper),
            presented_this_batch: false,
            pending_fences: Vec::new(),
            max_pending_fences: MAX_PENDING_FENCES,
            sync_fences: sync_fences_forced(),
        }
    }

    /// Whether this device advertises `VIRTIO_GPU_F_RESOURCE_BLOB` (a gfxstream renderer is attached),
    /// so the machine layer knows to give its PCI device the host-visible shared-memory BAR.
    pub fn advertises_blob(&self) -> bool {
        self.renderer.is_some()
    }

    /// Build a virtio-gpu device that advertises `VIRTIO_GPU_F_VIRGL` and routes the guest's 3D
    /// command stream into `renderer` (Phase 3c). 2D scanout (SET_SCANOUT / RESOURCE_FLUSH) keeps
    /// working — a flushed 3D resource is read back into the same scanout the presenter shows.
    pub fn with_renderer(
        width: u32,
        height: u32,
        renderer: Box<dyn super::gpu3d::Renderer3d>,
    ) -> Self {
        let mut g = Self::new(width, height);
        g.renderer = Some(renderer);
        g
    }

    /// Like [`Gpu::with_renderer`] but with the guest-mapping boundary substituted too — the only
    /// way a test can reach `map_blob`'s body without a live `hv_vm_create`d VM.
    #[cfg(test)]
    pub(crate) fn with_mapper(
        width: u32,
        height: u32,
        renderer: Box<dyn super::gpu3d::Renderer3d>,
        mapper: Box<dyn GuestMapper>,
    ) -> Self {
        let mut g = Self::with_renderer(width, height, renderer);
        g.mapper = mapper;
        g
    }

    /// Force the pre-deferral (synchronous) fence behaviour on this device, as
    /// `AEMU_GPU_SYNC_FENCES=1` does for the whole process.
    ///
    /// Test-only, and deliberately a per-device switch rather than an env write: the env is process
    /// global and the test binary runs its tests in parallel, so mutating it would make every other
    /// fence test's outcome depend on scheduling.
    #[cfg(test)]
    pub(crate) fn set_sync_fences(&mut self, on: bool) {
        self.sync_fences = on;
    }

    /// Lower the deferred-fence cap so the [`MAX_PENDING_FENCES`] fallback is reachable without
    /// pushing 1025 descriptor chains through a 256-entry ring. Test-only.
    #[cfg(test)]
    pub(crate) fn set_max_pending_fences(&mut self, n: usize) {
        self.max_pending_fences = n;
    }

    /// Dispatch one command and return its response bytes, discarding whether the device would have
    /// withheld publication until the host fence fired.
    ///
    /// Test-only. It exists because most device tests assert on *response content* — the parsed
    /// fields, the error code, the payload — for which completion timing is irrelevant. The tests
    /// that assert on timing drive [`Gpu::process_controlq`] + [`Gpu::complete_fence`] over a real
    /// `TestQueue` instead, which is the only place the deferral is observable anyway.
    #[cfg(test)]
    fn dispatch_resp(&mut self, mem: &Mem, cmd: &[u8]) -> Vec<u8> {
        self.dispatch(mem, cmd).into_resp()
    }

    /// A cloneable handle to the scanout framebuffer for the display layer.
    pub fn handle(&self) -> ScanoutHandle {
        self.scanout.clone()
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_GPU
    }

    pub fn features(&self) -> u64 {
        // Default: 2D only — no VIRGL / EDID / RESOURCE_BLOB / CONTEXT_INIT — so the guest stays on
        // the plain 2D command set. Phase 3c: when a gfxstream renderer is attached, advertise VIRGL
        // (3D command set) + CONTEXT_INIT (gfxstream selects its Vulkan/GLES renderer via the
        // per-context capset init) so the guest issues the virgl 3D stream this device forwards.
        if self.renderer.is_some() {
            return VIRTIO_F_VERSION_1
                | VIRTIO_GPU_F_EDID
                | super::gpu3d::VIRTIO_GPU_F_VIRGL
                | super::gpu3d::VIRTIO_GPU_F_RESOURCE_BLOB
                | super::gpu3d::VIRTIO_GPU_F_CONTEXT_INIT;
        }
        // EDID is always advertised (2D + 3D): it only adds a read-only display-descriptor query and
        // is what gives the guest connector a physical size → a valid Android DisplayViewport.
        VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID
    }

    pub fn num_queues(&self) -> usize {
        2
    }

    /// virtio_gpu_config: events_read(0), events_clear(4), num_scanouts(8)=1, num_capsets(12).
    /// `num_capsets` is 0 in 2D mode; with a gfxstream renderer it reports the host capset count so
    /// the guest enumerates them via GET_CAPSET_INFO.
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        let mut cfg = [0u8; 16];
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts = 1
        if let Some(r) = &self.renderer {
            cfg[12..16].copy_from_slice(&r.num_capsets().to_le_bytes());
        }
        read_le(&cfg, off as usize, len)
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match idx {
            CONTROLQ => self.process_controlq(mem, queue),
            CURSORQ => self.process_cursorq(mem, queue),
            _ => false,
        }
    }

    /// Drain the cursor queue. We have no host cursor yet: consume the command + ack it so the guest
    /// ring doesn't stall.
    fn process_cursorq(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let (_cmd, writable) = split_chain(mem, chain);
            // Cursor commands (UPDATE/MOVE) carry no response payload; ack via used ring.
            let written = if writable.is_empty() {
                0
            } else {
                let hdr = resp_hdr(VIRTIO_GPU_RESP_OK_NODATA, &ReqHdr::default());
                write_scatter(mem, &writable, &hdr)
            };
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    fn process_controlq(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        // One present per drain (see `presented_this_batch`). Reset here, not in `flush_3d`, so the
        // bound is per guest kick.
        self.presented_this_batch = false;
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let (cmd, writable) = split_chain(mem, chain);
            // `None` ⇒ the chain's device-readable half was rejected before it was assembled (over
            // MAX_CMD_BYTES). The chain is still answered and completed, so the guest ring cannot
            // stall on it.
            let disposition = match cmd {
                Some(cmd) => self.dispatch(mem, &cmd),
                None => Disposition::Complete(
                    resp_hdr(VIRTIO_GPU_RESP_ERR_UNSPEC, &ReqHdr::default()).to_vec(),
                ),
            };
            match disposition {
                Disposition::Complete(resp) => {
                    let written = write_scatter(mem, &writable, &resp);
                    let _ = queue.add_used(mem, head, written);
                    used = true;
                }
                // A host fence now exists for this command; the guest must not see the response —
                // and therefore must not see the fence signalled — until it fires. `Gpu::complete_fence`
                // (fence thread) or `Gpu::sweep_stale_fences` (watchdog) publishes it later.
                //
                // **Insert-before-complete is guaranteed by the transport mutex, and it has to be.**
                // gfxstream may complete a fence *synchronously* from inside `create_fence` /
                // `submit_cmd`, i.e. before `dispatch` has even returned — so the completion can
                // already be sitting in the fence channel when this push happens. That is safe only
                // because this whole drain runs inside the MMIO exit with `ctx.pci` (or `ctx.virtio`)
                // held, and the `gpu-fence` thread must take that same lock before it can look at
                // `pending_fences`. It therefore cannot observe the map until every entry from this
                // batch is in it. Without that ordering, an instantly-completed fence would find
                // nothing pending, and the command it belonged to would sit unpublished until the 2 s
                // watchdog — a 2-second stall per frame rather than a hang, and far harder to see.
                Disposition::Deferred { key, resp } => {
                    self.pending_fences.push((
                        key,
                        PendingFence {
                            head,
                            writable,
                            resp,
                            since: Instant::now(),
                        },
                    ));
                }
            }
        }
        used
    }

    /// Complete every deferred command on `fence`'s ring up to and including its fence id: write each
    /// held response into the chain's writable descriptors and publish the used entry.
    ///
    /// Returns whether anything was published — the caller (the `gpu-fence` thread) then sets the
    /// device ISR bit and raises the device's SPI. **Setting the ISR is not optional**: Linux's
    /// `vp_interrupt` reads it first and returns `IRQ_NONE` when it is zero, so an SPI raised without
    /// it is dropped on the floor.
    ///
    /// `fence_id <= ` rather than `==` on purpose. Fences on one `(ctx_id, ring_idx)` timeline
    /// complete **in order**, and the guest's fence ids are a single monotonic counter
    /// (`drv->current_fence_id`), so a completion at id N implies everything earlier on that ring is
    /// done. Keying strictly on equality would leave an earlier command wedged forever if gfxstream
    /// ever coalesced or dropped a callback — a permanent guest hang recoverable only by the watchdog.
    pub fn complete_fence(&mut self, mem: &Mem, queue: &mut Queue, fence: &FenceReq) -> bool {
        let signalled = FenceKey::of(fence);
        let mut published = false;
        let mut i = 0;
        while i < self.pending_fences.len() {
            let (key, _) = &self.pending_fences[i];
            if key.same_ring(&signalled) && key.fence_id <= signalled.fence_id {
                let (_, p) = self.pending_fences.remove(i);
                Self::publish(mem, queue, &p);
                published = true;
            } else {
                i += 1;
            }
        }
        published
    }

    /// Force-complete every deferred command whose host fence has not fired within
    /// [`FENCE_DEADLINE`], loudly. Returns whether anything was published.
    ///
    /// See [`FENCE_DEADLINE`] for why this exists at all. `now` is a parameter rather than an
    /// `Instant::now()` inside so the deadline is testable without sleeping through it.
    ///
    /// Loud, but **bounded**: one line naming the oldest offender plus a count. A line per swept
    /// command would be up to [`MAX_PENDING_FENCES`] lines every sweep tick, which buries the log this
    /// message exists to be read in.
    pub fn sweep_stale_fences(&mut self, mem: &Mem, queue: &mut Queue, now: Instant) -> bool {
        let mut swept = 0usize;
        let mut first: Option<(FenceKey, Duration)> = None;
        let mut i = 0;
        while i < self.pending_fences.len() {
            let (key, p) = &self.pending_fences[i];
            let waited = now.saturating_duration_since(p.since);
            if waited < FENCE_DEADLINE {
                i += 1;
                continue;
            }
            first.get_or_insert((*key, waited));
            let (_, p) = self.pending_fences.remove(i);
            Self::publish(mem, queue, &p);
            swept += 1;
        }
        if let Some((key, waited)) = first {
            eprintln!(
                "[gpu3d] fence WATCHDOG: {swept} guest command(s) force-completed; the oldest \
                 (ctx={} ring={} fence_id={}) had waited {waited:?}, past the {FENCE_DEADLINE:?} \
                 deadline. The host renderer has stopped signalling fences — the control ring is \
                 being kept moving at the cost of torn or stale frames.",
                key.ctx_id, key.ring_idx, key.fence_id
            );
        }
        swept > 0
    }

    /// Write a held response into its chain and publish the used entry.
    fn publish(mem: &Mem, queue: &mut Queue, p: &PendingFence) {
        let written = write_scatter(mem, &p.writable, &p.resp);
        let _ = queue.add_used(mem, p.head, written);
    }

    /// Everything the `gpu-fence` thread needs, in one call under one lock: complete each fence
    /// gfxstream reported, then run the watchdog sweep. Takes the device's whole queue array so
    /// [`CONTROLQ`] stays this module's business rather than the transport's.
    pub fn drain_fences(
        &mut self,
        mem: &Mem,
        queues: &mut [Queue],
        fences: &[FenceReq],
        now: Instant,
    ) -> bool {
        let Some(queue) = queues.get_mut(CONTROLQ as usize) else {
            return false;
        };
        let mut published = false;
        for f in fences {
            published |= self.complete_fence(mem, queue, f);
        }
        published |= self.sweep_stale_fences(mem, queue, now);
        published
    }

    /// Number of fenced commands currently withheld (test + diagnostics).
    pub fn pending_fence_count(&self) -> usize {
        self.pending_fences.len()
    }

    /// Parse one control command, run it, and decide whether its response may be published now.
    ///
    /// Structure, in order, and each step is load-bearing:
    ///   1. parse the ctrl header **once** into a [`ReqHdr`] (item 2.1);
    ///   2. run the command body;
    ///   3. create the host fence for it if the guest asked for one — from **one** call site covering
    ///      every opcode the device answers (item 2.4);
    ///   4. defer the response iff a host fence was actually created (item 2.5).
    ///
    /// Step 3 is deliberately *after* step 2, not before the opcode match as originally sketched:
    /// `stream_renderer_create_fence` enqueues the fence on the context's ring **behind** whatever is
    /// already queued there, so a fence created before `submit_cmd` would signal before the work it
    /// is supposed to fence. One call site is what makes "fenced command with no host fence"
    /// structurally impossible; its position is what makes the fence mean anything.
    fn dispatch(&mut self, mem: &Mem, cmd: &[u8]) -> Disposition {
        let Some(req) = ReqHdr::parse(cmd) else {
            return Disposition::Complete(
                resp_hdr(VIRTIO_GPU_RESP_ERR_UNSPEC, &ReqHdr::default()).to_vec(),
            );
        };
        let resp = self.dispatch_body(mem, cmd, &req);
        if !self.route_header_fence(&req) {
            return Disposition::Complete(resp);
        }
        // A host fence exists. Withhold the response unless an escape hatch says otherwise.
        if self.sync_fences {
            return Disposition::Complete(resp);
        }
        if self.pending_fences.len() >= self.max_pending_fences {
            // Rate-limited: once the cap is hit every subsequent fenced command takes this branch, so
            // an unlimited log here would flood exactly the path that is already in trouble.
            static CAPPED: AtomicU64 = AtomicU64::new(0);
            let n = CAPPED.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                eprintln!(
                    "[gpu3d] {} fenced commands already awaiting host completion (cap {}) — \
                     ctx={} ring={} fence_id={} and everything after it completes synchronously \
                     until the backlog drains. Further occurrences counted, not logged.",
                    self.pending_fences.len(),
                    self.max_pending_fences,
                    req.ctx_id,
                    req.ring_idx,
                    req.fence_id
                );
            }
            return Disposition::Complete(resp);
        }
        Disposition::Deferred {
            key: req.fence_key(),
            resp,
        }
    }

    /// Dispatch a command whose header has already been parsed, returning the response bytes.
    fn dispatch_body(&mut self, mem: &Mem, cmd: &[u8], req: &ReqHdr) -> Vec<u8> {
        let ok = |t: u32| resp_hdr(t, req).to_vec();

        match req.cmd_type {
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => self.resp_display_info(req),
            VIRTIO_GPU_CMD_GET_EDID => self.resp_edid(req),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => {
                // hdr(24) resource_id(24) format(28) width(32) height(36)
                let id = read_le(cmd, 24, 4) as u32;
                let format = read_le(cmd, 28, 4) as u32;
                let width = read_le(cmd, 32, 4) as u32;
                let height = read_le(cmd, 36, 4) as u32;
                if id == 0 || width == 0 || height == 0 {
                    return ok(VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
                }
                // Dimensions were previously only checked for zero, so `width * height * BPP` was a
                // guest-chosen `u32` product: 65536² overflowed it outright and 8192² asked for
                // 256 MiB per resource with no limit on how many.
                let Some(bytes) = resource_bytes(width, height) else {
                    eprintln!(
                        "[gpu] resource_create_2d res={id} {width}x{height} rejected: over \
                         MAX_DIM {MAX_DIM} or MAX_RESOURCE_BYTES {MAX_RESOURCE_BYTES:#x}"
                    );
                    return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                };
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        backing: Vec::new(),
                        pixels: vec![0u8; bytes],
                    },
                );
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_RESOURCE_UNREF => {
                let id = read_le(cmd, 24, 4) as u32;
                self.resources.remove(&id);
                if self.scanout_resource == id {
                    self.scanout_resource = 0;
                }
                self.release_3d_resource(id);
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_SET_SCANOUT => {
                // hdr(24) rect(24..40) scanout_id(40) resource_id(44)
                let scanout_id = read_le(cmd, 40, 4) as u32;
                let resource_id = read_le(cmd, 44, 4) as u32;
                if scanout_id as usize >= 1 {
                    // only scanout 0 exists
                    return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                }
                self.scanout_resource = resource_id;
                #[allow(unused_mut)] // `dims` is only reassigned on the feature build (3D lookup).
                let mut dims = self
                    .resources
                    .get(&resource_id)
                    .map(|r| (r.width, r.height));
                // Phase 3c: a virgl scanout resource lives in the renderer, not the 2D map.
                if dims.is_none() {
                    dims = self.res3d.get(&resource_id).copied();
                }
                if let Some((w, h)) = dims {
                    // A 3D resource's dimensions are NOT validated at create time (a `PIPE_BUFFER`
                    // resource legitimately reports its byte size as `width`), so this is where a
                    // resource bound to the scanout is first required to be a presentable image.
                    if !self.resize_scanout(w, h) {
                        return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                    }
                }
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => {
                // hdr(24) resource_id(24) nr_entries(28) then nr_entries * mem_entry(16: addr,len,pad)
                let id = read_le(cmd, 24, 4) as u32;
                let nr = read_le(cmd, 28, 4) as u32 as usize;
                // `nr` is guest-chosen and used to be a `Vec::with_capacity(nr)` (up to 4 G entries),
                // and a truncated tail was silently dropped — which shifts the offset of every entry
                // after it, so the resource is assembled out of the wrong bytes.
                let Some(backing) = parse_mem_entries(cmd, 32, nr) else {
                    eprintln!(
                        "[gpu] attach_backing res={id} nr_entries={nr} cmd_len={} rejected: over \
                         MAX_MEM_ENTRIES {MAX_MEM_ENTRIES} or the command is truncated",
                        cmd.len()
                    );
                    return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                };
                if backing_bytes(&backing).is_none() {
                    eprintln!(
                        "[gpu] attach_backing res={id} entries={} rejected: total backing exceeds \
                         MAX_RESOURCE_BYTES {MAX_RESOURCE_BYTES:#x}",
                        backing.len()
                    );
                    return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                }
                // Range-validate every entry BEFORE any of it is recorded or forwarded, and fail the
                // whole command on the first bad one. `filter_map`ping the bad entries away (what this
                // used to do for the 3D path) silently renumbers the rest of the backing.
                let iovs = match resolve_iovs(mem, &backing) {
                    Ok(iovs) => iovs,
                    Err(i) => {
                        let (addr, len) = backing[i];
                        eprintln!(
                            "[gpu] attach_backing res={id} entry={i} addr={addr:#x} len={len:#x} \
                             is not wholly inside guest RAM — rejecting the whole command"
                        );
                        return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                    }
                };
                if let Some(res) = self.resources.get_mut(&id) {
                    res.backing = backing;
                    return ok(VIRTIO_GPU_RESP_OK_NODATA);
                }
                // A virgl 3D resource — record its backing + register the iovecs with gfxstream.
                //
                // These must be *stable host pointers into mapped guest RAM*, resolved the same way
                // `RESOURCE_CREATE_BLOB` resolves its mem entries. gfxstream stores whatever we pass
                // in the resource (`VirtioGpuResource::AttachIov` → `mIovs`) and dereferences it on
                // later transfers, so handing it a gathered host copy leaves it holding a pointer
                // into freed memory the moment this command returns.
                if self.res3d.contains_key(&id) {
                    self.res3d_backing.insert(id, backing);
                    if let Some(r) = self.renderer.as_mut() {
                        let rc = r.attach_backing(id, &iovs);
                        if rc != 0 {
                            eprintln!(
                                "[gpu3d] attach_backing res={id} entries={} -> rc={rc}",
                                iovs.len()
                            );
                            return ok(VIRTIO_GPU_RESP_ERR_UNSPEC);
                        }
                    }
                    return ok(VIRTIO_GPU_RESP_OK_NODATA);
                }
                ok(VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID)
            }
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING => {
                let id = read_le(cmd, 24, 4) as u32;
                if let Some(res) = self.resources.get_mut(&id) {
                    res.backing.clear();
                }
                // The guest is revoking this memory, so gfxstream must drop the iovecs it is holding
                // into it — otherwise the next transfer dereferences a pointer the guest has reused.
                if self.res3d_backing.remove(&id).is_some() {
                    if let Some(r) = self.renderer.as_mut() {
                        r.detach_backing(id);
                    }
                }
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D => {
                // hdr(24) rect(24..40) offset(40) resource_id(48)
                let rect = Rect {
                    x: read_le(cmd, 24, 4) as u32,
                    y: read_le(cmd, 28, 4) as u32,
                    width: read_le(cmd, 32, 4) as u32,
                    height: read_le(cmd, 36, 4) as u32,
                };
                let offset = read_le(cmd, 40, 8);
                let id = read_le(cmd, 48, 4) as u32;
                if self.transfer_to_host(mem, id, rect, offset) {
                    ok(VIRTIO_GPU_RESP_OK_NODATA)
                } else {
                    ok(VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID)
                }
            }
            VIRTIO_GPU_CMD_RESOURCE_FLUSH => {
                // hdr(24) rect(24..40) resource_id(40)
                let id = read_le(cmd, 40, 4) as u32;
                // Phase 3c present coupling: if the flushed (or bound) resource is a virgl 3D
                // resource, read its pixels back out of gfxstream into the scanout the CAMetalLayer
                // presenter shows; otherwise fall through to the 2D copy.
                {
                    let rid = if self.res3d.contains_key(&id) {
                        id
                    } else if self.res3d.contains_key(&self.scanout_resource) {
                        self.scanout_resource
                    } else {
                        0
                    };
                    if rid != 0 {
                        self.flush_3d(rid);
                        return ok(VIRTIO_GPU_RESP_OK_NODATA);
                    }
                }
                self.flush(id);
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_UPDATE_CURSOR | VIRTIO_GPU_CMD_MOVE_CURSOR => {
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            _ => {
                // Phase 3c: the 3D/virgl command block (0x02xx) + capset queries route into the
                // gfxstream renderer when one is attached. Feature-off (or no renderer) ⇒ ERR_UNSPEC,
                // exactly the 2D behaviour.
                if self.renderer.is_some() {
                    if let Some(resp) = self.dispatch_3d(mem, cmd, req) {
                        return resp;
                    }
                }
                ok(VIRTIO_GPU_RESP_ERR_UNSPEC)
            }
        }
    }

    fn resp_display_info(&self, req: &ReqHdr) -> Vec<u8> {
        let mut buf = vec![0u8; CTRL_HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * 24];
        buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_DISPLAY_INFO, req));
        // pmodes[0] = { rect{0,0,W,H}, enabled=1, flags=0 }.
        let base = CTRL_HDR_LEN;
        buf[base + 8..base + 12].copy_from_slice(&self.default_width.to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&self.default_height.to_le_bytes());
        buf[base + 16..base + 20].copy_from_slice(&1u32.to_le_bytes()); // enabled
        buf
    }

    /// Answer `VIRTIO_GPU_CMD_GET_EDID` with a `virtio_gpu_resp_edid`:
    /// `hdr(24) | size:u32 | padding:u32 | edid[..]`. The guest copies `size` bytes of `edid` into
    /// its DRM connector; a valid 128-byte base block with a physical image size gives it
    /// `width_mm`/`height_mm` > 0 → a real dpi in Android's DisplayViewport.
    fn resp_edid(&self, req: &ReqHdr) -> Vec<u8> {
        let edid = build_edid(self.default_width, self.default_height, EDID_DPI);
        let mut buf = vec![0u8; CTRL_HDR_LEN + 8 + edid.len()];
        buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_EDID, req));
        buf[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&(edid.len() as u32).to_le_bytes());
        // padding (CTRL_HDR_LEN+4 .. +8) stays 0.
        buf[CTRL_HDR_LEN + 8..].copy_from_slice(&edid);
        buf
    }

    /// Copy the rect of guest backing into the host resource image (qemu 2D transfer semantics).
    ///
    /// The rect is guest-controlled and the row arithmetic used to be `u32`: `rect.x * BPP` and
    /// `(rect.y + row) * stride` both overflow for large coordinates — a panic in a debug build, a
    /// wrapped (i.e. *in-bounds-looking*) destination offset in a release build. Everything here is
    /// `u64`, and the whole transfer is gated on an explicit in-bounds precondition rather than the
    /// per-row `<=` guard, which silently copied the rows that happened to fit.
    fn transfer_to_host(&mut self, mem: &Mem, id: u32, rect: Rect, offset: u64) -> bool {
        let Some(res) = self.resources.get_mut(&id) else {
            return false;
        };
        if res.backing.is_empty() {
            return true; // nothing attached yet, but not an error
        }
        let (rx, ry) = (u64::from(rect.x), u64::from(rect.y));
        let (rw, rh) = (u64::from(rect.width), u64::from(rect.height));
        if rx + rw > u64::from(res.width) || ry + rh > u64::from(res.height) {
            eprintln!(
                "[gpu] transfer_to_host_2d res={id} rect {rw}x{rh}+{rx}+{ry} lies outside the \
                 {}x{} resource — no pixels transferred",
                res.width, res.height
            );
            return true;
        }
        let Some(backing) = gather_backing(mem, &res.backing) else {
            eprintln!("[gpu] transfer_to_host_2d res={id}: backing too large to gather");
            return false;
        };
        let stride = u64::from(res.width) * u64::from(BPP);
        let rowlen = (rw * u64::from(BPP)) as usize;
        for row in 0..rh {
            // `offset` is a raw u64 from the guest, so the source can be anywhere; the destination is
            // in-bounds by the precondition above.
            let Some(src) = offset
                .checked_add(row * stride)
                .and_then(|v| usize::try_from(v).ok())
            else {
                break;
            };
            let dst = ((ry + row) * stride + rx * u64::from(BPP)) as usize;
            if src + rowlen <= backing.len() && dst + rowlen <= res.pixels.len() {
                res.pixels[dst..dst + rowlen].copy_from_slice(&backing[src..src + rowlen]);
            }
        }
        true
    }

    /// Present the flushed resource: copy its image into the scanout framebuffer + bump generation.
    fn flush(&mut self, id: u32) {
        // Prefer the flushed resource; fall back to whatever is bound to the scanout.
        let rid = if self.resources.contains_key(&id) {
            id
        } else {
            self.scanout_resource
        };
        let Some((w, h)) = self.resources.get(&rid).map(|r| (r.width, r.height)) else {
            return;
        };
        if !self.resize_scanout(w, h) {
            return;
        }
        let Some(res) = self.resources.get(&rid) else {
            return;
        };
        let mut fb = self.scanout.fb.lock().unwrap();
        let n = fb.bytes.len().min(res.pixels.len());
        fb.bytes[..n].copy_from_slice(&res.pixels[..n]);
        drop(fb);
        self.scanout.generation.fetch_add(1, Ordering::Release);
    }

    /// Make the scanout framebuffer `w`×`h`, reallocating only if it is not already that size.
    ///
    /// `false` — nothing changed, the caller must reject the command — when `w`/`h` are outside the
    /// device limits. This is the single place a guest-supplied resolution turns into a host
    /// allocation, so it is the single place that has to be bounded: `SET_SCANOUT`,
    /// `SET_SCANOUT_BLOB`, the 2D flush and the 3D flush all route through here.
    fn resize_scanout(&mut self, w: u32, h: u32) -> bool {
        let mut fb = self.scanout.fb.lock().unwrap();
        if fb.width == w && fb.height == h {
            return true;
        }
        match Framebuffer::try_new(w, h) {
            Some(next) => {
                *fb = next;
                true
            }
            None => {
                eprintln!(
                    "[gpu] scanout {w}x{h} rejected: over MAX_DIM {MAX_DIM} or \
                     MAX_RESOURCE_BYTES {MAX_RESOURCE_BYTES:#x}"
                );
                false
            }
        }
    }
}

/// Whether the operator forced the legacy CPU-readback present path via `AEMU_PRESENT_READBACK=1`
/// (in-Vulkan present into the host CAMetalLayer is the default). When set, the display thread also
/// declines to publish a layer, so the readback path is the only one that can run. Evaluated once.
pub(crate) fn present_readback_forced() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| std::env::var_os("AEMU_PRESENT_READBACK").is_some())
}

/// One-shot log guards so the per-frame present path logs its mode exactly once.
static PRESENT_LOGGED: AtomicBool = AtomicBool::new(false);
static PRESENT_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

/// Phase 3c: the 3D/virgl command routing + present coupling. All feature-gated so the default build
/// compiles none of it.
impl Gpu {
    /// Route a 3D/virgl (or capset) control command into the attached renderer. Returns the response
    /// bytes, or `None` if `cmd_type` isn't one this device handles (the caller answers ERR_UNSPEC).
    /// Precondition: `self.renderer.is_some()` (the caller checks before dispatching here).
    fn dispatch_3d(&mut self, mem: &Mem, cmd: &[u8], req: &ReqHdr) -> Option<Vec<u8>> {
        use super::gpu3d::*;
        let nodata = |t: u32| resp_hdr(t, req).to_vec();
        // ctx_id came out of the ctrl header in `ReqHdr::parse`; every 3D command carries it there.
        let ctx_id = req.ctx_id;

        match req.cmd_type {
            VIRTIO_GPU_CMD_CTX_CREATE => {
                let nlen = read_le(cmd, 24, 4) as usize;
                let context_init = read_le(cmd, 28, 4) as u32;
                let start: usize = 32;
                let end = start.saturating_add(nlen).min(cmd.len());
                let name = cmd.get(start..end).unwrap_or(&[]);
                self.renderer
                    .as_mut()
                    .unwrap()
                    .context_create(ctx_id, context_init, name);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_DESTROY => {
                self.renderer.as_mut().unwrap().context_destroy(ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => {
                let res = read_le(cmd, 24, 4) as u32;
                self.renderer
                    .as_mut()
                    .unwrap()
                    .ctx_attach_resource(ctx_id, res);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => {
                let res = read_le(cmd, 24, 4) as u32;
                self.renderer
                    .as_mut()
                    .unwrap()
                    .ctx_detach_resource(ctx_id, res);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_RESOURCE_CREATE_3D => {
                let args = ResourceCreate3d {
                    resource_id: read_le(cmd, 24, 4) as u32,
                    target: read_le(cmd, 28, 4) as u32,
                    format: read_le(cmd, 32, 4) as u32,
                    bind: read_le(cmd, 36, 4) as u32,
                    width: read_le(cmd, 40, 4) as u32,
                    height: read_le(cmd, 44, 4) as u32,
                    depth: read_le(cmd, 48, 4) as u32,
                    array_size: read_le(cmd, 52, 4) as u32,
                    last_level: read_le(cmd, 56, 4) as u32,
                    nr_samples: read_le(cmd, 60, 4) as u32,
                    flags: read_le(cmd, 64, 4) as u32,
                };
                self.res3d
                    .insert(args.resource_id, (args.width, args.height));
                let rc = self.renderer.as_mut().unwrap().resource_create_3d(&args);
                warn_rc(rc, "resource_create_3d", args.resource_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => {
                let xfer = parse_transfer3d(cmd);
                // Copy the guest backing this transfer feeds gfxstream from. A resource with no
                // `RESOURCE_ATTACH_BACKING` of its own has NO gathered data, which is not the same as
                // zero bytes of data: the empty case must reach the renderer as `None` so gfxstream
                // uses the resource's own iovecs instead of being handed a zero-length one that
                // overrides them (see `Renderer3d::transfer_to_host_3d`).
                let mut data = match self.res3d_backing.get(&xfer.resource_id) {
                    Some(backing) => match gather_backing(mem, backing) {
                        Some(data) => Some(data),
                        None => {
                            eprintln!(
                                "[gpu3d] transfer_to_host_3d res={} rejected: backing too large \
                                 to gather",
                                xfer.resource_id
                            );
                            return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                        }
                    },
                    None => None,
                };
                let payload = data.as_deref_mut().filter(|d| !d.is_empty());
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .transfer_to_host_3d(ctx_id, &xfer, payload);
                warn_rc(rc, "transfer_to_host_3d", xfer.resource_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => {
                let xfer = parse_transfer3d(cmd);
                // Read host resource → a scratch buffer sized to the box, then scatter into the
                // guest backing so the guest sees the pixels.
                let (bw, bh) = (xfer.box_[3].max(1), xfer.box_[4].max(1));
                // `(bw * bh * BPP) as usize` was `u32` arithmetic over a guest-supplied box: a
                // 65536×65536 box overflowed it. The dimensions are deliberately NOT clamped to
                // MAX_DIM here — only the total is bounded — because a 3D *buffer* resource reports
                // its byte size as the box width with height 1, and that is a legitimate transfer.
                let Some(scratch) = transfer_scratch_bytes(bw, bh) else {
                    eprintln!(
                        "[gpu3d] transfer_from_host_3d res={} box {bw}x{bh} rejected: over \
                         MAX_RESOURCE_BYTES {MAX_RESOURCE_BYTES:#x}",
                        xfer.resource_id
                    );
                    return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                };
                let mut data = vec![0u8; scratch];
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .transfer_from_host_3d(ctx_id, &xfer, &mut data);
                warn_rc(rc, "transfer_from_host_3d", xfer.resource_id);
                if let Some(backing) = self.res3d_backing.get(&xfer.resource_id) {
                    scatter_backing(mem, backing, &data);
                }
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_SUBMIT_3D => {
                let size = read_le(cmd, 24, 4) as usize;
                let start: usize = 32; // hdr(24) + size(4) + padding(4)
                let end = start.saturating_add(size).min(cmd.len());
                let mut payload = cmd.get(start..end).unwrap_or(&[]).to_vec();
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .submit_cmd(ctx_id, &mut payload);
                warn_rc(rc, "submit_cmd", ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB => {
                let blob = CreateBlob {
                    resource_id: read_le(cmd, 24, 4) as u32,
                    blob_mem: read_le(cmd, 28, 4) as u32,
                    blob_flags: read_le(cmd, 32, 4) as u32,
                    nr_entries: read_le(cmd, 36, 4) as u32,
                    blob_id: read_le(cmd, 40, 8),
                    size: read_le(cmd, 48, 8),
                };
                // Resolve the trailing `nr_entries` guest mem-entries (addr,len,pad @ offset 56) into
                // host iovecs. A HOST3D blob (the ASG ring) carries none — the host allocates it.
                //
                // gfxstream stores these `(ptr, len)` pairs and memcpys within `iov_len` later, so an
                // entry that is not wholly inside guest RAM is an out-of-bounds host access, and an
                // entry silently skipped renumbers every entry after it. Both fail the command.
                let Some(entries) = parse_mem_entries(cmd, 56, blob.nr_entries as usize) else {
                    eprintln!(
                        "[gpu3d] create_blob res={} nr_entries={} cmd_len={} rejected: over \
                         MAX_MEM_ENTRIES {MAX_MEM_ENTRIES} or the command is truncated",
                        blob.resource_id,
                        blob.nr_entries,
                        cmd.len()
                    );
                    return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                };
                let iovs = match resolve_iovs(mem, &entries) {
                    Ok(iovs) => iovs,
                    Err(i) => {
                        let (addr, len) = entries[i];
                        eprintln!(
                            "[gpu3d] create_blob res={} entry={i} addr={addr:#x} len={len:#x} is \
                             not wholly inside guest RAM — rejecting the whole command",
                            blob.resource_id
                        );
                        return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                    }
                };
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .create_blob(ctx_id, &blob, &iovs);
                if rc == 0 {
                    self.blobs.insert(blob.resource_id, blob.size);
                } else {
                    eprintln!(
                        "[gpu3d] create_blob res={} mem={} flags={:#x} size={:#x} -> rc={rc}",
                        blob.resource_id, blob.blob_mem, blob.blob_flags, blob.size
                    );
                }
                Some(nodata(if rc == 0 {
                    VIRTIO_GPU_RESP_OK_NODATA
                } else {
                    VIRTIO_GPU_RESP_ERR_UNSPEC
                }))
            }
            VIRTIO_GPU_CMD_SET_SCANOUT_BLOB => {
                // hdr(24) rect(24..40) scanout_id(40) resource_id(44) width(48) height(52)
                // format(56) padding(60) strides[4](64..80) offsets[4](80..96)
                let scanout_id = read_le(cmd, 40, 4) as u32;
                let resource_id = read_le(cmd, 44, 4) as u32;
                let width = read_le(cmd, 48, 4) as u32;
                let height = read_le(cmd, 52, 4) as u32;
                if scanout_id >= 1 {
                    return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                }
                // resource_id 0 disables the scanout; otherwise adopt the blob's own dimensions,
                // which — unlike SET_SCANOUT — arrive in the command rather than from a prior
                // RESOURCE_CREATE_2D/_3D, so record them for the flush + present path.
                if resource_id != 0 && width != 0 && height != 0 {
                    // `width`/`height` arrive raw in the command — nothing else has vetted them — and
                    // they used to size a `Framebuffer` directly, while holding the scanout mutex.
                    // Reject before recording the resource, so a rejected scanout leaves no state.
                    if !self.resize_scanout(width, height) {
                        eprintln!(
                            "[gpu3d] set_scanout_blob res={resource_id} {width}x{height} rejected"
                        );
                        return Some(nodata(VIRTIO_GPU_RESP_ERR_UNSPEC));
                    }
                    self.res3d.insert(resource_id, (width, height));
                }
                self.scanout_resource = resource_id;
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID => {
                // hdr(24) resource_id(24) padding(28) -> resp_hdr(24) uuid[16].
                //
                // The UUID only has to be stable and collision-free within this device, so derive it
                // from the resource id rather than inventing a registry: byte-identical for repeated
                // queries on the same resource, distinct across resources.
                let resource_id = read_le(cmd, 24, 4) as u32;
                let mut buf = vec![0u8; CTRL_HDR_LEN + 16];
                buf[..CTRL_HDR_LEN]
                    .copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_RESOURCE_UUID, req));
                buf[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(b"AEMU");
                buf[CTRL_HDR_LEN + 12..CTRL_HDR_LEN + 16]
                    .copy_from_slice(&resource_id.to_be_bytes());
                Some(buf)
            }
            VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB => {
                // hdr(24) resource_id(24) padding(28) offset(32): map the host blob into the guest's
                // shared-memory window at PCI_SHM_BASE + offset via hv_vm_map (zero-copy ASG ring).
                let resource_id = read_le(cmd, 24, 4) as u32;
                let offset = read_le(cmd, 32, 8);
                Some(self.map_blob(resource_id, offset, req))
            }
            VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => {
                let resource_id = read_le(cmd, 24, 4) as u32;
                self.unmap_blob(resource_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_GET_CAPSET_INFO => {
                let index = read_le(cmd, 24, 4) as u32;
                let (id, max_ver, max_size) = self.renderer.as_mut().unwrap().capset_info(index);
                self.capset_size.insert(id, max_size);
                let mut buf = vec![0u8; CTRL_HDR_LEN + 16];
                buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_CAPSET_INFO, req));
                buf[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&id.to_le_bytes());
                buf[CTRL_HDR_LEN + 4..CTRL_HDR_LEN + 8].copy_from_slice(&max_ver.to_le_bytes());
                buf[CTRL_HDR_LEN + 8..CTRL_HDR_LEN + 12].copy_from_slice(&max_size.to_le_bytes());
                Some(buf)
            }
            VIRTIO_GPU_CMD_GET_CAPSET => {
                let capset_id = read_le(cmd, 24, 4) as u32;
                let version = read_le(cmd, 28, 4) as u32;
                let size = self.capset_size.get(&capset_id).copied().unwrap_or(4096) as usize;
                let mut caps = vec![0u8; size];
                self.renderer
                    .as_mut()
                    .unwrap()
                    .fill_caps(capset_id, version, &mut caps);
                let mut buf = vec![0u8; CTRL_HDR_LEN + size];
                buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_CAPSET, req));
                buf[CTRL_HDR_LEN..].copy_from_slice(&caps);
                Some(buf)
            }
            _ => None,
        }
    }

    /// If the ctrl header asked for a fence, create the matching host fence
    /// (`stream_renderer_create_fence`), which will later signal
    /// [`gpu3d::on_fence_complete`](super::gpu3d::on_fence_complete).
    ///
    /// Returns **whether a host fence now exists that is going to complete** — the only condition
    /// under which the guest's response may be withheld. Every other case (`no fence requested`, `no
    /// renderer attached`, `create_fence` refused) must answer synchronously, because nothing will
    /// ever arrive to publish it.
    ///
    /// This is the single call site for *all* ~24 opcodes the device answers. It used to be called
    /// from ten of them, so `RESOURCE_MAP_BLOB`, `RESOURCE_ASSIGN_UUID`, `GET_CAPSET_INFO`,
    /// `GET_CAPSET` and every 2D command created no host fence even with `FLAG_FENCE` set — harmless
    /// while responses were synchronous, a guaranteed guest hang the moment they are not.
    fn route_header_fence(&mut self, req: &ReqHdr) -> bool {
        if !req.fenced() {
            return false;
        }
        let Some(r) = self.renderer.as_mut() else {
            return false;
        };
        let rc = r.create_fence(&FenceReq {
            flags: req.flags,
            fence_id: req.fence_id,
            ctx_id: req.ctx_id,
            ring_idx: req.ring_idx,
        });
        if rc != 0 {
            eprintln!(
                "[gpu3d] create_fence ctx={} ring={} fence_id={} -> rc={rc} — no host fence will \
                 signal, so the command is completed synchronously",
                req.ctx_id, req.ring_idx, req.fence_id
            );
            return false;
        }
        true
    }

    /// Present coupling for a flushed virgl 3D scanout ColorBuffer. Two modes, selected by whether
    /// the display thread published a `CAMetalLayer*` on [`ScanoutHandle::metal_layer`]:
    ///
    ///   * **In-Vulkan present (windowed `--display`)** — hand the layer to the host renderer
    ///     ([`Renderer3d::present_to_layer`] → `stream_renderer_present_to_metal_layer`), which owns
    ///     the `VkSurfaceKHR`/`VK_KHR_swapchain` on it and blits the ColorBuffer in on the GPU. No
    ///     `stream_renderer_transfer_read` and no CPU framebuffer write on the present hot-path.
    ///   * **Readback (headless `--control`, or `AEMU_PRESENT_READBACK=1`, or a hard present
    ///     failure)** — read the pixels back into the CPU scanout framebuffer, which is what
    ///     softbuffer and every control verb consume.
    ///
    /// Either way the flush generation is bumped so the display layer redraws and `wait_stable`
    /// advances.
    fn flush_3d(&mut self, rid: u32) {
        let Some((w, h)) = self.res3d.get(&rid).copied() else {
            return;
        };
        // A 3D resource's dimensions are guest-supplied and unvalidated at create time, so a flush of
        // one that is not a presentable image is refused here rather than allocating for it.
        let Some(readback_bytes) = resource_bytes(w, h) else {
            eprintln!("[gpu3d] flush res={rid} {w}x{h} is not a presentable scanout — ignored");
            return;
        };
        if !self.resize_scanout(w, h) {
            return;
        }

        // At most one present per control-queue batch. Everything above this point is cheap
        // bookkeeping; everything below is either a swapchain acquire (bounded at 100 ms host-side)
        // or a full-frame GPU→CPU readback, and a guest that queues N flushes in one kick can only
        // ever show the last of them. The generation still advances per flush so `wait_stable` and
        // the display thread's redraw trigger see every frame boundary.
        if self.presented_this_batch {
            self.scanout.generation.fetch_add(1, Ordering::Release);
            return;
        }
        self.presented_this_batch = true;

        // In-Vulkan present. When the display thread published a CAMetalLayer, hand it to the host
        // renderer, which owns the VkSurfaceKHR + VK_KHR_swapchain on that layer and blits the
        // scanout ColorBuffer in with vkCmdBlitImage (which does the RGBA->BGRA conversion AND the
        // guest-resolution -> window-resolution scale in one command) before vkQueuePresentKHR.
        // Nothing crosses the FFI boundary in either direction but a resource id and a pointer.
        let layer = self.scanout.metal_layer.load(Ordering::Acquire);
        if layer != 0 && !present_readback_forced() {
            let rc = self.renderer.as_mut().unwrap().present_to_layer(rid, layer);
            // rc > 0 means the renderer deliberately dropped the frame (no swapchain image free in
            // time, or it is rebuilding after a resize). That is a healthy present path, so do NOT
            // degrade to readback for it — just bump the generation and move on.
            if rc >= 0 {
                if !PRESENT_LOGGED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[gpu3d] in-Vulkan present ACTIVE: scanout ColorBuffer res={rid} {w}x{h} \
                         -> CAMetalLayer {layer:#x} (vkCmdBlitImage + vkQueuePresentKHR, no CPU \
                         readback)"
                    );
                }
                // Only now is it safe for the display thread to stop blitting: a frame has actually
                // reached the drawable. rc > 0 (deliberately dropped frame) still counts — the
                // swapchain is healthy and the previously presented frame is still on screen.
                self.scanout.presented.store(true, Ordering::Release);
                self.scanout.generation.fetch_add(1, Ordering::Release);
                return;
            }
            // A hard present failure hands the window back to the CPU blit rather than freezing on
            // the last good frame.
            self.scanout.presented.store(false, Ordering::Release);
            if !PRESENT_FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[gpu3d] in-Vulkan present unavailable for res={rid} (rc={rc}) — falling back \
                     to stream_renderer_transfer_read readback"
                );
            }
        }

        // Readback present. This is the ONLY path in headless `--control` mode — that mode opens no
        // window, so no CAMetalLayer exists and `metal_layer` is permanently 0 — and it is the path
        // `AEMU_PRESENT_READBACK=1` forces. It fills the CPU scanout framebuffer, which is what
        // `display::present` (softbuffer) and every control verb (`frame`, `screenshot`, `hash`,
        // `info`, `wait_stable`) read. `read_pixels` swizzles to BGRA, preserving the framebuffer's
        // byte-order invariant.
        //
        // Settle the guest's in-flight GPU work on this ColorBuffer FIRST. `read_pixels`
        // (`stream_renderer_transfer_read_iov`) records its own copy command and waits only for
        // that; nothing in gfxstream's read path waits on the ColorBuffer's `latestUse`, i.e. the
        // rendering the guest submitted to produce this frame. Reading without the settle samples a
        // half-drawn image — visible as parts of the screen going black in a shifting, random
        // pattern while the rest of the frame is correct.
        self.renderer.as_mut().unwrap().wait_sync(rid);
        let mut tmp = vec![0u8; readback_bytes];
        let rc = self
            .renderer
            .as_mut()
            .unwrap()
            .read_pixels(rid, w, h, &mut tmp);
        if rc == 0 {
            let mut fb = self.scanout.fb.lock().unwrap();
            let n = fb.bytes.len().min(tmp.len());
            fb.bytes[..n].copy_from_slice(&tmp[..n]);
        }
        self.scanout.generation.fetch_add(1, Ordering::Release);
    }

    /// `RESOURCE_MAP_BLOB`: ask gfxstream for the blob's host pointer + size, then `hv_vm_map` that
    /// host memory into the guest's shared-memory window at `PCI_SHM_BASE + offset` (RW). This is the
    /// zero-copy ASG ring — the guest's Mesa `gfxstream_vk` then reads/writes the same bytes the host
    /// renderer does. Returns the wire response (`OK_MAP_INFO` on success, `ERR_UNSPEC` on any
    /// failure, with the exact cause logged so a boot report can name the next blocker).
    fn map_blob(&mut self, resource_id: u32, offset: u64, req: &ReqHdr) -> Vec<u8> {
        use crate::virtio::pci::{PCI_SHM_BASE, PCI_SHM_SIZE};
        let err = |t: u32| resp_hdr(t, req).to_vec();

        let Some((host_ptr, size)) = self.renderer.as_mut().unwrap().resource_map(resource_id)
        else {
            eprintln!("[gpu3d] map_blob res={resource_id}: stream_renderer_resource_map failed");
            return err(VIRTIO_GPU_RESP_ERR_UNSPEC);
        };
        // The window check MUST precede `PCI_SHM_BASE + offset`: `offset` is a raw guest u64, so that
        // sum overflows for a large one — a panic under `overflow-checks` and a wrapped (small,
        // plausible-looking) IPA without it. Ordering the check first makes the sum unreachable for
        // any offset that could overflow it.
        if offset
            .checked_add(size)
            .is_none_or(|end| end > PCI_SHM_SIZE)
        {
            eprintln!(
                "[gpu3d] map_blob res={resource_id} offset={offset:#x} size={size:#x} exceeds the {PCI_SHM_SIZE:#x} shmem window"
            );
            let _ = self.renderer.as_mut().unwrap().resource_unmap(resource_id);
            return err(VIRTIO_GPU_RESP_ERR_UNSPEC);
        }
        let ipa = PCI_SHM_BASE + offset;
        // hv_vm_map requires IPA / host / size all aligned to the host granule (16 KiB on Apple
        // Silicon). A guest that allocated the blob at a 4 KiB-aligned offset will trip this — log it
        // precisely (it is the most likely next blocker), then still attempt so HVF's rc is reported.
        let page = crate::host_page_size() as u64;
        if !ipa.is_multiple_of(page) || host_ptr % page != 0 || size % page != 0 {
            eprintln!(
                "[gpu3d] map_blob res={resource_id} ALIGNMENT ipa={ipa:#x} host={host_ptr:#x} size={size:#x} page={page:#x} \
                 (not host-granule aligned)"
            );
        }
        if let Err(rc) = self.mapper.map(host_ptr, ipa, size) {
            eprintln!(
                "[gpu3d] map_blob res={resource_id} hv_vm_map(host={host_ptr:#x}, ipa={ipa:#x}, size={size:#x}) -> {} ({:#010x})",
                crate::hv_return_name(rc),
                rc as u32
            );
            // The host-side map succeeded, so it must be released again — otherwise the renderer
            // holds a host mapping the guest can never reach and never unmaps.
            let _ = self.renderer.as_mut().unwrap().resource_unmap(resource_id);
            return err(VIRTIO_GPU_RESP_ERR_UNSPEC);
        }
        self.blob_maps.insert(resource_id, (offset, size));
        let map_info = self
            .renderer
            .as_mut()
            .unwrap()
            .resource_map_info(resource_id);
        println!(
            "[gpu3d] map_blob res={resource_id} -> ipa {ipa:#x} size {size:#x} map_info {map_info:#x} (host-visible ASG ring live)"
        );
        let mut buf = vec![0u8; CTRL_HDR_LEN + 8];
        buf[..CTRL_HDR_LEN]
            .copy_from_slice(&resp_hdr(super::gpu3d::VIRTIO_GPU_RESP_OK_MAP_INFO, req));
        buf[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&map_info.to_le_bytes());
        buf
    }

    /// `RESOURCE_UNREF` for a resource that may live in the host renderer: tear the whole thing down
    /// in the right order — drop any live host-visible blob mapping first (an `hv_vm_map`'d region
    /// must not outlive the host allocation behind it), then the attached iovecs, then the resource
    /// itself. Without this the guest's `RESOURCE_UNREF` only ever cleaned the device's own 2D map,
    /// so every 3D resource an app created leaked in gfxstream for the life of the VM.
    ///
    /// Safe to call for a 2D-only or unknown id: all the lookups miss and nothing is forwarded.
    fn release_3d_resource(&mut self, id: u32) {
        if self.renderer.is_none() {
            return;
        }
        let was_3d = self.res3d.remove(&id).is_some();
        let was_blob = self.blobs.remove(&id).is_some();
        if self.blob_maps.contains_key(&id) {
            self.unmap_blob(id);
        }
        if self.res3d_backing.remove(&id).is_some() {
            if let Some(r) = self.renderer.as_mut() {
                r.detach_backing(id);
            }
        }
        if was_3d || was_blob {
            if let Some(r) = self.renderer.as_mut() {
                r.resource_unref(id);
            }
        }
    }

    /// `RESOURCE_UNMAP_BLOB`: undo the `hv_vm_map` for this blob then `stream_renderer_resource_unmap`.
    fn unmap_blob(&mut self, resource_id: u32) {
        use crate::virtio::pci::PCI_SHM_BASE;
        if let Some((offset, size)) = self.blob_maps.remove(&resource_id) {
            let ipa = PCI_SHM_BASE + offset;
            if let Err(rc) = self.mapper.unmap(ipa, size) {
                eprintln!(
                    "[gpu3d] unmap_blob res={resource_id} hv_vm_unmap(ipa={ipa:#x}, size={size:#x}) -> {}",
                    crate::hv_return_name(rc)
                );
            }
        }
        let _ = self.renderer.as_mut().unwrap().resource_unmap(resource_id);
    }
}

/// Parse a `virtio_gpu_transfer_host_3d` (box @24..48, offset @48, resource @56, level/stride/…).
/// Report a non-zero gfxstream status from a 3D command.
///
/// These commands keep answering `OK_NODATA` regardless: the guest's `gfxstream_vk` carries its own
/// error signalling over the ASG ring, and turning a host-side hiccup into a virtio-level
/// `ERR_UNSPEC` mid-stream is a bigger behavioural change than the failure it reports. But silently
/// discarding the status is how a host renderer that has started failing looks identical to one that
/// is fine, so surface it — rate-limited, because these sit on the per-frame path and a persistent
/// failure would otherwise bury the log it is supposed to help you read.
fn warn_rc(rc: i32, what: &str, id: u32) {
    use std::sync::atomic::AtomicU32;
    if rc == 0 {
        return;
    }
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let n = SEEN.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        eprintln!("[gpu3d] {what} id={id} -> rc={rc}");
    } else if n == 16 {
        eprintln!("[gpu3d] (further 3D command failures suppressed)");
    }
}

fn parse_transfer3d(cmd: &[u8]) -> super::gpu3d::Transfer3d {
    super::gpu3d::Transfer3d {
        box_: [
            read_le(cmd, 24, 4) as u32,
            read_le(cmd, 28, 4) as u32,
            read_le(cmd, 32, 4) as u32,
            read_le(cmd, 36, 4) as u32,
            read_le(cmd, 40, 4) as u32,
            read_le(cmd, 44, 4) as u32,
        ],
        offset: read_le(cmd, 48, 8),
        resource_id: read_le(cmd, 56, 4) as u32,
        level: read_le(cmd, 60, 4) as u32,
        stride: read_le(cmd, 64, 4) as u32,
        layer_stride: read_le(cmd, 68, 4) as u32,
    }
}

/// Resolve `[guest_addr, guest_addr + len)` to a host pointer, `None` unless the **whole range** lies
/// inside mapped guest RAM.
///
/// This must not be `get_host_address`, which validates a **single byte**: the resulting
/// `(host_ptr, len)` pair is handed to gfxstream by `RESOURCE_ATTACH_BACKING` /
/// `RESOURCE_CREATE_BLOB`, which stores it in the resource and later `memcpy`s within `iov_len`. A
/// guest mem entry with a valid address near the top of RAM and `len = 0xFFFF_FFFF` therefore bought
/// a ~4 GiB out-of-bounds host read *and* write past the end of the guest RAM mapping.
///
/// `get_slice` is the right primitive: guest RAM here is a single `from_ranges` region (see
/// `crate::memory::GuestRam`), so it cannot false-negative on a legitimate iovec that straddles two
/// regions — there are none to straddle.
fn host_range(mem: &Mem, guest_addr: u64, len: u64) -> Option<u64> {
    use vm_memory::GuestMemoryBackend;
    let len = usize::try_from(len).ok()?;
    let slice = mem.get_slice(GuestAddress(guest_addr), len).ok()?;
    // For the plain mmap backend the guard is a no-op wrapper around the host address, which is what
    // gfxstream needs to hold: a stable pointer into the guest RAM mapping, live for the whole VM.
    Some(slice.ptr_guard_mut().as_ptr() as u64)
}

/// Range-validate every guest mem entry and resolve it to a host `(ptr, len)` iovec.
///
/// `Err(i)` names the index of the **first** entry that is not wholly inside guest RAM. The caller
/// must fail the whole command on that: dropping the bad entry and keeping the rest (what
/// `filter_map` did here) shifts the offset of every later entry, so gfxstream reads the wrong bytes
/// for the remainder of the resource — silent image corruption instead of a rejected command.
fn resolve_iovs(mem: &Mem, entries: &[(u64, u32)]) -> Result<Vec<(u64, usize)>, usize> {
    let mut iovs = Vec::with_capacity(entries.len());
    for (i, &(addr, len)) in entries.iter().enumerate() {
        match host_range(mem, addr, u64::from(len)) {
            Some(hva) => iovs.push((hva, len as usize)),
            None => return Err(i),
        }
    }
    Ok(iovs)
}

/// Parse the `nr` trailing `virtio_gpu_mem_entry` records (addr:u64, length:u32, padding:u32) that
/// start at `off`.
///
/// `None` — the caller rejects the whole command — when the guest claims more than
/// [`MAX_MEM_ENTRIES`] (the count used to size a `Vec::with_capacity` directly) or when the command
/// is too short to carry the entries it claims. The old loop `break`ed out of a truncated tail, which
/// is the same silent-renumbering bug as dropping an invalid entry.
fn parse_mem_entries(cmd: &[u8], off: usize, nr: usize) -> Option<Vec<(u64, u32)>> {
    if nr > MAX_MEM_ENTRIES {
        return None;
    }
    if off.checked_add(nr.checked_mul(MEM_ENTRY_BYTES)?)? > cmd.len() {
        return None;
    }
    Some(
        (0..nr)
            .map(|i| {
                let o = off + i * MEM_ENTRY_BYTES;
                (read_le(cmd, o, 8), read_le(cmd, o + 8, 4) as u32)
            })
            .collect(),
    )
}

/// Total byte length of a resource's guest backing, or `None` if it reaches
/// [`MAX_RESOURCE_BYTES`] — that total is what [`gather_backing`] allocates, on every
/// `TRANSFER_TO_HOST_2D`/`_3D`.
fn backing_bytes(entries: &[(u64, u32)]) -> Option<usize> {
    let total: u64 = entries.iter().map(|&(_, len)| u64::from(len)).sum();
    if total >= MAX_RESOURCE_BYTES {
        return None;
    }
    usize::try_from(total).ok()
}

/// Byte size of the scratch buffer a `TRANSFER_FROM_HOST_3D` of a `w`×`h` box reads into, in `u64`
/// arithmetic. `None` if it reaches [`MAX_RESOURCE_BYTES`].
///
/// Unlike [`resource_bytes`] this does **not** cap either dimension: a 3D `PIPE_BUFFER` resource
/// reports its byte size as the box width with height 1, so a per-axis cap would reject a legitimate
/// buffer transfer. Only the product is bounded.
fn transfer_scratch_bytes(w: u32, h: u32) -> Option<usize> {
    let bytes = u64::from(w) * u64::from(h) * u64::from(BPP);
    if bytes >= MAX_RESOURCE_BYTES {
        return None;
    }
    usize::try_from(bytes).ok()
}

/// Scatter a contiguous host buffer back across a resource's guest backing iovecs (best-effort).
fn scatter_backing(mem: &Mem, entries: &[(u64, u32)], src: &[u8]) {
    let mut off = 0usize;
    for &(addr, len) in entries {
        if off >= src.len() {
            break;
        }
        let n = (len as usize).min(src.len() - off);
        if mem
            .write_slice(&src[off..off + n], GuestAddress(addr))
            .is_err()
        {
            break;
        }
        off += n;
    }
}

/// Split a descriptor chain into (device-readable bytes, device-writable (addr,len) list). The
/// device-readable descriptors carry the command struct (+ any trailing mem entries); the
/// device-writable ones receive the response.
///
/// The readable half is `None` when the chain claims more than [`MAX_CMD_BYTES`] of command: each
/// descriptor's `len` is a guest-written `u32`, so one descriptor could ask this to allocate ~4 GiB
/// per control command. The writable list is still returned in full so the caller can answer
/// `ERR_UNSPEC` and complete the chain — a rejected command must not stall the guest's ring.
fn split_chain(
    mem: &Mem,
    chain: virtio_queue::DescriptorChain<&Mem>,
) -> (Option<Vec<u8>>, Vec<(u64, u32)>) {
    let descs: Vec<(u64, u32, bool)> = chain
        .map(|d| (d.addr().0, d.len(), d.is_write_only()))
        .collect();
    let mut readable = Vec::new();
    let mut writable = Vec::new();
    let mut too_big = false;
    for (addr, len, write_only) in descs {
        if write_only {
            writable.push((addr, len));
            continue;
        }
        if too_big {
            continue;
        }
        if readable.len() as u64 + u64::from(len) > MAX_CMD_BYTES as u64 {
            eprintln!(
                "[gpu] control command rejected: {:#x} + {len:#x} bytes exceeds MAX_CMD_BYTES \
                 {MAX_CMD_BYTES:#x}",
                readable.len()
            );
            too_big = true;
            readable = Vec::new();
            continue;
        }
        let mut buf = vec![0u8; len as usize];
        if mem.read_slice(&mut buf, GuestAddress(addr)).is_ok() {
            readable.extend_from_slice(&buf);
        }
    }
    (if too_big { None } else { Some(readable) }, writable)
}

/// Assemble a resource's scattered guest backing into one contiguous host buffer.
///
/// `None` if the entries total [`MAX_RESOURCE_BYTES`] or more. Every attach site already enforces
/// that cap, so this is a backstop — but it is the allocation that used to be a bare
/// `Vec::with_capacity(sum(len))` plus a temporary per entry, on *every* `TRANSFER_TO_HOST_2D`/`_3D`.
fn gather_backing(mem: &Mem, entries: &[(u64, u32)]) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(backing_bytes(entries)?);
    for &(addr, len) in entries {
        let mut t = vec![0u8; len as usize];
        if mem.read_slice(&mut t, GuestAddress(addr)).is_ok() {
            buf.extend_from_slice(&t);
        } else {
            // Zero-fill rather than skip: the entry's bytes still occupy their place in the gathered
            // image, so dropping them would shift everything after it.
            buf.extend(std::iter::repeat_n(0u8, len as usize));
        }
    }
    Some(buf)
}

/// Scatter `bytes` across the device-writable descriptors; returns bytes written (for `add_used`).
fn write_scatter(mem: &Mem, writable: &[(u64, u32)], bytes: &[u8]) -> u32 {
    let mut off = 0usize;
    for &(addr, len) in writable {
        if off >= bytes.len() {
            break;
        }
        let n = (len as usize).min(bytes.len() - off);
        if mem
            .write_slice(&bytes[off..off + n], GuestAddress(addr))
            .is_ok()
        {
            off += n;
        } else {
            break;
        }
    }
    off as u32
}

/// Build a 24-byte `virtio_gpu_ctrl_hdr` response header, echoing back every field of `req` the
/// guest needs to identify what this response completes.
///
/// The whole of item 2.1 is in the two `if`s below. This function allocated `[0u8; 24]` and then
/// wrote only bytes `0..16`, with `flags` masked down to `req_flags & VIRTIO_GPU_FLAG_FENCE`. So on
/// **every** response:
///
///   * `VIRTIO_GPU_FLAG_INFO_RING_IDX` (bit 1) was stripped, telling the guest this completion
///     belongs to the device's global timeline even when it asked for a per-context one;
///   * `ctx_id` (bytes 16..20) was zeroed;
///   * `ring_idx` (byte 20) was zeroed.
///
/// Since the guest genuinely runs two rings, that reported every ring-1 fence as a ring-0 fence of
/// context 0 — per-ring completion could not be routed at all.
///
/// The `if`s matter in the other direction too: a response must not *invent* an identity the request
/// did not carry. An unfenced command answers with a zeroed `flags`/`fence_id`, and a fenced command
/// that did not set `INFO_RING_IDX` answers with `ctx_id`/`ring_idx` left at zero, exactly as the
/// guest's default-ring bookkeeping expects.
fn resp_hdr(resp_type: u32, req: &ReqHdr) -> [u8; CTRL_HDR_LEN] {
    let mut h = [0u8; CTRL_HDR_LEN];
    h[0..4].copy_from_slice(&resp_type.to_le_bytes());
    let flags = req.flags & (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX);
    h[4..8].copy_from_slice(&flags.to_le_bytes());
    if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
        h[8..16].copy_from_slice(&req.fence_id.to_le_bytes());
    }
    if flags & VIRTIO_GPU_FLAG_INFO_RING_IDX != 0 {
        h[super::gpu3d::CTRL_HDR_CTX_ID_OFF..super::gpu3d::CTRL_HDR_CTX_ID_OFF + 4]
            .copy_from_slice(&req.ctx_id.to_le_bytes());
        h[super::gpu3d::CTRL_HDR_RING_IDX_OFF] = req.ring_idx;
    }
    h
}

/// Environment variable that reverts item 2.5: every fenced command completes at submit time again,
/// exactly as it did before deferred completion landed.
const SYNC_FENCES_ENV: &str = "AEMU_GPU_SYNC_FENCES";

/// Whether the operator asked for synchronous fence completion. Evaluated once, like
/// [`present_readback_forced`].
///
/// Deferred completion is the largest behavioural change in the fence work — it moves *when* the
/// guest observes every fenced command — so it ships with a runtime revert. Any value other than
/// `0` enables it, so `AEMU_GPU_SYNC_FENCES=1` reads as it looks and `=0` does not silently enable
/// the thing it appears to disable.
fn sync_fences_forced() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| sync_fences_requested(std::env::var_os(SYNC_FENCES_ENV).as_deref()))
}

/// The pure decision [`sync_fences_forced`] caches, split out so it is testable without mutating the
/// process environment (which no test can do safely while the others run in parallel).
fn sync_fences_requested(v: Option<&std::ffi::OsStr>) -> bool {
    v.is_some_and(|v| v != "0")
}

/// Read a little-endian value of `len` ∈ {1,2,4,8} bytes at `off`, zero-extended, bounds-safe.
fn read_le(b: &[u8], off: usize, len: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..len {
        if let Some(byte) = b.get(off + i) {
            v |= (*byte as u64) << (8 * i);
        }
    }
    v
}

/// Build a 128-byte EDID 1.4 base block describing a single `width`×`height`@60 detailed mode with a
/// physical image size derived from `dpi`. The physical size is what the guest DRM connector exposes
/// as `width_mm`/`height_mm`; Android turns those into the internal display's dpi + DisplayViewport,
/// which a DIRECT touchscreen requires to associate. The detailed-timing block is the VESA DMT
/// 1024×768@60 timing (the only resolution this device scans out); only the mm image-size bytes vary
/// with `dpi`.
fn build_edid(width: u32, height: u32, dpi: u32) -> [u8; 128] {
    let dpi = dpi.max(1);
    // Physical image size, clamped to the EDID field widths (mm ≤ 4095 via 12 bits, cm ≤ 255).
    let w_mm = ((width as u64 * 254 + dpi as u64 * 5) / (dpi as u64 * 10)).min(4095) as u32;
    let h_mm = ((height as u64 * 254 + dpi as u64 * 5) / (dpi as u64 * 10)).min(4095) as u32;
    let w_cm = ((w_mm + 5) / 10).min(255) as u8;
    let h_cm = ((h_mm + 5) / 10).min(255) as u8;

    let mut e = [0u8; 128];
    // Header.
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer id "AEM" (three 5-bit letters, 1='A', big-endian in bytes 8..10).
    let man: u16 = (1u16 << 10) | (5u16 << 5) | 13u16; // A=1 E=5 M=13
    e[8] = (man >> 8) as u8;
    e[9] = (man & 0xff) as u8;
    e[10..12].copy_from_slice(&0x0001u16.to_le_bytes()); // product code
    e[12..16].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // serial
    e[16] = 1; // week of manufacture
    e[17] = 34; // year = 1990 + 34 = 2024
    e[18] = 1; // EDID version 1
    e[19] = 4; // EDID revision 4
    e[20] = 0x80; // video input: digital
    e[21] = w_cm; // max horizontal image size (cm)
    e[22] = h_cm; // max vertical image size (cm)
    e[23] = 0x78; // display gamma 2.2
    e[24] = 0x02; // feature support: preferred timing is the DTD-1 native mode
                  // chromaticity (standard sRGB-ish coordinates).
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Established timings: byte 36 bit 3 = 1024x768@60.
    e[35] = 0x00;
    e[36] = 0x08;
    e[37] = 0x00;
    // Standard timings 1..8 unused (0x0101).
    for i in 0..8 {
        e[38 + i * 2] = 0x01;
        e[38 + i * 2 + 1] = 0x01;
    }
    // Detailed timing descriptor 1: 1024x768@60 (65.00 MHz pixel clock), physical mm from dpi.
    let dtd1: [u8; 18] = [
        0x64,
        0x19, // pixel clock 6500 (65.00 MHz)
        0x00, // hactive low  (1024 & 0xff)
        0x40, // hblank low   (320 & 0xff)
        0x41, // hactive/hblank high nibbles (4,1)
        0x00, // vactive low  (768 & 0xff)
        0x26, // vblank low   (38)
        0x30, // vactive/vblank high nibbles (3,0)
        0x18, // hsync offset 24
        0x88, // hsync pulse 136
        0x36, // vsync offset 3 / vsync pulse 6
        0x00, // sync high bits
        (w_mm & 0xff) as u8,
        (h_mm & 0xff) as u8,
        ((((w_mm >> 8) & 0x0f) << 4) | ((h_mm >> 8) & 0x0f)) as u8,
        0x00, // horizontal border
        0x00, // vertical border
        0x18, // flags: digital separate sync, -h -v
    ];
    e[54..72].copy_from_slice(&dtd1);
    // Descriptor 2: monitor name "aemulator".
    e[72..77].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    let name = b"aemulator\n            ";
    e[77..90].copy_from_slice(&name[..13]);
    // Descriptor 3: display range limits (50-75 Hz, 30-80 kHz, 100 MHz max), range-limits-only.
    e[90..108].copy_from_slice(&[
        0x00, 0x00, 0x00, 0xFD, 0x00, 0x32, 0x4B, 0x1E, 0x50, 0x0A, 0x01, 0x0A, 0x20, 0x20, 0x20,
        0x20, 0x20, 0x20,
    ]);
    // Descriptor 4: dummy (tag 0x10).
    e[108..111].copy_from_slice(&[0x00, 0x00, 0x00]);
    e[111] = 0x10;
    e[126] = 0x00; // no extension blocks
                   // Checksum: the 128 bytes must sum to 0 mod 256.
    let sum: u32 = e[..127].iter().map(|b| *b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(cmd: u32) -> [u8; CTRL_HDR_LEN] {
        let mut h = [0u8; CTRL_HDR_LEN];
        h[0..4].copy_from_slice(&cmd.to_le_bytes());
        h
    }

    /// `RESOURCE_CREATE_2D`: hdr(24) resource_id(24) format(28) width(32) height(36).
    fn create_2d_cmd(id: u32, width: u32, height: u32) -> Vec<u8> {
        let mut c = hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D).to_vec();
        for v in [id, 1, width, height] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c
    }

    /// `RESOURCE_ATTACH_BACKING`: hdr(24) resource_id(24) nr_entries(28) then
    /// `nr_entries` × `virtio_gpu_mem_entry` (addr:u64, length:u32, padding:u32).
    fn attach_cmd(id: u32, entries: &[(u64, u32)]) -> Vec<u8> {
        let mut c = hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING).to_vec();
        c.extend_from_slice(&id.to_le_bytes());
        c.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(addr, len) in entries {
            c.extend_from_slice(&addr.to_le_bytes());
            c.extend_from_slice(&len.to_le_bytes());
            c.extend_from_slice(&0u32.to_le_bytes()); // padding
        }
        c
    }

    #[test]
    fn config_reports_one_scanout() {
        let gpu = Gpu::new(1024, 768);
        assert_eq!(gpu.config_read(8, 4), 1); // num_scanouts
        assert_eq!(gpu.config_read(12, 4), 0); // num_capsets
    }

    #[test]
    fn edid_is_valid_and_carries_physical_size() {
        // Checksum: all 128 bytes sum to 0 mod 256; header magic present.
        let edid = build_edid(1024, 768, 320);
        assert_eq!(edid[0..8], [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        assert_eq!(edid.iter().map(|b| *b as u32).sum::<u32>() % 256, 0);
        // Physical image size (cm) is non-zero — the whole point (dpi -1 was the bug).
        assert!(edid[21] > 0 && edid[22] > 0, "cm image size must be > 0");
        // DTD1 mm image-size bytes (66,67 within the 128-byte block) non-zero.
        assert!(edid[66] > 0 && edid[67] > 0, "mm image size must be > 0");
    }

    #[test]
    fn get_edid_command_returns_ok_edid_with_128_byte_block() {
        let gpu = Gpu::new(1024, 768);
        // Device advertises the EDID feature.
        assert_ne!(gpu.features() & VIRTIO_GPU_F_EDID, 0);
        let resp = gpu.resp_edid(&ReqHdr::default());
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_EDID as u64);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN, 4), 128); // size field
                                                          // EDID payload begins at hdr(24)+size(4)+padding(4) = 32, with the header magic.
        assert_eq!(resp[32], 0x00);
        assert_eq!(resp[33], 0xFF);
    }

    #[test]
    fn resp_hdr_echoes_fence_only_when_requested() {
        let no_fence = resp_hdr(
            VIRTIO_GPU_RESP_OK_NODATA,
            &ReqHdr {
                fence_id: 0xdead,
                ..ReqHdr::default()
            },
        );
        assert_eq!(read_le(&no_fence, 8, 8), 0);
        let with_fence = resp_hdr(
            VIRTIO_GPU_RESP_OK_NODATA,
            &ReqHdr {
                flags: VIRTIO_GPU_FLAG_FENCE,
                fence_id: 0xdead,
                ..ReqHdr::default()
            },
        );
        assert_eq!(read_le(&with_fence, 8, 8), 0xdead);
        assert_eq!(read_le(&with_fence, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
    }

    // =============================================================================================
    // 2.1 — the response header carries `ctx_id` / `ring_idx` / `FLAG_INFO_RING_IDX`.
    // =============================================================================================

    /// `resp_hdr` built `[0u8; 24]` and wrote only bytes `0..16`, masking `flags` down to
    /// `FLAG_FENCE`. So `FLAG_INFO_RING_IDX` was stripped and bytes 16..21 were zeroed on every
    /// response — the guest was told each completion belonged to ring 0 of context 0 no matter what
    /// it had asked for, and per-ring fence completion could not be routed.
    #[test]
    fn resp_hdr_echoes_ring_idx_and_ctx_when_the_request_set_info_ring_idx() {
        let req = ReqHdr {
            cmd_type: 0,
            flags: VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX,
            fence_id: 0x4242,
            ctx_id: 3,
            ring_idx: 1,
        };
        let h = resp_hdr(VIRTIO_GPU_RESP_OK_NODATA, &req);

        assert_eq!(
            read_le(&h, 4, 4),
            u64::from(VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX),
            "both flag bits must survive into the response"
        );
        assert_eq!(read_le(&h, 8, 8), 0x4242);
        assert_eq!(read_le(&h, 16, 4), 3, "ctx_id must be echoed");
        assert_eq!(h[20], 1, "ring_idx must be echoed");
        assert_eq!(&h[21..24], &[0, 0, 0], "the header padding stays zero");
    }

    /// The other direction: a response must not invent an identity the request did not carry. An
    /// unfenced command answers with no fence and no ring, and a fenced command that stayed on the
    /// default ring answers with `ctx_id`/`ring_idx` zero — which is what the guest's default-ring
    /// bookkeeping expects.
    #[test]
    fn resp_hdr_does_not_invent_a_ring_idx_when_the_request_did_not_ask() {
        // Fenced, but no INFO_RING_IDX: ctx_id/ring_idx present in the *request* are not echoed.
        let fenced_default_ring = resp_hdr(
            VIRTIO_GPU_RESP_OK_NODATA,
            &ReqHdr {
                flags: VIRTIO_GPU_FLAG_FENCE,
                fence_id: 0x99,
                ctx_id: 7,
                ring_idx: 2,
                ..ReqHdr::default()
            },
        );
        assert_eq!(read_le(&fenced_default_ring, 4, 4), 1, "FENCE only");
        assert_eq!(read_le(&fenced_default_ring, 8, 8), 0x99);
        assert_eq!(
            &fenced_default_ring[16..24],
            &[0u8; 8],
            "ctx_id + ring_idx must stay zero when the guest did not set INFO_RING_IDX"
        );

        // Wholly unfenced: nothing but the response type.
        let unfenced = resp_hdr(
            VIRTIO_GPU_RESP_OK_NODATA,
            &ReqHdr {
                fence_id: 0x99,
                ctx_id: 7,
                ring_idx: 2,
                ..ReqHdr::default()
            },
        );
        assert_eq!(&unfenced[4..24], &[0u8; 20]);
        assert_eq!(read_le(&unfenced, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
    }

    /// `ReqHdr::parse` is the one place the ctrl header is read, so its field offsets are the whole
    /// wire contract: `le32 type | le32 flags | le64 fence_id | le32 ctx_id | u8 ring_idx | pad[3]`.
    #[test]
    fn req_hdr_parses_every_ctrl_header_field_at_its_wire_offset() {
        let mut c = vec![0u8; CTRL_HDR_LEN];
        c[0..4].copy_from_slice(&0x0207u32.to_le_bytes()); // SUBMIT_3D
        c[4..8].copy_from_slice(&0b11u32.to_le_bytes());
        c[8..16].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());
        c[16..20].copy_from_slice(&5u32.to_le_bytes());
        c[20] = 1;
        c[21..24].copy_from_slice(&[0xAA, 0xBB, 0xCC]); // padding must be ignored

        assert_eq!(
            ReqHdr::parse(&c),
            Some(ReqHdr {
                cmd_type: 0x0207,
                flags: 0b11,
                fence_id: 0xDEAD_BEEF,
                ctx_id: 5,
                ring_idx: 1,
            })
        );
        assert_eq!(
            ReqHdr::parse(&c[..CTRL_HDR_LEN - 1]),
            None,
            "a command shorter than one header is not a command"
        );
    }

    #[test]
    fn display_info_reports_default_mode() {
        let gpu = Gpu::new(1024, 768);
        let resp = gpu.resp_display_info(&ReqHdr::default());
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_DISPLAY_INFO as u64);
        // pmodes[0].rect.width/height then enabled.
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 8, 4), 1024);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 12, 4), 768);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 16, 4), 1);
    }

    #[test]
    fn create_2d_then_unref_tracks_resource() {
        let mut gpu = Gpu::new(64, 64);
        let mut cmd = hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D).to_vec();
        cmd.extend_from_slice(&7u32.to_le_bytes()); // resource_id
        cmd.extend_from_slice(&1u32.to_le_bytes()); // format
        cmd.extend_from_slice(&64u32.to_le_bytes()); // width
        cmd.extend_from_slice(&64u32.to_le_bytes()); // height
                                                     // dispatch needs a Mem; craft a tiny one.
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let resp = gpu.dispatch_resp(&mem, &cmd);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert!(gpu.resources.contains_key(&7));

        let mut unref = hdr(VIRTIO_GPU_CMD_RESOURCE_UNREF).to_vec();
        unref.extend_from_slice(&7u32.to_le_bytes());
        unref.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &unref);
        assert!(!gpu.resources.contains_key(&7));
    }

    #[test]
    fn transfer_and_flush_move_pixels_to_framebuffer() {
        let mut gpu = Gpu::new(2, 2);
        let mem =
            vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0x1000), 0x1000)]).unwrap();
        // A 2x2 BGRA image = 16 bytes of guest backing at 0x1000.
        let pixels: [u8; 16] = [
            0x11, 0x22, 0x33, 0xff, 0x44, 0x55, 0x66, 0xff, 0x77, 0x88, 0x99, 0xff, 0xaa, 0xbb,
            0xcc, 0xff,
        ];
        mem.write_slice(&pixels, GuestAddress(0x1000)).unwrap();

        // create_2d id=1 2x2
        let mut create = hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D).to_vec();
        create.extend_from_slice(&1u32.to_le_bytes());
        create.extend_from_slice(&1u32.to_le_bytes());
        create.extend_from_slice(&2u32.to_le_bytes());
        create.extend_from_slice(&2u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &create);

        // attach_backing id=1 nr=1 (addr=0x1000,len=16)
        let mut attach = hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING).to_vec();
        attach.extend_from_slice(&1u32.to_le_bytes());
        attach.extend_from_slice(&1u32.to_le_bytes());
        attach.extend_from_slice(&0x1000u64.to_le_bytes());
        attach.extend_from_slice(&16u32.to_le_bytes());
        attach.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &attach);

        // set_scanout scanout0 -> resource1
        let mut scan = hdr(VIRTIO_GPU_CMD_SET_SCANOUT).to_vec();
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        gpu.dispatch_resp(&mem, &scan);

        // transfer_to_host_2d full 2x2, offset 0, id 1
        let mut xfer = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D).to_vec();
        xfer.extend_from_slice(&0u32.to_le_bytes()); // x
        xfer.extend_from_slice(&0u32.to_le_bytes()); // y
        xfer.extend_from_slice(&2u32.to_le_bytes()); // w
        xfer.extend_from_slice(&2u32.to_le_bytes()); // h
        xfer.extend_from_slice(&0u64.to_le_bytes()); // offset
        xfer.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        xfer.extend_from_slice(&0u32.to_le_bytes()); // padding
        gpu.dispatch_resp(&mem, &xfer);

        // flush id=1
        let handle = gpu.handle();
        let before = handle.generation();
        let mut flush = hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH).to_vec();
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &flush);

        assert_eq!(handle.generation(), before + 1);
        let fb = handle.fb.lock().unwrap();
        assert_eq!(fb.bytes, pixels);
        assert!(fb.non_zero_bytes() > 0);
    }

    /// The **first** test in this crate to drive a device through a real virtqueue instead of calling
    /// its private `dispatch`. Everything between the guest's kick and the used ring —
    /// `pop_descriptor_chain`, [`split_chain`], [`write_scatter`], `add_used` — had never executed
    /// under test, and it is exactly the layer deferred fence completion has to change.
    ///
    /// Two writable descriptors on purpose: a single contiguous response buffer would not exercise
    /// `write_scatter`'s per-descriptor loop, which is where a response can be silently truncated.
    #[test]
    fn testkit_roundtrips_a_control_chain() {
        use super::super::testkit::TestQueue;

        let mut tq = TestQueue::new(0x8000);
        let mut gpu = Gpu::new(1024, 768);

        let resp_len = CTRL_HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * 24;
        let split = 24u32; // response header in the first descriptor, pmodes[] in the second
        let head = tq.push_chain(
            &hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO),
            &[split, resp_len as u32 - split],
        );

        assert!(
            gpu.process_controlq(&tq.mem, &mut tq.queue),
            "a queued chain must be reported as consumed"
        );

        let used = tq.used();
        assert_eq!(used.len(), 1, "exactly one used entry per chain: {used:?}");
        assert_eq!(used[0].0, u32::from(head), "used.id is the chain head");
        assert_eq!(
            used[0].1 as usize, resp_len,
            "used.len must be the full response length, scattered across both descriptors"
        );

        let resp = tq.resp_bytes(head);
        assert_eq!(
            read_le(&resp, 0, 4),
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO as u64,
            "the guest must read back OK_DISPLAY_INFO out of guest RAM"
        );
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 8, 4), 1024);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 12, 4), 768);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN + 16, 4), 1, "pmodes[0].enabled");

        assert!(
            !gpu.process_controlq(&tq.mem, &mut tq.queue),
            "the chain must not be popped twice — add_used has to have advanced next_avail"
        );
    }

    // =============================================================================================
    // Bounded allocation + checked arithmetic on the 2D path.
    //
    // Every size below is guest-controlled and used to be multiplied out in `u32` and handed to
    // `vec![0u8; …]` unchecked. Each test names the arithmetic it guards.
    // =============================================================================================

    /// The range check must be a **tightening** of the old one-byte `get_host_address`, not a
    /// different mapping: gfxstream keeps whatever pointer it is handed for the life of the resource.
    /// So an accepted range has to resolve to the exact same host address, and the boundary has to sit
    /// where the region ends.
    ///
    /// (`host_range` goes through `get_slice` + `ptr_guard_mut`, which for the plain mmap backend is a
    /// no-op wrapper around the region's host address. With vm-memory's `xen` feature it would instead
    /// be an `mmap` unmapped on drop — this test is what would fail if that feature ever arrived.)
    #[test]
    fn host_range_agrees_with_the_single_byte_resolution_and_refuses_the_overrun() {
        use vm_memory::GuestMemoryBackend;
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let expected = mem.get_host_address(GuestAddress(0x100)).unwrap() as u64;

        assert_eq!(host_range(&mem, 0x100, 0x40), Some(expected));
        assert_eq!(
            host_range(&mem, 0x100, 0xF00),
            Some(expected),
            "a range ending exactly at the end of the region is inside it"
        );
        assert_eq!(
            host_range(&mem, 0x100, 0xF01),
            None,
            "one byte past the end must be refused — this is the whole finding"
        );
        assert_eq!(
            host_range(&mem, 0x100, 0xFFFF_FFFF),
            None,
            "the 4 GiB case: a valid address with a guest-chosen length"
        );
        assert_eq!(
            host_range(&mem, 0x2000, 4),
            None,
            "an address outside guest RAM altogether"
        );
    }

    /// `(width * height * BPP) as usize` was `u32` arithmetic: 65536² × 4 is 2^36, so this request
    /// **panicked** in a debug build (a guest-triggerable VMM abort — there is no `[profile]`
    /// `overflow-checks` opt-out in this crate) and in a release build wrapped to 0, producing an
    /// empty buffer that the rest of the device believed was a 65536×65536 image.
    #[test]
    fn resource_create_2d_rejects_dimensions_that_overflow_u32() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut gpu = Gpu::new(64, 64);

        let resp = gpu.dispatch_resp(&mem, &create_2d_cmd(1, 0x10000, 0x10000));

        assert_eq!(
            read_le(&resp, 0, 4),
            VIRTIO_GPU_RESP_ERR_UNSPEC as u64,
            "a resource whose byte size does not fit in u32 must be refused"
        );
        assert!(
            gpu.resources.is_empty(),
            "a rejected create must record no resource"
        );
    }

    /// The DoS half, with no overflow involved: 8192×8192 BGRA is exactly 256 MiB, i.e. the
    /// [`MAX_RESOURCE_BYTES`] boundary, and nothing limited how many such resources a guest could
    /// create. The cap is exclusive, so the round boundary value is itself rejected.
    #[test]
    fn resource_create_2d_rejects_an_oversized_but_non_overflowing_image() {
        let mem = vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut gpu = Gpu::new(64, 64);

        assert_eq!(
            u64::from(8192u32) * u64::from(8192u32) * u64::from(BPP),
            MAX_RESOURCE_BYTES,
            "this test is the boundary case — keep it on the boundary"
        );
        let resp = gpu.dispatch_resp(&mem, &create_2d_cmd(1, 8192, 8192));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(gpu.resources.is_empty());

        // One pixel under the cap is still accepted, so the limit rejects the oversized case only.
        let ok = gpu.dispatch_resp(&mem, &create_2d_cmd(2, 8192, 8191));
        assert_eq!(read_le(&ok, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert!(gpu.resources.contains_key(&2));
    }

    /// `rect.x * BPP` and `(rect.y + row) * stride` were `u32` products over a guest-supplied rect,
    /// and the only bounds check was a per-row `<=` guard that copied whichever rows happened to fit.
    /// A rect outside the resource must move no pixels at all.
    #[test]
    fn transfer_to_host_2d_with_a_rect_outside_the_resource_is_a_no_op() {
        let mem =
            vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0x1000), 0x1000)]).unwrap();
        mem.write_slice(&[0xABu8; 16], GuestAddress(0x1000))
            .unwrap();
        let mut gpu = Gpu::new(2, 2);
        gpu.dispatch_resp(&mem, &create_2d_cmd(1, 2, 2));
        gpu.dispatch_resp(&mem, &attach_cmd(1, &[(0x1000, 16)]));

        // 4x4 at (1,1) on a 2x2 resource — every row is at least partly out of bounds.
        let mut xfer = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D).to_vec();
        for v in [1u32, 1, 4, 4] {
            xfer.extend_from_slice(&v.to_le_bytes()); // x, y, w, h
        }
        xfer.extend_from_slice(&0u64.to_le_bytes()); // offset
        xfer.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        xfer.extend_from_slice(&0u32.to_le_bytes()); // padding
        let resp = gpu.dispatch_resp(&mem, &xfer);

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert!(
            gpu.resources[&1].pixels.iter().all(|b| *b == 0),
            "an out-of-bounds rect must transfer nothing, not the rows that happen to fit"
        );

        // The in-bounds transfer of the same resource still works — the precondition rejects the
        // out-of-range rect, not the command.
        let mut good = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D).to_vec();
        for v in [0u32, 0, 2, 2] {
            good.extend_from_slice(&v.to_le_bytes());
        }
        good.extend_from_slice(&0u64.to_le_bytes());
        good.extend_from_slice(&1u32.to_le_bytes());
        good.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &good);
        assert_eq!(gpu.resources[&1].pixels, vec![0xABu8; 16]);
    }

    /// `split_chain` allocated `vec![0u8; len as usize]` per device-readable descriptor with `len`
    /// straight off the ring — up to ~4 GiB per control command, on the vCPU thread inside the MMIO
    /// exit. The oversized chain must be *answered* (`ERR_UNSPEC`) and completed, not dropped: a
    /// chain left un-`add_used`'d stalls the guest's ring forever.
    #[test]
    fn split_chain_rejects_an_oversized_control_command() {
        use super::super::testkit::TestQueue;

        let mut tq = TestQueue::new(0x40000);
        let mut gpu = Gpu::new(64, 64);

        let mut cmd = hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO).to_vec();
        cmd.resize(MAX_CMD_BYTES + 1, 0);
        let head = tq.push_chain(&cmd, &[CTRL_HDR_LEN as u32]);

        assert!(gpu.process_controlq(&tq.mem, &mut tq.queue));

        let used = tq.used();
        assert_eq!(used.len(), 1, "the rejected chain must still be completed");
        assert_eq!(used[0].1 as usize, CTRL_HDR_LEN);
        assert_eq!(
            read_le(&tq.resp_bytes(head), 0, 4),
            VIRTIO_GPU_RESP_ERR_UNSPEC as u64,
            "a command over MAX_CMD_BYTES must be reported to the guest as an error"
        );
    }
}

/// Phase 3c smoke test: with a gfxstream renderer attached the device advertises VIRGL and routes
/// the guest's 3D control stream into the renderer FFI boundary. Driven by a recording `MockRenderer`
/// so it needs no live guest, no host Vulkan driver, no GPU — it proves the parse + routing, per the task.
#[cfg(test)]
mod tests_3d {
    use super::super::gpu3d;
    use super::super::gpu3d::{Call, MockRenderer, VIRTIO_GPU_F_VIRGL};
    use super::*;

    const VIRTIO_GPU_CMD_CTX_CREATE: u32 = 0x0200;
    const VIRTIO_GPU_CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
    const VIRTIO_GPU_CMD_SUBMIT_3D: u32 = 0x0207;

    fn mem() -> vm_memory::GuestMemoryMmap {
        vm_memory::GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap()
    }

    /// Build a 24-byte virtio_gpu_ctrl_hdr (type, flags, fence_id, ctx_id, ring_idx).
    fn hdr3d(cmd_type: u32, ctx_id: u32) -> Vec<u8> {
        let mut h = vec![0u8; CTRL_HDR_LEN];
        h[0..4].copy_from_slice(&cmd_type.to_le_bytes());
        h[16..20].copy_from_slice(&ctx_id.to_le_bytes());
        h
    }

    #[test]
    fn advertises_virgl_and_capsets_only_with_renderer() {
        // Plain 2D device — no VIRGL, no capsets.
        let plain = Gpu::new(64, 64);
        assert_eq!(plain.features() & VIRTIO_GPU_F_VIRGL, 0);
        assert_eq!(plain.config_read(12, 4), 0); // num_capsets

        // With a gfxstream renderer attached — VIRGL advertised, capsets reported.
        let (mock, _log) = MockRenderer::new();
        let gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        assert_ne!(
            gpu.features() & VIRTIO_GPU_F_VIRGL,
            0,
            "device must advertise VIRTIO_GPU_F_VIRGL when a 3D renderer is attached"
        );
        assert_eq!(gpu.config_read(12, 4), 1); // MockRenderer::num_capsets()
    }

    #[test]
    fn renderer_device_advertises_resource_blob() {
        use super::super::gpu3d::VIRTIO_GPU_F_RESOURCE_BLOB;
        // A plain 2D device does NOT advertise blob resources.
        let plain = Gpu::new(64, 64);
        assert_eq!(plain.features() & VIRTIO_GPU_F_RESOURCE_BLOB, 0);
        assert!(!plain.advertises_blob());
        // With a gfxstream renderer, VIRTIO_GPU_F_RESOURCE_BLOB is advertised (unlocks the ASG ring).
        let (mock, _log) = MockRenderer::new();
        let gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        assert_ne!(
            gpu.features() & VIRTIO_GPU_F_RESOURCE_BLOB,
            0,
            "a gfxstream device must advertise VIRTIO_GPU_F_RESOURCE_BLOB"
        );
        assert!(gpu.advertises_blob());
    }

    #[test]
    fn ctx_create_and_submit_route_into_the_renderer() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        // CTX_CREATE ctx_id=1, context_init=2, name="aemu".
        let mut ctx = hdr3d(VIRTIO_GPU_CMD_CTX_CREATE, 1);
        ctx.extend_from_slice(&4u32.to_le_bytes()); // nlen
        ctx.extend_from_slice(&2u32.to_le_bytes()); // context_init
        ctx.extend_from_slice(b"aemu"); // debug name (nlen bytes)
        let resp = gpu.dispatch_resp(&mem, &ctx);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);

        // SUBMIT_3D ctx_id=1, size=8, payload = 8 sentinel bytes (the gfxstream command stream).
        let payload: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let mut sub = hdr3d(VIRTIO_GPU_CMD_SUBMIT_3D, 1);
        sub.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // size
        sub.extend_from_slice(&0u32.to_le_bytes()); // padding
        sub.extend_from_slice(&payload);
        let resp = gpu.dispatch_resp(&mem, &sub);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);

        // Assert the mock observed both routed calls, with the forwarded ctx id + verbatim cmd bytes.
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(|c| matches!(c,
                Call::ContextCreate { ctx_id: 1, context_init: 2, name } if name == b"aemu")),
            "CTX_CREATE did not route into stream_renderer_context_create: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| matches!(c,
                Call::SubmitCmd { ctx_id: 1, bytes } if bytes.as_slice() == payload)),
            "SUBMIT_3D did not forward the guest command stream into stream_renderer_submit_cmd: {calls:?}"
        );
    }

    /// The readback present mode — no `CAMetalLayer` published, which is what headless `--control`
    /// runs and what `AEMU_PRESENT_READBACK=1` forces. `flush_3d` must fill the CPU scanout, because
    /// in that mode it is the only surface `frame` / `screenshot` / `info` can read.
    #[test]
    fn resource_create_3d_and_flush_present_coupling() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        // RESOURCE_CREATE_3D id=10, 32x32.
        let mut cr = hdr3d(VIRTIO_GPU_CMD_RESOURCE_CREATE_3D, 1);
        for v in [10u32, 2, 67, 1, 32, 32, 1, 1, 0, 0, 0, 0] {
            cr.extend_from_slice(&v.to_le_bytes()); // id,target,format,bind,w,h,depth,array,last,nr,flags,pad
        }
        gpu.dispatch_resp(&mem, &cr);

        // SET_SCANOUT scanout0 -> resource 10 (a 3D resource — sizes the scanout to 32x32).
        let mut scan = hdr3d(VIRTIO_GPU_CMD_SET_SCANOUT, 0);
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        gpu.dispatch_resp(&mem, &scan);

        // RESOURCE_FLUSH resource 10 → read_pixels back into the scanout + bump generation.
        let handle = gpu.handle();
        let before = handle.generation();
        let mut flush = hdr3d(VIRTIO_GPU_CMD_RESOURCE_FLUSH, 0);
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &flush);

        assert_eq!(
            handle.generation(),
            before + 1,
            "3D flush must bump the scanout generation"
        );
        {
            let fb = handle.fb.lock().unwrap();
            assert_eq!(
                (fb.width, fb.height),
                (32, 32),
                "scanout resized to the 3D resource"
            );
            assert!(
                fb.non_zero_bytes() > 0,
                "read_pixels must fill the presented scanout"
            );
        }

        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(
                |c| matches!(c, Call::ResourceCreate3d(a) if a.resource_id == 10 && a.width == 32)
            ),
            "RESOURCE_CREATE_3D did not route: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::ReadPixels {
                    resource_id: 10,
                    w: 32,
                    h: 32
                }
            )),
            "RESOURCE_FLUSH did not read the 3D resource back for presenting: {calls:?}"
        );
    }

    /// Drive one `RESOURCE_FLUSH` of a freshly created + scanned-out 3D resource `rid` sized `w`x`h`.
    /// Returns the scanout handle so the caller can inspect the framebuffer and the generation.
    fn flush_scanout_3d(gpu: &mut Gpu, mem: &Mem, rid: u32, w: u32, h: u32) -> ScanoutHandle {
        create_3d(gpu, mem, rid, w, h);

        let mut scan = hdr3d(VIRTIO_GPU_CMD_SET_SCANOUT, 0);
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&rid.to_le_bytes());
        gpu.dispatch_resp(mem, &scan);

        let mut flush = hdr3d(VIRTIO_GPU_CMD_RESOURCE_FLUSH, 0);
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&rid.to_le_bytes());
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(mem, &flush);

        gpu.handle()
    }

    /// A published `CAMetalLayer*` switches the present path: the flush must route into
    /// `stream_renderer_present_to_metal_layer` with that exact pointer and must NOT read pixels
    /// back. The readback would be pure waste — under in-Vulkan present the renderer blits the
    /// ColorBuffer straight into the swapchain image and nothing in this process reads the CPU
    /// framebuffer.
    #[test]
    fn a_published_metal_layer_routes_the_flush_into_the_in_vulkan_present() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        const LAYER: usize = 0xCAFE_0000;
        gpu.handle().metal_layer.store(LAYER, Ordering::Release);

        let before = gpu.handle().generation();
        let handle = flush_scanout_3d(&mut gpu, &mem, 10, 32, 32);

        assert_eq!(
            handle.generation(),
            before + 1,
            "an in-Vulkan present must still bump the generation — `wait_stable` and the redraw \
             trigger both key off it"
        );
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::PresentToLayer { resource_id: 10, layer_ptr } if *layer_ptr == LAYER
            )),
            "RESOURCE_FLUSH did not present the ColorBuffer into the published layer: {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| matches!(c, Call::ReadPixels { .. })),
            "a successful in-Vulkan present must not also pay for a CPU readback: {calls:?}"
        );
    }

    /// A **positive** status is a deliberately dropped frame (no swapchain image free in time, or the
    /// swapchain is being rebuilt after a resize), not a failure. Falling back to readback for it
    /// would trade a skipped frame for a full GPU→CPU transfer on a present path that is healthy.
    #[test]
    fn a_dropped_frame_does_not_degrade_to_readback() {
        let (mock, log) = MockRenderer::builder().present_rc(1).build();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        gpu.handle()
            .metal_layer
            .store(0xBEEF_0000, Ordering::Release);

        let before = gpu.handle().generation();
        let handle = flush_scanout_3d(&mut gpu, &mem, 10, 32, 32);

        assert_eq!(
            handle.generation(),
            before + 1,
            "a dropped frame still advances the generation"
        );
        let calls = log.lock().unwrap();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::ReadPixels { .. })),
            "rc > 0 means the present path is healthy — it must not trigger the fallback: {calls:?}"
        );
    }

    /// A **negative** status is a hard failure (no Vulkan emulation, no `VK_EXT_metal_surface`, an
    /// unblittable ColorBuffer format). The flush must then fall back to the readback path so the
    /// CPU framebuffer — the only remaining pixel source — is still written.
    #[test]
    fn a_hard_present_failure_falls_back_to_the_readback_path() {
        let (mock, log) = MockRenderer::builder().present_rc(-4).build();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        gpu.handle()
            .metal_layer
            .store(0xDEAD_0000, Ordering::Release);

        let handle = flush_scanout_3d(&mut gpu, &mem, 10, 32, 32);

        {
            let fb = handle.fb.lock().unwrap();
            assert_eq!((fb.width, fb.height), (32, 32));
            assert!(
                fb.non_zero_bytes() > 0,
                "the fallback must fill the CPU scanout"
            );
        }
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::PresentToLayer {
                    resource_id: 10,
                    ..
                }
            )),
            "the in-Vulkan present must be attempted before falling back: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::ReadPixels {
                    resource_id: 10,
                    w: 32,
                    h: 32
                }
            )),
            "a negative present status must fall back to stream_renderer_transfer_read: {calls:?}"
        );
    }

    /// Create a 3D resource `id` sized `w`x`h` on `gpu`.
    fn create_3d(gpu: &mut Gpu, mem: &Mem, id: u32, w: u32, h: u32) {
        let mut cr = hdr3d(VIRTIO_GPU_CMD_RESOURCE_CREATE_3D, 1);
        for v in [id, 2, 67, 1, w, h, 1, 1, 0, 0, 0, 0] {
            cr.extend_from_slice(&v.to_le_bytes());
        }
        gpu.dispatch_resp(mem, &cr);
    }

    /// `RESOURCE_ATTACH_BACKING` for a 3D resource must hand gfxstream **guest-memory host pointers**,
    /// not a gathered copy: `VirtioGpuResource::AttachIov` stores `iov_base` and dereferences it on
    /// later transfers, so a temporary would dangle the instant the command returned.
    #[test]
    fn attach_backing_forwards_stable_guest_memory_pointers() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        create_3d(&mut gpu, &mem, 10, 32, 32);

        // Two mem entries at guest 0x100 (0x40 bytes) and 0x200 (0x80 bytes).
        let mut att = hdr3d(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, 1);
        att.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        att.extend_from_slice(&2u32.to_le_bytes()); // nr_entries
        for (addr, len) in [(0x100u64, 0x40u32), (0x200, 0x80)] {
            att.extend_from_slice(&addr.to_le_bytes());
            att.extend_from_slice(&len.to_le_bytes());
            att.extend_from_slice(&0u32.to_le_bytes()); // padding
        }
        let resp = gpu.dispatch_resp(&mem, &att);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);

        let calls = log.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::AttachBacking { resource_id: 10, len } if *len == 0xC0)),
            "both mem entries must reach stream_renderer_resource_attach_iov: {calls:?}"
        );
    }

    /// `RESOURCE_UNREF` must release the resource in the host renderer. Before this, unref only
    /// cleaned the device's own 2D map, so every 3D resource an app created leaked in gfxstream.
    #[test]
    fn resource_unref_releases_the_resource_and_its_backing_in_the_renderer() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        create_3d(&mut gpu, &mem, 10, 32, 32);

        let mut att = hdr3d(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, 1);
        att.extend_from_slice(&10u32.to_le_bytes());
        att.extend_from_slice(&1u32.to_le_bytes());
        att.extend_from_slice(&0x100u64.to_le_bytes());
        att.extend_from_slice(&0x40u32.to_le_bytes());
        att.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &att);

        let mut unref = hdr3d(VIRTIO_GPU_CMD_RESOURCE_UNREF, 0);
        unref.extend_from_slice(&10u32.to_le_bytes());
        unref.extend_from_slice(&0u32.to_le_bytes());
        let resp = gpu.dispatch_resp(&mem, &unref);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);

        let calls = log.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::DetachBacking { resource_id: 10 })),
            "unref must drop the iovecs gfxstream holds into guest memory: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::ResourceUnref { resource_id: 10 })),
            "unref must reach stream_renderer_resource_unref: {calls:?}"
        );
    }

    /// `RESOURCE_DETACH_BACKING` means the guest is revoking that memory — gfxstream has to stop
    /// holding pointers into it, or the next transfer reads memory the guest has already reused.
    #[test]
    fn detach_backing_routes_into_the_renderer() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        create_3d(&mut gpu, &mem, 11, 16, 16);

        let mut att = hdr3d(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, 1);
        att.extend_from_slice(&11u32.to_le_bytes());
        att.extend_from_slice(&1u32.to_le_bytes());
        att.extend_from_slice(&0x100u64.to_le_bytes());
        att.extend_from_slice(&0x40u32.to_le_bytes());
        att.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &att);

        let mut det = hdr3d(VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING, 0);
        det.extend_from_slice(&11u32.to_le_bytes());
        det.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch_resp(&mem, &det);

        let calls = log.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::DetachBacking { resource_id: 11 })),
            "DETACH_BACKING did not route into stream_renderer_resource_detach_iov: {calls:?}"
        );
    }

    /// A device advertising `F_RESOURCE_BLOB` must answer `SET_SCANOUT_BLOB` — the guest's KMS path
    /// uses it whenever the framebuffer it wants to scan out is a blob. It previously fell through to
    /// `ERR_UNSPEC`, which reads to the guest as "this scanout cannot be set".
    #[test]
    fn set_scanout_blob_binds_the_blob_and_sizes_the_scanout() {
        use super::super::gpu3d::VIRTIO_GPU_CMD_SET_SCANOUT_BLOB;
        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let mut scan = hdr3d(VIRTIO_GPU_CMD_SET_SCANOUT_BLOB, 0);
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&7u32.to_le_bytes()); // resource_id
        scan.extend_from_slice(&40u32.to_le_bytes()); // width
        scan.extend_from_slice(&24u32.to_le_bytes()); // height
        scan.extend_from_slice(&2u32.to_le_bytes()); // format
        scan.extend_from_slice(&0u32.to_le_bytes()); // padding
        scan.extend_from_slice(&[0u8; 32]); // strides[4] + offsets[4]

        let resp = gpu.dispatch_resp(&mem, &scan);
        assert_eq!(
            read_le(&resp, 0, 4),
            VIRTIO_GPU_RESP_OK_NODATA as u64,
            "SET_SCANOUT_BLOB must be accepted by a device advertising F_RESOURCE_BLOB"
        );
        let handle = gpu.handle();
        let fb = handle.fb.lock().unwrap();
        assert_eq!(
            (fb.width, fb.height),
            (40, 24),
            "the blob carries its own dimensions; the scanout must adopt them"
        );
    }

    /// `RESOURCE_ASSIGN_UUID` must return an `OK_RESOURCE_UUID` response with a 16-byte uuid that is
    /// stable per resource and distinct across resources (it names an allocation for external-memory
    /// import). It previously fell through to `ERR_UNSPEC`.
    #[test]
    fn assign_uuid_answers_with_a_stable_per_resource_uuid() {
        use super::super::gpu3d::{
            VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID, VIRTIO_GPU_RESP_OK_RESOURCE_UUID,
        };
        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let uuid_of = |gpu: &mut Gpu, id: u32| -> Vec<u8> {
            let mut c = hdr3d(VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID, 0);
            c.extend_from_slice(&id.to_le_bytes());
            c.extend_from_slice(&0u32.to_le_bytes());
            let resp = gpu.dispatch_resp(&mem, &c);
            assert_eq!(
                read_le(&resp, 0, 4),
                VIRTIO_GPU_RESP_OK_RESOURCE_UUID as u64
            );
            assert_eq!(
                resp.len(),
                CTRL_HDR_LEN + 16,
                "uuid response is hdr + 16 bytes"
            );
            resp[CTRL_HDR_LEN..].to_vec()
        };

        let a1 = uuid_of(&mut gpu, 5);
        let a2 = uuid_of(&mut gpu, 5);
        let b = uuid_of(&mut gpu, 6);
        assert_eq!(a1, a2, "the same resource must keep the same uuid");
        assert_ne!(a1, b, "different resources must not collide");
    }

    // =============================================================================================
    // RESOURCE_MAP_BLOB / RESOURCE_UNMAP_BLOB.
    //
    // Until `MockRenderer::resource_map` could report a host pointer and `hv_vm_map` could be
    // substituted, `Gpu::map_blob` returned at its very first line in every test — the window bounds
    // check, the granule-alignment check, the mapping, the `blob_maps` bookkeeping and the
    // `OK_MAP_INFO` reply had never run. These are the first tests to enter that body.
    // =============================================================================================

    /// Host pointer the mock hands back, 16 KiB-aligned like a real `stream_renderer_resource_map`
    /// result (HVF requires host / IPA / size all aligned to the host granule).
    const BLOB_HOST_PTR: u64 = 0x1_0000_0000;
    const BLOB_SIZE: u64 = 0x8000;
    const BLOB_OFFSET: u64 = 0x4000;
    const BLOB_RES: u32 = 9;

    /// `RESOURCE_MAP_BLOB`: hdr(24) resource_id(24) padding(28) offset(32).
    fn map_blob_cmd(resource_id: u32, offset: u64) -> Vec<u8> {
        let mut c = hdr3d(super::super::gpu3d::VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB, 0);
        c.extend_from_slice(&resource_id.to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes()); // padding
        c.extend_from_slice(&offset.to_le_bytes());
        c
    }

    /// `RESOURCE_UNMAP_BLOB`: hdr(24) resource_id(24) padding(28).
    fn unmap_blob_cmd(resource_id: u32) -> Vec<u8> {
        let mut c = hdr3d(super::super::gpu3d::VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB, 0);
        c.extend_from_slice(&resource_id.to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes());
        c
    }

    /// A device whose renderer maps `BLOB_RES` to `(BLOB_HOST_PTR, BLOB_SIZE)` and whose guest
    /// mapping is a [`RecordingMapper`].
    fn blob_gpu(mapper: Box<dyn GuestMapper>) -> (Gpu, super::super::gpu3d::CallLog) {
        let (mock, log) = MockRenderer::builder()
            .resource_map(Some((BLOB_HOST_PTR, BLOB_SIZE)))
            .map_info(3) // STREAM_RENDERER_MAP_CACHE_* value echoed in OK_MAP_INFO
            .build();
        (Gpu::with_mapper(64, 64, Box::new(mock), mapper), log)
    }

    /// The happy path through the whole of `map_blob`: one mapping installed, at the IPA derived from
    /// `PCI_SHM_BASE + offset`, with the host pointer and size the renderer reported — and an
    /// `OK_MAP_INFO` reply carrying `map_info`. "Once" is the load-bearing word: a second `hv_vm_map`
    /// for the same blob would leak the first.
    #[test]
    fn map_blob_maps_once_and_records_the_mapping() {
        use super::super::gpu3d::VIRTIO_GPU_RESP_OK_MAP_INFO;
        use crate::virtio::pci::PCI_SHM_BASE;

        let (mapper, ops) = RecordingMapper::new();
        let (mut gpu, log) = blob_gpu(Box::new(mapper));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, BLOB_OFFSET));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_MAP_INFO as u64);
        assert_eq!(
            resp.len(),
            CTRL_HDR_LEN + 8,
            "OK_MAP_INFO is hdr + map_info"
        );
        assert_eq!(
            read_le(&resp, CTRL_HDR_LEN, 4),
            3,
            "the reply must carry the renderer's map_info, not a hardcoded default"
        );
        assert_eq!(
            *ops.lock().unwrap(),
            vec![MapOp::Map {
                host_ptr: BLOB_HOST_PTR,
                ipa: PCI_SHM_BASE + BLOB_OFFSET,
                size: BLOB_SIZE,
            }],
            "exactly one guest mapping, at PCI_SHM_BASE + offset"
        );
        assert_eq!(
            gpu.blob_maps.get(&BLOB_RES),
            Some(&(BLOB_OFFSET, BLOB_SIZE)),
            "the live map must be recorded so unref/unmap can tear it down"
        );
        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::ResourceUnmap { .. })),
            "a successful map must not release the host mapping: {calls:?}"
        );
    }

    /// `hv_vm_map` failing leaves the host-side blob map live but unreachable by the guest, so it must
    /// be released again. This branch had **never executed** — an `hv_vm_map` failure needs a live VM
    /// to provoke, which is exactly what [`GuestMapper`] removes from the path.
    #[test]
    fn map_blob_releases_the_host_map_when_the_guest_mapping_fails() {
        let (mapper, ops) = RecordingMapper::failing(crate::HV_DENIED as i32);
        let (mut gpu, log) = blob_gpu(Box::new(mapper));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, BLOB_OFFSET));

        assert_eq!(
            read_le(&resp, 0, 4),
            VIRTIO_GPU_RESP_ERR_UNSPEC as u64,
            "a failed guest mapping must be reported to the guest, not answered OK"
        );
        assert_eq!(
            ops.lock().unwrap().len(),
            1,
            "the map was attempted exactly once and not retried"
        );
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(
                |c| matches!(c, Call::ResourceUnmap { resource_id } if *resource_id == BLOB_RES)
            ),
            "the host blob map must be released again: {calls:?}"
        );
        assert!(
            gpu.blob_maps.is_empty(),
            "a failed map must leave no bookkeeping behind"
        );
    }

    /// The shmem-window bounds check. A guest offset whose blob would run past `PCI_SHM_SIZE` must be
    /// rejected **before** any `hv_vm_map` — a mapping outside BAR2 is not reachable by the guest and
    /// on a large enough overrun would collide with whatever else the VMM has placed above it.
    #[test]
    fn map_blob_rejects_an_offset_past_the_shmem_window() {
        use crate::virtio::pci::PCI_SHM_SIZE;

        let (mapper, ops) = RecordingMapper::new();
        let (mut gpu, log) = blob_gpu(Box::new(mapper));
        let mem = mem();

        // Aligned, inside the window itself, but offset + BLOB_SIZE runs past its end.
        let offset = PCI_SHM_SIZE - 0x4000;
        let resp = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, offset));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(
            ops.lock().unwrap().is_empty(),
            "an out-of-window offset must be rejected before the guest mapping is attempted"
        );
        assert!(
            gpu.blob_maps.is_empty(),
            "a rejected map must leave no bookkeeping behind"
        );
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(
                |c| matches!(c, Call::ResourceUnmap { resource_id } if *resource_id == BLOB_RES)
            ),
            "the host map obtained before the bounds check must be released: {calls:?}"
        );
    }

    /// The window check has to come *before* `PCI_SHM_BASE + offset`, because `offset` is a raw guest
    /// `u64`: computing the IPA first overflows that sum, which panics under the `overflow-checks`
    /// backstop this crate now sets in both profiles (and silently wrapped to a small, plausible IPA
    /// before it). Found by enabling `overflow-checks`, not by the bounds check itself — the bounds
    /// check was already there, just one line too late.
    #[test]
    fn map_blob_rejects_an_offset_that_would_overflow_the_window_base() {
        let (mapper, ops) = RecordingMapper::new();
        let (mut gpu, _log) = blob_gpu(Box::new(mapper));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, u64::MAX));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(
            ops.lock().unwrap().is_empty(),
            "no mapping may be attempted for an offset that cannot be added to the window base"
        );
        assert!(gpu.blob_maps.is_empty());
    }

    /// `RESOURCE_UNMAP_BLOB` then a fresh `RESOURCE_MAP_BLOB` for the same resource. The unmap must
    /// target the *same* `(ipa, size)` the map installed — an unmap of a different range leaves the
    /// original mapping live, which is how a stale host pointer stays reachable by the guest after the
    /// renderer has freed it.
    #[test]
    fn unmap_then_remap_succeeds() {
        use super::super::gpu3d::VIRTIO_GPU_RESP_OK_MAP_INFO;
        use crate::virtio::pci::PCI_SHM_BASE;

        let (mapper, ops) = RecordingMapper::new();
        let (mut gpu, _log) = blob_gpu(Box::new(mapper));
        let mem = mem();
        let ipa = PCI_SHM_BASE + BLOB_OFFSET;
        let mapped = MapOp::Map {
            host_ptr: BLOB_HOST_PTR,
            ipa,
            size: BLOB_SIZE,
        };

        let first = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, BLOB_OFFSET));
        assert_eq!(read_le(&first, 0, 4), VIRTIO_GPU_RESP_OK_MAP_INFO as u64);

        let unmap = gpu.dispatch_resp(&mem, &unmap_blob_cmd(BLOB_RES));
        assert_eq!(read_le(&unmap, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert!(
            gpu.blob_maps.is_empty(),
            "unmap must drop the live-map bookkeeping"
        );

        let second = gpu.dispatch_resp(&mem, &map_blob_cmd(BLOB_RES, BLOB_OFFSET));
        assert_eq!(
            read_le(&second, 0, 4),
            VIRTIO_GPU_RESP_OK_MAP_INFO as u64,
            "the window is free again, so the same offset must map a second time"
        );

        assert_eq!(
            *ops.lock().unwrap(),
            vec![
                mapped,
                MapOp::Unmap {
                    ipa,
                    size: BLOB_SIZE
                },
                mapped,
            ],
            "unmap must release exactly the (ipa, size) that was mapped"
        );
    }

    // =============================================================================================
    // Range-validated host pointers + hard-fail on an invalid mem entry.
    //
    // `host_addr` resolved a guest address with `get_host_address`, which validates ONE BYTE. The
    // resulting `(host_ptr, guest_len)` pair went straight to gfxstream, which stores it in the
    // resource and `memcpy`s within `iov_len` on every later transfer — so a valid address plus
    // `len = 0xFFFF_FFFF` was a 4 GiB out-of-bounds host read *and* write.
    //
    // The second half of the finding is worse: both call sites dropped an entry they could not
    // resolve (`filter_map` / `if let Some(hva)`), which shifts the offset of every entry after it.
    // gfxstream then reads the wrong bytes for the rest of the resource, silently. Hence every test
    // here asserts the mock log records **no** forwarded call — the whole command must be refused.
    // =============================================================================================

    /// `RESOURCE_ATTACH_BACKING`: hdr(24) resource_id(24) nr_entries(28) then
    /// `nr_entries` × `virtio_gpu_mem_entry` (addr:u64, length:u32, padding:u32).
    fn attach_3d_cmd(resource_id: u32, entries: &[(u64, u32)]) -> Vec<u8> {
        let mut c = hdr3d(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING, 1);
        c.extend_from_slice(&resource_id.to_le_bytes());
        c.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(addr, len) in entries {
            c.extend_from_slice(&addr.to_le_bytes());
            c.extend_from_slice(&len.to_le_bytes());
            c.extend_from_slice(&0u32.to_le_bytes()); // padding
        }
        c
    }

    /// `RESOURCE_CREATE_BLOB`: hdr(24) resource_id(24) blob_mem(28) blob_flags(32) nr_entries(36)
    /// blob_id(40) size(48) then the mem entries at 56.
    fn create_blob_cmd(resource_id: u32, size: u64, entries: &[(u64, u32)]) -> Vec<u8> {
        let mut c = hdr3d(super::super::gpu3d::VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB, 1);
        for v in [resource_id, 3 /* GUEST */, 1, entries.len() as u32] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c.extend_from_slice(&1u64.to_le_bytes()); // blob_id
        c.extend_from_slice(&size.to_le_bytes());
        for &(addr, len) in entries {
            c.extend_from_slice(&addr.to_le_bytes());
            c.extend_from_slice(&len.to_le_bytes());
            c.extend_from_slice(&0u32.to_le_bytes()); // padding
        }
        c
    }

    /// `SET_SCANOUT_BLOB`: hdr(24) rect(24..40) scanout_id(40) resource_id(44) width(48) height(52)
    /// format(56) padding(60) strides[4](64..80) offsets[4](80..96).
    fn set_scanout_blob_cmd(resource_id: u32, width: u32, height: u32) -> Vec<u8> {
        let mut c = hdr3d(super::super::gpu3d::VIRTIO_GPU_CMD_SET_SCANOUT_BLOB, 0);
        c.extend_from_slice(&[0u8; 16]); // rect
        for v in [0u32 /* scanout_id */, resource_id, width, height, 2, 0] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        c.extend_from_slice(&[0u8; 32]); // strides[4] + offsets[4]
        c
    }

    /// `TRANSFER_TO_HOST_3D`: hdr(24) box[6](24..48) offset(48) resource_id(56) level(60) stride(64)
    /// layer_stride(68).
    fn transfer_to_host_3d_cmd(resource_id: u32, w: u32, h: u32) -> Vec<u8> {
        let mut c = hdr3d(super::super::gpu3d::VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D, 1);
        for v in [0u32, 0, 0, w, h, 1] {
            c.extend_from_slice(&v.to_le_bytes()); // box: x,y,z,w,h,d
        }
        c.extend_from_slice(&0u64.to_le_bytes()); // offset
        for v in [resource_id, 0, w * BPP, 0] {
            c.extend_from_slice(&v.to_le_bytes()); // resource_id, level, stride, layer_stride
        }
        c
    }

    /// A fenced `RESOURCE_FLUSH`: hdr(24) rect(24..40) resource_id(40) padding(44), with the ctrl
    /// header carrying `VIRTIO_GPU_FLAG_FENCE` + `fence_id` so the response identifies its chain.
    fn fenced_flush_cmd(resource_id: u32, fence_id: u64) -> Vec<u8> {
        let mut c = hdr3d(VIRTIO_GPU_CMD_RESOURCE_FLUSH, 0);
        c[4..8].copy_from_slice(&VIRTIO_GPU_FLAG_FENCE.to_le_bytes());
        c[8..16].copy_from_slice(&fence_id.to_le_bytes());
        c.extend_from_slice(&[0u8; 16]); // rect
        c.extend_from_slice(&resource_id.to_le_bytes());
        c.extend_from_slice(&0u32.to_le_bytes()); // padding
        c
    }

    /// The 4 GiB out-of-bounds read/write: a mem entry whose address resolves (so the one-byte
    /// `get_host_address` check passed) but whose length runs far past the end of the guest RAM
    /// mapping. gfxstream would hold that `(ptr, len)` for the life of the resource.
    #[test]
    fn attach_backing_rejects_an_entry_whose_length_runs_past_guest_ram() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem(); // 0x1000 bytes of guest RAM at 0
        create_3d(&mut gpu, &mem, 10, 32, 32);

        let resp = gpu.dispatch_resp(&mem, &attach_3d_cmd(10, &[(0xF00, 0xFFFF_FFFF)]));

        assert_eq!(
            read_le(&resp, 0, 4),
            VIRTIO_GPU_RESP_ERR_UNSPEC as u64,
            "an entry that is not wholly inside guest RAM must be refused"
        );
        assert!(
            gpu.res3d_backing.is_empty(),
            "a rejected attach must record no backing"
        );
        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::AttachBacking { .. })),
            "no (ptr, len) pair may reach stream_renderer_resource_attach_iov: {calls:?}"
        );
    }

    /// `[valid, invalid, valid]`. The whole command must fail. Forwarding the two valid entries would
    /// hand gfxstream a backing in which entry 2's bytes sit where entry 1's belong — every later
    /// transfer reads the wrong offsets, with no error anywhere.
    #[test]
    fn attach_backing_rejects_the_whole_command_when_one_entry_is_invalid() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        create_3d(&mut gpu, &mem, 10, 32, 32);

        let entries = [(0x100u64, 0x40u32), (0x900, 0x1000), (0x200, 0x80)];
        let resp = gpu.dispatch_resp(&mem, &attach_3d_cmd(10, &entries));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(gpu.res3d_backing.is_empty());
        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::AttachBacking { .. })),
            "one bad entry must reject the command, not renumber the rest: {calls:?}"
        );
    }

    /// The same `(ptr, len)` resolution feeds `RESOURCE_CREATE_BLOB`, whose iovecs gfxstream keeps for
    /// the life of the blob.
    #[test]
    fn create_blob_rejects_an_out_of_range_mem_entry() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &create_blob_cmd(9, 0x1000, &[(0xF00, 0xFFFF_FFFF)]));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(
            gpu.blobs.is_empty(),
            "a rejected blob must leave no bookkeeping behind"
        );
        {
            // Scoped: the mock records into this same mutex, so a live guard would deadlock the next
            // dispatch.
            let calls = log.lock().unwrap();
            assert!(
                !calls.iter().any(|c| matches!(c, Call::CreateBlob { .. })),
                "no out-of-range iovec may reach stream_renderer_create_blob: {calls:?}"
            );
        }

        // The in-range form of the same command is still accepted, so the check rejects the range and
        // not the code path.
        let ok = gpu.dispatch_resp(&mem, &create_blob_cmd(9, 0x100, &[(0x100, 0x100)]));
        assert_eq!(read_le(&ok, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert_eq!(gpu.blobs.get(&9), Some(&0x100));
    }

    /// `SET_SCANOUT_BLOB` carries its own `width`/`height`, which nothing else has vetted, and used to
    /// size a `Framebuffer` from them directly — in `u32` arithmetic, while holding the scanout mutex.
    #[test]
    fn set_scanout_blob_rejects_absurd_dimensions() {
        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &set_scanout_blob_cmd(7, 0x10000, 0x10000));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert!(
            !gpu.res3d.contains_key(&7),
            "a rejected scanout must record no resource"
        );
        assert_eq!(
            gpu.scanout_resource, 0,
            "a rejected scanout must not become the bound scanout"
        );
        let handle = gpu.handle();
        let fb = handle.fb.lock().unwrap();
        assert_eq!(
            (fb.width, fb.height),
            (64, 64),
            "the framebuffer must be untouched by a rejected scanout"
        );
    }

    /// A 3D resource with no `RESOURCE_ATTACH_BACKING` gathered `Vec::new()`, and the forwarder built
    /// ONE iovec with `iov_len = 0` from it. gfxstream's `AsVecOption(iov, 1)` returns `Some(..)` for
    /// any `iovec_cnt > 0`, which **overrides** the resource's own `mIovs`, and `TransferWithIov` then
    /// fails with "write request overflowed iovs" — so such a resource could never be written to.
    /// `Option<&mut [u8]>` in the trait is what makes the empty case unrepresentable at the FFI.
    #[test]
    fn transfer_to_host_3d_with_no_gathered_backing_passes_no_iovec() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();
        create_3d(&mut gpu, &mem, 10, 4, 4);
        create_3d(&mut gpu, &mem, 11, 4, 4);
        // Resource 11 gets a real backing; resource 10 deliberately gets none.
        gpu.dispatch_resp(&mem, &attach_3d_cmd(11, &[(0x100, 0x40)]));

        gpu.dispatch_resp(&mem, &transfer_to_host_3d_cmd(10, 4, 4));
        gpu.dispatch_resp(&mem, &transfer_to_host_3d_cmd(11, 4, 4));

        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::TransferToHost { xfer, data: None, .. } if xfer.resource_id == 10
            )),
            "a resource with no gathered backing must forward NO iovec, not an empty one: {calls:?}"
        );
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::TransferToHost { xfer, data: Some(0x40), .. } if xfer.resource_id == 11
            )),
            "a resource with a backing must still forward the gathered bytes: {calls:?}"
        );
    }

    /// `process_queue` drains the control ring until it is empty, inside the MMIO exit, holding
    /// `ctx.pci`. The expensive part is the present in `flush_3d` — a swapchain acquire or a full
    /// GPU→CPU readback — so a batch of N flushes must cost ONE present. Not an iteration cap: the
    /// ring is 256 deep and stopping early risks stalling a guest that does not re-kick. Each flush
    /// still advances the generation, which is what `wait_stable` and the redraw trigger read.
    ///
    /// The three chains are **fenced**, so since item 2.5 they are published by `complete_fence`
    /// rather than by `process_controlq` — the presents, the generation bumps and the per-chain
    /// responses are all unchanged, only the moment of publication moved. That is exercised directly
    /// by `a_fenced_command_is_not_completed_until_the_host_fence_fires`; here it just means the
    /// fences have to be fired before the used ring is inspected.
    #[test]
    fn at_most_one_present_runs_per_controlq_batch() {
        use super::super::testkit::TestQueue;

        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut tq = TestQueue::new(0x8000);
        gpu.handle()
            .metal_layer
            .store(0xCAFE_0000, Ordering::Release);

        // Create + bind the scanout resource outside the batch under test.
        create_3d(&mut gpu, &tq.mem, 10, 32, 32);
        let mut scan = hdr3d(VIRTIO_GPU_CMD_SET_SCANOUT, 0);
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        gpu.dispatch_resp(&tq.mem, &scan);

        let before = gpu.handle().generation();
        let heads: Vec<u16> = (0..3)
            .map(|i| tq.push_chain(&fenced_flush_cmd(10, 0x100 + i), &[CTRL_HDR_LEN as u32]))
            .collect();

        gpu.process_controlq(&tq.mem, &mut tq.queue);
        for i in 0..3u64 {
            assert!(
                gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(0, 0, 0x100 + i)),
                "each host fence must publish its own chain"
            );
        }

        assert_eq!(tq.used().len(), 3, "all three chains must be completed");
        for (i, &head) in heads.iter().enumerate() {
            assert_eq!(
                read_le(&tq.resp_bytes(head), 8, 8),
                0x100 + i as u64,
                "each chain must be answered with its own fence id — proof all three dispatched"
            );
        }
        assert_eq!(
            gpu.handle().generation(),
            before + 3,
            "every flush still advances the generation"
        );
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c, Call::PresentToLayer { .. }))
                .count(),
            1,
            "only one present may run per control-queue batch"
        );
    }

    // =============================================================================================
    // 2.4 + 2.5 — one host fence per fenced command, and deferred completion.
    //
    // Before this, `process_controlq` wrote the response and `add_used`'d it the instant `dispatch`
    // returned, and that same response echoed the guest's `fence_id` — so **every** guest fence
    // signalled at submit time. Mesa's `ResourceTracker` fences real Vulkan queue submissions with
    // `kFenceOut | kRingIdx` and `VirtioGpuAddressSpaceStream` does `execBuffer(...)` then
    // `blob->wait()`; all of it returned instantly, making `vkWaitForFences` a no-op and every frame
    // a candidate for tearing.
    //
    // And `route_header_fence` was called from only the ten 3D arms, so `RESOURCE_MAP_BLOB`,
    // `RESOURCE_ASSIGN_UUID`, `GET_CAPSET_INFO`, `GET_CAPSET` and every 2D command created no host
    // fence at all — invisible while responses were synchronous, a guaranteed hang once they are not.
    // =============================================================================================

    /// The `(ctx_id, ring_idx, fence_id)` a host fence completion carries.
    fn fence_of(ctx_id: u32, ring_idx: u8, fence_id: u64) -> super::super::gpu3d::FenceReq {
        super::super::gpu3d::FenceReq {
            flags: VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX,
            fence_id,
            ctx_id,
            ring_idx,
        }
    }

    /// A command of `cmd_type` whose ctrl header asks for a per-ring fence, with 128 zero bytes of
    /// payload — enough for the longest command the device parses (`SET_SCANOUT_BLOB`, 96 B).
    fn fenced_cmd(cmd_type: u32, ctx_id: u32, ring_idx: u8, fence_id: u64) -> Vec<u8> {
        let mut c = vec![0u8; CTRL_HDR_LEN + 128];
        c[0..4].copy_from_slice(&cmd_type.to_le_bytes());
        c[4..8].copy_from_slice(
            &(VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX).to_le_bytes(),
        );
        c[8..16].copy_from_slice(&fence_id.to_le_bytes());
        c[16..20].copy_from_slice(&ctx_id.to_le_bytes());
        c[20] = ring_idx;
        c
    }

    /// The `Call::CreateFence`s a mock recorded, in order.
    fn fences_created(log: &super::super::gpu3d::CallLog) -> Vec<super::super::gpu3d::FenceReq> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Call::CreateFence(f) => Some(*f),
                _ => None,
            })
            .collect()
    }

    /// Every opcode this device answers, by name. Kept exhaustive on purpose: the point of moving
    /// `route_header_fence` to a single site in `dispatch` is that adding an opcode cannot introduce a
    /// fenced command with no host fence, and this table is what proves the current set is covered.
    const HANDLED_OPCODES: &[(u32, &str)] = &[
        // 2D control block — NONE of these created a host fence before item 2.4.
        (VIRTIO_GPU_CMD_GET_DISPLAY_INFO, "GET_DISPLAY_INFO"),
        (VIRTIO_GPU_CMD_GET_EDID, "GET_EDID"),
        (VIRTIO_GPU_CMD_RESOURCE_CREATE_2D, "RESOURCE_CREATE_2D"),
        (VIRTIO_GPU_CMD_RESOURCE_UNREF, "RESOURCE_UNREF"),
        (VIRTIO_GPU_CMD_SET_SCANOUT, "SET_SCANOUT"),
        (VIRTIO_GPU_CMD_RESOURCE_FLUSH, "RESOURCE_FLUSH"),
        (VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, "TRANSFER_TO_HOST_2D"),
        (
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
            "RESOURCE_ATTACH_BACKING",
        ),
        (
            VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING,
            "RESOURCE_DETACH_BACKING",
        ),
        (VIRTIO_GPU_CMD_UPDATE_CURSOR, "UPDATE_CURSOR"),
        (VIRTIO_GPU_CMD_MOVE_CURSOR, "MOVE_CURSOR"),
        // 3D block — these ten already did.
        (gpu3d::VIRTIO_GPU_CMD_CTX_CREATE, "CTX_CREATE"),
        (gpu3d::VIRTIO_GPU_CMD_CTX_DESTROY, "CTX_DESTROY"),
        (
            gpu3d::VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE,
            "CTX_ATTACH_RESOURCE",
        ),
        (
            gpu3d::VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE,
            "CTX_DETACH_RESOURCE",
        ),
        (
            gpu3d::VIRTIO_GPU_CMD_RESOURCE_CREATE_3D,
            "RESOURCE_CREATE_3D",
        ),
        (
            gpu3d::VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D,
            "TRANSFER_TO_HOST_3D",
        ),
        (
            gpu3d::VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D,
            "TRANSFER_FROM_HOST_3D",
        ),
        (gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, "SUBMIT_3D"),
        (
            gpu3d::VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB,
            "RESOURCE_CREATE_BLOB",
        ),
        (gpu3d::VIRTIO_GPU_CMD_SET_SCANOUT_BLOB, "SET_SCANOUT_BLOB"),
        // …and these four did NOT, despite being 3D-block commands.
        (gpu3d::VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB, "RESOURCE_MAP_BLOB"),
        (
            gpu3d::VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB,
            "RESOURCE_UNMAP_BLOB",
        ),
        (
            gpu3d::VIRTIO_GPU_CMD_RESOURCE_ASSIGN_UUID,
            "RESOURCE_ASSIGN_UUID",
        ),
        (gpu3d::VIRTIO_GPU_CMD_GET_CAPSET_INFO, "GET_CAPSET_INFO"),
        (gpu3d::VIRTIO_GPU_CMD_GET_CAPSET, "GET_CAPSET"),
    ];

    /// Table-drive **every** opcode the device answers with `FLAG_FENCE | FLAG_INFO_RING_IDX` and
    /// assert each one creates exactly one host fence carrying the request's `(ctx_id, ring_idx,
    /// fence_id)`, and answers with those same ids echoed back.
    ///
    /// `RESOURCE_MAP_BLOB`, `RESOURCE_ASSIGN_UUID`, `GET_CAPSET_INFO`, `GET_CAPSET` and all eleven 2D
    /// commands fail this test as the code stood before item 2.4.
    #[test]
    fn every_command_the_device_answers_creates_a_host_fence_when_asked() {
        assert!(
            HANDLED_OPCODES.len() >= 26,
            "the opcode table has shrunk — a missing entry is a silently untested command"
        );
        for &(op, name) in HANDLED_OPCODES {
            let (mock, log) = MockRenderer::new();
            let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
            let mem = mem();

            let resp = gpu.dispatch_resp(&mem, &fenced_cmd(op, 1, 1, 0xF00D));

            assert_eq!(
                fences_created(&log),
                vec![fence_of(1, 1, 0xF00D)],
                "{name} ({op:#06x}) must create exactly one host fence, carrying the request's ids"
            );
            assert_eq!(
                read_le(&resp, 4, 4),
                u64::from(VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX),
                "{name}: the response must keep both fence flags"
            );
            assert_eq!(read_le(&resp, 8, 8), 0xF00D, "{name}: response fence_id");
            assert_eq!(
                &resp[16..21],
                &[1, 0, 0, 0, 1],
                "{name}: response ctx_id + ring_idx"
            );
        }
    }

    /// The concrete case the guest actually runs: Mesa fences a real Vulkan queue submission with
    /// `kFenceOut | kRingIdx` on **ring 1** of its context. Bytes 16..21 of the response are the ring
    /// identity, and all three of them (`ctx_id` 16..20, `ring_idx` 20) plus the `INFO_RING_IDX` flag
    /// bit were zeroed on every response before item 2.1.
    ///
    /// Also pins that carrying the ring identity does not disturb the payload parse — the guest's
    /// command stream must reach `submit_cmd` byte for byte.
    #[test]
    fn a_fenced_submit_3d_answers_with_the_ring_it_was_submitted_on() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let payload: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4];
        let mut cmd = fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x1234);
        cmd.truncate(CTRL_HDR_LEN + 8); // hdr(24) size(4) padding(4)
        cmd[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        cmd.extend_from_slice(&payload);

        let resp = gpu.dispatch_resp(&mem, &cmd);

        assert_eq!(
            &resp[16..21],
            &[1, 0, 0, 0, 1],
            "ctx_id=1 (le32) then ring_idx=1 — the whole of the ring identity"
        );
        assert_eq!(
            read_le(&resp, 4, 4),
            0b11,
            "FLAG_FENCE | FLAG_INFO_RING_IDX; the second bit used to be masked off"
        );
        assert_eq!(read_le(&resp, 8, 8), 0x1234);

        let calls = log.lock().unwrap();
        assert!(
            calls.iter().any(|c| matches!(
                c,
                Call::SubmitCmd { ctx_id: 1, bytes } if bytes.as_slice() == payload
            )),
            "the guest command stream must still be forwarded verbatim: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::CreateFence(f) if *f == fence_of(1, 1, 0x1234))),
            "and the host fence must carry the same identity: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .position(|c| matches!(c, Call::SubmitCmd { .. }))
                < calls.iter().position(|c| matches!(c, Call::CreateFence(_))),
            "the fence must be created AFTER the work it fences — gfxstream enqueues it behind \
             whatever is already on the ring, so the reverse order signals early: {calls:?}"
        );
    }

    /// An opcode the device does *not* implement still gets a host fence: the guest is waiting on that
    /// fence regardless of the `ERR_UNSPEC` answer, so refusing to create it would hang the guest on
    /// an unknown command. Covered separately because it is the arm the table above cannot name.
    #[test]
    fn even_an_unhandled_opcode_gets_the_host_fence_the_guest_is_waiting_on() {
        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mem = mem();

        let resp = gpu.dispatch_resp(&mem, &fenced_cmd(0x0fff, 2, 1, 0x5));

        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_ERR_UNSPEC as u64);
        assert_eq!(fences_created(&log), vec![fence_of(2, 1, 0x5)]);
    }

    /// The core of item 2.5. A fenced command must be **withheld** from the used ring until the host
    /// fence fires, because the response is what signals the guest's fence: publishing it at submit
    /// time is what made `vkWaitForFences` return before the host had done the work.
    ///
    /// Also pins the key: `ring_idx` and `ctx_id` are part of the identity, so a completion on
    /// another ring (or in another context) must not publish this chain. That is only meaningful
    /// because `resp_hdr` now carries the real ring index (item 2.1).
    #[test]
    fn a_fenced_command_is_not_completed_until_the_host_fence_fires() {
        use super::super::testkit::TestQueue;

        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut tq = TestQueue::new(0x8000);

        let cmd = fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x77);
        let head = tq.push_chain(&cmd, &[CTRL_HDR_LEN as u32]);

        assert!(
            !gpu.process_controlq(&tq.mem, &mut tq.queue),
            "a batch in which every command deferred completes nothing, so no IRQ is owed yet"
        );
        assert!(
            tq.used().is_empty(),
            "THE finding: the guest must not see the response — and so must not see its fence \
             signalled — before the host fence has fired"
        );
        assert_eq!(gpu.pending_fence_count(), 1);
        assert_eq!(
            tq.resp_bytes(head),
            vec![0u8; CTRL_HDR_LEN],
            "the response descriptor must still be untouched while the command is pending"
        );
        assert_eq!(
            fences_created(&log),
            vec![fence_of(1, 1, 0x77)],
            "the host fence must have been created, or nothing would ever publish this chain"
        );

        // Same fence id, wrong ring / wrong context: not this command.
        assert!(!gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 0, 0x77)));
        assert!(!gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(9, 1, 0x77)));
        assert!(
            tq.used().is_empty(),
            "a fence on another ring is not this one"
        );
        assert_eq!(gpu.pending_fence_count(), 1);

        assert!(gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 1, 0x77)));

        let used = tq.used();
        assert_eq!(used.len(), 1, "exactly one used entry: {used:?}");
        assert_eq!(
            used[0],
            (u32::from(head), CTRL_HDR_LEN as u32),
            "the head and the full response length must be published"
        );
        let resp = tq.resp_bytes(head);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert_eq!(
            read_le(&resp, 4, 4),
            u64::from(VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX)
        );
        assert_eq!(read_le(&resp, 8, 8), 0x77);
        assert_eq!(&resp[16..21], &[1, 0, 0, 0, 1]);
        assert_eq!(gpu.pending_fence_count(), 0);

        assert!(
            !gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 1, 0x77)),
            "a repeated completion must not publish the chain twice"
        );
        assert_eq!(tq.used().len(), 1);
    }

    /// Deferral applies only to fenced commands. An unfenced one — which is most of the boot path —
    /// must still be answered inside `process_controlq`, or the guest's very first
    /// `GET_DISPLAY_INFO` would wait for a fence that nobody ever asked for.
    #[test]
    fn an_unfenced_command_still_completes_synchronously() {
        use super::super::testkit::TestQueue;

        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut tq = TestQueue::new(0x8000);

        let resp_len = (CTRL_HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * 24) as u32;
        let head = tq.push_chain(&hdr3d(VIRTIO_GPU_CMD_GET_DISPLAY_INFO, 0), &[resp_len]);

        assert!(gpu.process_controlq(&tq.mem, &mut tq.queue));

        assert_eq!(tq.used(), vec![(u32::from(head), resp_len)]);
        assert_eq!(gpu.pending_fence_count(), 0, "nothing may be withheld");
        assert!(
            fences_created(&log).is_empty(),
            "an unfenced command must not create a host fence either"
        );
        assert_eq!(
            read_le(&tq.resp_bytes(head), 0, 4),
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO as u64
        );
    }

    /// Safety net (a) — the watchdog. Without it, a renderer that stops signalling turns deferral into
    /// a **permanent** guest hang, which is strictly worse than the submit-time completion it
    /// replaces. `sweep_stale_fences` takes `now` so the 2 s deadline is testable without sleeping.
    #[test]
    fn a_fence_that_never_fires_is_force_completed_after_the_deadline() {
        use super::super::testkit::TestQueue;

        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut tq = TestQueue::new(0x8000);

        let head = tq.push_chain(
            &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x31),
            &[CTRL_HDR_LEN as u32],
        );
        gpu.process_controlq(&tq.mem, &mut tq.queue);
        assert_eq!(gpu.pending_fence_count(), 1);

        assert!(
            !gpu.sweep_stale_fences(&tq.mem, &mut tq.queue, Instant::now()),
            "a command inside the deadline must be left alone — the watchdog is not a shortcut"
        );
        assert!(tq.used().is_empty());

        let past_deadline = Instant::now() + FENCE_DEADLINE + Duration::from_millis(1);
        assert!(gpu.sweep_stale_fences(&tq.mem, &mut tq.queue, past_deadline));

        assert_eq!(
            tq.used(),
            vec![(u32::from(head), CTRL_HDR_LEN as u32)],
            "the wedged command must be force-completed so the control ring keeps moving"
        );
        assert_eq!(read_le(&tq.resp_bytes(head), 8, 8), 0x31);
        assert_eq!(gpu.pending_fence_count(), 0);

        assert!(
            !gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 1, 0x31)),
            "a fence that arrives after the sweep must not re-publish the chain"
        );
        assert_eq!(tq.used().len(), 1);
    }

    /// Safety net (b) — the cap. The watchdog bounds how *long* one wedged fence stalls a command;
    /// this bounds how *much* state a renderer that has stopped signalling can accumulate. Past the
    /// cap, `dispatch` reverts to the pre-2.5 behaviour rather than growing the pending map.
    #[test]
    fn pending_fences_are_capped_and_fall_back_to_synchronous_completion() {
        use super::super::testkit::TestQueue;

        assert_eq!(
            Gpu::new(64, 64).max_pending_fences,
            MAX_PENDING_FENCES,
            "production must use the constant; only tests lower it"
        );
        assert!(
            MAX_PENDING_FENCES > super::super::QUEUE_SIZE as usize,
            "a cap at or below the ring depth would be reachable by a well-behaved guest"
        );

        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        gpu.set_max_pending_fences(2);
        let mut tq = TestQueue::new(0x8000);

        let heads: Vec<u16> = (0..4)
            .map(|i| {
                tq.push_chain(
                    &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x40 + i),
                    &[CTRL_HDR_LEN as u32],
                )
            })
            .collect();

        assert!(
            gpu.process_controlq(&tq.mem, &mut tq.queue),
            "the commands past the cap were completed, so an IRQ is owed"
        );

        assert_eq!(gpu.pending_fence_count(), 2, "the cap must hold");
        assert_eq!(
            tq.used(),
            vec![
                (u32::from(heads[2]), CTRL_HDR_LEN as u32),
                (u32::from(heads[3]), CTRL_HDR_LEN as u32),
            ],
            "the two commands past the cap must be answered at submit time, not dropped"
        );
        assert_eq!(read_le(&tq.resp_bytes(heads[2]), 8, 8), 0x42);
    }

    /// The runtime revert. Deferral moves *when* the guest observes every fenced command, so it ships
    /// with an escape hatch: `AEMU_GPU_SYNC_FENCES=1` restores submit-time completion exactly.
    ///
    /// The env decision is a pure function so this test needs no process-environment mutation (which
    /// would leak into every other test in this binary, all of which run in parallel); the behavioural
    /// half is driven through the same flag the env sets.
    #[test]
    #[allow(non_snake_case)] // named for the environment variable it pins
    fn AEMU_GPU_SYNC_FENCES_restores_synchronous_completion() {
        use super::super::testkit::TestQueue;
        use std::ffi::OsStr;

        assert_eq!(SYNC_FENCES_ENV, "AEMU_GPU_SYNC_FENCES");
        assert!(sync_fences_requested(Some(OsStr::new("1"))));
        assert!(
            !sync_fences_requested(None),
            "unset ⇒ deferral is the default"
        );
        assert!(
            !sync_fences_requested(Some(OsStr::new("0"))),
            "=0 must not enable the thing it looks like it disables"
        );

        let (mock, log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        gpu.set_sync_fences(true);
        let mut tq = TestQueue::new(0x8000);

        let head = tq.push_chain(
            &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x88),
            &[CTRL_HDR_LEN as u32],
        );

        assert!(gpu.process_controlq(&tq.mem, &mut tq.queue));

        assert_eq!(
            tq.used(),
            vec![(u32::from(head), CTRL_HDR_LEN as u32)],
            "with the hatch open the fenced command completes at submit time again"
        );
        assert_eq!(gpu.pending_fence_count(), 0);
        assert_eq!(read_le(&tq.resp_bytes(head), 8, 8), 0x88);
        assert_eq!(
            fences_created(&log),
            vec![fence_of(1, 1, 0x88)],
            "the host fence is still created — only the completion timing reverts, so the renderer \
             sees an identical command stream either way"
        );
    }

    /// Fences on one `(ctx_id, ring_idx)` timeline complete in order, and guest fence ids come from a
    /// single monotonic counter — so a completion at id N implies every earlier id on that ring is
    /// done. Keying strictly on equality would leave an earlier command wedged forever if gfxstream
    /// ever coalesced or dropped a callback, recoverable only by the 2 s watchdog.
    #[test]
    fn completing_a_later_fence_publishes_the_earlier_ones_on_the_same_ring() {
        use super::super::testkit::TestQueue;

        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut tq = TestQueue::new(0x8000);

        // Three on ring 1, one on ring 0 — the ring-0 chain must be untouched.
        let mut heads = Vec::new();
        for id in [0x10u64, 0x11, 0x12] {
            heads.push(tq.push_chain(
                &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, id),
                &[CTRL_HDR_LEN as u32],
            ));
        }
        let ring0 = tq.push_chain(
            &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 0, 0x13),
            &[CTRL_HDR_LEN as u32],
        );
        gpu.process_controlq(&tq.mem, &mut tq.queue);
        assert_eq!(gpu.pending_fence_count(), 4);

        assert!(gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 1, 0x12)));

        let used = tq.used();
        assert_eq!(
            used.len(),
            3,
            "all three ring-1 chains must be published, oldest first: {used:?}"
        );
        assert_eq!(
            used.iter().map(|u| u.0).collect::<Vec<_>>(),
            heads.iter().map(|h| u32::from(*h)).collect::<Vec<_>>(),
            "publication order must follow submission order"
        );
        assert_eq!(
            gpu.pending_fence_count(),
            1,
            "ring 0 is an independent timeline and must still be pending"
        );

        assert!(gpu.complete_fence(&tq.mem, &mut tq.queue, &fence_of(1, 0, 0x13)));
        assert_eq!(tq.used().len(), 4);
        assert_eq!(read_le(&tq.resp_bytes(ring0), 8, 8), 0x13);
        assert_eq!(&tq.resp_bytes(ring0)[16..21], &[1, 0, 0, 0, 0]);
    }

    /// `drain_fences` is what the `gpu-fence` thread actually calls: it must pick the **control**
    /// queue out of the device's queue array (not the cursor queue) and run the watchdog in the same
    /// pass, so one lock acquisition covers both.
    #[test]
    fn drain_fences_completes_and_sweeps_on_the_control_queue() {
        use super::super::testkit::TestQueue;

        let (mock, _log) = MockRenderer::new();
        let mut gpu = Gpu::with_renderer(64, 64, Box::new(mock));
        let mut ctrl = TestQueue::new(0x8000);
        let mut cursor = TestQueue::new(0x8000);

        let fired = ctrl.push_chain(
            &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 1, 1, 0x1),
            &[CTRL_HDR_LEN as u32],
        );
        let wedged = ctrl.push_chain(
            &fenced_cmd(gpu3d::VIRTIO_GPU_CMD_SUBMIT_3D, 2, 1, 0x2),
            &[CTRL_HDR_LEN as u32],
        );
        gpu.process_controlq(&ctrl.mem, &mut ctrl.queue);

        // Queue array in transport order: CONTROLQ = 0, CURSORQ = 1.
        let mut queues = vec![
            std::mem::replace(&mut ctrl.queue, virtio_queue::Queue::new(4).unwrap()),
            std::mem::replace(&mut cursor.queue, virtio_queue::Queue::new(4).unwrap()),
        ];

        assert!(gpu.drain_fences(
            &ctrl.mem,
            &mut queues,
            &[fence_of(1, 1, 0x1)],
            Instant::now()
        ));
        assert_eq!(ctrl.used(), vec![(u32::from(fired), CTRL_HDR_LEN as u32)]);
        assert!(
            cursor.used().is_empty(),
            "nothing may be published on the cursor queue"
        );
        assert_eq!(gpu.pending_fence_count(), 1);

        // No fences this pass, but the sweep still runs.
        assert!(gpu.drain_fences(
            &ctrl.mem,
            &mut queues,
            &[],
            Instant::now() + FENCE_DEADLINE + Duration::from_millis(1)
        ));
        assert_eq!(ctrl.used().len(), 2);
        assert_eq!(read_le(&ctrl.resp_bytes(wedged), 8, 8), 0x2);
        assert_eq!(gpu.pending_fence_count(), 0);
    }
}
