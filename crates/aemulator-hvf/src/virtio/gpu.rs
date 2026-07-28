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

use super::{Mem, VIRTIO_F_VERSION_1};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;
const CTRL_HDR_LEN: usize = 24;
const BPP: u32 = 4;

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
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            bytes: vec![0u8; (width * height * BPP) as usize],
        }
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
}

impl Gpu {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            default_width: width,
            default_height: height,
            resources: HashMap::new(),
            scanout_resource: 0,
            scanout: ScanoutHandle {
                fb: Arc::new(Mutex::new(Framebuffer::new(width, height))),
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
                let hdr = resp_hdr(VIRTIO_GPU_RESP_OK_NODATA, 0, 0);
                write_scatter(mem, &writable, &hdr)
            };
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    fn process_controlq(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let (cmd, writable) = split_chain(mem, chain);
            let resp = self.dispatch(mem, &cmd);
            let written = write_scatter(mem, &writable, &resp);
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    /// Parse one control command from its device-readable bytes and return the response bytes.
    fn dispatch(&mut self, mem: &Mem, cmd: &[u8]) -> Vec<u8> {
        if cmd.len() < CTRL_HDR_LEN {
            return resp_hdr(VIRTIO_GPU_RESP_ERR_UNSPEC, 0, 0).to_vec();
        }
        let cmd_type = read_le(cmd, 0, 4) as u32;
        let flags = read_le(cmd, 4, 4) as u32;
        let fence = read_le(cmd, 8, 8);
        let ok = |t: u32| resp_hdr(t, flags, fence).to_vec();

        match cmd_type {
            VIRTIO_GPU_CMD_GET_DISPLAY_INFO => self.resp_display_info(flags, fence),
            VIRTIO_GPU_CMD_GET_EDID => self.resp_edid(flags, fence),
            VIRTIO_GPU_CMD_RESOURCE_CREATE_2D => {
                // hdr(24) resource_id(24) format(28) width(32) height(36)
                let id = read_le(cmd, 24, 4) as u32;
                let format = read_le(cmd, 28, 4) as u32;
                let width = read_le(cmd, 32, 4) as u32;
                let height = read_le(cmd, 36, 4) as u32;
                if id == 0 || width == 0 || height == 0 {
                    return ok(VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID);
                }
                self.resources.insert(
                    id,
                    Resource {
                        width,
                        height,
                        format,
                        backing: Vec::new(),
                        pixels: vec![0u8; (width * height * BPP) as usize],
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
                    let mut fb = self.scanout.fb.lock().unwrap();
                    if fb.width != w || fb.height != h {
                        *fb = Framebuffer::new(w, h);
                    }
                }
                ok(VIRTIO_GPU_RESP_OK_NODATA)
            }
            VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING => {
                // hdr(24) resource_id(24) nr_entries(28) then nr_entries * mem_entry(16: addr,len,pad)
                let id = read_le(cmd, 24, 4) as u32;
                let nr = read_le(cmd, 28, 4) as u32 as usize;
                let mut backing = Vec::with_capacity(nr);
                let mut off = 32;
                for _ in 0..nr {
                    if off + 16 > cmd.len() {
                        break;
                    }
                    let addr = read_le(cmd, off, 8);
                    let len = read_le(cmd, off + 8, 4) as u32;
                    backing.push((addr, len));
                    off += 16;
                }
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
                    let iovs: Vec<(u64, usize)> = backing
                        .iter()
                        .filter_map(|&(addr, len)| host_addr(mem, addr).map(|h| (h, len as usize)))
                        .collect();
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
            other => {
                // Phase 3c: the 3D/virgl command block (0x02xx) + capset queries route into the
                // gfxstream renderer when one is attached. Feature-off (or no renderer) ⇒ ERR_UNSPEC,
                // exactly the 2D behaviour.
                if self.renderer.is_some() {
                    if let Some(resp) = self.dispatch_3d(mem, cmd, other, flags, fence) {
                        return resp;
                    }
                }
                let _ = other;
                ok(VIRTIO_GPU_RESP_ERR_UNSPEC)
            }
        }
    }

    fn resp_display_info(&self, flags: u32, fence: u64) -> Vec<u8> {
        let mut buf = vec![0u8; CTRL_HDR_LEN + VIRTIO_GPU_MAX_SCANOUTS * 24];
        buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(
            VIRTIO_GPU_RESP_OK_DISPLAY_INFO,
            flags,
            fence,
        ));
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
    fn resp_edid(&self, flags: u32, fence: u64) -> Vec<u8> {
        let edid = build_edid(self.default_width, self.default_height, EDID_DPI);
        let mut buf = vec![0u8; CTRL_HDR_LEN + 8 + edid.len()];
        buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(VIRTIO_GPU_RESP_OK_EDID, flags, fence));
        buf[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&(edid.len() as u32).to_le_bytes());
        // padding (CTRL_HDR_LEN+4 .. +8) stays 0.
        buf[CTRL_HDR_LEN + 8..].copy_from_slice(&edid);
        buf
    }

    /// Copy the rect of guest backing into the host resource image (qemu 2D transfer semantics).
    fn transfer_to_host(&mut self, mem: &Mem, id: u32, rect: Rect, offset: u64) -> bool {
        let Some(res) = self.resources.get_mut(&id) else {
            return false;
        };
        if res.backing.is_empty() {
            return true; // nothing attached yet, but not an error
        }
        let backing = gather_backing(mem, &res.backing);
        let stride = (res.width * BPP) as usize;
        let rowlen = (rect.width * BPP) as usize;
        for row in 0..rect.height {
            let src = offset as usize + (row as usize) * stride;
            let dst = ((rect.y + row) as usize) * stride + (rect.x * BPP) as usize;
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
        let Some(res) = self.resources.get(&rid) else {
            return;
        };
        let mut fb = self.scanout.fb.lock().unwrap();
        if fb.width != res.width || fb.height != res.height {
            *fb = Framebuffer::new(res.width, res.height);
        }
        let n = fb.bytes.len().min(res.pixels.len());
        fb.bytes[..n].copy_from_slice(&res.pixels[..n]);
        drop(fb);
        self.scanout.generation.fetch_add(1, Ordering::Release);
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
    fn dispatch_3d(
        &mut self,
        mem: &Mem,
        cmd: &[u8],
        cmd_type: u32,
        flags: u32,
        fence: u64,
    ) -> Option<Vec<u8>> {
        use super::gpu3d::*;
        let nodata = |t: u32| resp_hdr(t, flags, fence).to_vec();
        // ctx_id is carried in the ctrl header (offset 16) on every 3D command.
        let ctx_id = read_le(cmd, CTRL_HDR_CTX_ID_OFF, 4) as u32;

        match cmd_type {
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
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_DESTROY => {
                self.renderer.as_mut().unwrap().context_destroy(ctx_id);
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE => {
                let res = read_le(cmd, 24, 4) as u32;
                self.renderer
                    .as_mut()
                    .unwrap()
                    .ctx_attach_resource(ctx_id, res);
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE => {
                let res = read_le(cmd, 24, 4) as u32;
                self.renderer
                    .as_mut()
                    .unwrap()
                    .ctx_detach_resource(ctx_id, res);
                self.route_header_fence(cmd, flags, fence, ctx_id);
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
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D => {
                let xfer = parse_transfer3d(cmd);
                // Copy the guest backing this transfer feeds gfxstream from.
                let mut data = self
                    .res3d_backing
                    .get(&xfer.resource_id)
                    .map(|b| gather_backing(mem, b))
                    .unwrap_or_default();
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .transfer_to_host_3d(ctx_id, &xfer, &mut data);
                warn_rc(rc, "transfer_to_host_3d", xfer.resource_id);
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D => {
                let xfer = parse_transfer3d(cmd);
                // Read host resource → a scratch buffer sized to the box, then scatter into the
                // guest backing so the guest sees the pixels.
                let (bw, bh) = (xfer.box_[3].max(1), xfer.box_[4].max(1));
                let mut data = vec![0u8; (bw * bh * BPP) as usize];
                let rc = self
                    .renderer
                    .as_mut()
                    .unwrap()
                    .transfer_from_host_3d(ctx_id, &xfer, &mut data);
                warn_rc(rc, "transfer_from_host_3d", xfer.resource_id);
                if let Some(backing) = self.res3d_backing.get(&xfer.resource_id) {
                    scatter_backing(mem, backing, &data);
                }
                self.route_header_fence(cmd, flags, fence, ctx_id);
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
                self.route_header_fence(cmd, flags, fence, ctx_id);
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
                let mut iovs: Vec<(u64, usize)> = Vec::with_capacity(blob.nr_entries as usize);
                let mut off = 56usize;
                for _ in 0..blob.nr_entries {
                    if off + 16 > cmd.len() {
                        break;
                    }
                    let addr = read_le(cmd, off, 8);
                    let len = read_le(cmd, off + 8, 4) as usize;
                    if let Some(hva) = host_addr(mem, addr) {
                        iovs.push((hva, len));
                    }
                    off += 16;
                }
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
                self.route_header_fence(cmd, flags, fence, ctx_id);
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
                self.scanout_resource = resource_id;
                // resource_id 0 disables the scanout; otherwise adopt the blob's own dimensions,
                // which — unlike SET_SCANOUT — arrive in the command rather than from a prior
                // RESOURCE_CREATE_2D/_3D, so record them for the flush + present path.
                if resource_id != 0 && width != 0 && height != 0 {
                    self.res3d.insert(resource_id, (width, height));
                    let mut fb = self.scanout.fb.lock().unwrap();
                    if fb.width != width || fb.height != height {
                        *fb = Framebuffer::new(width, height);
                    }
                }
                self.route_header_fence(cmd, flags, fence, ctx_id);
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
                buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(
                    VIRTIO_GPU_RESP_OK_RESOURCE_UUID,
                    flags,
                    fence,
                ));
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
                Some(self.map_blob(resource_id, offset, flags, fence))
            }
            VIRTIO_GPU_CMD_RESOURCE_UNMAP_BLOB => {
                let resource_id = read_le(cmd, 24, 4) as u32;
                self.unmap_blob(resource_id);
                self.route_header_fence(cmd, flags, fence, ctx_id);
                Some(nodata(VIRTIO_GPU_RESP_OK_NODATA))
            }
            VIRTIO_GPU_CMD_GET_CAPSET_INFO => {
                let index = read_le(cmd, 24, 4) as u32;
                let (id, max_ver, max_size) = self.renderer.as_mut().unwrap().capset_info(index);
                self.capset_size.insert(id, max_size);
                let mut buf = vec![0u8; CTRL_HDR_LEN + 16];
                buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(
                    VIRTIO_GPU_RESP_OK_CAPSET_INFO,
                    flags,
                    fence,
                ));
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
                buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(
                    VIRTIO_GPU_RESP_OK_CAPSET,
                    flags,
                    fence,
                ));
                buf[CTRL_HDR_LEN..].copy_from_slice(&caps);
                Some(buf)
            }
            _ => None,
        }
    }

    /// If the ctrl header's FENCE flag is set, route a `stream_renderer_create_fence` for it. (The
    /// async fence-callback → guest-fence-ring → device-IRQ bridge needs the live renderer + GIC
    /// handle and is exercised only with a real guest — deferred to Phase 4. The response already
    /// echoes the fence id, which completes command-scoped fencing for the synchronous model here.)
    fn route_header_fence(&mut self, cmd: &[u8], flags: u32, fence: u64, ctx_id: u32) {
        if flags & VIRTIO_GPU_FLAG_FENCE == 0 {
            return;
        }
        let ring_idx = read_le(cmd, 20, 1) as u8;
        if let Some(r) = self.renderer.as_mut() {
            r.create_fence(&super::gpu3d::FenceReq {
                flags,
                fence_id: fence,
                ctx_id,
                ring_idx,
            });
        }
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
        {
            let mut fb = self.scanout.fb.lock().unwrap();
            if fb.width != w || fb.height != h {
                *fb = Framebuffer::new(w, h);
            }
        }

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
        let mut tmp = vec![0u8; (w * h * BPP) as usize];
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
    fn map_blob(&mut self, resource_id: u32, offset: u64, flags: u32, fence: u64) -> Vec<u8> {
        use crate::virtio::pci::{PCI_SHM_BASE, PCI_SHM_SIZE};
        let err = |t: u32| resp_hdr(t, flags, fence).to_vec();

        let Some((host_ptr, size)) = self.renderer.as_mut().unwrap().resource_map(resource_id)
        else {
            eprintln!("[gpu3d] map_blob res={resource_id}: stream_renderer_resource_map failed");
            return err(VIRTIO_GPU_RESP_ERR_UNSPEC);
        };
        let ipa = PCI_SHM_BASE + offset;
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
        // hv_vm_map requires IPA / host / size all aligned to the host granule (16 KiB on Apple
        // Silicon). A guest that allocated the blob at a 4 KiB-aligned offset will trip this — log it
        // precisely (it is the most likely next blocker), then still attempt so HVF's rc is reported.
        let page = crate::host_page_size() as u64;
        if ipa % page != 0 || host_ptr % page != 0 || size % page != 0 {
            eprintln!(
                "[gpu3d] map_blob res={resource_id} ALIGNMENT ipa={ipa:#x} host={host_ptr:#x} size={size:#x} page={page:#x} \
                 (not host-granule aligned)"
            );
        }
        let rw = crate::HV_MEMORY_READ | crate::HV_MEMORY_WRITE;
        let rc = unsafe {
            crate::hv_vm_map(
                host_ptr as *mut std::os::raw::c_void,
                ipa,
                size as usize,
                rw,
            )
        };
        if rc != crate::HV_SUCCESS {
            eprintln!(
                "[gpu3d] map_blob res={resource_id} hv_vm_map(host={host_ptr:#x}, ipa={ipa:#x}, size={size:#x}) -> {} ({:#010x})",
                crate::hv_return_name(rc),
                rc as u32
            );
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
        buf[..CTRL_HDR_LEN].copy_from_slice(&resp_hdr(
            super::gpu3d::VIRTIO_GPU_RESP_OK_MAP_INFO,
            flags,
            fence,
        ));
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
            let rc = unsafe { crate::hv_vm_unmap(ipa, size as usize) };
            if rc != crate::HV_SUCCESS {
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

/// Resolve a guest physical address to its backing host pointer (for building blob iovecs handed to
/// gfxstream). `None` if the address is outside mapped guest RAM.
fn host_addr(mem: &Mem, guest_addr: u64) -> Option<u64> {
    use vm_memory::GuestMemoryBackend;
    mem.get_host_address(GuestAddress(guest_addr))
        .ok()
        .map(|p| p as u64)
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
fn split_chain(
    mem: &Mem,
    chain: virtio_queue::DescriptorChain<&Mem>,
) -> (Vec<u8>, Vec<(u64, u32)>) {
    let descs: Vec<(u64, u32, bool)> = chain
        .map(|d| (d.addr().0, d.len(), d.is_write_only()))
        .collect();
    let mut readable = Vec::new();
    let mut writable = Vec::new();
    for (addr, len, write_only) in descs {
        if write_only {
            writable.push((addr, len));
        } else {
            let mut buf = vec![0u8; len as usize];
            if mem.read_slice(&mut buf, GuestAddress(addr)).is_ok() {
                readable.extend_from_slice(&buf);
            }
        }
    }
    (readable, writable)
}

/// Assemble a resource's scattered guest backing into one contiguous host buffer.
fn gather_backing(mem: &Mem, entries: &[(u64, u32)]) -> Vec<u8> {
    let total: usize = entries.iter().map(|(_, l)| *l as usize).sum();
    let mut buf = Vec::with_capacity(total);
    for &(addr, len) in entries {
        let mut t = vec![0u8; len as usize];
        if mem.read_slice(&mut t, GuestAddress(addr)).is_ok() {
            buf.extend_from_slice(&t);
        } else {
            buf.extend(std::iter::repeat(0u8).take(len as usize));
        }
    }
    buf
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

/// Build a 24-byte `virtio_gpu_ctrl_hdr` response header. Echoes the request fence when the guest
/// asked for one (`VIRTIO_GPU_FLAG_FENCE`).
fn resp_hdr(resp_type: u32, req_flags: u32, fence: u64) -> [u8; CTRL_HDR_LEN] {
    let mut h = [0u8; CTRL_HDR_LEN];
    h[0..4].copy_from_slice(&resp_type.to_le_bytes());
    let flags = req_flags & VIRTIO_GPU_FLAG_FENCE;
    h[4..8].copy_from_slice(&flags.to_le_bytes());
    if flags != 0 {
        h[8..16].copy_from_slice(&fence.to_le_bytes());
    }
    h
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
        let resp = gpu.resp_edid(0, 0);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_EDID as u64);
        assert_eq!(read_le(&resp, CTRL_HDR_LEN as usize, 4), 128); // size field
                                                                   // EDID payload begins at hdr(24)+size(4)+padding(4) = 32, with the header magic.
        assert_eq!(resp[32], 0x00);
        assert_eq!(resp[33], 0xFF);
    }

    #[test]
    fn resp_hdr_echoes_fence_only_when_requested() {
        let no_fence = resp_hdr(VIRTIO_GPU_RESP_OK_NODATA, 0, 0xdead);
        assert_eq!(read_le(&no_fence, 8, 8), 0);
        let with_fence = resp_hdr(VIRTIO_GPU_RESP_OK_NODATA, VIRTIO_GPU_FLAG_FENCE, 0xdead);
        assert_eq!(read_le(&with_fence, 8, 8), 0xdead);
        assert_eq!(read_le(&with_fence, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
    }

    #[test]
    fn display_info_reports_default_mode() {
        let gpu = Gpu::new(1024, 768);
        let resp = gpu.resp_display_info(0, 0);
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
        let resp = gpu.dispatch(&mem, &cmd);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);
        assert!(gpu.resources.contains_key(&7));

        let mut unref = hdr(VIRTIO_GPU_CMD_RESOURCE_UNREF).to_vec();
        unref.extend_from_slice(&7u32.to_le_bytes());
        unref.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(&mem, &unref);
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
        gpu.dispatch(&mem, &create);

        // attach_backing id=1 nr=1 (addr=0x1000,len=16)
        let mut attach = hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING).to_vec();
        attach.extend_from_slice(&1u32.to_le_bytes());
        attach.extend_from_slice(&1u32.to_le_bytes());
        attach.extend_from_slice(&0x1000u64.to_le_bytes());
        attach.extend_from_slice(&16u32.to_le_bytes());
        attach.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(&mem, &attach);

        // set_scanout scanout0 -> resource1
        let mut scan = hdr(VIRTIO_GPU_CMD_SET_SCANOUT).to_vec();
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        gpu.dispatch(&mem, &scan);

        // transfer_to_host_2d full 2x2, offset 0, id 1
        let mut xfer = hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D).to_vec();
        xfer.extend_from_slice(&0u32.to_le_bytes()); // x
        xfer.extend_from_slice(&0u32.to_le_bytes()); // y
        xfer.extend_from_slice(&2u32.to_le_bytes()); // w
        xfer.extend_from_slice(&2u32.to_le_bytes()); // h
        xfer.extend_from_slice(&0u64.to_le_bytes()); // offset
        xfer.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        xfer.extend_from_slice(&0u32.to_le_bytes()); // padding
        gpu.dispatch(&mem, &xfer);

        // flush id=1
        let handle = gpu.handle();
        let before = handle.generation();
        let mut flush = hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH).to_vec();
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&1u32.to_le_bytes()); // resource_id
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(&mem, &flush);

        assert_eq!(handle.generation(), before + 1);
        let fb = handle.fb.lock().unwrap();
        assert_eq!(fb.bytes, pixels);
        assert!(fb.non_zero_bytes() > 0);
    }
}

/// Phase 3c smoke test: with a gfxstream renderer attached the device advertises VIRGL and routes
/// the guest's 3D control stream into the renderer FFI boundary. Driven by a recording `MockRenderer`
/// so it needs no live guest, no host Vulkan driver, no GPU — it proves the parse + routing, per the task.
#[cfg(test)]
mod tests_3d {
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
        let resp = gpu.dispatch(&mem, &ctx);
        assert_eq!(read_le(&resp, 0, 4), VIRTIO_GPU_RESP_OK_NODATA as u64);

        // SUBMIT_3D ctx_id=1, size=8, payload = 8 sentinel bytes (the gfxstream command stream).
        let payload: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let mut sub = hdr3d(VIRTIO_GPU_CMD_SUBMIT_3D, 1);
        sub.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // size
        sub.extend_from_slice(&0u32.to_le_bytes()); // padding
        sub.extend_from_slice(&payload);
        let resp = gpu.dispatch(&mem, &sub);
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
        gpu.dispatch(&mem, &cr);

        // SET_SCANOUT scanout0 -> resource 10 (a 3D resource — sizes the scanout to 32x32).
        let mut scan = hdr3d(VIRTIO_GPU_CMD_SET_SCANOUT, 0);
        scan.extend_from_slice(&[0u8; 16]); // rect
        scan.extend_from_slice(&0u32.to_le_bytes()); // scanout_id
        scan.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        gpu.dispatch(&mem, &scan);

        // RESOURCE_FLUSH resource 10 → read_pixels back into the scanout + bump generation.
        let handle = gpu.handle();
        let before = handle.generation();
        let mut flush = hdr3d(VIRTIO_GPU_CMD_RESOURCE_FLUSH, 0);
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&10u32.to_le_bytes()); // resource_id
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(&mem, &flush);

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
        gpu.dispatch(mem, &scan);

        let mut flush = hdr3d(VIRTIO_GPU_CMD_RESOURCE_FLUSH, 0);
        flush.extend_from_slice(&[0u8; 16]); // rect
        flush.extend_from_slice(&rid.to_le_bytes());
        flush.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(mem, &flush);

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
        let (mock, log) = MockRenderer::with_present_rc(1);
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
        let (mock, log) = MockRenderer::with_present_rc(-4);
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
        gpu.dispatch(mem, &cr);
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
        let resp = gpu.dispatch(&mem, &att);
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
        gpu.dispatch(&mem, &att);

        let mut unref = hdr3d(VIRTIO_GPU_CMD_RESOURCE_UNREF, 0);
        unref.extend_from_slice(&10u32.to_le_bytes());
        unref.extend_from_slice(&0u32.to_le_bytes());
        let resp = gpu.dispatch(&mem, &unref);
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
        gpu.dispatch(&mem, &att);

        let mut det = hdr3d(VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING, 0);
        det.extend_from_slice(&11u32.to_le_bytes());
        det.extend_from_slice(&0u32.to_le_bytes());
        gpu.dispatch(&mem, &det);

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

        let resp = gpu.dispatch(&mem, &scan);
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
            let resp = gpu.dispatch(&mem, &c);
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
}
