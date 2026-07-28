# Phase 5 — De-brand / Stealth (identity injection + tell reduction)

**Goal:** make the booting Android device stop looking like a Cuttlefish VM. Apply a coherent
real-device identity (aemulator-identity DeviceProfile → **Redmi K40 `alioth` / Snapdragon SM8250**)
across the surfaces we own, and **measure** the tell reduction — while keeping the hard-won Phase-4
boot-to-home-screen green.

Status: **DONE (safe pass).** Boot still reaches the HOME SCREEN. Property-map tells **28 → 20**
(−8) plus two surfaces the linter doesn't score (DTB root identity + serialno) fully de-branded and
**confirmed live in the kernel log**. The load-bearing / compressed-partition tells are measured and
documented as structural-remaining for the deep pass.

---

## What the image actually is

- Target: `aosp_cf_arm64_only_phone`, **Android 17 / SDK 37** (`CP2A.260605.016`, build 15885347),
  64-bit-only. (The task brief said "Android 16"; the extracted `ro.build.version.release=17` /
  `sdk=37` is what the image really carries — reported as-found.)
- All logical partitions in `super.raw.img` are **EROFS (read-only)**. The load-bearing build.props
  (`system` / `product` / `vendor`) are **EROFS-compressed** (datalayout 3); only `odm` +
  `system_ext` build.props are **FLAT_INLINE (uncompressed)**.
- SELinux boots **permissive** (`androidboot.selinux=permissive`), AVB is patched
  **VERIFICATION_DISABLED** (no valid Cuttlefish signing key), dm-verity disabled.

---

## Baseline audit (BEFORE)

Tooling (reuses the workspace crates, no reinvention):

- `aemulator-detection::lint_properties` via the new **`aemulator lint-props <dir>`** CLI subcommand
  — merges every `build.prop` in a dir into one property map and runs the blocklist linter.
- `aemulator-detection::scan_dir` via `aemulator lint <dir>` — path+content blocklist scan.
- Property maps extracted host-side (no Linux host) with `erofs_extract.py` (FLAT inodes) +
  `fsck.erofs --extract` (compressed inodes).

**Merged getprop map (5 partitions, 316 keys): `28` blocklist tells.**

Token breakdown: `generic` ×14, `vsoc` ×12, `cutf` ×1, `ro.dalvik.vm.native.bridge` ×1.

Top offenders (the coherent Cuttlefish identity, repeated across the 5×5 matrix):

| Property | Baseline value |
|---|---|
| `ro.product.{product,vendor,odm,system_ext}.model` | `Cuttlefish arm64 phone 64-bit only` |
| `ro.product.*.device` | `vsoc_arm64_only` |
| `ro.product.*.brand` | `generic` |
| `ro.product.*.manufacturer` | `Google` |
| `ro.product.*.name` | `aosp_cf_arm64_only_phone` |
| `ro.*.build.fingerprint` | `generic/aosp_cf_arm64_only_phone/vsoc_arm64_only:17/…` |
| `ro.product.board` | `cutf` |
| `ro.board.platform` | `vsoc_arm64` |
| `ro.soc.manufacturer` / `ro.soc.model` | `Google` / `vsoc_arm64_only` |
| `ro.hardware.virtual_device` | `1` (not a blocklist substring, but a hard tell) |

Additional non-property tells (baseline, pre-fix):

- **DTB root** `compatible = "linux,dummy-virt"` (a `dummy-virt` blocklist tell) + `model =
  "aemulator-hvf"`.
- **`androidboot.serialno = AEMULATORHVF01`** (synthetic).
- **`androidboot.verifiedbootstate = orange`** (unlocked-bootloader signal).

---

## What was de-branded (this pass)

### 1. DT root identity — VMM-emitted (`src/boot.rs`, confirmed live) ✅

`build_fdt` now stamps the DeviceProfile root identity via **`aemulator-dtb::root_compatible` /
`machine_name`** when the Android path supplies it (`Config.dt_identity`, threaded through
`stage_boot`). Linux path passes `None` → keeps the `linux,dummy-virt` placeholder, so the Phase-1/2/3
regression boots stay byte-identical.

- `compatible`: `linux,dummy-virt` → **`["xiaomi,alioth", "qcom,kona"]`**
- `model`: `aemulator-hvf` → **`Qualcomm Technologies, Inc SM8250`**

**Verified live** — the guest kernel prints it:
```
[    0.000000][    T0] Machine model: Qualcomm Technologies, Inc SM8250
```
This removes the `dummy-virt` tell from `/proc/device-tree/compatible` + the `/proc/cpuinfo`
`Hardware:` line the kernel derives via `of_flat_dt_get_machine_name()`.

### 2. Serialno — VMM-emitted bootconfig (`src/bin/aemulator-hvf.rs`) ✅

`androidboot.serialno` now comes from the DeviceProfile (`emit_bootconfig`):
`AEMULATORHVF01` → **`237a85a896a20723`** (deterministic, non-synthetic-looking). Live in the boot
log's `identity:` line + the kernel cmdline.

### 3. odm + system_ext build.prop — in-place, byte-length-preserving (`prep-debrand.sh`) ✅

`debrand_buildprop_inplace.py` rewrites the OEM identity strings of the **uncompressed** (FLAT_INLINE)
`odm` + `system_ext` build.props **without changing file length** — no EROFS repack, no verity fixup.
The new content is always shorter than the Cuttlefish original, so the tail is padded with a comment
line to the exact original byte count; the EROFS inode (i_size, block map) is untouched. When the edit
lands in block 0 the EROFS superblock crc32c is recomputed (matches `fsck.erofs`). Idempotent; wired
into `run-android64.sh` (skip with `SKIP_PREP_DEBRAND=1`) and `fsck.erofs`-clean.

Per partition `<p> ∈ {odm, system_ext}`: `ro.product.<p>.{brand,device,manufacturer,model,name}` →
`Redmi / alioth / Xiaomi / M2012K11AC / alioth`; `ro.<p>.build.fingerprint` →
`Redmi/alioth/alioth:17/CP2A.260605.016/15885347:userdebug/test-keys` (keeps the running build's
version/id/incremental/tags so the fingerprint stays internally coherent with SDK 37).

### 4. MIDR — already spoofed (verified) ✅

`FAKE_QUALCOMM_MIDR = 0x511F8041` (`src/vcpu.rs`): implementer `0x51` (Qualcomm), Kryo part `0x804`.
Applied per-vCPU via `set_midr` (HVF accepts the write). Unchanged this pass; confirmed present.

---

## Re-audit (AFTER)

**Merged getprop map: `20` tells** (−8 vs baseline). Removed: the full odm + system_ext identity
(`generic` brand, `vsoc` device, `generic`+`vsoc` fingerprint on each = 8 findings).

Plus, off the property-map score: DTB `dummy-virt` tell eliminated (root compatible/model now
SM8250 — the biggest single VM tell in the device tree) and the synthetic serial replaced.

Residual **20** = 11 `generic` + 7 `vsoc` + 1 `cutf` + 1 `native_bridge`, **all** inside the three
EROFS-**compressed** partitions (`product` / `system` / `vendor`) — see structural-remaining below.

---

## Structural-remaining (known tells, deferred to the deep pass)

Honest note, mirroring the ranchu 113→81 finding on the other path — the residue is load-bearing or
structurally blocked, not overlooked:

1. **Compressed `product` / `system` / `vendor` build.props** (the 20 residual property tells,
   incl. the RESOLVED `ro.product.model = Cuttlefish arm64 phone 64-bit only`, since `product` wins
   the `property_source_order` and is compressed). In-place same-length editing is **impossible** on
   EROFS-compressed inodes. A repack (`fsck.erofs --extract` → edit → `mkfs.erofs` → write back into
   the LP extent) is the path, but it is **not safe on this macOS host**: (a) `fsck.erofs --extract`
   hits **APFS case-insensitivity collisions** (`.../notifications/arcturus.ogg` vs `Arcturus.ogg`
   → "File exists", extract aborts), silently dropping files; (b) `security.selinux` + file-capability
   xattrs are lost (tolerable under permissive SELinux, but caps are not). Needs a **case-sensitive
   Linux host** with SELinux/file_contexts + xattr fidelity. Deferred.
2. **`androidboot.verifiedbootstate = orange`** stays orange. This is a **hard structural
   constraint**, empirically confirmed: forcing `verifiedbootstate=green` + `flash.locked=1` flips
   `libfs_avb` into strict enforcement against our VERIFICATION_DISABLED/unsigned image →
   `ERROR_VERIFICATION / PUBLIC_KEY_REJECTED isn't allowed` → `/system` fails to mount →
   `init: InitFatalReboot: signal 6` (boot dies at T≈1.03s). Going green requires **re-signing the
   image under our own AVB key + embedding the pubkey** — the deep verified-boot pass. `veritymode`
   stays `disabled` for the same reason.
3. **`androidboot.hardware = cutf_cvm`** kept (the `cutf` tell + the vendor `virtual_device`
   scaffolding). It is **load-bearing** for vendor-HAL resolution (`init.<hw>.rc`, `fstab.<hw>`,
   `<mod>.<hw>.so`). A full rename requires renaming the vendor HALs + SELinux labels + VINTF in
   lockstep — the deep-rename pass.
4. **GPU renderer string = SwiftShader** (GL `glGetString`, not a property). De-branding the GPU
   identity to `Adreno (TM) 650` needs the gfxstream→Metal GPU path working (a separate task); under
   the current CPU SwiftShader render the renderer string can't be truthfully spoofed.

---

## Verification

- **Boot still reaches HOME:** re-ran `SWRENDER=1 AEMU_FB_DUMP=/tmp/fb.ppm AEMU_SWIPE_UNLOCK=105
  ./run-android64.sh --display`. Kernel accepted the SM8250 root identity (`Machine model:` line),
  first-stage AVB mounted `/system` (orange), SurfaceFlinger `Boot is finished (22992 ms)`,
  `Launcher3QuickStep` loaded (5×5 grid). Screenshot: `screenshots/phase5-debranded-home.png`.
- **Identity live in the boot log:**
  `identity : model="Qualcomm Technologies, Inc SM8250" compatible=["xiaomi,alioth","qcom,kona"]
  serialno=237a85a896a20723 verifiedbootstate=orange`
- **Regression:** `cargo build` (default workspace) + `cargo test -p aemulator-dtb -p
  aemulator-identity -p aemulator-detection -p aemulator-cli` green (31/31 in dtb/identity/detection).
  `aemulator-hvf` default build hermetic (the two new path deps — identity + dtb — are pure-Rust).
  Linux path FDT unchanged (`dt_identity = None`).

## Next stealth steps

1. **Compressed-partition repack on a Linux host** (case-sensitive FS + xattr/file_contexts fidelity):
   de-brand `product` (flips the RESOLVED `ro.product.model` → M2012K11AC), `system`, `vendor` —
   incl. `ro.product.board`, `ro.board.platform`, `ro.soc.*`, `ro.hardware.virtual_device`. Targets
   the residual 20.
2. **Verified-boot pass:** custom AVB key → re-sign vbmeta → `verifiedbootstate=green` +
   `flash.locked=1` coherently (unblocks Play-Integrity-grade posture).
3. **Deep `cutf_cvm` rename** (HALs + SELinux + VINTF in lockstep) to kill the `cutf` /
   `virtual_device` scaffolding.
4. **gfxstream→Metal GPU** so the GL/Vulkan renderer reports Adreno, not SwiftShader.

## Files

- `src/boot.rs`, `src/machine.rs`, `src/bin/aemulator-hvf.rs` — DT identity + serialno threaded
  through `Config.dt_identity` / bootconfig (Android path only).
- `crates/aemulator-dtb/src/lib.rs` — `root_compatible` / `machine_name` made `pub`.
- `crates/aemulator-cli/src/main.rs` — new `lint-props` subcommand.
- `debrand_buildprop_inplace.py`, `prep-debrand.sh` — in-place odm/system_ext de-brand.
- `screenshots/phase5-debranded-home.png` — home screen after de-brand.
