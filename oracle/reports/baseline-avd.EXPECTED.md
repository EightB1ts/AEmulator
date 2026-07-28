# Expected baseline — signals a stock ranchu emulator trips

_Reference catalogue (from the research dossier), pending a live `detect-baseline.sh` run._

This is what we **expect** a stock Android Studio `arm64-v8a` (goldfish/ranchu) image to
trip when the detection catalogue is run against it. `detect-baseline.sh` produces the
ground-truth `baseline-avd.md` that confirms/denies each row against a live boot. Strings
below are verbatim from the research (`hw-identifiers` + `detection-adversary`).

Every 🔴 here is a signal a real AEmulator build **must not** trip — this table is the
adversary's-eye acceptance target for the `aemulator-detection` crate.

## Build & system-property tier

| Signal | Fires when | Why (research) |
|--------|-----------|----------------|
| `ro.kernel.qemu` | `== 1` | legacy emulator flag |
| `ro.boot.qemu` | present / `== 1` | modern `Build.IS_EMULATOR` source (AOSP 9158d6db) |
| `ro.hardware` | `goldfish` \| `ranchu` | goldfish HAL family; also load-bearing for HAL resolution |
| `ro.product.model` | `sdk` \| `Emulator` \| `Android SDK built for arm64` | SDK image model |
| `ro.product.name` | `sdk_gphone64_arm64` \| `sdk` | SDK image product |
| `ro.product.device` | `emu64a` \| `generic` | SDK image device |
| `ro.build.fingerprint` | contains `sdk_gphone` \| `emu64` \| `generic` \| `test-keys` \| `dev-keys` | e.g. `google/sdk_gphone64_arm64/emu64a:16/…:user/dev-keys` |
| `ro.build.tags` | `test-keys` | non-release signing |
| `ro.build.type` | `!= user` (`userdebug`/`eng`) | non-retail build |
| `ro.secure` | `== 0` | RootBeer dangerous prop |
| `ro.debuggable` | `== 1` | RootBeer dangerous prop |
| `ro.build.characteristics` | contains `emulator` | explicit self-declaration |
| `ro.hardware.egl` | `emulation` | SwiftShader/translator GL |
| `ro.kernel.qemu.gles` / `ro.kernel.android.qemud` | present | goldfish init props |
| `qemu.hw.mainkeys` / `qemu.sf.fake_camera` / `qemu.sf.lcd_density` | present | `qemu-props` service output |
| `vendor.qemu.dev.bootcomplete` / `vendor.qemu.*` | present | modern ranchu `init.ranchu.rc` surface |
| `ro.boot.qemu.*` (`.gltransport`, `.hardware.vulkan`, …) | present | kernel-cmdline-derived qemu props |

framgia's `MIN_PROPERTIES_THRESHOLD = 5` means 5 hits is already a verdict — the stock
image trips well over a dozen.

## Device nodes / sockets / files

| Path | Fires when | Why |
|------|-----------|-----|
| `/dev/qemu_pipe` | exists | qemu pipe transport |
| `/dev/goldfish_pipe` / `/dev/goldfish_sync` / `/dev/goldfish_dma` | exists | goldfish devices |
| `/dev/socket/qemud` | exists | qemud socket |
| `/sys/qemu_trace` | exists | qemu trace sysfs |
| `/vendor/etc/fstab.ranchu` | exists | ranchu fstab |
| `/system/bin/qemu-props` | exists | qemu-props binary |
| `/dev/gnss0` | exists | goldfish GNSS vport |
| `/proc/tty/drivers` | contains `goldfish` | tty driver name |

## /proc, /sys, sensors, network

| Signal | Fires when | Why |
|--------|-----------|-----|
| `/proc/cpuinfo` | contains `goldfish`/`ranchu`/`qemu`; may lack a `Hardware:` line | QEMU `virt` DT has no machine name; real Qualcomm kernels print `Hardware : … SM8550` |
| sensors HAL | `dumpsys sensorservice` shows `Goldfish 3-axis Accelerometer`, vendor `The Android Open Source Project` | goldfish `sensors_qemu.c` names |
| network | `ip addr` shows `10.0.2.15` (SLIRP; gw `10.0.2.2`) | QEMU user-net default |
| `/proc/device-tree/compatible` | contains `linux,dummy-virt` (often SELinux-denied to apps) | QEMU `virt` machine root compatible |

## Not directly reachable from an unprivileged app (deprioritized)

- IMEI/IMSI/serial — gated behind `READ_PRIVILEGED_PHONE_STATE` on Android 10+ (`SecurityException`).
- `/proc/cmdline` — SELinux `proc_cmdline`, denied to `untrusted_app`; `androidboot.*` leaks via derived `ro.boot.*`.
- MIDR — at `/sys/devices/system/cpu/cpu0/regs/identification/midr_el1`; readable, but the app-facing
  tell is the `/proc/cpuinfo` `CPU implementer`/`CPU part` derived from it.

## Cross-check against a real device

The live `baseline-avd.md` "Key identity properties" block should show an **incoherent** set on
ranchu (`google` brand / `sdk_gphone64_arm64` name / `ranchu` hardware). The DeviceProfile that
AEmulator ships must instead reproduce a single **coherent** real-device set (the reference
profile is Redmi K40 `alioth`/SM8250; fingerprint
`Redmi/alioth/alioth:12/RKQ1.211001.001/22.2.9:user/release-keys`).
