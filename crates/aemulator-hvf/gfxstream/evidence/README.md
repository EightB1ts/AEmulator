# `gfxstream/evidence/` — observed artifacts, not prose

Everything in this directory was **captured from a running system**. The status documents
(`PHASE3-STATUS.md`, `../../PHASE-KOSMICKRISP-STATUS.md`) narrate; this directory is the record they
narrate *from*. When the two disagree, this directory wins.

This is deliberately tracked by git — the root `.gitignore` excludes build output under
`gfxstream/` but not `evidence/`. Verify with `git check-ignore -v <file>` before adding anything
(no output = tracked, which is what you want).

## Files

| File | What it is |
|---|---|
| `host-vulkan-identity.json` | **Phase-4 identity golden (host side).** Everything the *host* Vulkan driver reports about itself: full `VkPhysicalDeviceProperties` + limits, the `pNext` property chain, all 55 `VkPhysicalDeviceFeatures` bools by name, the sorted device-extension list, the BC1–BC7 format properties, and the first 32 bytes of `vkGetPipelineCacheData`. |
| `guest-vulkan-identity.json` | **Phase-4 identity golden (guest side). NOT YET CAPTURED** — needs a booted VM. See "Capturing the guest twin" below. |
| `3b-metal-presenter.png` | Phase 3b: `aemu-display-selftest` attaching a `CAMetalLayer` to the winit `NSView` and presenting a host-generated gradient+checker through `presentDrawable` on the Apple M2 GPU. This is the exact surface a gfxstream ColorBuffer readback is presented through. |
| `phase2-display-regression.png` | Phase 2 regression: `run-linux.sh --display` rendering the guest framebuffer console. The guest dmesg in that shot reads `[drm] features: -virgl … number of cap sets: 0` — pure 2D, VIRGL off — which is what proves the `--gpu3d` upgrade is genuinely opt-in and invisible to the default path. |
| `gpu3d-guest-negotiation.txt` | Phase 3d: a real guest kernel negotiating virtio-gpu 3D. `stream_renderer_init returned 0`, `+virgl +context_init`, 2 cap sets, `/dev/dri/renderD128` present. |

## The identity goldens (Phase 0.6 → Phase 4)

`host-vulkan-identity.json` exists because **Phase 4 of the graphics plan changes GPU identity, and
an identity change with no before-state is unverifiable by construction.** Every Phase-4 item is
specified as a diff against this file. Notably:

- **4.7** rewrites `vendorID`, `deviceID`, `deviceName` and `pipelineCacheUUID`. The baseline records
  what they are now: `vendorID` `0x0000106b` (Apple), `deviceID` `0x00000064`, `deviceName`
  `Apple M2`, `pipelineCacheUUID` `1655d6f09ee4d2a99972c55da30e2a0c`.
- **4.8** clamps `textureCompressionBC` and `multiViewport` to FALSE and pairs the latter with
  `maxViewports = 1`. The baseline records that the host reports **both TRUE** and
  `maxViewports = 16`, so those are real clamps, not no-ops — and that `geometryShader` and
  `wideLines` are already FALSE, so any dump showing them TRUE means something forced a feature the
  host does not have, which makes the guest's `vkCreateDevice` fail outright.
- **4.8** also zeroes the BC1–BC7 format properties. The baseline records
  `optimalTilingFeatures = 0x0001d401` on all sixteen; a clamp that moves the feature bool without
  moving these leaves an internally inconsistent device, which is a louder tell than admitting BC.
- **4.9** rewrites bytes 8..32 of the pipeline-cache header. The baseline's
  `pipelineCacheHeader.leak.*AgreesWithProperties` flags all read **`true`** today: the 32-byte
  `VkPipelineCacheHeaderVersionOne` Mesa emits carries the real `vendorID` (bytes 8..12), `deviceID`
  (12..16) and `pipelineCacheUUID` (16..32), and `vkGetPipelineCacheData` is a raw passthrough in
  gfxstream (there is no `on_vkGetPipelineCacheData` in `VkDecoderGlobalState`; `VkDecoder.cpp:4403`
  calls the driver directly). So any guest app recovers all three from a cache blob regardless of
  what the properties call reported. **After 4.9 those three flags must read `false`.** That is the
  whole test.

The file is a golden, so it is built to be diffable and nothing else: fixed key order, extensions
sorted by name, bitmasks as hex strings, floats at `%.9g`, **no timestamp, no pointer, no filesystem
path**. A rerun on an unchanged host is byte-identical (verified: three consecutive runs, including
the stdout path, hash the same). Any diff is therefore a real change — either intended, or a
regression.

Two entries do legitimately move without any code change of ours, and both are provenance worth
keeping: `loaderInstanceApiVersion` / `headerVersion` (a `brew upgrade vulkan-headers vulkan-loader`)
and `driverProperties.driverInfo` (the pinned Mesa SHA in `third_party/MANIFEST.sh`). Today they read
`1.4.350` / `350` and `Mesa 26.3.0-devel (git-4f4b73dfe5)`.

## Regenerating the host golden

```bash
bash crates/aemulator-hvf/kosmickrisp/run-gate.sh --dump-identity
```

That rebuilds `kk-gate` if `kk-gate.c` changed, resolves the ICD through `third_party/paths.sh`
(`TP_KOSMICKRISP_ICD`), and rewrites `host-vulkan-identity.json` in place — via a temp file, so a
driver that dies halfway cannot leave a truncated baseline behind. It prints the identity fields that
matter to **stderr**; stdout is reserved for JSON.

Needs the driver built first (`bash third_party/bootstrap.sh mesa`, or the
`kosmickrisp/build-kosmickrisp.sh` shim).

For an ad-hoc diff without touching the golden:

```bash
bash crates/aemulator-hvf/kosmickrisp/run-gate.sh --dump-identity - \
  | diff crates/aemulator-hvf/gfxstream/evidence/host-vulkan-identity.json -
```

`run-gate.sh` with no arguments is still the capability gate, unchanged.

## Capturing the guest twin — `guest-vulkan-identity.json`

**Not captured.** It needs a booted Android VM, which the host-side tooling above does not require;
whoever has a boot up should run this. The guest twin is what shows which host values survive the
gfxstream round trip and which the guest image overwrites on its own — `ResourceTracker.cpp:2244-2275`
*inside the guest image* hard-writes `deviceType = VIRTUAL_GPU`, prefixes `deviceName` with
`"Virtio-GPU GFXStream ("`, sets `driverID = MESA_GFXSTREAM` and zeroes the UUIDs, so those four
fields are **expected** to differ from the host dump before any Phase-4 change lands. The fields to
watch are the ones the guest does *not* overwrite: `vendorID`, `deviceID`, `pipelineCacheUUID`, the
feature bools, and the pipeline-cache header.

### Route A — cross-compile the same dumper for the guest (authoritative)

This is the one that produces a real twin, because it is literally the same code emitting the same
schema, including the pipeline-cache header that no stock Android tool exposes. Compile against the
**homebrew** Vulkan headers (they are platform-independent, and the NDK's bundled copy is too old for
the `VK_EXT_external_memory_metal` types the gate half of the file references) while targeting the
NDK sysroot:

```bash
# 1. boot with a GPU path (headless with neither --gpu3d nor SWRENDER does NOT reach a usable
#    system — zygote aborts in preload; see ../../PHASE6-STATUS.md)
cd crates/aemulator-hvf && ./run-android64.sh --gpu3d --display

# 2. bring up networking + adbd over the control socket, then adb connect
#    (full sequence, including the load-bearing `ip rule ... lookup main`, in ../../PHASE6-STATUS.md;
#    port 5555 is in adb's emulator-console range, hence the 6555 relay documented there)
adb connect localhost:6555

# 3. cross-compile --dump-identity for the guest. No NDK is installed on this host as of writing:
#    `brew install --cask android-ndk`, or Android Studio > SDK Manager > NDK. The prebuilt
#    triple is darwin-x86_64 on most NDK releases even on Apple Silicon, hence the glob.
NDK="$(ls -d "$HOME"/Library/Android/sdk/ndk/* | sort -V | tail -1)"
CC="$(ls "$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang | head -1)"
"$CC" -O1 -o /tmp/kk-gate-guest crates/aemulator-hvf/kosmickrisp/kk-gate.c \
  -I"$(brew --prefix vulkan-headers)/include" -lvulkan

# 4. push, run, capture. `exec-out`, NOT `shell` — `adb shell` translates LF to CRLF on some
#    versions, which silently corrupts a byte-compared golden. If the loader cannot reach the
#    driver as uid shell, wrap the invocation in `su 0` (the image is userdebug).
adb push /tmp/kk-gate-guest /data/local/tmp/kk-gate-guest
adb shell chmod 755 /data/local/tmp/kk-gate-guest
adb exec-out /data/local/tmp/kk-gate-guest --dump-identity \
  > crates/aemulator-hvf/gfxstream/evidence/guest-vulkan-identity.json

# 5. the diff that is the actual deliverable
diff crates/aemulator-hvf/gfxstream/evidence/{host,guest}-vulkan-identity.json
```

### Route B — the stock Android dump (no build, smaller coverage)

```bash
adb exec-out cmd gpu vkjson > crates/aemulator-hvf/gfxstream/evidence/guest-vkjson.json
```

Covers properties, features, extensions and format properties in Android's own VkJSON schema — so it
is *not* byte-comparable with the host dump, only field-comparable — and gives **no pipeline-cache
header**, which is the single most important field. Treat it as a cross-check, not the twin.
(Unverified on this Cuttlefish image: `cmd gpu vkjson` is a `GpuService` command and its presence has
not been confirmed here.)

### Route C — no adb, over the control socket

The control socket's `sh` verb sends a command to the guest serial console and the output lands in
the VMM's streamed console log, not back on the socket:

```bash
crates/aemulator-hvf/target/release/aemulator-hvf sh 'su 0 getprop ro.hardware.vulkan' \
  --socket /tmp/aemulator-hvf-control.sock
```

That is fine for the short Phase-3/Phase-4 checks (`getprop ro.sf.lcd_density`, `wm size`,
`wm density`, `cat /sys/class/drm/card0-*/modes`) but a poor fit for scraping a 30 KB JSON out of a
console log. Use Route A for the dump.
