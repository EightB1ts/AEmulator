# Phase 4 (pass 1) — Boot Android (Cuttlefish arm64) under aemulator-hvf

**Goal of this pass (honest scope):** acquire the Cuttlefish images, add virtio-net, add an Android
boot path, and get the Cuttlefish GKI kernel + Android init to *run* under aemulator-hvf — then
diagnose **exactly** how far Android gets and where it stalls. Reaching a launcher is a later pass.

**Result:** the real **android14 GKI kernel (6.1.157) boots**, brings up **2 SMP CPUs with the
spoofed Qualcomm MIDR (`0x511f8041`, impl `0x51`)**, unpacks the Cuttlefish initramfs, and runs
**first-stage `init`** all the way into **`first_stage_mount` block-device discovery**. It then
stalls — precisely diagnosed below — well before the expected HAL/vsock/secure_env layer, because
the raw per-partition virtio-blk disks carry **no GPT partition labels**, so the by-name partitions
Android's first-stage mount requires are never discovered. Boot did **not** reach SurfaceFlinger;
the gfxstream→Metal triangle is therefore **not** closed this pass (`triangleClosed=false`), though
the `--gpu3d` renderer initializes cleanly on the Android path (see below).

---

## What was built

### virtio-net (`src/virtio/net.rs`) — enumerate-only stub NIC
Modern (VERSION_1) virtio-net advertising `VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS`, one RX/TX queue
pair, link reported **UP**, a QEMU-OUI MAC in config space. TX chains are drained-and-dropped (no
host NAT yet); RX posts are held. Enough for the guest to enumerate `eth0` and for Android's
`netd`/module load to probe a NIC without erroring. Attached via `--net` (`run-linux`) and always-on
for `run-android`. (This module pre-existed from the acquire stage; wired into the boot fleet here.)

### Multi-disk virtio-blk (`src/machine.rs`, `src/bin/aemulator-hvf.rs`)
`Config.disk_path: Option<&Path>` → **`Config.disk_paths: &[PathBuf]`**. `--disk` is now repeatable;
`machine::boot_and_run` builds one virtio-blk transport per disk (`/dev/vda`, `vdb`, …), each on its
own virtio-mmio slot/SPI. Backward-compatible — a single `--disk` still yields `/dev/vda` (Phase-1
regression verified, see below).

### Android bootconfig footer (`src/boot.rs`)
`stage_boot` gained a `bootconfig: Option<&[u8]>` parameter. When set, it appends the blob + the
kernel's `#BOOTCONFIG` footer (`[text][u32 size][u32 checksum][ "#BOOTCONFIG\n" ]`,
`checksum = byte-sum`, kernel `get_boot_config_from_initrd`) to the tail of the initrd in guest RAM
and extends `linux,initrd-end` to cover it. **Verified working** — the kernel parsed it without a
checksum panic and merged the bootconfig-derived params into the command line.

### `run-android` subcommand (`src/bin/aemulator-hvf.rs`, module `android`)
Wires the whole Cuttlefish layout:
- **Disks** (raw images, one virtio-blk each, in attach order): `super.raw.img`, `vbmeta.img`,
  `vbmeta_system.img`, `vbmeta_vendor_dlkm.img`, `boot.img`, `init_boot.img`, `vendor_boot.img`,
  `userdata.img`, plus **fabricated empty `misc.img` + `metadata.img`** (16 MiB) when absent.
- **Kernel** `testdata/cf/kernel` (GKI arm64 Image) + **initramfs** `testdata/cf/initramfs.cf.gz`
  (LZ4-legacy concatenated init_boot + vendor ramdisk) + the appended **bootconfig**.
- **Command line + bootconfig** carry the `androidboot.*` set (hardware=cutf_cvm, serialno,
  boot_devices=<all disk mmio addrs>, slot_suffix=_a, fstab_suffix=cf.ext4.cts, verifiedbootstate,
  veritymode=disabled, selinux=permissive, force_normal_boot, …).
- Defaults: **4096 MiB / 2 vCPUs**, NIC on, `--gpu3d` optional.
- Flags: `--cf-dir --mem-mb --cpus --max-iters --display --gpu3d --selinux --fstab-suffix
  --first-stage-console --extra-cmdline`.

Reproduce with **`./run-android.sh [--gpu3d] [--display] [--first-stage-console]`**.

---

## How far Android gets — the boot trail (verbatim, `run-android` headless)

```
Linux version 6.1.157-android14-11-g22a65aa13222-ab14517476 ... #1 SMP PREEMPT
CPU1: Booted secondary processor 0x0000000001 [0x511f8041]     <- spoofed Qualcomm MIDR, SMP up
SMP: Total of 2 processors activated.
Run /init as init process
init: Loaded 14 kernel modules took 75 ms                       <- virtio_blk/console/net/gpu/... + vsock
init: Switching root to '/first_stage_ramdisk'
init: [libfs_mgr] ReadFstabFromDt(): failed to read fstab from dt
init: Using Android DT directory /proc/device-tree/firmware/android/
init: BlockDevInitializer::InitDevices: partition(s) not found in /sys, waiting for their uevent(s):
        boot_a, init_boot_a, metadata, super, vbmeta_a, vbmeta_system_a, vbmeta_vendor_dlkm_a
init: Wait for partitions returned after 10011ms
init: BlockDevInitializer::InitDevices: partition(s) not found after polling timeout:
        boot_a, init_boot_a, metadata, super, vbmeta_a, vbmeta_system_a, vbmeta_vendor_dlkm_a
init: Failed to mount required partitions early ...
init: InitFatalReboot: signal 6
reboot: Restarting system with command 'bootloader'
```

**Highest milestone observed: `first-stage-init`** (deep — fstab loaded + parsed, entered
`first_stage_mount` block-device discovery). Not `selinux-second-stage`.

### The exact stall
`first_stage_mount` calls `BlockDevInitializer::InitDevices` to resolve its required partitions by
**by-name label** (`super`, `boot_a`, `vbmeta_a`, `vbmeta_system_a`, `vbmeta_vendor_dlkm_a`,
`init_boot_a`, `metadata`). Those names come from **GPT partition labels** (`PARTNAME`), surfaced by
ueventd as `/dev/block/by-name/<label>` symlinks. **Our virtio-blk disks are raw images with no GPT
and no partition labels**, so no by-name device ever appears; init waits the full 10 s uevent
timeout, then `InitFatalReboot(signal 6)` → reboot-to-bootloader (which the VMM observes as a guest
PSCI reset and stops). This is the direct, expected consequence of the pass-1 "separate raw disk per
partition" layout — real Cuttlefish hands crosvm a **GPT composite disk** whose partition entries
carry exactly these labels.

### The debug iterations that got us here (each advanced the stall)
1. `first_stage_console=1` dropped init into an interactive shell **before** mount → removed (made it
   an opt-in `--first-stage-console`) so boot proceeds to the real mount.
2. `ReadDefaultFstab` couldn't find `fstab.cutf_cvm` — Cuttlefish ships `fstab.cf.{ext4,f2fs}.{cts,hctr2}`
   and normally feeds one via a DT node → pointed **`androidboot.fstab_suffix=cf.ext4.cts`** at the
   ramdisk file. Fstab file then **found**.
3. Fstab parse failed: `Error updating for slotselect` (entries are `slotselect`, empty slot invalid)
   → set **`androidboot.slot_suffix=_a`** (Cuttlefish virtual-A/B). Fstab then **parsed**, and init
   advanced into by-name block-device discovery — the stall above.

---

## gfxstream / `--gpu3d` on the Android path (renderer ready; guest never reaches it)
`run-android --gpu3d` on the `--features gfxstream` build initializes the host renderer cleanly:
```
[gpu3d] stream_renderer_init returned 0
[ok] virtio-gpu 3D (VIRGL+CONTEXT_INIT) 1024x768 → gfxstream host renderer, /dev/dri/renderD128
```
The virtio-gpu device comes up in VIRGL/gfxstream mode with MoltenVK staged. But Android reboots at
`first_stage_mount` long before SurfaceFlinger, so nothing is ever rendered. **`triangleClosed=false`
(no screenshot).** The gfxstream→Metal path proven in Phase 3 is wired and waiting on the guest
reaching SurfaceFlinger.

---

## Regression — Phase 1/2/3 still green
- **Default build hermetic w.r.t. the accelerator stack:** `cargo build` (no features) links **no**
  gfxstream/MoltenVK/Metal (`otool -L` shows Hypervisor.framework + the pre-existing Phase-2 winit
  window frameworks only; the gfxstream/Metal path stays behind `--features gfxstream`). Both
  `cargo build` and `cargo build --features gfxstream` compile clean.
- **Phase-1 headless Linux boot** (`run-linux --kernel Image --initrd initramfs-virt --disk
  scratch.img`) still boots Alpine 6.6 and enumerates the single disk:
  `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)` — the multi-disk refactor is
  backward-compatible.

---

## Remaining integration (to reach a launcher — later passes)

1. **Partition presentation → GPT composite disk (the immediate blocker).** Present the Cuttlefish
   partitions with GPT `PARTNAME` labels so `/dev/block/by-name/{super,boot_a,vbmeta*_a,metadata,…}`
   resolve. Either wrap each raw image in a single-entry GPT, or (better, matches Cuttlefish) build a
   crosvm-style **composite disk**: one virtio-blk exposing a synthetic GPT that maps each label to
   its backing file. Until this lands, first-stage mount cannot start.
2. **Dynamic (logical) partitions.** `system/vendor/product/system_ext/*_dlkm` live **inside super**
   as LP metadata → first-stage must read liblp metadata and program **dm-linear**. Needs a working
   device-mapper path in the guest (kernel supports it; nothing host-side required beyond serving the
   super blocks) and correct by-name for `super`.
3. **AVB / dm-verity.** Fstab entries carry `avb=vbmeta*`. With `verifiedbootstate=orange` +
   `veritymode=disabled` fs_mgr should mount in allow-error mode, but the vbmeta chain still has to
   load; expect an AVB-handle step to exercise next.
4. **secure_env re-host (KeyMint / Gatekeeper).** Cuttlefish runs these on the host and exposes them
   to the guest over **vsock** HAL channels. We have no host backend → keystore2 / weaver will block
   second-stage. Needs a host `secure_env` equivalent (or stubbed KeyMint/Gatekeeper) behind vsock.
5. **HAL console channels + vsock transport.** Cuttlefish wires ~16 `virtio-console`/HVC channels
   (hvc0 console, hvc2 logcat, hvc3 keymaster, hvc4 gatekeeper, …) and a real **vhost-vsock**
   (guest-cid=3). We expose one virtio-console and load `vmw_vsock_virtio_transport` in the guest but
   have **no host vsock backend** — needed for the HAL/secure_env RPC once second-stage runs.
6. **GPU.** Once SurfaceFlinger runs, Android's Mesa `gfxstream_vk` drives our proven virtio-gpu-3D →
   gfxstream → Metal path; that closes the triangle (`--gpu3d`, screenshot).

## Next-pass steps (in order)
1. Add a **GPT composite-disk** virtio-blk backend (label→file map) and switch `run-android` to it →
   satisfies `BlockDevInitializer`; watch first-stage advance to LP-metadata / dm-linear.
2. Bring up **dm-linear for logical partitions** + confirm the super LP metadata reads through the
   block backend; land AVB in allow-error mode.
3. Reach **SELinux / second-stage init**; capture the next stall (expected: keystore/secure_env over
   vsock).
4. Add a **host vsock backend** + minimal **KeyMint/Gatekeeper** stubs (or a `secure_env` re-host)
   and the multi-HVC console channels.
5. With second-stage up, drive **SurfaceFlinger** on `--gpu3d` and screenshot the gfxstream→Metal
   frame (close the triangle).

---

## Provenance / repro
- Images: branch `aosp-android14-gsi`, target `aosp_cf_arm64_phone-userdebug`, build `15722683`
  (see `testdata/cf/SOURCE.txt`; md5s verified). Partitions under `testdata/cf/img/` (gitignored).
- Build: `cargo build --manifest-path Cargo.toml [--features gfxstream] --bin aemulator-hvf`
- Sign: `codesign --sign - --entitlements hv.entitlements --force target/debug/aemulator-hvf`
- Run: `./run-android.sh [--gpu3d] [--display] [--first-stage-console]`
- Boot logs captured this pass: `/tmp/android-boot-{1..4}.log`, `/tmp/android-gpu3d.log`.

---

## Pass 2 (disk) — GPT composite virtio-blk backend + the virtio-MMIO transport blocker

**What was built (done + verified host-side):** a **GPT composite virtio-blk backend**
(`src/virtio/block_composite.rs`, `CompositeBlock`) that presents **one** virtio-blk disk carrying a
synthetic GUID Partition Table — protective MBR + primary GPT header + 128-entry partition array +
backup GPT — whose entries carry the exact Cuttlefish by-name labels
(`super`, `boot_a`, `init_boot_a`, `vbmeta_a`, `vbmeta_system_a`, `vbmeta_vendor_dlkm_a`,
`metadata`, `userdata`, `misc`, `vendor_boot_a`), each mapped to its raw image file at a 4 KiB-aligned
LBA offset. Guest block reads/writes are routed by byte-offset to the owning component file (GPT
regions served from in-memory buffers; alignment tails + past-EOF zero-filled; writes gated to
writable partitions within file bounds). Byte layout mirrors crosvm's `composite.rs`/`gpt.rs`.
`super` = `super.raw.img` (the 7 GB unsparsed LP super). Wired into `run-android` (single composite
`/dev/vda`, `boot_devices` = the one disk's `a000400.virtio_mmio`); the per-raw-disk path is retired
for Android but preserved for `run-linux` (composite is opt-in via `Config.composite_partitions`).

- **Unit-tested** (`cargo test block_composite`): CRC32 known-answer; primary GPT header CRC
  self-consistent; partition-array CRC matches header; first entry names `super` at LBA 40; backup
  GPT signature at the last sector; content routing returns each backing file's bytes; alignment tail
  zero-fills. **Both builds green**; **default build stays hermetic** (`otool -L`: no
  gfxstream/Metal/Vulkan). Phase-1/2/3 `run-linux` path byte-unchanged (the raw-disk loop is the
  untouched `else` branch).

**THE STALL DID NOT ADVANCE — and the cause is UPSTREAM of the disk format.** With the composite disk
the guest still reboots at the SAME `BlockDevInitializer` timeout (verbatim):
```
init: ... BlockDevInitializer::InitDevices: partition(s) not found in /sys, waiting for their uevent(s):
      boot_a, init_boot_a, metadata, super, vbmeta_a, vbmeta_system_a, vbmeta_vendor_dlkm_a
init: Wait for partitions returned after 10010ms
init: ... partition(s) not found after polling timeout: boot_a, init_boot_a, metadata, super, ...
init: Failed to mount required partitions early ...
init: InitFatalReboot: signal 6
reboot: Restarting system with command 'bootloader'
```
**Root cause (proven, not inferred):** host-side instrumentation (`AEMU_CDISK_DEBUG=1`) shows
**ZERO block requests ever reach the composite disk** — the guest never reads LBA 0/1, so the GPT is
never parsed and no `vda1..vdaN` partitions are created. The guest has **no virtio-MMIO transport**:
  - The Cuttlefish vendor ramdisk ships **no `virtio_mmio.ko`** — `lib/modules/` contains only
    `virtio_pci.ko` + `virtio_pci_{modern,legacy}_dev.ko`, and `modules.load` loads **`virtio_pci.ko`**,
    never `virtio_mmio`. (14 modules load; `virtio_mmio` is not among them.)
  - The GKI kernel core config (`/proc/config.gz` embedded in `testdata/cf/kernel`) has
    `# CONFIG_VIRTIO_MMIO is not set` with `CONFIG_PCI=y`.
  - So `virtio_blk.ko` loads but has no MMIO transport to bind to → our MMIO virtio-blk (and every
    other MMIO device) is never enumerated. Cuttlefish is a **virtio-PCI** guest; aemulator-hvf
    presents **virtio-MMIO**. This is a transport mismatch, independent of the (correct) GPT.

**The composite GPT disk is correct and ready; it cannot be exercised by this guest until the VMM
presents virtio over PCI** (ECAM + virtio-pci-modern + MSI/legacy INTx), or a matching
`virtio_mmio.ko` is built for `6.1.157-android14-11-g22a65aa13222` and injected into `modules.load`
(not shippable for this kernel — `CONFIG_VIRTIO_MMIO=n`). **Next-pass step 1 is therefore a
virtio-PCI transport**, after which the composite disk should immediately satisfy `BlockDevInitializer`.

---

## Pass 3 (transport) — virtio-PCI: first-stage → SECOND-STAGE init + SELinux + apexd

**The pass-2 blocker is fixed.** aemulator-hvf now presents virtio over **PCI** (a generic
`pci-host-cam-generic` host bridge + virtio-pci-modern devices + legacy INTx), which is the transport
the Cuttlefish android14 GKI kernel actually speaks (`CONFIG_VIRTIO_MMIO=n`, ships only
`virtio_pci.ko`). The guest now **reads the disk**, and boot advanced **five milestones** in one pass:
first-stage by-name → dm-linear logical partitions → AVB-disabled mounts → **second-stage init +
SELinux** → apexd.

### What was built
- **`src/virtio/pci.rs`** — a single-bus PCI host bridge (`PciBus`) + virtio-pci device
  (`PciDevice`) reusing the SAME [`Backend`] fleet (console, rng, composite block, gpu, input, net).
  * CAM config space at `0x3000_0000` (`(bus<<16)|(dev<<11)|(func<<8)|reg`, 256 B/function).
  * virtio-pci-**modern** capability layout inside one 32-bit non-prefetchable memory **BAR0** (16 KiB
    per device in the `0x1000_0000` MMIO window): common-cfg @ 0x0000, notify @ 0x1000
    (`notify_off_multiplier = 0` → the written value is the vq index), ISR @ 0x2000 (read-to-clear),
    device-cfg @ 0x3000.
  * **Legacy INTx** (INTA) → a GIC SPI. `msix_config`/`queue_msix_vector` advertise NO_VECTOR
    (0xffff) so Linux virtio-pci falls back to INTx, routed by the FDT `interrupt-map`. On a
    used-buffer notify: set ISR bit0 + raise the device's SPI (the same one-shot injection the MMIO
    path used). BAR-sizing + `linux,pci-probe-only = 1` are both honored.
- **`src/boot.rs`** — a `/pci@30000000` FDT node (`pci-host-cam-generic`, `reg`=CAM, `ranges`=the
  32-bit MMIO window, per-device `interrupt-map` INTA→SPI level-high, `dma-coherent`) +
  `linux,pci-probe-only=1` in `/chosen`. Emitted only when the PCI fleet is non-empty; the Linux
  virtio-mmio nodes are unchanged.
- **`src/machine.rs`** — the Android path (`composite_partitions.is_some()`) now builds the whole
  fleet on `PciBus` (console 00:01.0, rng 00:02.0, composite block **00:03.0**, gpu 00:04.0, input
  00:05.0, net 00:06.0) instead of virtio-mmio. The Linux path is byte-identical (virtio-mmio, empty
  PciBus). Shared boot tail = `boot_and_run_common`.
- **`src/vcpu.rs`** — `VmContext` gained a `pci: Mutex<PciBus>`; the data-abort handler dispatches
  CAM (`PciBus::cam_contains`) + BAR MMIO (`PciBus::mmio_contains`), raising the device SPI on a
  used-buffer notify exactly like the MMIO path.
- **`src/bin/aemulator-hvf.rs`** — `boot_devices` is now the PCI host-bridge platform device
  `30000000.pci` (see below), and the top-level **vbmeta is patched with
  `AVB_VBMETA_IMAGE_FLAGS_VERIFICATION_DISABLED` (flag=2)** into a copy so first-stage fs_mgr skips
  verity + signature and mounts the logical partitions directly.

### The boot trail (verbatim, `run-android` headless)
```
pci-host-generic 30000000.pci: PCI host bridge to bus 0000:00
PCI: OF: PROBE_ONLY enabled
pci 0000:00:03.0: [1af4:1042] type 00 class 0x018000          <- virtio-blk over PCI
virtio_blk virtio2: [vda] 15029080 512-byte logical blocks (7.69 GB/7.17 GiB)
 vda: vda1 vda2 vda3 vda4 vda5 vda6 vda7 vda8 vda9 vda10       <- composite GPT parsed over PCI
init: [libfs_mgr] Created logical partition system_a on device /dev/block/dm-1   <- LP metadata + dm-linear
init: [libfs_mgr] Created logical partition vendor_a on device /dev/block/dm-6
   ... product_a/system_ext_a/system_dlkm_a/odm_a/vendor_dlkm_a/odm_dlkm_a ...
selinux: SELinux: Loaded file context from: /system/etc/selinux/plat_file_contexts   <- /system MOUNTED, 2nd stage
   ... /system_ext/.../product/.../vendor/... file_contexts ...
init: Parsing file /system/etc/init/apexd.rc...
init: starting service 'apexd-bootstrap'...
apexd-bootstrap: Found pre-installed APEX /system/apex/com.android.runtime.apex   (+ ~35 more)
apexd-bootstrap: Found two apex packages .../com.google.emulated.camera.provider.hal.apex and
   .../com.google.emulated.camera.provider.hal.fastscenecycle.apex with the same module name
init: Service 'apexd-bootstrap' (pid 73) received signal 6 ...
reboot: Restarting system with command 'bootloader,bootstrap-apexd-failed'
```

**Highest milestone observed: `selinux-second-stage`** — second-stage init running, SELinux policy +
file-contexts loaded (permissive), apexd started. (Pass 2 died at first-stage `BlockDevInitializer`.)

### Debug iterations that got us here (each advanced the stall)
1. **virtio-PCI transport** → the guest reads the disk; `vda1..vda10` appear. Prior stall
   (`BlockDevInitializer` 10 s uevent timeout) gone — nodes created immediately.
2. `realpath /dev/block/by-name/super: No such file or directory` → **`boot_devices=30000000.pci`**
   (the PCI host bridge's *platform-device* name, not the PCI BDF — `FindPlatformDevice` resolves the
   partitions to the bridge). by-name then resolved; LP metadata read; dm-linear logical partitions
   created.
3. `[libfs_avb] Unknown androidboot.veritymode: disabled` / `unknown public key` / `Failed to setup
   verity for '/system'` → **patch the top-level vbmeta with VERIFICATION_DISABLED (flag=2)**. AVB
   short-circuits; `/system` + `/vendor` + `/product` + `/system_ext` mount; second-stage init runs.

### The exact stall (next blocker — image-prep, NOT the VMM)
`apexd-bootstrap` **aborts (SIGABRT)** on a **duplicate vendor APEX module name**: `/vendor/apex`
carries BOTH `com.google.emulated.camera.provider.hal.apex` and its `…fastscenecycle.apex` variant,
which share the module name `com.google.emulated.camera.provider.hal`. `assemble_cvd` normally prunes
the unused camera variant before boot; the raw downloaded `super` keeps both, and apexd treats a
duplicate module name as fatal (`reboot_on_failure`). Last log line:
`apexd-bootstrap: Found two apex packages … with the same module name …` → signal 6.

**Fix (next pass):** remove one of the two camera-HAL apexes from the `vendor_a` logical partition
(host-side: parse the LP metadata for the `vendor_a` extent in `super.raw.img`, `debugfs -w -R "rm
/apex/…fastscenecycle.apex"` on that extent — verity is disabled so no hashtree to fix), or
re-acquire `super` through `assemble_cvd`. Then expect the next stall at the HAL/secure_env layer
(KeyMint/Gatekeeper over vsock — no host backend yet), exactly as the original pass-1 roadmap called.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + the pre-existing winit window frameworks only;
  no gfxstream/Metal/Vulkan). Both `cargo build` and `cargo build --features gfxstream` compile clean.
- **Phase-1 headless Linux boot** (`run-linux.sh`) still boots Alpine 6.6 over **virtio-mmio** and
  enumerates the disk: `virtio_blk virtio2: [vda] 131072 512-byte logical blocks` — the PCI work is
  an additive Android-only branch (`composite_partitions.is_some()`); the Linux virtio-mmio path is
  untouched (empty `PciBus`, no `/pci@…` FDT node).
- `cargo test`: 20/20 pass (composite-GPT layout/CRC/routing).

### Next-pass steps (in order)
1. Prune the duplicate camera-HAL apex from `vendor_a` (or re-acquire `super` via `assemble_cvd`) →
   apexd-bootstrap succeeds; watch second-stage advance to `servicemanager`/`hwservicemanager`/zygote.
2. Stand up the **HAL/secure_env** layer: a host **vhost-vsock** backend (guest-cid=3) + minimal
   **KeyMint/Gatekeeper** (secure_env re-host) + the multi-HVC console channels — expected next stall
   once servicemanager/keystore2 come up.
3. Drive **SurfaceFlinger** on `--gpu3d` (the gfxstream→Metal path is wired + waiting) → close the
   triangle (screenshot).

---

## Pass 4 (HAL) — multi-HVC console fleet + software secure_env: the KeyMint/keystore2 wall is cleared

**The pass-3 apex wall was already fixed** (`prep-super.sh` de-dups the camera HAL apex + disables the
32-bit BoringSSL self-tests — Apple Silicon has no AArch32). With that in place the guest reached the
HAL layer and stalled precisely where pass-3 predicted: the **KeyMint Rust HAL**
(`android.hardware.security.keymint-service.rust`, service `vendor.keymint-rust`) **aborted at
`open("/dev/hvc11")` → `Os code 19 No such device`** (~guest 5.8s), which cascaded
keystore2 SIGABRT → apexd `reboot_on_failure` → `reboot: apexd-failed`. The VMM exposed only ONE
virtio-console (hvc0), so the KeyMint HAL's HVC channel didn't exist.

This pass built the **multi-HVC console fleet** + a **from-scratch software `secure_env`** and pushed
the boot **two full walls** deeper — through KeyMint HAL bring-up, keystore2 registration, apexd
second-stage APEX processing, and into the vold disk-encryption layer.

### What was built
- **`src/virtio/console.rs` — 12 HVC channels + a role model.** `Console` gained a `ConsoleRole`
  (`Terminal` = hvc0 stdout/stdin as before; `Log` = a stdout tap for logcat on hvc2; `Sink` = a
  silent drain; `Secure` = a HAL channel bridged to `secure_env`). `on_notify(idx, mem, &mut [Queue])`
  replaced the single-queue `process_queue` for consoles so a **Secure** channel can drain the TX
  request and fill the guest's already-posted RX buffers **in one interrupt cycle** (the transports —
  `mmio.rs`/`pci.rs` — now route `Backend::Console` through it; every other backend is byte-unchanged).
  `feed_console_input` only feeds the `Terminal` console (HAL channels never take host keystrokes).
- **`src/machine.rs` — the Android fleet now has 12 virtio-console PCI devices FIRST** (slots 1..12 →
  the guest's `virtio_console` driver numbers them `hvc0..hvc11` in probe order), then rng / composite
  block / gpu / input / net. Channel roles from the vendor `ueventd.rc` map: hvc0 console, hvc2
  logcat, **hvc3 keymaster / hvc4 gatekeeper / hvc10 oemlock / hvc11 keymint** → `secure_env`, the rest
  drain. (17 PCI devices total; INTIDs 48..64 / SPIs 16..32, well inside the GIC's 288 IRQ lines.)
- **`src/secure_env.rs` — a minimal software secure_env (NOT a TEE).** A self-contained **CBOR codec**
  + the `kmr_wire` message *shape*. The transport framing was **discovered empirically from the wire**
  (a raw-byte dump the first time each channel speaks): it is a **4-byte BIG-ENDIAN payload length +
  payload** (NOT the `keymaster_channel` `[cmd|resp][len]` header — the Rust `kmr` HAL uses the plain
  length-prefixed transport). The KeyMint opcodes were reverse-engineered from the captured requests
  (`[opcode, req_struct]`, a struct = a CBOR array of its fields). Modeled responses (well-formed
  `PerformOpResponse = [error_code, Option<PerformOpRsp>]`):
  * `SetHalInfo(129)` / `SetBootInfo(130)` / `SetAttestationIds(131)` / any "set/void" op → empty-success.
  * `GetHardwareInfo(0x11)` → `KeyMintHardwareInfo{ver=3, **securityLevel=TRUSTED_ENVIRONMENT(1)**,
    "aemulator-keymint", "aemulator", tstok=false}` — reporting **TEE** is mandatory: keystore2's
    "default" instance aborts with *"Trying to construct mandatory security level TEE"* on SOFTWARE(0).
  * `GetSharedSecretParameters(0x51)` → `{seed=[], nonce=32B}` and `ComputeSharedSecret(0x52)` →
    `{sharing_check=32B}` (stable constants; a single instance → keystore2 accepts).
- **`src/bin/aemulator-hvf.rs` — scratch-partition sizing.** The fabricated `metadata` image was 16 MiB;
  `make_f2fs` refuses a volume that small (*"Device size is not sufficient for F2FS volume"*), so
  /metadata never mounted RW and the whole vold key-store / /data / keystore-DB / apexd chain broke.
  `metadata` is now (re)sized to **128 MiB** and the ~2 MB placeholder `userdata` grown to **6 GiB**
  (`ensure_size` — sparse grow, never shrinks) so /metadata + /data are formattable.

### The boot trail (verbatim highlights, `run-android` headless)
```
[ok] virtio-pci consoles: 12 HVC channels (hvc0..hvc11); secure_env on hvc3/4/10/11
init: ... started service 'vendor.keymint-rust' has pid 158           <- opens /dev/hvc11 OK (no abort)
[secure_env] keymint(hvc11) RAW first 4B: 0000004f                    <- framing = 4B big-endian length
[secure_env] keymint op=Some(130) ... op=Some(129) ... op=Some(131)   <- SetBootInfo/SetHalInfo/SetAttestationIds
[secure_env] keymint op=Some(17)  <- GetHardwareInfo   op=Some(81)/Some(82) <- Shared-secret negotiation
servicemanager: Found android.hardware.security.keymint.IKeyMintDevice/default in device VINTF manifest
init: starting service 'keystore2' ...   (constructs TRUSTED_ENVIRONMENT security level + registers)
apexd: Decompressing /system/apex/com.android.conscrypt.capex ...      <- second-stage apexd running
init: starting service 'exec 9 (/system/bin/vdc --wait cryptfs init_user0)' ...
vold: read_key failed in mountFstab   /   cutils: Failed to mkdir(/data/misc/vold/user_keys)
init: Exec service failed, status 25: Rebooting into recovery, reason: init_user0_failed
```

**Highest milestone observed: `hwservicemanager-hals`** — servicemanager + hwservicemanager +
keystore2 up; the KeyMint HAL negotiates with `secure_env` (GetHardwareInfo + shared-secret) and
keystore2 **constructs its mandatory TEE security level and registers `IKeystoreService`**; apexd runs
second-stage and processes the compressed system APEXes. (Pass 3 died at apexd-bootstrap; this pass is
past apexd entirely, into post-fs-data / vold.) **SurfaceFlinger not reached** → `triangleClosed=false`,
no screenshot.

### The exact stall (next blocker — disk-encryption KeyMint key ops, NOT the console/transport)
Two walls were cleared in sequence this pass (KeyMint-HAL-open → keystore2-registration → storage
sizing); the remaining wall is **vold's metadata-encryption + FBE key setup**:
`vdc cryptfs mountFstab userdata /data` fails with **`vold: read_key failed in mountFstab`** and
`vdc cryptfs init_user0` fails to prepare `/data/misc/vold/user_keys` → **`reboot: recovery
(init_user0_failed)`**. `/data` is metadata-encrypted (dm-default-key) + FBE; establishing those keys
needs **real KeyMint key operations** (`generateKey`/`importKey` + a real keyblob format + AES/HKDF) —
exactly the crypto the minimal empty-success `secure_env` does **not** implement yet. keystore2's own
`Failed to attach database persistent` on earlier runs was the same root cause (RO /data before the
key setup). This is the KeyMint-TA-depth wall, cleanly isolated from the HAL/transport layer this pass
delivered.

### Debug iterations that got us here (each advanced the stall)
1. **12 HVC console channels** → `open("/dev/hvc11")` succeeds; the KeyMint HAL no longer aborts.
2. **Transport framing** captured from a raw hex dump: `[u32 length]` is **big-endian**, not
   little-endian, and there is no command word — a bare length-prefixed payload.
3. **`GetHardwareInfo` with `securityLevel=TRUSTED_ENVIRONMENT`** → keystore2 stops aborting
   *"mandatory security level TEE"*; **shared-secret** (`getSharedSecretParameters`/`computeSharedSecret`)
   answers → keystore2 registers.
4. **metadata 16 MiB → 128 MiB** → `make_f2fs` succeeds, /metadata mounts RW, /data mount is attempted,
   apexd processes APEXes → the wall moves from "metadata RO" to vold's `read_key` / `init_user0`.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + winit frameworks only; no gfxstream/Metal/Vulkan).
  Both `cargo build` and `cargo build --features gfxstream` compile clean.
- **`cargo test`: 23/23** (20 composite-GPT + **3 new** secure_env CBOR/response tests).
- **Phase-1 Linux boot** (`run-linux.sh`): Alpine 6.6 boots over virtio-**mmio**, `virtio_blk [vda]`
  (67.1 MB) + virtio-input enumerate, reaches the initramfs rescue shell — **no panic**. The console
  refactor (`on_notify`) is transport-neutral; the MMIO console path is unchanged in behaviour.

### Next-pass steps (in order)
1. Implement **real KeyMint key operations** in `secure_env` (`generateKey`/`importKey`/`exportKey`
   + a keyblob format + AES-GCM/HKDF) so vold can establish the metadata-encryption + FBE keys — OR,
   as a faster boot-advance, patch the fstab to mount `/data` **unencrypted** (drop
   `fileencryption`/`metadata_encryption`). Then watch `init_user0` succeed → zygote.
2. From zygote → **SurfaceFlinger** on `--gpu3d`; the gfxstream→Metal path (Phase 3) is wired + waiting.
   If a boot-animation frame renders, screencapture the window (`triangleClosed=true` only if seen).
3. Optionally add the **virtio-vsock** device (device_type 19, guest-cid=3) — not needed for the
   KeyMint/Gatekeeper HALs (confirmed HVC-based this pass), but other Cuttlefish services use it.

---

## Pass 5 (DataMount) — /data mounts unencrypted → past vold → ZYGOTE (fully preloads)

**The pass-4 vold crypto wall is cleared via the fast path** (mount `/data` as plain ext4, skipping the
metadata-encryption + FBE key ops the minimal `secure_env` doesn't implement). Boot advanced **three
milestones** past vold: `mountFstab`/`init_user0` succeed → `/data` mounts RW → **zygote starts and
fully preloads** (17,308 classes) → system_server/surfaceflinger are started. Highest milestone now
**`zygote`** (pass 4 died at vold `read_key`/`init_user0_failed`).

### What was built (host-side image prep — repeatable scripts, NO VMM change)
- **`prep-data-unencrypted.sh` (new, wired into `run-android.sh`, idempotent).** Two steps:
  1. **Neutralize the crypto fs_mgr flags on the `/data` fstab entry.** The `/data` line
     (`/dev/block/by-name/userdata /data ext4 … formattable,keydirectory=/metadata/vold/metadata_encryption,fileencryption=aes-256-xts:aes-256-cts,checkpoint=block`)
     lives in the **vendor_a EROFS** logical partition of `super.raw.img` (4 slot variants:
     ext4.cts/ext4.hctr2/f2fs.cts/f2fs.hctr2; `androidboot.fstab_suffix=cf.ext4.cts` selects ext4.cts,
     and second-stage vold reads the SAME vendor copy via `fs_mgr_read_fstab_default`). The script
     corrupts the **first byte** of each crypto keyword (`keydirectory`→`xeydirectory`,
     `fileencryption`→`xileencryption`, `checkpoint`→`xheckpoint`) — a **same-length** edit, so fs_mgr
     parses each as an *unknown flag* (`Warning: unknown flag: …` then continues) and drops
     metadata-encryption + FBE + fs-checkpointing, while `latemount,wait,check,quota,formattable`
     survive. Line-scoped to the `/dev/block/by-name/userdata /data` lines only (the other 8
     `keydirectory=` / 12 `checkpoint=` occurrences in super are untouched). Edits land in EROFS
     file-data blocks far from vendor_a's block-0 superblock, so **no EROFS crc recompute** (same
     reasoning as prep-super.sh's 32-bit self-test edit). Verified in the boot log:
     `init: [libfs_mgr] Warning: unknown flag: xeydirectory=/metadata/vold/metadata_encryption`.
  2. **Pre-format `userdata.img` as a valid empty ext4** (`mke2fs -F -q -t ext4 -b 4096 -O ^orphan_file
     -m 0 -L data`, e2fsprogs 1.47; grown to the harness's 6 GiB /data first). **Required** because
     init's `mount_all --late` does NOT auto-mkfs a wiped `formattable` partition — it returns
     `FS_MGR_MNTALL_FAIL` (`Invalid ext4 superblock on userdata` → `queue_fs_event() failed: Invalid
     code: -1`) so /data stayed unmounted/RO (`init_user0_failed`, `/data: Read-only file system`).
     Laying down a real ext4 host-side makes the first mount succeed. `^orphan_file` keeps the feature
     set conservative for the guest GKI kernel. Idempotent — skips reformat if userdata already carries
     an ext4 superblock (so `/data` survives across boots).

### The boot trail (verbatim highlights, `run-android` headless — default hermetic build)
```
init: [libfs_mgr] Warning: unknown flag: xeydirectory=/metadata/vold/metadata_encryption   <- patch honored
init: [libfs_mgr] Warning: unknown flag: xileencryption=aes-256-xts:aes-256-cts
EXT4-fs (vda8): mounted filesystem with ordered data mode. Quota mode: journalled.          <- /data RW
init: [libfs_mgr] __mount(source=/dev/block/by-name/userdata,target=/data,type=ext4)=0: Success
vold_prepare_subdirs: Setting up … /data/misc_de/0/apexdata/com.android.wifi                 <- init_user0 OK
init: starting service 'zygote'...                                                            <- zygote
init: starting service 'surfaceflinger' has pid 382
zygote64: … nativePreloadGraphicsDriver … eglGetDisplay  → Fatal signal 6 (headless, no GPU)  <- headless stall
```

**Highest milestone: `zygote`.** `mountFstab`/`init_user0` no longer `read_key`-fail; `/data` mounts RW
(journalled ext4); vold `vold_prepare_subdirs` builds `/data/misc_de/0/…`; **zygote + surfaceflinger
are started by init.** `system_server` did **not** stabilize (see below). **SurfaceFlinger not
rendered** → `triangleClosed=false`, no screenshot.

### The exact next stall (GPU-transport wall — the SurfaceFlinger/`--gpu3d` pass, not DataMount)
Two boundaries, both isolated cleanly:
- **Headless (no `--gpu3d`):** `zygote64` aborts in `ZygoteInit.preload` →
  `maybePreloadGraphicsDriver` → `nativePreloadGraphicsDriver` → `eglGetDisplay` (SIGABRT) — no GPU
  renderer at all.
- **`--gpu3d` + injected graphics props** (`androidboot.hardware.gralloc=minigbm
  hwcomposer=ranchu egl=emulation vulkan=ranchu opengles.version=196609` via `--extra-cmdline`): the
  gfxstream host renderer inits (`stream_renderer_init (VK/MoltenVK)`), and **zygote now fully
  preloads past the graphics driver** (`Zygote: …preloaded 17308 classes in 133ms`). The new wall is
  **`surfaceflinger` SIGABRT**:
  ```
  surfaceflinger: Could not open '/dev/goldfish_pipe_dprctd': No such file or directory
  surfaceflinger: open_verbose: both vsock and goldfish_pipe paths failed
  EGL_emulation: Failed to establish connection with the host
  RenderEngine: no suitable EGLConfig found, giving up   →  Fatal signal 6 (surfaceflinger)
  ```
  **Root cause (proven):** `androidboot.hardware.egl=emulation` makes the guest load the legacy
  **goldfish `libEGL_emulation.so`**, whose gfxstream transport is `/dev/goldfish_pipe` (or vsock) —
  a **QEMU goldfish-pipe** device our VMM does not present. Our GPU path is **virtio-gpu-3D + Mesa
  `gfxstream_vk`**, a different transport. So the guest is on the wrong gfxstream transport. Fixing
  this (present a goldfish_pipe backend, OR switch the guest to Mesa `gfxstream_vk` over our
  virtio-gpu-3D via the correct `ro.hardware.egl`/gralloc/hwcomposer selection) is the SurfaceFlinger
  pass. Because surfaceflinger is `critical`, its crash restarts zygote, and the zygote↔SF restart
  loop prevents **`system_server`** from ever forking (no `Start proc … system_server`).

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + the pre-existing winit window frameworks only;
  **no gfxstream/Metal/Vulkan**). Both `cargo build` and `cargo build --features gfxstream` compile clean.
- **`cargo test`: 23/23** (unchanged — this pass added no Rust; all image-prep is host-side scripts).
- **Phase-1 Linux boot** (`run-linux.sh`): Alpine 6.6 boots over virtio-**mmio**, spoofed MIDR
  `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)` — no panic.

### Next-pass steps (in order)
1. **SurfaceFlinger GPU transport.** Make the guest reach our virtio-gpu-3D/gfxstream renderer: select
   the Mesa `gfxstream_vk` guest driver (correct `ro.hardware.egl`/`gralloc`/`hwcomposer` for the
   virtio-gpu path) instead of goldfish `libEGL_emulation`, OR present a goldfish_pipe host backend.
   Then `surfaceflinger` should init RenderEngine → boot animation → drive the gfxstream→Metal path
   (screenshot; `triangleClosed=true` only if seen).
2. With SF stable, the zygote↔SF loop breaks → **system_server** forks → watch for
   `Boot is finished` / `boot-completed`.
3. (Later hardening) Implement **real KeyMint key ops** in `secure_env` so `/data` can use genuine
   metadata-encryption + FBE instead of the plain-ext4 fast path.

---

## Pass 6 (SurfaceFlinger) — Android renders to our window: the "android" boot logo, screenshot-verified

**SurfaceFlinger is up and presenting real Android pixels into the `aemulator-hvf — guest display`
window** (`screenshots/phase4-pass6-android-bootlogo.png`). The pass-5 GPU-transport wall is cleared
by routing the guest onto a rendering path our VMM can actually serve **today**, and boot advanced
past the zygote↔SF crash loop into **`system_server`**.

### The honest render path — SwiftShader (guest CPU), NOT gfxstream→Metal
The frame on screen is produced by **ANGLE-over-SwiftShader**: SurfaceFlinger's RenderEngine runs
GLES on `libEGL_angle.so` → ANGLE → **`vulkan.pastel.so` (SwiftShader, CPU Vulkan)** — entirely on the
guest CPU, no host GPU, **no gfxstream, no MoltenVK, no Metal, no ASG**. The `composer3-service.ranchu`
HWC3 (in `mode=client`, `display_finder_mode=drm`) discovers **our virtio-gpu KMS display**
(`init display:0 … w:1024 h:768 … vsync:60`) and client-composites the single output buffer to it via
DRM; our **Phase-2 virtio-gpu 2D scanout → window presenter** shows it. So: **`triangleClosed=false`**
by the schema's strict definition (that boolean means "via gfxstream→Metal", which did NOT render this
frame), but **`reachedSurfaceFlinger=true`** and **Android pixels are screenshot-verified on screen**.
This runs on the **DEFAULT hermetic build** (no `--features gfxstream`, no `--gpu3d`).

### What was diagnosed + fixed (each step, verified from the wire)
1. **Capset mismatch (fixed, verified).** The device advertised gfxstream capset **1 (VIRGL)**, which
   this gfxstream host build does **not** implement (`host/VirtioGpuFrontend.cpp` `getCapset`/`fillCaps`
   handle only `GFXSTREAM_VULKAN(3)`/`MAGMA(7)`/`GLES(8)`/`COMPOSER(9)` per
   `host/virtgpu_gfxstream_protocol.h`). The guest's `GET_CAPSET(1)` (minigbm's
   `DRM_IOCTL_VIRTGPU_GET_CAPS`) hit `fillCaps: "Incorrect capability set specified"` and failed.
   **Fix:** `GFXSTREAM_CAPSETS = [3]` in `src/virtio/gpu3d.rs` (was `[3,1]`). Verified: the
   "Incorrect capability" host error is **gone** (count 0) after the change.
2. **gfxstream/ASG transport is a dead-end for now (proven, documented).** Setting
   `androidboot.hardware.gltransport=virtio-gpu-asg` moved the guest gfxstream userspace **off** the
   QEMU `goldfish_pipe` (which we don't present) **onto our virtio-gpu** — the `goldfish_pipe` error
   disappeared — but EGL still failed: on Android BOTH `virtio-gpu-asg` and `virtio-gpu-pipe` resolve
   to `GFXSTREAM_TRANSPORT_VIRTIO_GPU_ADDRESS_SPACE` (Address Space Graphics) in the guest
   (`guest/renderControl_enc/GfxStreamRenderControl.cpp`), and **ASG needs a host-visible virtio-gpu
   blob ring** (`VIRTIO_GPU_F_RESOURCE_BLOB` + `RESOURCE_MAP_BLOB` mapped into guest IPA via
   `hv_vm_map` + the `stream_renderer_resource_map` host pointer). We advertise **neither** the blob
   feature nor a host-visible memory region, so the ASG ring can't be created → the gfxstream→Metal
   path stays blocked. This is the real remaining GPU work (see next steps).
3. **The SwiftShader escape hatch (the win).** The Cuttlefish image ships `libEGL_angle.so` +
   `vulkan.pastel.so` (SwiftShader) — the `guest_swiftshader` GPU mode that needs **no host GPU
   transport at all**. Props: `egl=angle vulkan=pastel gralloc=minigbm`. SurfaceFlinger's RenderEngine
   then initialized cleanly: `renderer: ANGLE (Vulkan 1.3.0 (SwiftShader Device …)) … OpenGL ES 3.1.0`.
4. **Composer device-config SIGABRT (fixed).** `composer3-service.ranchu` then crashed in
   `cuttlefish::GetDeviceConfig()` (the `DeviceConfig` copy-ctor) — it fetches screen params from the
   Cuttlefish **config server over vsock** (`ro.boot.cuttlefish_config_server_port`), which we don't
   run. **Fix:** `androidboot.hardware.hwcomposer.display_finder_mode=drm` +
   `androidboot.hardware.hwcomposer.mode=client` → the HWC uses the **DRM display finder** (our
   virtio-gpu KMS, `[drm] number of scanouts: 1`) instead of the vsock config server, and
   client-composition instead of host composition. Composer then init'd `display:0 1024x768@60` and
   stopped crashing.
5. **Result:** `BootAnimation … ShownTiming`, SF stable (**0** SF/composer SIGABRTs in the windowed
   run), the zygote↔SF restart loop broke, and **`system_server` forked and started**. The window
   shows the "android" boot logo (Cuttlefish ships "No animation file", so the static boot screen is
   the composited output). Two screenshots captured ~60s apart, both show Android pixels.

### The harness (repeatable, no VMM behaviour change for the win)
`run-android.sh` gained a **`SWRENDER=1`** mode that selects the SwiftShader props above on the default
hermetic build (`SWRENDER=1 ./run-android.sh --display`). The `--gpu3d` path additionally now bakes
`gltransport=virtio-gpu-asg` for the (still-blocked) gfxstream route. Capture:
`screencapture -x -R<win> out.png`.

### The exact next stall (past the graphics milestone)
`system_server` **forks but does not stabilize** — it restart-loops early in its own bring-up
(repeated `Using CollectorTypeCC GC` / `Not starting debugger`), so boot does **not** reach
`boot_completed` / the launcher. This is a system_server/HAL/service-dependency stall (e.g. a HAL that
needs vsock or a 32-bit service such as `drmserver`, which fails `Exec format error` — no AArch32 on
Apple Silicon), **not** a graphics stall. The SurfaceFlinger/GPU milestone for this pass is met and
screenshot-verified.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + AppKit/CoreGraphics/QuartzCore winit frameworks
  only; **no gfxstream/Metal/Vulkan/libEGL**). Both `cargo build` and `cargo build --features
  gfxstream` compile clean. The SwiftShader win runs on the **default** build.
- **`cargo test`: 23/23 default, 26/26 gfxstream** (capset change is behind `--features gfxstream`; no
  test churn).
- **Phase-1 Linux boot** (`run-linux.sh`): Alpine 6.6.134 boots over virtio-**mmio**, spoofed MIDR
  `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)` — no panic.

### Next-pass steps (in order)
1. **Stabilize `system_server` → launcher.** Diagnose the system_server restart loop (likely a missing
   HAL/service or the 32-bit `drmserver`); prune/replace the failing 32-bit services (same technique as
   `prep-super.sh`'s BoringSSL self-test disable), then watch for `boot_completed` + the launcher
   surface (still rendered by SwiftShader → our scanout, so the display path is already proven).
2. **The real gfxstream→Metal triangle (`triangleClosed`).** Implement virtio-gpu **host-visible blob
   memory**: advertise `VIRTIO_GPU_F_RESOURCE_BLOB` + a PCI shared-memory region, handle
   `RESOURCE_CREATE_BLOB(HOST3D)` + `RESOURCE_MAP_BLOB`/`UNMAP_BLOB` by calling
   `stream_renderer_resource_map` and `hv_vm_map`-ing the host pointer into the guest IPA window, and
   wire the **async `FENCE_CALLBACK` → guest fence-ring + device SPI** (currently a no-op in
   `display/metal.rs`). That unblocks the ASG ring so `egl=emulation`/`vulkan=ranchu` (gfxstream) run
   on MoltenVK/Metal — the GPU-accelerated path — instead of CPU SwiftShader.

---

## Pass 7 (SystemServer) — the system_server restart loop is FIXED: it stabilizes, scans all packages, and runs deep into `startOtherServices` (next wall: the 32-bit audio HAL)

**The pass-6 `system_server` restart loop is diagnosed to the byte and fixed.** system_server no longer
dies every ~5s at ART bring-up; it now runs as a **single stable instance** that completes the
first-boot **PackageManagerService scan (all packages)**, advances through init phases **100 → 200**,
and reaches **`SystemServer.startOtherServices()`** — orders of magnitude further than pass 6. The pass
delivered **no Rust changes** (default build byte-identical to pass 6; all fixes are host-side
image-prep, exactly the `prep-super.sh` technique). It then hits a **new, cleanly-isolated wall**: the
Android `Watchdog` kills system_server after 66s because `AudioService.<init>` blocks on the absent
**32-bit audio HAL**.

### Root cause of the loop (proven from logcat on hvc2, `/tmp/ss-boot-pass6-diagnosis.log`)
The `aosp_cf_arm64` image is a **dual-ABI (`zygote64_32`) build**. Apple Silicon (M2) has **no AArch32
execution state**, so every 32-bit binary yields `cannot execv(...): Exec format error` and the
service exits **status 127**. Four such services loop: `zygote_secondary` (`/system/bin/app_process32`),
`drm` (`/system/bin/drmserver`), `media` (`/system/bin/mediaserver`), `vendor.audio-hal`
(`/vendor/bin/hw/android.hardware.audio.service`).

The **fatal** one is `zygote_secondary`. Its `init.zygote64_32.rc` block carries **`onrestart restart
zygote`**. So the sequence, every ~5s, was (verbatim):
```
init: Service 'zygote_secondary' (pid 512) exited with status 127
init: Sending signal 9 to service 'zygote' (pid 513) process group...   <- kills the PRIMARY zygote's
                                                                            process group…
Zygote  : Process 502 exited due to signal 9 (Killed)                    <- …which contains the freshly
Zygote  : Exit zygote because system server (pid 502) has terminated        forked system_server.
```
The 64-bit primary zygote fully preloads (17308 classes), forks `system_server` ("System server
process NNNN has been created" → ART "Using CollectorTypeCC GC"), and ~5s later the **whole process
group** is signal-9'd by the secondary's `onrestart`. Observed **43×** in the pass-6 log (fork → GC →
group-kill → repeat) — that is the "repeated Using CollectorTypeCC GC / jdwp" symptom, precisely.

### The fix — `prep-disable-32bit.sh` (new; wired into `run-android.sh`; idempotent)
Surgical, **same-length EROFS file-data edits** in `super.raw.img` (verity disabled; these bytes are in
`system_a` file-data far from any EROFS block-0 superblock → no csum recompute — same reasoning as
`prep-super.sh`):
1. `zygote_secondary` **`class main` → `class hvfx`** — a class no `class_start` ever names.
2. `zygote_secondary` **`onrestart restart zygote` → `#nrestart restart zygote`** — the **essential
   loop-breaker**: the 32-bit secondary can no longer kill the primary zygote / system_server. (Kept as
   belt-and-suspenders because `init.rc`'s `on zygote-start` action does an explicit `start
   zygote_secondary`, which bypasses the class — so the secondary still 127-loops harmlessly, but with
   **0** primary-zygote group-kills.)
3. `drmserver` — comment its two `start drm` property triggers (`start` → `#tart`); it is `disabled`,
   so those are its only start path.
4. `mediaserver` **`class main` → `class hvfx`** — stop its `class_start main` autostart loop.

Block bounding uses a **first-NUL/non-rc-byte truncation** (`rc_block()`), NOT a scan for the next
`\nservice ` — because each `.rc` file is a separate EROFS inode followed by binary padding, so a
naïve window bleeds into the *next* file (a first draft clobbered `mtpd`'s class; reverted + fixed).
Idempotent: a re-run is all no-ops. 23/23 `cargo test`; **default build hermetic** (`otool -L`:
Hypervisor + winit frameworks only; no gfxstream/Metal/Vulkan).

### After the fix — system_server stabilizes and runs deep (verbatim, `/tmp/ss-fix.log`)
```
== prep disable 32-bit services (zygote_secondary loop-breaker + drm/media) ==
init: ... started service 'zygote' has pid 316           <- primary zygote — stays up
System server process 502 has been created               <- ONE instance (was 43 restarts)
SystemServiceManager: Starting phase 100                 <- PHASE_WAIT_FOR_DEFAULT_DISPLAY
SystemServiceManager: Starting phase 200                 <- boot phases advancing
   (PackageManagerService scans ALL packages — 500+ log lines — then startOtherServices)
```
**0** `Sending signal 9 to service 'zygote'` from the secondary cascade (was every 5s). `drmserver` /
`mediaserver` no longer `execv`-loop (0 Exec-format errors for either). Highest milestone:
**`system_server` reaches `SystemServer.startOtherServices()` (AudioService construction).**

### The exact next stall (a DIFFERENT, deeper wall — the 32-bit audio HAL)
system_server is then killed at 66s by its own **Watchdog** (verbatim):
```
Waiting for service 'media.audio_policy'                                  (46×; AudioService blocks here)
Watchdog: *** WATCHDOG KILLING SYSTEM PROCESS: Blocked in handler on  on main thread (main) for 66s
Watchdog:     at com.android.server.audio.AudioService$Lifecycle.<init>(AudioService.java:939)
Watchdog:     at com.android.server.SystemServer.startOtherServices(SystemServer.java:2195)
init: Exit zygote because system server (pid 502) has terminated
```
**Chain (proven):** `AudioService.<init>` (system_server main thread) → waits for **`media.audio_policy`**
→ `audioserver` (64-bit, starts fine) → `AudioPolicyService`/`AudioFlinger` → **`DevicesFactoryHalAidl`
blocking-waits for the AIDL audio HAL `android.hardware.audio.core-V1-ndk` (`vendor.audio-hal`)** →
that HAL is the **32-bit `/vendor/bin/hw/android.hardware.audio.service`** → `Exec format error`, never
registers → `audioserver` never publishes `media.audio_policy` → `AudioService.<init>` blocks 66s →
`Watchdog` SIGKILLs system_server → zygote exits "because system server has terminated" → a **slower,
66-second** restart loop (system_server pids 502 → 2153 → 2790 across the run). This is **not** the
pass-6 zygote cascade (that is fixed); it is a genuine **HAL-provisioning** wall.

`vendor.audio-hal` was deliberately **left enabled** this pass (it has several `start vendor.audio-hal`
triggers across `audioserver.rc`; disabling it cleanly needs more edits, and — proven now — even fully
disabling it would NOT unblock boot, because `audioserver` blocks on the HAL's **VINTF declaration**
via `AServiceManager_waitForService`, not on the service's presence). The real fix is a **64-bit audio
HAL** the guest can actually run.

### Regression — Phase 1/2/3 still green
- **No Rust changed** — default build byte-identical to pass 6. `otool -L`: Hypervisor + winit
  frameworks only (**hermetic**). `cargo build` + `cargo build --features gfxstream` both clean.
- **`cargo test`: 23/23** (default). **Phase-1 Linux boot** (`run-linux.sh`): Alpine 6.6.134 boots over
  virtio-**mmio**, spoofed MIDR `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks
  (67.1 MB)` — no panic.

### Next-pass steps (in order)
1. **Provide a runnable audio HAL so `AudioService` doesn't block.** Best options, in order of
   tractability: (a) switch to the **`aosp_cf_arm64_only_phone`** target, whose HALs are all 64-bit
   (kills the entire 32-bit-HAL whack-a-mole class at the root); (b) point the guest at the **stub
   audio policy config** (`audio_policy_configuration_stub.xml`, a software no-op module needing no
   HAL) so `AudioPolicyManager` registers `media.audio_policy` with no HW module; (c) a minimal guest
   **AIDL `android.hardware.audio.core.IModule` stub** that just registers. Then re-check whether the
   NEXT service in `startOtherServices` also blocks on a 32-bit HAL (sensors/thermal/etc.) — expect
   more of the same class until the image is 64-bit-only.
2. With system_server past `startOtherServices` → watch for **`SystemServiceManager: Starting phase
   1000`** / `sys.boot_completed=1` / `PhoneWindowManager … systemReady`, then the launcher (rendered by
   SwiftShader → our virtio-gpu scanout, already proven).
3. (Unchanged) the real gfxstream→Metal `triangleClosed` — virtio-gpu host-visible blob memory + ASG.

---

## Pass 8 (Audio HAL wall) — root-caused to a HARD dual-ABI image wall; launcher NOT reachable on THIS image

**Goal:** get system_server past the pass-7 `AudioService.<init>` Watchdog wall → `boot_completed` →
launcher. **Result:** the wall is **root-caused to the byte with ground-truth EROFS extraction and
proven to be a hard image (not VMM) wall** that no clean host-side edit can clear on this dual-ABI
image. The launcher is therefore **not reached this pass** (`launcherShown=false`); the display path is
re-confirmed rendering the "android" boot logo (`screenshots/phase4-pass8-android-bootlogo.png`).
**No Rust changed** — the default build is byte-identical to pass 6/7 (hermetic; `cargo test` 23/23;
Phase-1 Linux boot green: Alpine 6.6.134, MIDR `0x511f8041`, `virtio_blk [vda] 131072 blocks`).

### The wall, proven from the wire + ground-truth image extraction
`system_server` forks (pid 502), reaches `Starting phase 200`, then `AudioService.<init>`
(`SystemServer.startOtherServices`) blocks on the main thread waiting for **`media.audio_policy`**,
which `audioserver` never publishes because **`AudioPolicyService::onFirstRef` → AudioFlinger →
loadHwModule → the audio devices-factory** can never be created — the Android Watchdog SIGKILLs
system_server at ~66-67s and zygote exits, a slow restart loop.

The audio HAL is the **legacy HIDL passthrough** stack. With `erofs-utils` (installed this pass:
`brew install erofs-utils` → `fsck.erofs --extract`) the `vendor_a` EROFS was extracted and every
audio binary's ELF class read directly:
- `/vendor/bin/hw/android.hardware.audio.service` — **ELF 32-bit ARM** (the only passthrough-registrant
  host binary; `Exec format error` on Apple Silicon, which has no AArch32).
- `/vendor/lib64/hw/android.hardware.audio@7.1-impl.ranchu.so`, `audio.primary.default.so`,
  `android.hardware.audio.effect@7.0-impl.so` — **ELF 64-bit** (the impls exist in 64-bit!).
- The vendor VINTF declares the HAL as **`android.hardware.audio@7.1::IDevicesFactory/default`
  transport=`hwbinder`** (plaintext fragment at super byte 1139002999, inside `vendor_a`).

So the impls are 64-bit but their **only host process is the 32-bit service binary**, and under
**Treble the system-domain `audioserver` cannot dlopen `/vendor/lib64` in-process** (linker-namespace
isolation — not SELinux, which is permissive here), so it can't host the passthrough impls itself.

### Two host-edit approaches EMPIRICALLY dead-ended (both → Watchdog)
1. **HAL declared (pristine):** `IDevicesFactory::getService` sees it declared-but-unregistered and
   lazy-starts the 32-bit binary (`Exec format error`, forever) → `onFirstRef` blocks →
   `media.audio_policy` never publishes → 66s Watchdog. (verbatim: `DevicesFactory/default is not
   registered, trying to start it as a lazy HAL …` ×15; `AudioPolicyService not published, waiting…`
   ×43.)
2. **HAL undeclared** (`prep-audio-stub.sh`: same-length plaintext edit
   `<name>android.hardware.audio</name>` → `…audiX`, verified `hwservicemanager: Cannot find entry
   android.hardware.audio@{7.1,7.0,6.0,5.0,4.0}::IDevicesFactory` for every version): the probe returns
   null fast — but then **`AudioFlinger::AudioFlinger()` SIGSEGVs at +2696** (tombstone pid 759)
   dereferencing the now-null devices-factory → audioserver crash-loops → `media.audio_policy` still
   never publishes → 67s Watchdog. `AudioFlinger` **hard-requires** a non-null audio devices factory.
   → `prep-audio-stub.sh` is kept as a documented **dead-end** (DEFAULT-OFF; `PREP_AUDIO_STUB=1` repro).

### Why the root fix is unavailable
The clean fix is a **64-bit vendor audio service** (the all-64-bit **`aosp_cf_arm64_only_phone`**
target ships one, killing the whole 32-bit-HAL class). Verified this pass that this target **is not
built on public AOSP CI** — the build API returns no builds for `aosp_cf_arm64_only_phone-userdebug`
on `aosp-android14-gsi`, `aosp-android15-gsi`, or `aosp-main` (the query is correct: the working
`aosp_cf_arm64_phone-userdebug` returns builds). No 64-bit audio-service host binary exists anywhere in
the current image, and defeating Treble's linker isolation (widening `audioserver`'s namespace to
`/vendor/lib64` via the runtime-generated `/linkerconfig/ld.config.txt`) is not a clean host-side edit.

**Conclusion: on THIS dual-ABI `aosp_cf_arm64_phone` image the launcher is blocked by a 32-bit vendor
HAL that cannot run on Apple Silicon and cannot be synthesized host-side.** This is an **image
problem, not a VMM problem** — the VMM (virtio-PCI fleet, composite disk, 12 HVC + secure_env,
SurfaceFlinger scanout) is proven through second-stage, keystore2, zygote, and SurfaceFlinger.

### Phase 4 core-boot status: NOT complete (Android renders but is not yet usable)
Highest milestone remains **`system_server` at `startOtherServices` (AudioService)** + a
**screenshot-verified boot logo**. `boot_completed`/launcher are **not** reached. Remaining is
fundamentally the 32-bit-HAL class (audio now, then likely sensors/thermal/etc.), curable only by a
64-bit-userspace image or a built 64-bit audio service.

### Next-pass steps (in order)
1. **Obtain a 64-bit-userspace guest.** Either (a) build `aosp_cf_arm64_only_phone` from AOSP source
   (needs an Android build env — not available in this session), or (b) build just a **64-bit
   `android.hardware.audio.service`** (or a minimal 64-bit AIDL `IModule`/HIDL `IDevicesFactory`
   passthrough registrant) against the vendor 64-bit `.so`s and inject it into `vendor_a` via
   `mkfs.erofs` (erofs-utils is now installed; verity is disabled so no hashtree). Either publishes
   `media.audio_policy` → AudioService returns → system_server proceeds.
2. Then re-run and capture the **next** `startOtherServices` blocker (expect more 32-bit HALs on the
   dual-ABI image; each is the same class) until `Starting phase 1000` / `sys.boot_completed=1` /
   `PhoneWindowManager … systemReady` → the launcher (SwiftShader → our virtio-gpu scanout, proven).
3. (Unchanged) the real gfxstream→Metal `triangleClosed` — virtio-gpu host-visible blob memory + ASG.

---

## Pass 9/10 (BootCompleted) — 64-bit-only image reaches `sys.boot_completed` + the launcher runs; the Android **lock screen** renders into our window (screenshot-verified)

**The Phase-4 core-boot finish line is reached.** On the 64-bit-only `aosp_cf_arm64_only_phone`
(Android 16, kernel 6.12.74) image, Android now boots all the way to **`SystemServer` boot phase 1000
(`PHASE_BOOT_COMPLETED`) + "Boot is finished"**, **QuickstepLauncher (the home launcher) forks and
RESUMES**, and the **real Android lock screen** (keyguard: live clock, date, battery 85%, "Emergency
calls only", "Unlock for all features and data", Android-16 wallpaper) renders through our virtio-gpu
2D scanout into the `aemulator-hvf — guest display` window. Screenshot-verified via a **direct guest
framebuffer dump** (`screenshots/phase4-launcher.png` + `phase4-lockscreen.png`). This is real Android
UI far beyond the boot logo, on a booted-to-completion system. Pass 8 died at `AudioService.<init>` on
the dual-ABI image; this pass is past audio, past `system_server`, into the running launcher.

### Root causes cleared this pass (each verified from logcat on hvc2)
1. **The 32-bit HAL class is gone** — moving to the 64-bit-only image (pass 9) removed the audio-HAL
   Watchdog wall (audioserver's 64-bit AIDL HAL `vendor.audio-hal-aidl` registers
   `android.hardware.audio.core.IModule/default`; the `service vendor.audio-hal not found` init lines
   are HARMLESS non-fatal `onrestart` noise referencing the retired HIDL name).
2. **`system_server` bootstrap Watchdog → the vsock-HAL class (fix: `prep-disable-vsock-hals.sh`).**
   Three Cuttlefish HALs hard-depend on a host cuttlefish backend over **virtio-vsock**, which we do
   not present, so each aborts + crash-loops: `vendor.light-cuttlefish` (`android.hardware.light`),
   `vendor.sensors-hal-multihal` (`android.hardware.sensors`: "Could not connect to sensors control"),
   `vendor.threadnetwork_hal` (`android.hardware.threadnetwork`: "Check failed: node_id > 0" — needs a
   vsock CID). While DECLARED in vendor VINTF, `LightsService.<init>` (in `startBootstrapServices`)
   blocked on `waitForDeclaredService` → **Watchdog killed system_server** (751→1094→1550 restart
   loop); and even undeclared, the crash-loop churn + `sys.init.updatable_crashing` health-check
   storms + audit-log flooding **starved** system_server so it crawled at phase 500. The new prep
   (a) VINTF-undeclares all three (waitForDeclaredService returns null; LightsService/SensorService
   tolerate an absent HAL), (b) neuters the sensors init service (`class hal`→`class hxl`), and
   (c) disables the light + threadnetwork **APEXes** (rename `/vendor/apex/*.apex`→`.apeX`, so their
   in-APEX `.rc` + VINTF fragment vanish → zero crash churn; apexd tolerates the absent optional
   module, "Activated 85 packages", no bootstrap failure).
3. **`FATAL EXCEPTION IN SYSTEM PROCESS: PersistentDataBlockService init timeout` at phase 500
   (fix: add the `frp` partition to the composite disk).** system_server's PDB service reads
   `ro.frp.pst=/dev/block/by-name/frp` on a background init thread; the VMM's composite GPT disk had
   NO `frp` partition, so the read threw "persistent partition not readable", the init-done signal
   never fired, `onBootPhase(500)` timed out, and **every system_server instance crash-looped**.
   `src/bin/aemulator-hvf.rs` now fabricates an empty writable `frp` partition
   (`Part("frp.img","frp",true,true)`) — PDB formats the uninitialized block itself (deactivateFrp),
   signals init-done, and boot sails **500 → 1000 (boot_completed)** with a single stable
   system_server. This was the last hard wall.

### The boot trail (verbatim highlights, `SWRENDER=1 ./run-android64.sh --display`)
```
[android] fabricated/grew frp.img → 16 MiB                          <- frp partition present
apexd: Activated 85 packages ...                                    <- light/thread APEXes pruned; no fail
init: ... started service 'zygote' has pid ...
Zygote  : System server process 759 has been created               <- ONE instance, no restart loop
SystemServiceManager: Starting phase 500                            <- past PDB (no init-timeout crash)
SystemServiceManager: Starting phase 1000                           <- PHASE_BOOT_COMPLETED
ActivityManager: Displayed com.android.settings/.FallbackHome
ActivityManager: Start proc ...:com.android.launcher3/... for ...   <- launcher forks
WindowManager: wm_on_resume_called: [...,com.android.launcher3.uioverrides.QuickstepLauncher,RESUME_ACTIVITY]
[display] scanout gen=... non_zero_fb_bytes=3145728                 <- full 1024x768 frame = lock screen
```
**Highest milestone: `boot_completed` (phase 1000) + QuickstepLauncher RESUMED.** Screenshot:
`screenshots/phase4-launcher.png` (the Android lock screen, screenshot-verified from the guest FB).

### VMM changes this pass (default build stays hermetic)
- `src/bin/aemulator-hvf.rs` — add the `frp` composite-disk partition (fabricated empty, writable).
- `src/display/mod.rs` — **`AEMU_FB_DUMP=<path.ppm>`**: env-gated direct guest-framebuffer PPM dump
  (every diag tick) so a booted UI is screenshot-able on a **headless/remote host** (macOS
  `screencapture` returns black when the host screen is asleep/locked — the guest FB does not). Also
  **`AEMU_SWIPE_UNLOCK=<sec>`**: scripted touch-drag + MENU/HOME/ENTER key injection (keyguard-dismiss
  attempt; see blocker below).
- `src/virtio/input.rs` — advertise **`INPUT_PROP_POINTER`** on the tablet. Without any INPUT_PROP_*,
  Android classifies the ABS+BTN_TOUCH device as a direct TOUCHSCREEN and rejects it ("could not query
  the properties of its associated display. The device will be inoperable"), dropping every touch. The
  pointer prop binds it to the default 1024x768 viewport as an operable indirect pointer.

### The exact remaining blocker (well-isolated — the home-screen icons are behind the keyguard)
The launcher is running and RESUMED, but sits **behind the swipe lock screen**, so the screenshot is
the lock screen, not the app-drawer/home-icons. Dismissing the (insecure, swipe-only) keyguard needs a
real **DIRECT touchscreen swipe**, which is blocked by an Android input↔display integration gap: our
virtio-gpu display exposes **no `DisplayViewport` with physical properties**, so InputReader deactivates
a DIRECT touch device ("could not query the properties of its associated display" / "deactivated
because the associated viewport is not found"). Workarounds attempted and their results:
- **`INPUT_PROP_POINTER`** → the device becomes operable, but in POINTER (touchpad) mode Android uses
  RELATIVE pointer gestures, so a scripted ABSOLUTE bottom→top drag does not produce a directional
  screen swipe → keyguard not dismissed.
- **Keyboard MENU/HOME/ENTER** (the EV_KEY path IS operable) → the Android-16 keyguard ignores them
  (modern keyguard removed key-dismiss). No `keyguardGoingAway`.
The correct fix is to make the virtio-gpu display report a proper DisplayViewport (physical size/mm)
so a DIRECT (absolute) touchscreen associates and an absolute swipe dismisses the keyguard — a
display/input-integration item (Phase-6-shaped), OR disabling the lock screen via a persisted
`/data` settings edit. This is the single remaining step between the screenshot-verified lock screen
and the home-icon launcher.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + AppKit/QuartzCore/CoreGraphics winit frameworks
  only; **no gfxstream/Metal/Vulkan**). The `AEMU_FB_DUMP` path is std::fs only.
- **`cargo test`: 23/23.**
- **Phase-1 Linux boot** (`run-linux.sh`): Alpine **6.6.134** boots over virtio-mmio, spoofed MIDR
  `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)`, reaches the
  initramfs rescue shell — **no panic** (the `INPUT_PROP_POINTER` change does not affect Linux boot).

### Exact repro
```
# 64-bit-only image already prepped at testdata/cf64 (see testdata/cf64/SOURCE.txt).
# run-android64.sh now also runs prep-disable-vsock-hals.sh (VINTF-undeclare + neuter light/sensors/
# threadnetwork). The frp partition is fabricated by the binary. To reach boot_completed + the lock
# screen and dump the guest framebuffer headlessly:
cd AEmulator/crates/aemulator-hvf
SWRENDER=1 AEMU_FB_DUMP=/tmp/aemu-fb.ppm ./run-android64.sh --display    # wait ~100s for boot_completed
sips -s format png /tmp/aemu-fb.ppm --out screenshots/phase4-launcher.png
# Boot markers to confirm in the console/logcat tee:
#   "SystemServiceManager: Starting phase 1000"  +  "Boot is finished"
#   "wm_on_resume_called: [...QuickstepLauncher,RESUME_ACTIVITY]"
#   "[display] scanout ... non_zero_fb_bytes=3145728"
```

---

## Pass 11 (HomeScreen) — touch works: the keyguard is swiped away and the **HOME SCREEN** renders; injected touch drives the launcher UI (screenshot-verified)

**Phase-4 core boot is COMPLETE and Phase-6 touch control is proven.** The pass-9/10 remaining
blocker — the home-screen icons stuck behind an un-dismissable swipe keyguard — is **fixed**. A
scripted bottom→top swipe now dismisses the insecure keyguard (`LOCKSCREEN → GONE`), the
**QuickstepLauncher home screen renders** (app icons + hotseat + status bar) into our virtio-gpu
scanout, and a second injected gesture makes the launcher **respond interactively** (its home-options
popup appears). Screenshot-verified from the guest framebuffer:
- `screenshots/phase4-homescreen.png` — the home screen (Camera / Android / Messaging / Phone in the
  hotseat, Gallery on the grid, clock 3:23, battery).
- `screenshots/phase4-touch-response.png` — the launcher's "Wallpaper & style / Widgets / Apps list /
  Home settings" popup, opened by the injected touch gesture (UI demonstrably responded to touch).
- `screenshots/phase4-lockscreen-before.png` — the lock screen just before the swipe (for the
  before/after).

### The three root causes — the gap was NOT (only) INPUT_PROP; input injection never reached the guest at all

1. **No PCI input-injection path (the silent showstopper).** `spawn_input_inject_thread` searched only
   the virtio-**mmio** transports (`ctx.virtio`), which are **empty on the Android path** (the whole
   fleet is on the PCI host bridge). `PciBus` had `feed_console_input` but **no** input equivalent, so
   every injected touch/key frame from the display layer hit a dead `mpsc` receiver and was dropped —
   the pass-9/10 swipe/keys **never reached the guest**. Fix: `PciBus::inject_input_events` +
   `has_input` (mirrors `feed_console_input`: queue the frame on the virtio-input `EVENTQ`, flush to
   the guest buffers, set ISR, raise the device SPI), and `spawn_input_inject_thread` now routes to
   whichever transport actually carries the input device (MMIO Linux / PCI Android). *This is why the
   earlier "keyboard MENU/HOME is operable" note was wrong — nothing was operable; injection was a
   no-op.*
2. **No display viewport → DIRECT touch deactivated (EDID).** Android's InputReader deactivated the
   touch device because the internal display had no physical size (`dpi-x:-1`, "could not query the
   properties of its associated display"). Fix: `src/virtio/gpu.rs` now advertises
   **`VIRTIO_GPU_F_EDID`** and answers `GET_EDID` with a valid 128-byte EDID 1.4 base block carrying a
   physical image size derived from a target dpi (320 → 81×61 mm for 1024×768). Verified in logcat:
   the display now reports **`density 213, 321.0 x 319.0 dpi`** (was dpi -1) and the internal
   **`Viewport [0] … isActive: true`**.
3. **Touch was an indirect touchpad, not a touchscreen.** `src/virtio/input.rs` advertised
   `INPUT_PROP_POINTER`; an absolute drag became a relative touchpad motion, never a directional screen
   swipe. Fix: advertise **`INPUT_PROP_DIRECT`**. With the viewport now valid, InputReader reconfigures
   the device to **`mode DIRECT, size 1024x768, display id 0`** (verbatim logcat) — an operable
   touchscreen bound to the internal display.

### The proof trail (verbatim, `SWRENDER=1 AEMU_FB_DUMP=… AEMU_SWIPE_UNLOCK=105 ./run-android64.sh --display`)
```
InputReader: New device: … name='aemulator-tablet', classes=KEYBOARD | ALPHAKEY | TOUCH
DisplayDeviceRepository: … "Built-in Screen" … density 213, 321.0 x 319.0 dpi   <- EDID physical size
InputManager-JNI: Viewport [0] to add: local:… isActive: true                   <- viewport now active
InputReader: Device reconfigured: id=2, name='aemulator-tablet', size 1024x768, mode DIRECT, display id 0
SystemServiceManager: Starting phase 1000  +  SurfaceFlinger: Boot is finished   <- boot_completed
[display] swipe-unlock: keyguard drag 1                                          <- injected swipe REACHES guest
KeyguardViewMediator: keyguardGoingAway requested for userId: 0
KeyguardTransitionRepository: STARTED … (from=LOCKSCREEN, to=GONE, … keyguard is going away)
KeyguardTransitionRepository: FINISHED … (from=LOCKSCREEN, to=GONE, value=1.0)   <- keyguard dismissed → HOME
[display] touch-prove: app-drawer swipe-up injected                              <- launcher popup responds
```
**Highest milestone: HOME SCREEN reached + touch input demonstrably driving the guest UI.**

### VMM changes this pass (default build stays hermetic)
- `src/virtio/gpu.rs` — advertise `VIRTIO_GPU_F_EDID`; `GET_EDID` → a valid 128-byte EDID base block
  with a physical image size (`build_edid`); `resp_edid`. (+2 unit tests.)
- `src/virtio/input.rs` — advertise `INPUT_PROP_DIRECT` (touchscreen) instead of `INPUT_PROP_POINTER`.
  (+1 unit test.)
- `src/virtio/pci.rs` — `PciBus::inject_input_events` + `has_input` (the missing PCI input path).
- `src/machine.rs` — `spawn_input_inject_thread` routes input to the MMIO **or** PCI transport,
  whichever carries the virtio-input device.
- `src/display/mod.rs` — `AEMU_SWIPE_UNLOCK` reworked into clean DIRECT-touchscreen vertical drags
  (down → interpolated moves → up), a keyguard-dismiss pass + an app-drawer swipe-up "touch-prove" pass.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + winit frameworks only; no gfxstream/Metal/Vulkan).
  Both `cargo build` and `cargo build --features gfxstream` compile clean.
- **`cargo test`: 26/26** (23 prior + 2 EDID + 1 INPUT_PROP_DIRECT).
- **Phase-1 Linux boot** (`run-linux.sh`): Alpine 6.6.134 boots over virtio-**mmio**, spoofed MIDR
  `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)`, virtio-gpu 2D +
  virtio-input tablet enumerate — no panic. The EDID/INPUT_PROP/PCI-inject changes don't affect the
  MMIO Linux path.

### Exact repro
```
cd AEmulator/crates/aemulator-hvf
SWRENDER=1 AEMU_FB_DUMP=/tmp/aemu-fb.ppm AEMU_SWIPE_UNLOCK=105 ./run-android64.sh --display
# ~90s → boot_completed; the swipe fires at 105s and dismisses the keyguard → home screen.
# Snapshot the guest FB (independent of host screen sleep) at the right moment:
sips -s format png /tmp/aemu-fb.ppm --out screenshots/phase4-homescreen.png
# logcat markers on hvc2: "mode DIRECT, display id 0", "density … 321.0 x 319.0 dpi",
#   "from=LOCKSCREEN, to=GONE, value=1.0".
```

### Remaining
- The "touch-prove" gesture currently lands the launcher's long-press **home-options popup** rather
  than fully opening the all-apps drawer (gesture tuning — the down→up timing reads as a long-press);
  touch reaching + driving the UI is nonetheless screenshot-proven. Tightening the fling into a clean
  app-drawer open is cosmetic.
- The real gfxstream→Metal `triangleClosed` (virtio-gpu host-visible blob memory + ASG) is still the
  separate GPU-accel task; today's frames are SwiftShader → our 2D scanout (unchanged from pass 6).

---

## Pass 12 (BlobMem/ASG) — host-visible blob memory + ASG ring live: the guest's Mesa `gfxstream_vk` drives our host renderer on MoltenVK/Metal (SurfaceFlinger + launcher create host VkDevices); next wall is a host gfxstream VkDecoder extension gap

**The missing piece for the real gfxstream path is built and PROVEN engaged.** Behind `--features
gfxstream`, the virtio-gpu device now advertises **`VIRTIO_GPU_F_RESOURCE_BLOB`** + a **host-visible
PCI shared-memory BAR** (BAR2, 64-bit prefetchable, `PCI_SHM_BASE=0x2_0000_0000`+1 GiB, virtio
shared-memory capability **shmid 1** = `VIRTIO_GPU_SHM_ID_HOST_VISIBLE`), handles
`RESOURCE_CREATE_BLOB` / `RESOURCE_MAP_BLOB` / `RESOURCE_UNMAP_BLOB` by calling
`stream_renderer_resource_map` + **`hv_vm_map`-ing the host pointer into the guest IPA window**
(zero-copy ASG ring), and wires the async **FENCE_CALLBACK → guest virtio-gpu SPI**. With the guest
switched to the gfxstream_vk path (`egl=angle vulkan=ranchu gltransport=virtio-gpu-asg`), Android's
Mesa `gfxstream_vk` created the ASG ring and drove the host gfxstream renderer on MoltenVK → Metal
for **real UI apps** (SurfaceFlinger, launcher3, SystemUI, Settings, bootanimation, provision).
`triangleClosed=false` still (no screenshot — the run aborts mid-render on a host decoder gap).

### What was built (all behind `--features gfxstream`; default build byte-identical + hermetic)
- **`src/virtio/gpu3d.rs`** — `VIRTIO_GPU_F_RESOURCE_BLOB` (bit 3), `RESOURCE_MAP_BLOB`(0x0208) /
  `RESOURCE_UNMAP_BLOB`(0x0209) opcodes + `RESP_OK_MAP_INFO`(0x1106); `Renderer3d` gains
  `resource_map` / `resource_unmap` / `resource_map_info` (thin `stream_renderer_resource_map/_unmap/
  _map_info` shims) and `create_blob(…, iovs)`; a global **fence bridge** (`on_fence_complete` records
  the completion + wakes a VMM thread; `fence_count()` evidence).
- **`src/virtio/gpu.rs`** — advertises the blob feature with a renderer; `RESOURCE_CREATE_BLOB`
  resolves guest mem-entries → host iovecs; `map_blob` = `resource_map` → bounds/alignment check →
  `hv_vm_map(host_ptr → PCI_SHM_BASE+offset, RW)` → `OK_MAP_INFO`; `unmap_blob` = `hv_vm_unmap` +
  `resource_unmap`. Blob-map bookkeeping per resource.
- **`src/virtio/pci.rs`** — a 64-bit prefetchable **BAR2** host-visible window on the GPU PCI device
  (`attach_shm_bar`) + the **virtio shared-memory capability** (`virtio_pci_cap64`, cfg_type 8, bar 2,
  **id 1**) in the cap chain, with BAR2/BAR3 sizing. `gpu_intid()` for the fence thread. (+3 tests.)
- **`src/boot.rs`** — a second `ranges` entry (64-bit prefetchable, `0x4300_0000`) for the shmem window.
- **`src/display/metal.rs`** — installs the real `on_fence_complete` callback (was a no-op).
- **`src/machine.rs`** — attaches the shmem BAR to the GPU device when it advertises blob; spawns the
  **gpu-fence thread** (fence completion → raise virtio-gpu SPI + kick vCPU); fence-count teardown line.
- **`src/bin/aemulator-hvf.rs`** — `audit=0 loglevel=3` on the Android kernel cmdline (stops the
  SELinux-permissive avc-denied console flood).
- **`run-android64.sh`** — `--gpu3d` props switched to the real gfxstream path: `gralloc=minigbm
  egl=angle vulkan=ranchu hwcomposer=ranchu gltransport=virtio-gpu-asg` (+ drm/client hwcomposer,
  drawFlushInterval). **Why `egl=angle`, not `egl=emulation`:** our host dylib is a **GuestVulkanOnly**
  build (VK-only via MoltenVK — macOS has no host GLES), so `egl=emulation` aborts the host
  (`getEmulationGl: GL/EGL emulation not enabled`). `egl=angle` makes the guest's ANGLE translate
  GLES→Vulkan in-guest, so ONLY Vulkan crosses the ASG ring → gfxstream_vk → MoltenVK → Metal.

### The boot trail (verbatim highlights, `./run-android64.sh --gpu3d`, headless)
```
[ok] virtio-gpu host-visible shmem: BAR2 0x200000000+0x40000000 → guest IPA, shmid 1 (VIRTIO_GPU_F_RESOURCE_BLOB)
[gpu3d] map_blob res=9  -> ipa 0x200510000 size 0x800000 map_info 0x1 (host-visible ASG ring live)   <- 8 MiB blob, hv_vm_map'd
   … 21 successful blob maps (0x4000 … 0x800000), 0 hv_vm_map failures, 0 alignment failures …
VkDecoderGlobalState: Created VkDevice for application:surfaceflinger  engine:ANGLE        <- host GPU VkDevice, guest app
VkDecoderGlobalState: Created VkDevice for application:com.android.launcher3 engine:ANGLE  <- the LAUNCHER on Metal via ASG
   … + systemui, settings, bootanimation, provision …
SurfaceFlinger: Threaded RenderEngine with SkiaGL Backend (Ganesh)                          <- RenderEngine up (no EGL abort)
BootAnimation: BootAnimationShownTiming start
ActivityTaskManager: Displayed com.android.settings/.FallbackHome for user 0                <- home displayed, clock synced
 reservedunmarshal_extension_struct, Unhandled Vulkan structure type
   VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT [1000237000], aborting.   <- the next wall (host decoder)
```

**How far the gfxstream pipeline gets (observed):** host-visible blob memory + the ASG ring are fully
live — 21 blobs mapped host↔guest via `hv_vm_map` (every size a 16 KiB-granule multiple, so the 4 KiB
guest kernel never tripped HVF's alignment requirement), the guest's `gfxstream_vk` created the
`AddressSpaceStream`, and the host gfxstream renderer created **real Vulkan devices on MoltenVK/Metal
for six guest processes including SurfaceFlinger and QuickstepLauncher** (guest engine string:
`ANGLE`, i.e. GLES→Vulkan→gfxstream_vk→ASG→MoltenVK). This is the gfxstream→Metal path engaged for
real Android UI, not SwiftShader.

**The exact next blocker (host gfxstream, NOT the VMM/blob/ASG layer):** the vendored host gfxstream
`VkDecoder::unmarshal_extension_struct` **aborts** on
`VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT (1000237000)` — the guest ANGLE queries
`VK_EXT_memory_budget`, which this gfxstream host build doesn't decode → `GFXSTREAM_ABORT` → the VMM
process dies. Fix is host-side: rebuild gfxstream with that extension handled, or have ANGLE not
request `VK_EXT_memory_budget` (feature override). Because the abort kills the renderer mid-render,
**no frame was screenshot-captured → `triangleClosed=false` (rendering NOT visually claimed)**; the
gfxstream/Metal path is proven only by the host VkDevice creations + the guest ANGLE engine string.

**The key bug found + fixed this pass:** the virtio shared-memory capability shmid must be **1**
(`VIRTIO_GPU_SHM_ID_HOST_VISIBLE`), not 0 (`SHM_ID_NONE`) — with id 0 the guest kernel's
`virtio_get_shm_region(HOST_VISIBLE)` never matched, so `VIRTGPU_PARAM_HOST_VISIBLE` stayed 0 and Mesa
failed `VirtGpuDevice must support host-visible memory / Failed to create virtgpu AddressSpaceStream`.
Flipping id 0→1 is what unlocked every `map_blob` above.

### Regression — Phase 1/2/3 still green
- **Default build hermetic** (`otool -L`: Hypervisor + winit frameworks only; **no
  gfxstream/Metal/Vulkan**), warning-clean; `--features gfxstream` also warning-clean.
- **`cargo test`: 32 default / 39 gfxstream** (+4 new: `renderer_device_advertises_resource_blob`,
  `shm_bar2_reports_64bit_prefetchable_base_and_size`, `shm_bar2_sizing_returns_region_length_mask`,
  `shared_memory_capability_present_with_host_visible_shmid`).
- **Phase-1 Linux boot** (`run-linux.sh`, default build): Alpine 6.6.134 boots over virtio-**mmio**,
  spoofed MIDR `0x511f8041`, `virtio_blk virtio2: [vda] 131072 512-byte logical blocks (67.1 MB)`.

### Next-pass steps (in order)
1. **Clear the host VkDecoder gap** — rebuild the vendored gfxstream host so
   `unmarshal_extension_struct` handles `VK_EXT_memory_budget`
   (`VkPhysicalDeviceMemoryBudgetPropertiesEXT`), OR disable that ANGLE feature; then re-run and
   capture the next `VkDecoder` gap (expect a few more extension structs until the guest's ANGLE VK
   feature set is fully covered).
2. With the decoder stable, drive a frame all the way through hwcomposer(ranchu,drm,client) → our
   virtio-gpu scanout and **screenshot the guest FB** (`AEMU_FB_DUMP`) — only then `triangleClosed=true`.
3. Consider deferring the fenced-command used-ring completion until the async fence fires (today the
   device completes synchronously + the fence thread pokes the IRQ) if a later stage needs strict
   host-drain ordering.
