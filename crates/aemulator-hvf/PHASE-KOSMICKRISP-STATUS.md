# Phase KosmicKrisp — the host Vulkan driver is now Mesa/LunarG **KosmicKrisp** and MoltenVK is gone from the process: the `MTLTexture` zero-copy present was rebuilt as an **in-Vulkan swapchain present** (`vkCreateMetalSurfaceEXT` + `VK_KHR_swapchain` + `vkCmdBlitImage`), gfxstream's portability machinery is deleted, and Android boots and renders on the new driver

**Result.** gfxstream's host Vulkan driver was swapped from MoltenVK to KosmicKrisp
(`driverName = KosmicKrisp`, `driverInfo = Mesa 26.3.0-devel (git-4f4b73dfe5)`, a Khronos-conformant
Vulkan 1.3 Metal driver upstreamed in Mesa; this build advertises `apiVersion = 1.4.354` and
`conformanceVersion 1.4.3.2`). **Proven:** the new present path renders a real Android
app on *both* drivers; Android boots to the lock screen on KosmicKrisp with zero gfxstream aborts and
zero present-path fallbacks; MoltenVK is still installed on this machine but **no MoltenVK image is
loaded into the running VMM**; the 2D/CPU present path still paints when no 3D frame exists. The
guest-visible capability diff (`cmd gpu vkjson`: **87** extensions vs the MoltenVK baseline's 85) and
the **3DMark** workload have both since been measured on KosmicKrisp and folded in below, as has a
**torn-readback fix** for partial black flicker that this migration introduced and that only shows up
under animation.

**Not proven here:** the Phase 3 gfxstream C++ edits have no automated guard (they live in an external
checkout); the Phase 3 boot gate reached the lock screen rather than the home screen (input timing,
not rendering — not root-caused). Both are detailed in *Known, still open*.

macOS 26+ on Apple Silicon is now a **hard requirement**. There is no second driver to fall back to.

---

## Why the present path had to be **rebuilt**, not ported — the one load-bearing fact

The MoltenVK present was zero-copy by way of `VK_EXT_metal_objects`: gfxstream called
`vkExportMetalObjectsEXT` on the scanout ColorBuffer's `VkImage`, got an `id<MTLTexture>` back, and
the VMM blitted that texture into the window's `CAMetalLayer` (documented in
`PHASE-METAL-STATUS.md`). **That construction cannot exist on KosmicKrisp**, for two independent
reasons:

1. **KosmicKrisp does not implement `VK_EXT_metal_objects` at all.** It is absent from the 140
   device extensions (verified: `vulkaninfo` grep count 0), so `vkExportMetalObjectsEXT` has no
   entry point to load.
2. **Its Metal-handle export supports only `MTLHeap`.** `VK_EXT_external_memory_metal` is present,
   but in
   `SampleRepos/mesa-kosmickrisp/mesa/src/kosmickrisp/vulkan/kk_device_memory.c:265`,
   `kk_GetMemoryMetalHandleEXT` opens with:

   ```c
   /* We only support heaps since that's the backing for all our memory and
    * simplifies implementation */
   assert(pGetMetalHandleInfo->handleType ==
          VK_EXTERNAL_MEMORY_HANDLE_TYPE_MTLHEAP_BIT_EXT);
   ```

   and returns `mem->bo->mtl_handle` — the heap. There is no code path that hands out a standalone
   `id<MTLTexture>` for an image, and none can be added from our side: in this driver *all* device
   memory is heap-backed by design, which is the stated reason for the restriction.

So the export-a-texture design is not "slow on KosmicKrisp" or "missing an extension we could
enable" — it is structurally unavailable. The frame has to leave the renderer as a **Vulkan
present**, inside the renderer's own `VkDevice`, or not at all.

**What replaced it (Phase 1).** The VMM hands gfxstream the `CAMetalLayer` pointer and stops
touching it; gfxstream owns presentation end to end:

```
Gpu::flush_3d → Renderer3d::present_to_layer
  → stream_renderer_present_to_metal_layer(colorBufferHandle, caMetalLayer)
    → gfxstream::vk::presentColorBufferToMetalLayer   (host/vulkan/MetalLayerPresenterVk.cpp)
      → vkCreateMetalSurfaceEXT (VK_EXT_metal_surface, cached per layer)
      → VK_KHR_swapchain  →  vkAcquireNextImageKHR
      → vkCmdBlitImage (ColorBuffer → swapchain image)
      → vkQueuePresentKHR
```

Still no CPU readback and still no GPU→CPU→GPU roundtrip; the copy is a `vkCmdBlitImage` inside the
renderer's device instead of a Metal blit inside ours. The presenter reports a typed status
(`MetalLayerPresenterVk.h`: `kMetalPresentOk` … `kMetalPresentUnblittable`, mirrored verbatim by the
C entry point and by `Renderer3d::present_to_layer`), so a failure is attributable to a stage rather
than appearing as a black window.

`src/display/metal.rs` carries the ownership rule that makes this safe, in the doc comment on
`layer_for_window`: *"Once the pointer is published, gfxstream owns presentation into this layer …
and this process must never acquire a drawable from it again."*

---

## Phase 1 — in-Vulkan present, gated **on MoltenVK**

The present rewrite was gated *before* the driver swap, deliberately: proving the new path on the
old driver separates "the present path works" from "KosmicKrisp works", so a later failure has one
suspect instead of two.

### Proof (observed in `/tmp/aemu-display-gate.log`, `--gpu3d --display`, MoltenVK)

```
I0727 23:53:18.536777 d79b000 VkCommonOperations.cpp:1250] Selecting Vulkan device: Apple M2, Version: 1.1.357
[display] CAMetalLayer 0x79158d95c0 published for in-Vulkan present (2048x1536 px)
[gpu3d] in-Vulkan present ACTIVE: scanout ColorBuffer res=15 1024x768 -> CAMetalLayer 0x79158d95c0 (vkCmdBlitImage + vkQueuePresentKHR, no CPU readback)
```

- **`Version: 1.1.357` is the driver fingerprint.** MoltenVK reports 1.1.357; KosmicKrisp reports
  1.4.354. The version alone tells you which driver a log came from — no other marker needed.
- Negative present statuses on the active path: **0** (`present_to_layer rc` failures: 0).
- Real app rendered through the path: the Android **Files** app (`DocumentsUI` present in the run),
  correct colors — screenshot `screenshots/phase1-swapchain-present-moltenvk.png`.
- The CPU scanout stays empty by design on this path (`[display] scanout gen=N
  non_zero_fb_bytes=0`, every generation of that run). That is the intended consequence of never reading the
  frame back — and it is also the root of the open capture gap recorded below.

---

## Phase 3 — gfxstream stops being a portability client, and the driver is swapped

Five changes in `SampleRepos/platform_hardware_google_gfxstream/host/vulkan/`:

| Change | Why |
|---|---|
| Dropped `VK_KHR_portability_subset`, `VK_KHR_portability_enumeration` and `VK_MVK_macos_surface` from the required sets | KosmicKrisp is a *conformant* driver, not a portability driver: it exposes none of the three (verified, grep count 0 each). Requiring them makes `vkCreateInstance`/`vkCreateDevice` fail on the only driver we ship. |
| Deleted the `MVK_CONFIG_*` block | MoltenVK-private tuning knobs, meaningless to any other driver. Includes `MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE=0` — see the single-queue note below for why removing it is structurally safe rather than merely untested. |
| `instanceSupportsMoltenVK` → `useMetalExternalMemory`, and it is now a **capability probe** | It used to be an ICD-name test. It now asks the *selected physical device* whether it supports `VK_EXT_EXTERNAL_MEMORY_METAL_EXTENSION_NAME` (`VkCommonOperations.cpp:1193`), so it is true for any conformant Metal driver and false — with an explicit abort message naming KosmicKrisp — for anything else. The comment states the ordering constraint that makes it correct: it must be assigned before the `getImageFormatExternalMemorySupportInfo()` loop that reads it. |
| **Latent bug fixed:** the `vkCreateDevice` result was discarded | The `if` after `vkCreateDevice` re-tested a stale `res` left over from `vkCreateInstance`, so a failed device creation was reported as success and the failure surfaced later as something else entirely. Pre-existing; found while reading the path, not caused by this migration. |
| The one-off `VK_EXT_memory_budget` filter became a 9-entry strip list | See *The strip list* below — the reasoning is the part worth keeping. |

### Proof (observed in `/tmp/aemu-kk-gate.log`, `--gpu3d --display`, KosmicKrisp)

```
I0728 00:52:19.957780 b5df000 VkCommonOperations.cpp:1220] Selecting Vulkan device: Apple M2, Version: 1.4.354
[gpu3d] in-Vulkan present ACTIVE: scanout ColorBuffer res=15 1024x768 -> CAMetalLayer 0x799c488b70 (vkCmdBlitImage + vkQueuePresentKHR, no CPU readback)
[logcat] I ANGLE   : Renderer (Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x00000064)))
```

- **`Version: 1.4.354`** — KosmicKrisp is the live driver, not MoltenVK.
- Android boots and renders to the **lock screen**; screenshot
  `screenshots/phase3-android-on-kosmickrisp.png`.
- Guest renderer is the real host GPU path: `Virtio-GPU GFXStream (Apple M2)`. **SwiftShader string
  count in the whole run: 0.**
- gfxstream aborts: **0**. The 13 `Runtime aborting` lines are all
  `F droid.bluetooth: runtime.cc:761] Runtime aborting...` — the pre-existing Bluetooth-HAL
  crash-loop already documented in `PHASE-METAL-STATUS.md`, unrelated to graphics.
- Present-path fallbacks: **0** (the 6 `falling back` lines are guest-side noise about ringer
  vibration, `LocalImageResolver` drawable decoding and screen-brightness nits).

**The single-queue point, measured rather than assumed.** KosmicKrisp exposes exactly one queue
family with `queueCount = 1` (`vulkaninfo`: `queueProperties[0] … queueCount = 1`, flags
`GRAPHICS | COMPUTE | TRANSFER`, `present support = true`). MoltenVK's
`MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE=0` mode existed to make single-queue implicit
synchronization work; with a genuinely single-queue driver there is no cross-queue ordering for that
knob to have been protecting. `present support = true` on that same family is also what makes
`vkQueuePresentKHR` from the emulation queue legal, which is why the presenter has a
`kMetalPresentNoPresentQueue` status but never returns it here.

---

## Phase 4 — MoltenVK deleted from the repo, not just from the code path

Purged from the six `run-*.sh` launchers, ICD staging, the Studio preflight, `build.rs` and the UI
strings. Every launcher now exports `VK_DRIVER_FILES` to the committed KosmicKrisp manifest and sets
`ANDROID_EMU_VK_ICD=kosmickrisp`.

One real bug fixed in the same pass: a **swallowed `fs::copy` error** in ICD staging let a missing
ICD reach the renderer, where it presented as a **SEGFAULT** rather than "the driver is not built".
The failure is now reported at the copy.

The word *MoltenVK* still appears in this tree **on purpose**, in prose explaining what was removed
and why. That is the honest state to leave behind; deleting the explanations would only make the next
reader rediscover the `MTLHEAP` constraint the hard way.

---

## Hard-cutover proof — installed is not the same as loaded

MoltenVK is **still installed on this machine**: `brew list --versions molten-vk` → `molten-vk
1.4.2`, and `/opt/homebrew/opt/molten-vk/lib/libMoltenVK.dylib` exists. That is what makes the
following meaningful rather than tautological:

- `vmmap` of the **running** VMM shows **zero MoltenVK images loaded**, and
  `libvulkan_kosmickrisp.dylib` present.
- `Selecting Vulkan device: Apple M2, Version: 1.4.354` — the version fingerprint agrees.

Both halves are needed. The version line alone could be a stale build; the absent-image check alone
could be a machine where MoltenVK simply is not installed. Together they say: MoltenVK is available
and is not being used.

---

## Measured capability delta (host driver, `vulkaninfo`, this build)

`Apple M2`, `driverID = DRIVER_ID_MESA_KOSMICKRISP`, `apiVersion = 1.4.354`,
`driverVersion = 26.2.99`, `conformanceVersion 1.4.3.2`, **140 device extensions** (MoltenVK: 130).
One memory heap, one queue family.

**Absent on KosmicKrisp, present on MoltenVK** (each verified by grep count 0):

| Extension | Consequence |
|---|---|
| `VK_EXT_metal_objects` | The reason the present path was rebuilt. See above. |
| `VK_KHR_portability_subset`, `VK_MVK_macos_surface` | Removed from gfxstream's required sets in Phase 3. Not a loss — their absence *is* conformance. |
| `VK_KHR_external_memory_fd` (absent on MoltenVK too — listed only because gfxstream used to require it on the non-Metal path) | Never used on macOS; gfxstream's fd paths are `#if defined(__QNX__)` / Linux-only. |
| `VK_KHR_deferred_host_operations` | Only consumed by ray-tracing pipeline builds, which do not exist here anyway. |
| `VK_EXT_fragment_shader_interlock`, `VK_KHR_fragment_shader_barycentric` | Real capability losses. Nothing in this harness has been observed to want them; that is an absence of evidence, not a measurement. |
| `VK_KHR_incremental_present` | Damage-region presents unavailable; our present is a full-surface blit, so no current cost. |
| `VK_GOOGLE_display_timing` | **Worth watching.** Android sometimes uses it for frame pacing. Not observed to be requested in these runs, but this is the one loss with a plausible path to a user-visible symptom (jitter, not a failure). |

**The ceiling that is hardware, not a porting gap.** `geometryShader = false` on KosmicKrisp
(`vulkaninfo`, verbatim) — Apple GPUs have no geometry-shader stage. GLES 3.2 requires geometry +
tessellation, so `androidboot.opengles.version=196609` (3.1) remains the honest ceiling, exactly as
it was under MoltenVK. Likewise **no Vulkan ray tracing**: `ray_tracing` / `ray_query` /
`acceleration_structure` grep count in the KosmicKrisp extension list is **0**. Metal 3 has ray
tracing, but no Vulkan-on-Metal driver here exposes the Vulkan RT extensions and gfxstream does not
forward them. Switching drivers does not move either ceiling, and no future driver work on our side
will move the first one at all.

---

## The strip list — why it is **9** extensions and not **19**

The method (`host/vulkan/VkDecoderGlobalState.cpp`, `kGuestHiddenDeviceExtensions` and the comment
above it): this build's gfxstream cereal codegen knows **523** `VkStructureType`s in
`goldfish_vk_extension_struct_size_with_stream_features` but unmarshals only **194** in
`reservedunmarshal_extension_struct`. Every type in that **329-type gap** is sized as "known" and
then falls to `reservedunmarshal`'s `default:` arm → `abort()` → the whole VMM dies, if a guest ever
chains it.

**The method was validated before it was trusted.** `VK_EXT_memory_budget` was already known to
abort the VMM from empirical evidence (`PHASE-METAL-STATUS.md`), and the gap analysis independently
predicts exactly that. A predictor that reproduces a known answer is worth extending; one that does
not is worth discarding.

Applied to what the host advertises, the analysis matched **19** extensions. **9** are stripped:

```
VK_EXT_memory_budget                        (empirically confirmed aborter)
VK_EXT_attachment_feedback_loop_dynamic_state
VK_EXT_attachment_feedback_loop_layout
VK_EXT_blend_operation_advanced
VK_EXT_conditional_rendering
VK_EXT_multi_draw
VK_EXT_mutable_descriptor_type
VK_EXT_nested_command_buffer
VK_KHR_workgroup_memory_explicit_layout
```

The other **10** matches — `buffer_device_address`, `depth_clip_control`, `image_2d_view_of_3d`,
`pipeline_robustness`, `sample_locations`, `shader_atomic_float`, `present_id`, `present_wait`,
`push_descriptor`, `shader_subgroup_uniform_control_flow` — are **deliberately not stripped**, and
the reasoning is the valuable part:

- The 8 newly stripped ones are extensions **KosmicKrisp newly exposes** (of the 23 it adds over
  MoltenVK) whose feature/properties struct lands in the gap. Nothing has ever enabled them here, so
  they are pure new abort surface with zero demonstrated value.
- The 10 left alone are extensions **MoltenVK already advertised and that demonstrably did not
  abort** across every boot and 3DMark run in `PHASE-METAL-STATUS.md`. Advertisement is harmless —
  only *enabling plus chaining* reaches the decoder. Stripping them would delete real, working guest
  capability (`buffer_device_address` and `push_descriptor` are not obscure) to pre-empt a failure
  the evidence says does not occur.

So the boundary is not "everything the analysis matched", it is "everything the analysis matched that
we have no counter-evidence for". The filter is also deliberately **not** `#if defined(__APPLE__)`:
the codegen gap is a property of this gfxstream build, not of macOS.

The proper fix is regenerating the host cereal from a complete registry so the two tables agree.
Stripping is a containment measure and is labelled as one in the source.

---

## The Phase 1 regression adversarial review caught

Shipping Phase 1 as first written would have blanked the window for a large class of runs. Two
compounding defects, both in the VMM's display thread:

1. **The softbuffer CPU blit was gated on a `CAMetalLayer` *existing*, not on a frame having been
   *presented*.** The layer is created eagerly at window open. So for every run that never produces
   a 3D frame — 2D-only guests, renderer-init failure, the whole boot-console phase before the first
   gfxstream frame, and every `rc < 0` from the presenter — the CPU path was suppressed while the
   Vulkan path had nothing to show. Black window. Worse, it made the presenter's own fallback branch
   **dead code**: the branch existed, was correct, and could never run.
2. **Layer creation *replaced* `view.layer`**, orphaning the softbuffer's sublayer — so even
   re-enabling the CPU blit would have drawn into a layer that was no longer in the tree.

**Fix.** The Metal layer is a **hidden sublayer**, and a separate `presented` flag — set only when an
in-Vulkan frame has actually reached the layer — drives both the sublayer's visibility and the
softbuffer blit (`src/display/mod.rs`: `scanout.presented` at :237, `metal::set_layer_visible` at
:239, and `presented` cleared on teardown at :420 with the comment *"it means 'an in-Vulkan frame is
on the layer'"*). One predicate, two consumers, so they cannot disagree.

### Proof (observed in `/tmp/aemu-2dlinux.log`, Alpine 2D console, `--display`)

The layer is published **and** the CPU framebuffer is painting in the same run — which is precisely
the state the bug made impossible:

```
[display] CAMetalLayer 0x7946ea1110 published for in-Vulkan present (2048x1536 px)
[display] scanout gen=96 non_zero_fb_bytes=127650
```

Screenshot: `screenshots/phase1-2d-fallback-not-black.png`. Contrast with the 3D run, where
`non_zero_fb_bytes=0` for all 554 generations — the two paths are now distinguished by whether a
frame was presented, not by whether a layer exists.

---

## Building the driver — three constraints, each of which fails `meson setup` if you get it wrong

Encoded in `kosmickrisp/build-kosmickrisp.sh`, which is the executable version of this section:

1. **libclc is mandatory.** Mesa's `meson.build` lists `with_kosmickrisp_vk` in
   `with_driver_using_cl`, so selecting this driver turns on `with_clc`, which hard-requires
   `dependency('libclc')`. It is not optional and the error does not mention KosmicKrisp.
2. **The venv must be *first* on `PATH`.** Mesa walks candidate interpreters by name
   (`python3.14`, `python3.13`, … `python3`) and silently skips any that cannot import
   `mako`/`packaging`/`yaml`. Homebrew's pythons are found but bare, so without the venv first Mesa
   walks past every candidate and then errors **"Python >= 3.10 not found"** — *while listing the
   pythons it just rejected*. The message actively misleads.
3. **Every compiler must be pinned to Apple clang** (`CC`/`CXX`/`OBJC`/`OBJCXX=/usr/bin/clang…`).
   brew LLVM has to be on `PATH` for `llvm-config`, and that also makes meson pick LLVM's clang for
   **Objective-C** while C/C++ stay on Apple clang. Mixing toolchains against the macOS SDK's ObjC
   runtime and framework headers is not a risk worth taking for a driver we cannot fall back from.

Also required: Mesa **`main`**, not the 26.1 release branch — `KK_WORKAROUND_15` (2026-07-14) fixes
an MSL miscompile that only manifests on macOS 27, which is what this machine runs.

---

## Guards added this phase

Two, chosen to fail in the two different ways this migration could silently regress. Both were
**negatively validated**: each was confirmed to fail when the regression it targets is injected, and
the file then restored byte-identically.

**`ci/hvf-doctor.sh`** — new step 2, adjacent to the gfxstream renderer check. It stats the canonical
ICD manifest, extracts `library_path` from the JSON, resolves it against the manifest directory if
relative, and stats the dylib — because **the loader treats an ICD whose library is missing as *no
ICD*, silently**. It then reports `sw_vers -productVersion` and hard-fails below major 26. It
deliberately does **not** honour `$VK_DRIVER_FILES` (unlike `run-gate.sh`): the point is proving the
committed location every launcher exports `VK_DRIVER_FILES` *to* is actually populated.

```
==> KosmicKrisp Vulkan driver (the only host Vulkan driver — no MoltenVK fallback)
  ✓ ICD manifest /Users/cam/AEmulator/SampleRepos/mesa-kosmickrisp/install/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json
  ✓ driver /Users/cam/AEmulator/SampleRepos/mesa-kosmickrisp/install/lib/libvulkan_kosmickrisp.dylib
  ✓ macOS 27.0 (macOS 26 or newer is REQUIRED — KosmicKrisp is the only driver)
```

**`src/gfxstream/driver_pin.rs`** (new, `#[cfg(test)]` only — nothing added to the built crate) —
five tests, all pure text reads rooted at `env!("CARGO_MANIFEST_DIR")`, so no GPU, no boot, no
loader, no dependency on the driver having been built:
`every_gfxstream_launcher_selects_the_kosmickrisp_icd` (all 6 run scripts + `spawn.rs` must name the
manifest directory and basename *and* contain an exact `ANDROID_EMU_VK_ICD` assignment, not a loose
substring the explanatory comments would satisfy by accident),
`no_launch_path_can_reach_moltenvk`, `the_capability_gate_drives_the_same_manifest_as_the_emulator`
(otherwise "the gate passed" stops being evidence about the driver the emulator loads),
`every_launch_path_names_the_driver_build_script_as_remediation`, and
`hvf_doctor_still_checks_the_vulkan_driver`.

`no_launch_path_can_reach_moltenvk` forbids the *operative* forms — `moltenvk_icd.json`,
`libmoltenvk`, `mvk_config_`, `android_emu_vk_icd=moltenvk`, `"moltenvk"` — and **deliberately
allows** the bare word `moltenvk` and `VK_EXT_metal_objects` in prose. Forbidding those would only
teach people to delete the explanation of why the driver changed.

**What these tests do not prove, stated in their own module docs:** they prove the repo *asks* for
KosmicKrisp, not that KosmicKrisp *answers*. `hvf-doctor.sh` and `run-gate.sh` cover that half.
Neither substitutes for the other. And the Phase 3 gfxstream C++ edits are pinned by **nothing** —
they live in `SampleRepos/platform_hardware_google_gfxstream`, an external checkout a hermetic test
of this crate must not require to be present.

`cargo test --lib`: **61 passed**, 0 failed.

---

## Guest-visible capability delta — measured on KosmicKrisp (`cmd gpu vkjson`)

The host-side delta above is `vulkaninfo`. This is what **ANGLE and Android actually see** through
gfxstream, which is the number that matters for app compatibility. Measured on the KosmicKrisp boot
(`/tmp/aemu-p5.log`, `sys.boot_completed=1`):

| | MoltenVK baseline | KosmicKrisp |
|---|---|---|
| Guest device | `Virtio-GPU GFXStream (Apple M2)` **`(0x1B000208)`** | `Virtio-GPU GFXStream (Apple M2)` **`(0x00000064)`** |
| Guest Vulkan | 1.3.0 | 1.3.0 |
| Guest device extensions | 85 | **87** (net **+2**) |
| `VK_ANDROID_external_memory_android_hardware_buffer` | ✅ | ✅ |
| `VK_KHR_sampler_ycbcr_conversion`, `VK_EXT_descriptor_indexing` | ✅ | ✅ |
| `tessellationShader`, ASTC/ETC2, `shaderInt64`, `multiViewport` | ✅ | ✅ |
| `geometryShader`, ray tracing | ❌ | ❌ (structural to Apple GPUs — unchanged) |

The guest device ID is an independent driver fingerprint: `0x1B000208` → `0x00000064`.

**The strip list is airtight, verified against the guest's own view:** of the 9 stripped extensions,
**0 leaked** to the guest.

**3DMark, real workload, 3 launch/force-stop cycles** (`screenshots/phase5-3dmark-on-kosmickrisp.png`):
full UI renders; guest log `Renderer (Vulkan 1.3.0 (Virtio-GPU GFXStream (Apple M2) (0x00000064)))`;
**0 host gfxstream aborts, 0 `[gpu3d]` command failures**, VMM alive throughout. Its 13 compatibility
verdicts are **byte-identical to the MoltenVK baseline** — same 13 items, same true/false for each,
`Sling Shot Extreme` still the one compatible test. **Zero application-visible capability regression.**

**Retraction of a retraction — read this before repeating either claim.** An earlier revision of this
document "corrected" the strip-list reasoning by asserting that the **10** deliberately-unstripped
matches are "not guest-visible anyway, so the harm avoided was smaller than claimed". **That
correction was itself wrong**, and it is withdrawn. Measured against the guest's own 87-extension
list: `VK_KHR_buffer_device_address` and `VK_KHR_present_id` **are guest-visible**. Over-stripping
would therefore have removed real, guest-reachable capability — including
`buffer_device_address`, which the section above independently flags as not obscure. The original
reasoning for keeping them stands, for exactly the reason first given.

(The remaining 8 of the 10 are indeed not in the guest list, because gfxstream curates the
guest-visible set independently of the host's 140. That is why the count matters: "some are not
guest-visible" is true and was wrongly generalised to "none are".)

---

## The crash this migration shipped, and the guard that now catches it

**Symptom:** Studio reported `emulator process exited unexpectedly (status signal)` — a hard
`SIGSEGV` minutes into boot, during the boot animation. Every graphics signal looked healthy right up
to the fault.

**Root cause — not a graphics bug.** The macOS crash report
(`~/Library/Logs/DiagnosticReports/aemulator-hvf-*.ips`) named it exactly:
`EXC_BAD_ACCESS / KERN_INVALID_ADDRESS at 0x0`, faulting thread `vcpu3`, top frame
**`GfxstreamRenderer::scanout_mtl_texture`** — the function Phase 1 *deleted*. It was jumping to
address 0.

The chain, and why it hid so well:

1. Phase 1 removed `stream_renderer_get_color_buffer_mtl_texture` from the dylib's exports.
2. **Studio launches `target/release`, not `target/debug`** (`paths.rs::hvf_binary` prefers release
   on purpose, so nobody silently gets a slow picture). All validation had been on debug.
3. The release binary was never rebuilt, so it still *imported* the deleted symbol. The dylib is
   loaded by `@rpath` with **lazy** binding, so this does **not** fail at load with
   "symbol not found" — it boots Android and then jumps through a stub that resolved to 0 the first
   time that codepath runs.
4. `spawn::ensure_signed` re-signs on every launch, which **rewrites the binary's mtime**. The stale
   binary was dated to the crash minute, so it looked freshly built.

**Fixed:** rebuilt release — 0 stale imports, 20/20 gfxstream symbols matched. The exact Studio
launch (release binary, `control --display --gpu3d`) then reached `sys.boot_completed` with 0 host
gfxstream aborts.

**Guard added — `ci/hvf-doctor.sh`, "gfxstream ABI match".** Diffs each binary's imported
`stream_renderer_*` / `gfxstream_backend_*` / `android_*Opengles*` symbols against the dylib's
exports, for **both** profiles, since whichever is stale is the one that crashes. Negatively
validated: against a fabricated stale binary it fails and names the missing symbol.

```
==> gfxstream ABI match (every stream_renderer_* the binaries import is exported by the dylib)
      missing: stream_renderer_get_color_buffer_mtl_x
  ✗ target/release/aemulator-hvf imports gfxstream symbols the dylib does not export …
```

**The transferable lesson:** verifying the dylib's exports and the *debug* binary proved nothing about
the binary the product actually runs. Any change to the renderer's exported ABI is a hard break for
every previously-built binary, and lazy binding converts it from a clean load error into a null jump
deep inside a vCPU thread. Rebuild both profiles, and let the doctor prove it.

---

## The second bug this migration shipped: torn readback (partial black flicker)

**Symptom:** parts of the screen going black in a shifting, random-looking pattern while the rest of
the frame rendered correctly. Reported against the Studio live view, i.e. the headless readback path.

**Root cause — a real regression introduced by Phase 1, not a driver problem.** Nothing in gfxstream's
read path waits for the ColorBuffer's pending guest GPU work:

- `stream_renderer_transfer_read_iov` → `readColorBufferToBytesLocked` records **its own** copy
  command buffer and `vkWaitForFences` on **that**. It never touches `latestUse` — the fence tracking
  the rendering the guest submitted to produce the frame. Verified by inspection: `latestUse` appears
  only in `setColorBufferLatestUse` and `waitSyncVkColorBuffer`, never in the read path.
- So the readback samples a **half-drawn** ColorBuffer. Whatever the GPU had not finished writing
  reads as zeroes → partial black, in a pattern that shifts frame to frame.

Why Phase 1 exposed it: the old path handed the control plane an `id<MTLTexture>` and captured through
`aemu_metal_texture_read_rgba`, a Metal blit with `waitUntilCompleted` — GPU-synchronised by
construction. Deleting `VK_EXT_metal_objects` moved headless capture onto
`stream_renderer_transfer_read_iov`, which has no such guarantee.

**Fix:** settle before every readback. `Renderer3d::wait_sync` →
`stream_renderer_wait_sync_resource` → `VirtioGpuResource::WaitSyncResource` →
`FrameBuffer::waitSyncColorBuffer`, called in `flush_3d` immediately before `read_pixels`. That
entrypoint already existed in the dylib and was **declared but never called** from this VMM.

### Proof (KosmicKrisp, headless control path, under continuous animation)

A static screen cannot show this bug, so the guest was driven with 60 back-to-back scroll gestures
while the control plane was sampled as fast as it would answer:

```
1024x768 gen=347 nonzero=2359296     … 20 consecutive captures …
1024x768 gen=358 nonzero=2359296
```

- Generation advances (347 → 358), so these are **distinct frames during real motion**, not a
  repeated still.
- `nonzero` is **constant at 2359296** across all 20 = `1024 × 768 × 3`, i.e. every pixel carries
  colour. **A frame with black regions reports a lower count** — that is what makes this a measurement
  rather than an impression.
- Visual check of a mid-animation frame: clean home screen, crisp icons, smooth wallpaper gradient, no
  torn regions.
- 0 host gfxstream aborts, 0 `[gpu3d]` command failures, VMM alive throughout.

**Note for the windowed path:** `MetalLayerPresenterVk` already settles correctly (design decision D5
replaced `borrowColorBufferForDisplay`, which does no GPU wait at all, with `waitSyncVkColorBuffer`
plus explicit barriers). This fix brings the *readback* path to the same standard. Both present paths
now settle before touching ColorBuffer pixels.

---

## Known, still open

- **The Studio live view costs a readback per guest frame** — but it is **not** blank. An earlier
  draft of this document claimed it had "no pixel source"; that was **wrong** and is corrected here.
  Studio passes `--display`, but the `control` subcommand short-circuits it
  (`bin/aemulator-hvf.rs` routes `--control` first and returns), so **no window and therefore no
  `CAMetalLayer` is ever created**. `metal_layer` stays `0` for the whole run, `flush_3d` takes the
  readback branch, and the CPU scanout framebuffer the `frame`/`screenshot`/`hash` verbs read is
  fully written. Measured on the exact Studio launch: `shot` over the control socket returned a
  correct 1024×768 Android frame, `nonzero=2359296`, and `info` reported `nonzero=3145728`
  (a fully painted 1024×768×4 buffer). The real consequence is **cost**, not correctness: one
  `stream_renderer_transfer_read` per *guest* flush rather than per frame the *viewer* asks for.
  Making it demand-driven needs a VMM-side on-demand capture path; `AEMU_PRESENT_READBACK` is **not**
  that, since it would force the same readback *and* retire the in-Vulkan present path in the
  windowed configuration.
- **The Phase 3 gate reached the lock screen, not the home screen.** The keyguard was not dismissed
  in that run. Every graphics signal in it is healthy (present ACTIVE, 0 gfxstream aborts, 0
  present-path fallbacks, real GPU renderer string, SwiftShader count 0), so the evidence points at
  scripted-input timing rather than rendering — but *points at* is the correct strength. It has not
  been root-caused.
- ~~`cmd gpu vkjson` and 3DMark have not been re-run into this document.~~ **DONE — reduced and
  reviewed; see "Guest-visible capability delta" below.** The guest-side statement of record is now
  KosmicKrisp-era, not MoltenVK-era.
- **`VK_GOOGLE_display_timing` is gone** and Android sometimes uses it for frame pacing. Not
  observed to be requested; the plausible symptom is jitter, not failure. Worth watching, not worth
  pre-emptively working around.
- **`cf_lock::tests::second_acquire_is_refused_while_first_is_held` remains a pre-existing flake**
  (documented in `PHASE-METAL-STATUS.md`; passed on both full-suite runs this pass, so it did not
  reproduce here). Unrelated to graphics, but it guards the single-writer lock the Studio depends on
  and should not stay flaky.
- **`control/mod.rs:888` `frame_args_edge_cases` has never run** — a test body inside `mod tests`
  that is **missing its `#[test]` attribute**, surfaced by the `never used` warning. Pre-existing,
  unrelated to this migration, untouched here; a one-line fix.
- The Phase 3 gfxstream C++ edits have **no automated guard** (see above). A `git`-level pin or a
  vendored patch series is the honest next step.

---

## Reproduce

```
# 1. Build the driver (once). Prints the dylib + ICD paths; fails loudly if the manifest is missing.
bash crates/aemulator-hvf/kosmickrisp/build-kosmickrisp.sh

# 2. Capability gate — isolates DRIVER from RENDERER before you blame gfxstream for anything.
#    Proves: vkCreateDevice with VK_EXT_external_memory_metal, an external-memory VkImage exported
#    as an MTLHeap (the ONLY handle type KosmicKrisp supports), and a real queue submit + fence.
#    This same gate FAILS on MoltenVK with VK_ERROR_INCOMPATIBLE_DRIVER (it requires portability
#    opt-in), which is what makes a pass here evidence about KosmicKrisp specifically.
bash crates/aemulator-hvf/kosmickrisp/run-gate.sh

# 3. Dependency + driver preflight (ICD manifest, its library_path, macOS floor, rpaths, entitlement).
bash ci/hvf-doctor.sh

# 4. Guards (hermetic — no GPU, no boot, no driver required).
cd crates/aemulator-hvf && cargo test --lib          # 61 passed

# 5. Host driver identity + the capability numbers quoted above.
VK_DRIVER_FILES="$TP_KOSMICKRISP_ICD"   # resolved by third_party/paths.sh \
  vulkaninfo | grep -E 'driverName|driverInfo|apiVersion|Device Extensions: count|geometryShader|queueCount'
#   expect: KosmicKrisp / Mesa 26.3.0-devel (git-4f4b73dfe5) / 1.4.354 / count = 140 /
#           geometryShader = false / queueCount = 1

# 6. Android on KosmicKrisp, windowed, in-Vulkan present:
rm -f crates/aemulator-hvf/testdata/cf64/img/userdata.img
cd crates/aemulator-hvf && SKIP_PREP_SUPER=1 SKIP_PREP_32BIT=1 SKIP_PREP_APEX=1 \
  SKIP_PREP_VSOCK_HALS=1 SKIP_PREP_DEBRAND=1 ./run-android64.sh --gpu3d --display
#   grep for, in order:
#     "Selecting Vulkan device: Apple M2, Version: 1.4.354"   <- KosmicKrisp, not MoltenVK (1.1.357)
#     "CAMetalLayer 0x… published for in-Vulkan present"
#     "in-Vulkan present ACTIVE: scanout ColorBuffer res=… (vkCmdBlitImage + vkQueuePresentKHR"
#     "Virtio-GPU GFXStream (Apple M2)"                        <- real GPU path, not SwiftShader
#   and the ABSENCE of: "Unhandled Vulkan structure type … aborting"

# 7. Hard-cutover check on a LIVE process (installed != loaded):
vmmap $(pgrep -f aemulator-hvf) | grep -i -E 'moltenvk|kosmickrisp'
#   expect: libvulkan_kosmickrisp.dylib present, ZERO MoltenVK images —
#   meaningful because `brew list --versions molten-vk` still reports molten-vk 1.4.2.

# 8. 2D fallback (the Phase 1 regression): the layer is published AND the CPU blit paints.
cd crates/aemulator-hvf && ./run-linux-display.sh
#   expect BOTH: "CAMetalLayer 0x… published for in-Vulkan present"
#           and: "[display] scanout gen=… non_zero_fb_bytes=<nonzero>"
```

Only one VMM may hold the `testdata/cf64` disk images at a time — a second concurrent instance
corrupts the shared `userdata` ext4. Kill any stale VMM (`pkill -f "aemulator-hvf run-android"`)
before a run.

No git commits made.
