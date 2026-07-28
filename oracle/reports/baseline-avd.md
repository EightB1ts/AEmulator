# Baseline — stock Android emulator detection signals

_Generated: 2026-07-24T23:30:40Z · AVD `aemu_ranchu_arm64`_

Ground-truth run of the AEmulator detection catalogue against a **live stock**
Android Studio emulator (goldfish/ranchu). Every 🔴 below is a signal the stock
emulator trips — i.e. a signal a real AEmulator build MUST NOT trip. This is the
adversary's-eye baseline the `aemulator-detection` crate is validated against.

## Summary

| | Count |
|--|--|
| 🔴 Signals FIRING (emulator revealed) | 16 |
| 🟢 Signals clear | 17 |
| ⚪ Informational (identity context) | 15 |
| Total probed | 48 |

## Environment

| Item | Value |
|------|-------|
| adb | `/Users/cam/Library/Android/sdk/platform-tools/adb` |
| adb version | Android Debug Bridge version 1.0.41 |
| emulator | `/Users/cam/Library/Android/sdk/emulator/emulator` |
| emulator version | Android emulator version 36.6.11.0 (build_id 15507667) (CL:N/A) |
| AVD | `aemu_ranchu_arm64` |
| GPU mode | `swiftshader_indirect` |
| Host | Darwin 27.0.0 arm64 |

## Signal-by-signal

| Signal | Probe | Live value | Verdict |
|--------|-------|------------|---------|
| `ro.kernel.qemu` | == 1 | `1` | 🔴 FIRES |
| `ro.boot.qemu` | present ⇒ tell | `1` | 🔴 FIRES |
| `ro.hardware` | =~ /goldfish|ranchu|vbox86|cutf/ | `ranchu` | 🔴 FIRES |
| `ro.product.model` | =~ /sdk|emulator|Android SDK built/ | `sdk_gphone64_arm64` | 🔴 FIRES |
| `ro.product.name` | =~ /sdk|vbox86|sdk_gphone|emu64/ | `sdk_gphone64_arm64` | 🔴 FIRES |
| `ro.product.device` | =~ /generic|emu64|vsoc/ | `emu64a` | 🔴 FIRES |
| `ro.build.fingerprint` | =~ /generic|unknown|sdk_gphone|emu64|test-keys|dev-keys/ | `google/sdk_gphone64_arm64/emu64a:14/UE1A.230829.050/12077443:userdebug/dev-keys` | 🔴 FIRES |
| `ro.build.flavor` | =~ /sdk_gphone|-userdebug|-eng/ | `sdk_gphone64_arm64-userdebug` | 🔴 FIRES |
| `ro.build.tags` | == test-keys | `dev-keys` | 🟢 clear |
| `ro.build.type` | != user (retail=user) | `userdebug` | 🔴 FIRES |
| `ro.secure` | == 0 | `1` | 🟢 clear |
| `ro.debuggable` | == 1 | `1` | 🔴 FIRES |
| `ro.build.characteristics` | =~ /emulator/ | `emulator` | 🔴 FIRES |
| `ro.hardware.egl` | == emulation | `emulation` | 🔴 FIRES |
| `ro.kernel.qemu.gles` | present ⇒ tell | `<empty>` | 🟢 clear |
| `ro.kernel.android.qemud` | present ⇒ tell | `<empty>` | 🟢 clear |
| `qemu.hw.mainkeys` | present ⇒ tell | `<empty>` | 🟢 clear |
| `qemu.sf.fake_camera` | present ⇒ tell | `<empty>` | 🟢 clear |
| `ro.boot.qemu.gltransport` | present ⇒ tell | `<empty>` | 🟢 clear |
| `vendor.qemu.dev.bootcomplete` | present ⇒ tell | `<empty>` | 🟢 clear |
| `ro.product.cpu.abilist` | ABI list (expect arm64-v8a on AS) | `arm64-v8a` | ⚪ info |
| `ro.product.manufacturer` | MANUFACTURER | `Google` | ⚪ info |
| `ro.product.brand` | BRAND | `google` | ⚪ info |
| `ro.product.board` | BOARD codename | `goldfish_arm64` | ⚪ info |
| `ro.board.platform` | SoC platform (separate from board) | `<empty>` | ⚪ info |
| `ro.soc.model` | SoC model | `ranchu` | ⚪ info |
| `ro.build.host` | build host (OEM farm vs android-build) | `r-d3d21742fc70d910-p10v` | ⚪ info |
| `ro.build.user` | build user | `android-build` | ⚪ info |
| `gsm.version.baseband` | baseband (empty/1.0.0.0 on emu) | `1.0.0.0` | ⚪ info |
| `ro.serialno` | serial | `EMULATOR36X6X11X0` | ⚪ info |
| `ro.boot.verifiedbootstate` | absent ⇒ tell | `<empty>` | 🔴 FIRES |
| `ro.boot.flash.locked` | flash.locked (retail=1) | `<empty>` | ⚪ info |
| `ro.boot.vbmeta.device_state` | device_state (retail=locked) | `<empty>` | ⚪ info |
| `ro.boot.veritymode` | veritymode (retail=enforcing) | `enforcing` | ⚪ info |
| `/dev/qemu_pipe` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/dev/goldfish_pipe` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/dev/goldfish_sync` | node exists ⇒ tell | `present` | 🔴 FIRES |
| `/dev/goldfish_dma` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/dev/socket/qemud` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/sys/qemu_trace` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/vendor/etc/fstab.ranchu` | node exists ⇒ tell | `present` | 🔴 FIRES |
| `/system/bin/qemu-props` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/dev/gnss0` | node exists ⇒ tell | `absent` | 🟢 clear |
| `/proc/tty/drivers` | grep goldfish | `no-match/denied` | 🟢 clear |
| `/proc/cpuinfo` | grep goldfish|ranchu|qemu | `<no Hardware line>` | ⚪ info |
| `sensors HAL` | dumpsys sensorservice grep Goldfish | `0000000000) Goldfish 3-axis Accelerometer \| The Android Open Source Project \| ver: 1 \| type: android.sensor.accelerometer(1) \| perm: n/a \| flags: 0x00000050` | 🔴 FIRES |
| `network` | ip addr grep 10.0.2.15 (SLIRP) | `not 10.0.2.15` | 🟢 clear |
| `/proc/device-tree/compatible` | read (SELinux may deny untrusted_app) | `denied/empty` | ⚪ info |

## Raw qemu/goldfish property surface

Every property whose key or value mentions qemu/goldfish/ranchu (bulk context):

```
[init.svc.android-hardware-media-c2-goldfish-hal-1-0]: [running]
[init.svc.goldfish-logcat]: [running]
[init.svc.qemu-adb-keys]: [stopped]
[init.svc.qemu-adb-setup]: [stopped]
[init.svc.qemu-device-state]: [stopped]
[init.svc.qemu-props]: [running]
[init.svc.qemu-props-bootcomplete]: [stopped]
[init.svc.ranchu-net]: [stopped]
[init.svc.ranchu-setup]: [stopped]
[qemu.sf.lcd_density]: [420]
[ro.boot.hardware]: [ranchu]
[ro.boot.hardware.vulkan]: [ranchu]
[ro.boot.qemu]: [1]
[ro.boot.qemu.avd_name]: [aemu_ranchu_arm64]
[ro.boot.qemu.camera_hq_edge_processing]: [0]
[ro.boot.qemu.camera_protocol_ver]: [1]
[ro.boot.qemu.cpuvulkan.version]: [4206592]
[ro.boot.qemu.gltransport.drawFlushInterval]: [800]
[ro.boot.qemu.gltransport.name]: [pipe]
[ro.boot.qemu.hwcodec.avcdec]: [2]
[ro.boot.qemu.hwcodec.hevcdec]: [2]
[ro.boot.qemu.hwcodec.vpxdec]: [2]
[ro.boot.qemu.settings.system.screen_off_timeout]: [2147483647]
[ro.boot.qemu.skin]: [1080x2400]
[ro.boot.qemu.virtiowifi]: [1]
[ro.boot.qemu.vsync]: [60]
[ro.hardware]: [ranchu]
[ro.hardware.gralloc]: [ranchu]
[ro.hardware.power]: [ranchu]
[ro.hardware.vulkan]: [ranchu]
[ro.kernel.qemu]: [1]
[ro.product.board]: [goldfish_arm64]
[ro.soc.model]: [ranchu]
[vendor.qemu.sf.fake_camera]: [back]
[vendor.qemu.timezone]: [America/New_York]
[vendor.qemu.vport.bluetooth]: [/dev/vport8p2]
[vendor.qemu.vport.modem]: [/dev/vport9p1]
```

## Key identity properties (the DeviceProfile must reproduce a *coherent* set)

```
ro.product.brand=google
ro.product.name=sdk_gphone64_arm64
ro.product.device=emu64a
ro.product.model=sdk_gphone64_arm64
ro.product.manufacturer=Google
ro.product.board=goldfish_arm64
ro.board.platform=
ro.hardware=ranchu
ro.build.fingerprint=google/sdk_gphone64_arm64/emu64a:14/UE1A.230829.050/12077443:userdebug/dev-keys
ro.build.type=userdebug
ro.build.tags=dev-keys
ro.product.cpu.abilist=arm64-v8a
```

---
_Artifacts: full property dump at `oracle/downloads/getprop-aemu_ranchu_arm64.txt`,
emulator log at `oracle/downloads/emulator-aemu_ranchu_arm64.log`._
