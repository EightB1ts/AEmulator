# Vulkan compatibility: what landed, what is measured, what is still broken

Status document for the 21-commit run on `main` (`d918ef8..52db103`). Written to be useful to
whoever picks this up next, which means the open problems and the wrong turns are here too, not just
the wins.

Every number below was measured on this machine (Apple M2, macOS 27.0 build 26A5388g, Xcode 27) and
is reproducible with the commands given. Where something was *not* measured, it says so.

---

## 1. How this started, and the trap at the front door

The ask was "install the Vulkan test app into the running emulator". The app in the repo root,
`Vulkan+Android+Test_1.0.17_APKPure.xapk`, **cannot be installed** and never could:

* it ships only `config.armeabi_v7a` (`lib/armeabi-v7a/libminimal-example.so`); the base APK has no
  native libs at all
* the guest is `aosp_cf_arm64_only_phone` — `ro.product.cpu.abilist = arm64-v8a`, `ro.zygote =
  zygote64`, and per `testdata/cf64/SOURCE.txt` it deliberately ships **no 32-bit userspace**: empty
  `/system/lib`, no `app_process32`, no 32-bit linker
* Apple Silicon has no AArch32 EL0 either

Result: `INSTALL_FAILED_NO_MATCHING_ABIS (res=-113)`. This is a hard wall, not a setting.

**Resolved.** The genuine arm64-v8a build of the same app (`tech.incr.vulkanandroid` 1.0.17) was
obtained and it renders and animates in the guest. Upstream source (`solid-ux`, per the build paths
baked into the v7a `.so`) is not public, which turned out not to matter.

---

## 2. The three ceilings, and where each ended up

Guest-visible Vulkan is gated in three independent places. Understanding that they are separate is
the single most useful thing in this document.

| Layer | What caps it | Start | Now |
|---|---|---|---|
| Host driver | what Mesa KosmicKrisp exposes on Metal | 140 device extensions | 145 |
| Host transport | what gfxstream's cereal can **decode** | 168 extensions in the gap | see §3 |
| Guest ICD | a hardcoded allow-list in `ResourceTracker.cpp` | 104 names | 153 |
| **Guest-visible result** | all three intersected | **89** | **151** |

`cereal gap: hiding 0 of 145 host device extensions (0 kept by allowlist)` — nothing the host driver
advertises is withheld from the guest any more.

---

## 3. The cereal gap, and what "213 extensions" in the strip list now means

`goldfish_vk_extension_struct_size()` can *size* far more `VkStructureType`s than
`reservedunmarshal_extension_struct()` can *decode*, and the latter's `default:` arm is `abort()` of
the whole VMM. The difference is the "cereal gap".

The cause was never struct complexity. `cerealgenerator.py` carries a hand-maintained
`SUPPORTED_FEATURES` allow-list, and `UNSUPPORTED_FEATURE_MODULES = {"goldfish_vk_extension_structs"}`
routes every *unlisted* feature to the size-table wrapper only. Adding names and regenerating closes
it.

`VkDecoderCerealGap.inc` currently reports **213 extensions / 477 struct types**, which is *larger*
than the 168/329 we started with — and that is not a regression. Phase C bumped the vendored registry
from 1.3.269 to **1.4.348** (552 → 682 extension names), so there are simply more extensions in the
world for the list to describe. The number that matters is the intersection with what the host driver
actually advertises, and that is **zero**.

### The regeneration harness

`third_party/regen-cereal.sh` — modes `--reproduce`, `--pristine`, `--opcode-diff`,
`--record-baseline`, and bare (regenerate in place). Non-obvious facts baked into it:

* **The generator is a git blob in gfxstream's own history**, `TP_GFXSTREAM_CODEGEN_REF=838dd1d61` —
  the commit titled "gfxstream: rerun codegen from external/mesa3d", whose successor deleted the
  directory. It is **not** `third_party/mesa`'s copy: pinned mesa is ~20 months newer and emits
  `vk_decoder` / lowercase mesa-era includes that exist nowhere in this gfxstream.
* **The pin's generated files are generated-PLUS-hand-work.** Five upstream commits hand-edited them
  after the codegen was deleted (`875145234`, `4d94b0813`, `3e073ad03`, `699158abd`, `b32fb06f0`).
  No generator revision can reproduce them; chasing one through mesa's history is a dead end and was
  checked. That delta is recorded as `reproduce-baseline.diff` and **re-applied automatically** after
  every regeneration. Losing it deletes `OP_vkCmdSetLineStippleEXT` — a wire break against the
  prebuilt guest ICD. `--opcode-diff` caught exactly that during bring-up.
* **The banner cannot be reproduced** — `cerealgenerator.py` stamps each file with
  `os.path.relpath()` of its own argv, and the checkout's copy encodes an AOSP superproject path.
  Filtering it out is not enough (differing line counts shift everything after), so the checkout's
  banner is **grafted** into freshly generated files before comparing.
* Scratch lives at `third_party/.regen-cereal-scratch` — a **fixed** path, not `mktemp`, because a
  random temp path leaks into the banner and makes the baseline unreproducible.

### Extensions deliberately left undecodable

Four, each for a mechanical reason, all documented at the site:

| Extension | Why |
|---|---|
| `VK_KHR_push_descriptor` | `vkCmdPushDescriptorSetWithTemplateKHR` passes an opaque `const void*` described by a `VkDescriptorUpdateTemplate`; gfxstream routes templates through `DescriptorSetVirtualization`. Non-template path decodes; template path decodes-and-drops. |
| `VK_EXT_extended_dynamic_state3` | *(now decodable)* — `REQUIRED_TYPES` force-declares `VkViewportSwizzleNV` |
| `VK_EXT_multi_draw` | *(now decodable)* — stride became a first-class concept in the generator |
| `VK_EXT_mutable_descriptor_type` | *(now decodable)* — under its `VK_VALVE_` guard |

Only `push_descriptor` remains partial.

### The second hazard class the strip list cannot express

A strip list answers "what can the host size but not decode?". It is structurally blind to
extensions the codegen has **never heard of**, whose `sType`s are in *neither* switch and therefore
reach `goldfish_vk_extension_struct_size()`'s `default:` arm — `GFXSTREAM_ABORT`, one step earlier.

At the time this was found, KosmicKrisp advertised **26** extensions absent from the vendored
registry, **5** of them on the guest ICD's allow-list (`VK_KHR_maintenance6..9`,
`VK_KHR_vertex_attribute_divisor`) — live abort paths. Fixed by emitting a second table,
`kCerealRegistryExtensions`, and **whitelisting** against it before consulting the blacklist.
Membership is tested against the **registry**, not against guard names, because a promoted extension
(`VK_EXT_descriptor_indexing`, promoted to 1.2) files its structs under a `VK_VERSION_*` guard and
owns no guard of its own — whitelisting by guard would wrongly hide it.

---

## 4. Ray tracing: working, end to end, on hardware that has none

`VK_KHR_acceleration_structure` + `VK_KHR_ray_query` + `VK_KHR_deferred_host_operations` are
implemented in KosmicKrisp and reach the guest. Measured in the guest:

```
ok: ray 0 hit distance t = 1.000000
ok: ray 0 primitive/geometry/instance index = 0/0/0
ok: ray 0 instanceCustomIndex = 0x123456 (the TLAS instance record survived the round trip)
ok: ray 0 barycentrics = (0.250000, 0.250000) — analytically exact for this triangle
ok: rays 1..7 all missed (7 of 7)
VK_RT_PROBE: PASS
```

Host: `run-rq-smoke.sh` **16/16** in four modes (identity, `--transform`, `--triangle-transform`,
`--resubmit`). Control: `MESA_KK_RT_LEGACY_BUILDER=0` → **12/16**, failing exactly the four hit-path
checks. Single variable.

Visual proof: a `VK_KHR_ray_query` shadow app renders with cast and self shadows
(`screenshots/phasepv-*`, and `scratchpad/gmiss/rayquery-SHADOWS.png`).

### The finding that shaped all of it: the M2 has no hardware ray tracing

Apple M2 is GPU family **Apple8**. Hardware RT arrives with **Apple9**. Below that Metal *emulates*
RT with a software BVH (private `-requiresRaytracingEmulation` returns YES), and **the Metal 4
ray-tracing surface is not implemented against that emulated builder**.

Nothing public says so. `supportsRaytracing`, `supportsRaytracingFromRender`,
`supportsPrimitiveMotionBlur` and `supportsFamily:MTLGPUFamilyMetal4` are all YES, and
`system_profiler` prints "Metal Support: Metal 4". Only API validation admits it:

```
_validateRTDescriptorCompatibility: failed assertion
  'Acceleration Structure Descriptor Validation / this device does not support Metal 4 ray tracing.'
```

The failure was **silent and three-fold**:

1. An MTL4-built BLAS raises no error, no exception, no commit feedback — and intersects nothing.
2. `-[MTLDevice accelerationStructureSizesWithDescriptor:]` given an **MTL4** descriptor is a stub
   that never inspects the geometry: a constant 1280/5632/512 bytes for 1 triangle *and* for 10,000.
   The Metal 3 descriptor correctly returns 1760768/1942272/768 for 10k. So every BLAS was
   under-allocated.
3. An `MTL4InstanceAccelerationStructureDescriptor` cannot even be size-queried — uncaught
   `NSInvalidArgumentException`, `-motionKeyframeCount` unrecognized. kk avoided that abort only by
   accident, because its sizing path happened to use the Metal 3 class.

Crucially **Metal 4 traversal is fine** — only the build is unsupported. Descriptors, MSL codegen,
argument tables and `nir_to_msl`'s `intersection_query` emission were all innocent.

**Fix:** build through the legacy `MTLAccelerationStructureCommandEncoder` (patch 0012), with a
device-address → `id<MTLBuffer>` interval map (patch 0011) because legacy descriptors take
`id<MTLBuffer>` while kk carries only raw `VkDeviceAddress`.

The advertisement is gated on a runtime capability probe (patch 0010), written as *"can this device
build an intersectable acceleration structure"* rather than *"is this Apple9"*, so it flips true on
the same Apple8 once the legacy builder is present. `MESA_KK_FORCE_RAY_TRACING=1` overrides;
`MESA_KK_RT_DEBUG=1` prints the probe's inputs and verdict. The probe costs 0.0008 ms; a full
build+trace self-test was measured at 575 ms first-run / 3.8–6.3 ms warm and rejected as too
expensive for `vkEnumeratePhysicalDevices`.

### Three bugs between "host hits" and "guest renders shadows"

1. **The guest silently dropped every acceleration-structure descriptor write.**
   `DescriptorSetVirtualization.cpp:186` — a `// TODO` and a `mesa_logw`. It never set `entry.type`,
   so the slot stayed `Empty` and the flush loop skipped it. The host built a perfect BVH that
   nothing was ever pointed at. Evidence had been sitting in three boot logs:
   `Ignoring emulated write for descriptor type 0x3b9d13f0`
   (= 1000150000 = `VK_DESCRIPTOR_TYPE_ACCELERATION_STRUCTURE_KHR`), five times in the
   no-shadows run.
2. **A landmine underneath it:** `ResourceTracker.cpp:941` has
   `case DescriptorWriteType::AccelerationStructure: → abort()`, unreachable *only because of* bug 1.
   Fixing the drop site alone converts a silent miss into a guest-process abort.
3. **Every BVH sized for one triangle** — `kk_GetAccelerationStructureBuildSizesKHR` passed an MTL4
   descriptor to the pre-Metal-4 query, which ignores `triangleCount` (see above). 1 triangle hit,
   100 missed, 53583 failed to build. After the fix: 1536 / 18688 / 9431552, all hit.

### Also fixed on the way

* `deepcopy_VkDeviceOrHostAddress{,Const}KHR` dereferenced a **union** arm holding a *device* address
  as a CPU pointer — SIGSEGV in the guest ICD, reproduced by both the probe and a real app at the
  same instruction. It had been patched by hand *inside a generated file*; re-exporting 0007
  regenerated the file and erased it. Now fixed in the **generator**, with a guard test verified red
  by injecting the broken body.
* `deepcopy` recurses into every arm of a union (its signature is fixed by every `VkEncoder` call
  site, so it has no discriminant). For a geometry holding `triangles`, the `aabbs` arm's `data`
  overlaps `triangles::vertexFormat` — the integer 106 — which reached
  `dupArray(from->hostAddress, 1)`, a read of address `0x6a`.
* **Discriminated unions**: both generators now parse `vk.xml`'s own `selector`/`selection`
  attributes and emit a `switch` on the discriminant. Before, an AABB address
  `0x0000001508000000` decoded as `0xcdcdcdcd08000000` — the high dword never written.
* `kk_CreateQueryPool` initialised the per-query remap index only for occlusion and timestamp pools;
  every other type indexed its results through uninitialised heap memory.

---

## 5. OpenGL ES 2.0 → 3.1

The guest was stuck at ES 2.0 because of **one missing Vulkan extension**.

ANGLE's `getMaxSupportedESVersion()` has a hard gate — `VK_EXT_provoking_vertex` is required for flat
shading → `LimitVersionTo(2,0)`. Every other ES 3.0/3.1 condition was already satisfied. Not
cosmetic: SurfaceFlinger requests an `EGL_OPENGL_ES3_BIT` config first and only falls back.

| | before | after |
|---|---|---|
| `SurfaceFlinger: version` | `OpenGL ES 2.0 (ANGLE 2.1 …)` | **`OpenGL ES 3.1`** |
| EGLConfigs with `ES3_BIT` | **0 of 45** | **45 of 45** |
| `eglCreateContext(CLIENT_VERSION=3)` | `EGL_BAD_MATCH` | ok |
| `GL_SL_VERSION` | `GLSL ES 1.00` | **`GLSL ES 3.10`** |
| 3DMark tests offered | 3 of 13 | **13 of 13** |

Settled by **disassembling the guest's own ANGLE** (`libGLESv2_angle.so`, extracted from the EROFS
system image with `fsck.erofs`, disassembled with the NDK's `llvm-objdump`):

* `0x2b9748` — `movk w15, #0x3b9e` → `1000254001`
  (`VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_PROVOKING_VERTEX_STATE_CREATE_INFO_EXT`), then
  `mov w16, #0x1` = `LAST_VERTEX`, an **unconditional immediate** with no `csel`. ANGLE requests LAST
  on every pipeline.
* `0x325488` — `mFeatures.provokingVertex.enabled = (provokingVertexLast == VK_TRUE)`.
* An exhaustive constant scan found `VK_DYNAMIC_STATE_PROVOKING_VERTEX_MODE_EXT` **0 times**, so
  per-pipeline state suffices.

And **Metal has no provoking-vertex API at all** — 16 `respondsToSelector:` probes all no;
`grep -ril provoking` over the whole SDK hits only OpenGL headers. Metal is **first**-vertex,
verified on a triangle strip's second triangle (the only place first/last diverge). The fix is index
rotation, proven pixel-exact (`px where F != B+2: 0`) and winding-neutral at both cull orientations.

10 of 13 3DMark verdicts flipped. The two that did not are honest: Steel Nomad needs 8 GB (guest has
4), Opacity Micromaps is a real Vulkan feature check.

---

## 6. Other driver work

| Feature | State |
|---|---|
| `VK_FORMAT_D24_UNORM_S8_UINT` / `X8_D24_UNORM_PACK32` | **Implemented**, tier 1 + tier 2 |
| `VK_EXT_metal_objects` | **Implemented** (~160 lines of C) |
| Sparse binding | **Implemented** on Metal placement-sparse; only honestly-supported bools flipped |
| `geometryShader` | **Partial** — static output shapes render; restart-index unroll landed |

**On D24S8:** `PHASE-METAL-STATUS.md:110-112` claimed *"This is hardware, not a gap in the port"*.
The hardware fact is real — `isDepth24Stencil8PixelFormatSupported` is NO on M2 and the format
hard-aborts descriptor validation — but **the conclusion was wrong**. Vulkan never requires D24S8 to
be *stored* at 24 bits for `VK_IMAGE_TILING_OPTIMAL`; the layout is opaque and only the copy
interfaces constrain encoding. Promotion to `Depth32Float_Stencil8` is a legal substitution, which is
what MoltenVK ships on the same silicon. Tier 2 additionally converts the depth aspect f32↔unorm24
on copy via a compute kernel — a conformance fix **MoltenVK does not do**.

The one genuine semantic wall found anywhere in this effort: **depth bias**. Vulkan defines the
minimum-resolvable-difference `r` differently for fixed-point vs float depth, and
`-[MTLRenderCommandEncoder setDepthBias:slopeScale:clamp:]` has no fixed-point mode. Visual-fidelity
difference, not corruption, and not recoverable by emulation.

---

## 7. Infrastructure added

* `third_party/regen-cereal.sh` — the regeneration harness (§3)
* `third_party/downgrade-registry.py` — preprocessor letting the era codegen eat a modern `vk.xml`.
  Five fixups; the nastiest is the **core-version profile split** (landed between v1.4.328 and
  v1.4.335: `VK_BASE_`/`VK_COMPUTE_`/`VK_GRAPHICS_` → `VK_VERSION_1_x` chained by feature-level
  `depends`), which the era `reg.py` ignores — leaving `VK_VERSION_1_0` with **zero commands** and no
  core Vulkan generated at all.
* `crates/aemulator-hvf/prep-vulkan-icd.sh` — swaps `vulkan.ranchu.so` inside the guest image.
  Install / `--verify` / `--revert`, byte-exact.
* `crates/aemulator-hvf/prep-adb-keys.sh` — provisions adb keys into the image.
* Smoke harnesses: `run-rq-smoke.sh`, `run-gs-smoke.sh`, `run-pv-smoke.sh`, `run-sparse-smoke.sh`,
  plus pure-Metal probes (`mtl4-rt-matrix.m`, `mtl-rt-matrix-layout.m`, `mtl4-sparse-matrix.m`).
* `ci/hvf-doctor.sh` — new cereal-gap ∩ host-extension intersection check.

### Guest-image surgery, for the record

The ICD lives in `/vendor/apex/com.google.cf.vulkan.apex`, a real ZIP whose `apex_payload.img` is
STORED at offset 4096; the `.apex` is EROFS **FLAT_PLAIN** inside `vendor_a`, so it is rewritable in
place. Four integrity structures, **none** of which bite: partition dm-verity is off (vbmeta
VERIFICATION_DISABLED); APEX payload verity is not used for pre-installed APEXes (all 85 mount on
bare loop devices, no dm-verity target anywhere in the boot); the container APK signature is not
enforced on this path; the ZIP CRC-32 is unchecked but recomputed anyway.

**Shadowing at `/vendor/lib64/hw/` cannot work** — confirmed in the boot log:
`Loading builtin Vulkan driver from APEX: ro.vulkan.apex=com.google.cf.vulkan`.

---

## 8. Current verified state

`main` at `52db103`, 21 commits ahead of `d918ef8`, tree clean. **No git remote is configured**, so
nothing is pushed.

```
bootstrap --verify        VERIFIED (14 mesa patches + 5 codegen patches + gfxstream digest)
bootstrap mesa            READY
bootstrap gfxstream       dylib built and patch-verified
run-gate.sh               GATE PASSED 3/3
run-rq-smoke.sh           16/16
run-gs-smoke.sh           37/37
run-pv-smoke.sh           28/28
run-sparse-smoke.sh       32/35   (the 3 are documented-expected; see sparse-smoke.c:743-749)
ci/hvf-doctor.sh          EMULATOR DEPENDENCY CHAIN OK
ci/check.sh               ALL CHECKS PASSED
cargo test --workspace    245 passed, 0 failed
```

Live boot: `Boot is finished (14964 ms)`, `OpenGL ES 3.1`,
`cereal gap: hiding 0 of 145`, **151 guest device extensions**, and **0** each of
`Internal Vulkan error` / `Unhandled Vulkan structure type` / `Fatal signal` / `on_DeviceLost`.

### Seeing it yourself

The window needs a GUI session — a detached/`nohup` launch renders fine but can never create an
`NSWindow` (AppKit requires the window and event loop on the process main thread, and a background
process has no window-server session). Run it from your own terminal:

```
cd /Users/cam/AEmulator && bash crates/aemulator-hvf/run-android64.sh --gpu3d --display
```

That script signs the binary itself, so the entitlement problem in §9 cannot bite there.

---

## 9. Open items, known problems, and dead ends

### Open: no 3DMark score

Both graphical tests start and neither finishes.

**Solar Bay hangs at LOADING**, deterministically (2 of 2 runs). Guest parked in
`AddressSpaceStream::speculativeRead`; all host ASG threads in `onUnavailableRead`. On one of those
runs it also killed the Metal device (`on_DeviceLost, err code: -4` → SIGABRT).

**Sling Shot Extreme renders real frames then spins** at ~100% of one vCPU inside its own lock-free
queue in `lib3dmark_arkan.so`, with **no** GL/EGL/`vulkan.ranchu` frame on the stack. Frames were
demonstrably drawn (frame1 vs frame5 differ by 74.95% of pixels). So it is not blocked on graphics.

Neither is a regression — both were unlaunchable before this work.

### What is known about the ASG stall, and what has been eliminated

Established:

* The host is **not** parked on a missed doorbell. `RingStream.cpp:190-222` — `read()` spins to
  `maxSpins` with `ring_buffer_yield()`, calls `onUnavailableRead()`, and **continues the loop**. It
  is actively polling an empty ring.
* There are **three** rings per context (`asg_ring_storage`: `to_host`, `to_host_large_xfer`,
  `from_host_large_xfer`, 4096 bytes each) and the host only reaches `onUnavailableRead` when
  **both** `ringAvailable` and `ringLargeXferAvailable` are zero. `RING_BUFFER_SIZE` is **2048
  bytes**, so anything larger must use the large-transfer path (`transfer_mode == 3`) — which is
  exactly what a bulk scene upload does, and exactly what no lighter workload touches.
* The virtio-gpu fence bridge is **not** the cause. `gpu3d.rs::on_fence_complete` reads its argument,
  does an unbounded non-blocking send, and takes one lock contended only at install.
  `machine.rs::spawn_gpu_fence_thread` uses a **bounded 250 ms** `recv_timeout` specifically so a
  renderer that stops signalling still gets the watchdog sweep, sets the ISR **before** raising the
  SPI (Linux's `vp_interrupt` returns `IRQ_NONE` on a zero ISR), never nests the pci and gic locks,
  and pokes the guest even on a completion matching nothing pending.
* **Ring geometry is provably correct** — this refutes the leading theory, which was mine.
  `ring_buffer_view_init` silently shrinks a non-power-of-two size to the next lower power of two,
  which would give guest and host different masks over the same memory. But both sides split at
  `sizeof(asg_ring_storage)` (guest: `VirtioGpuAddressSpaceStream.cpp:130`; host:
  `address_space_graphics.cpp:284`) and both pass **1048576 = 2^20**
  (`VirtioGpuFrontend.cpp:583`, `perContextBufferSize()`). Clean power of two, same offset, same
  mask. Not the bug.

**Next step, and it is not more static reasoning — that is exhausted.** One instrumented boot
dumping, for every ring and from both sides, the live head/tail and `ring_buffer_available_read`
result at the moment of stall. If the guest's write position has advanced and the host reads the old
value for the *same* ring, it is a visibility problem; if the two are looking at different rings, it
is routing.

Second question for the same run: **is the device loss the cause of the hang or a separate bug?** The
device-loss logging landed (§below), so a fresh run should finally name the failing Metal command
buffer.

### Fixed this session, but worth knowing

**`hvf-doctor` signed the wrong binary.** `BIN` was hardcoded to `target/release`, so the entitlement
step checked and signed release **only** — while `run-android64.sh` and the control harness launch
**debug**. The doctor could print `✓ signed with com.apple.security.hypervisor` about a file you were
not going to run, and the next boot would die on
`hv_vm_create -> HV_DENIED (0xfae94007)` with a green doctor. It now iterates both profiles and names
the offending one. If a rebuild ever refuses to boot, that is the first thing to check —
`cargo build` relinks and **discards** the ad-hoc signature; only `pnpm hvf:build` chains the signing.

**Device losses used to be undebuggable.** `vk_device_set_lost`'s message does not survive a release
build: `__vk_log_impl` early-returns unless `enable_debug_logging` or a debug-utils callback exists,
then gates on `MESA_VK_LOG || enable_debug_logging`. `MESA_VK_LOG` comes from meson's `debug` option
— **0** in our `buildtype=release` driver; kosmickrisp never sets `enable_debug_logging` (only the
Intel drivers do); gfxstream registers no messenger. The string was formatted and dropped every time.
`kk_queue` now logs `MTLCommandBuffer.error` with `mesa_loge` first and unconditionally.

### Not started

* **`ray_tracing_pipeline`** (`vkCmdTraceRaysKHR` + SBTs). Metal has no direct SBT/`traceRayEXT`
  equivalent; intersection function tables are the closest. Estimated 2–4 months on top of ray query.
  MoltenVK PR #2771 and SPIRV-Cross PR #2649 are CTS-validated reference implementations — their
  *code* cannot transfer (SPIRV-Cross is SPIR-V→MSL; kk is NIR→MSL, zero SPIRV-Cross references) but
  their **MSL emission patterns are the specification**.
* **Full sparse residency** — only `sparseBinding`-class capability is honest today.
* **GLES 3.2** — needs geometry + tessellation as real features. Tessellation is already true;
  geometry is partial.
* **Registry beyond 1.4.348** — 348 was chosen as the *oldest* tag covering all 26 previously-missing
  host extensions.

### Genuinely out of reach

* **`VK_NV_ray_tracing_invocation_reorder`** — ray reorder is unsupported by Metal hardware.
* **Motion blur** — needs a separate intersector path.
* **Exact depth-bias semantics** — §6.
* `shaderFloat64` and a native geometry stage are absent from Apple GPU hardware. Both are
  *emulatable* in principle (soft-float, compute pre-pass) but at costs that make advertising them
  questionable. Note this is a **driver/perf** judgement, not a physics one — an earlier version of
  this analysis wrongly filed several driver gaps under "hardware limits", and that was corrected.

---

## 10. Process notes that saved real time

* **Run a positive control before believing a negative.** Three times this session a query returned
  empty and looked like a missing capability; twice the real cause was a `zsh` quirk (an unquoted
  variable holding `cmd + args` is **not** word-split, so `$ADB shell …` silently fails). Once it was
  swallowed pipeline output over the serial console. Every "X is absent" claim needs a control that
  proves the measurement works at all.
* **The serial console is not a measurement instrument.** It echoes commands, wraps at the terminal
  width (mangling `$(...)`), and frequently swallows pipeline output while letting `echo` through.
  Use adb.
* **Do not `git apply` the codegen patches.** The scratch dir lives under `third_party/`, inside this
  repo's worktree, so `git apply` resolves paths against the **repo root**, matches nothing, changes
  nothing, and **exits 0** — a silent no-op behind a success log. Use `patch` and assert the tree
  checksum moved. `regen-cereal.sh` does this correctly.
* **Patch discipline is load-bearing.** Every patch must apply to the bare pin **and**
  reverse-apply. A patch may not edit a line another patch added — re-export the owning patch
  instead. This bit repeatedly (0007, 0012). Verify by reconstruction: apply the series to a bare pin
  and byte-compare against the live tree.
* **Uncommitted work in `third_party/` is one `bootstrap.sh` away from deletion.** An agent died
  mid-run leaving the device-loss logging only in the mesa working tree; the next `bootstrap.sh mesa`
  would have erased it. Export to a patch as soon as it works.
* **Verify the composition, not just the parts.** Four changes each individually green produced a
  guest SIGSEGV when combined, because re-exporting one patch regenerated a file and erased a
  hand-edit inside it.
