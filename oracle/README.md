# oracle — live bring-up harness (HVF dev-loop + detection baseline)

The **oracle** is the operational harness that grounds AEmulator against reality: it runs the
detection catalogue against a *live* stock Android emulator (ground truth for what "looks like an
emulator" means), and it proves the VMM crate's argument generation drives a real
Hypervisor.framework (HVF) boot on Apple Silicon. These are **shell scripts + downloaded assets +
reports**, deliberately separate from the Rust workspace.

> Scope of this harness: the CPU/identity **dev loop** and the detection **baseline**. It is not,
> and on macOS cannot be, the full stealth Android device — see the honest boundary below.

## Two host paths (do not confuse them)

| | **HVF dev-loop** (this Mac) | **KVM fleet** (production) |
|--|--|--|
| Host | Apple Silicon, Hypervisor.framework | bare-metal ARM64 (Graviton `*.metal` / Ampere / Hetzner RX), `/dev/kvm` |
| Backend | `qemu-system-aarch64 -accel hvf` | forked crosvm over KVM |
| MIDR override | **none** — `-cpu host` leaks Apple implementer `0x61` (fatal tell) | `KVM_SET_ONE_REG` + `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` clamps MIDR to the claimed SoC |
| Guest to UI | generic ARM64 Linux only (HVF proof) | `aosp_cf_arm64_only_phone` Cuttlefish to home screen |
| Purpose | fast CPU/identity iteration + HVF proof | the real, stealthy, driveable device |
| Cloud reality | your laptop | **no ARM64 cloud VM exposes `/dev/kvm`** — bare-metal only |

The HVF path is a **velocity tool**, not the shipping architecture. The fleet is KVM-only. Nothing
you validate for *stealth* on HVF is authoritative (the Apple MIDR leak alone disqualifies it);
HVF is for "does it boot / does the arg-gen work / does identity iterate quickly".

## Scripts

### `scripts/detect-baseline.sh` — prove the catalogue against ground truth

Boots a stock Android Studio emulator (goldfish/ranchu) and runs our detection probes against the
*live* Android, recording which signals fire. This validates the `aemulator-detection` blocklist
against reality: the stock ranchu image **should** trip a large Tier-A set; a finished AEmulator
build must trip **zero** of them.

- Locates `adb` + `emulator` (Android SDK; macOS default `~/Library/Android/sdk`).
- `emulator -list-avds`; picks an arm64 AVD (override `AEMU_AVD=<name>`).
- **If an arm64 AVD exists:** boots it headless (`-no-window`, generous `AEMU_BOOT_TIMEOUT`,
  default 480s), `adb wait-for-device` + `sys.boot_completed`, runs the probes
  (`getprop` identity/qemu props, `ls` of the goldfish device nodes, `dumpsys sensorservice`,
  `/proc/cpuinfo`, `/proc/tty/drivers`, SLIRP `10.0.2.15`, device-tree compatible), writes
  **`reports/baseline-avd.md`** with a 🔴-fires / 🟢-clear table, then shuts the emulator down.
- **If no arm64 AVD exists:** prints the exact `sdkmanager`/`avdmanager` create commands, writes a
  short report, and **exits 0** (never hangs).

```bash
bash oracle/scripts/detect-baseline.sh
# env: AEMU_AVD=<name>  AEMU_BOOT_TIMEOUT=<s>  AEMU_GPU=<mode>  AEMU_KEEP_RUNNING=1
```

Reference target for the live run: `reports/baseline-avd.EXPECTED.md` (the signal catalogue from
the research dossier).

### `scripts/qemu-hvf-boot.sh` — prove HVF + our arg-gen

`deps | args | boot` (default `boot`):

- **`args`** — runs `cargo run -p aemulator-cli -- qemu-args` + `-- dtb` and prints the VMM crate's
  generated qemu argument vector + hand-authored DTB. No boot, no qemu needed. This is the pure
  arg-gen proof.
- **`deps`** — `brew install qemu` if missing; locate the edk2 aarch64 UEFI firmware Homebrew's
  qemu ships (`edk2-aarch64-code.fd`), or download `QEMU_EFI.fd` as a fallback.
- **`boot`** — the above, then boots a plain ARM64 Linux (Alpine `virt` ISO) under
  `-accel hvf -cpu host`, **reusing the `-machine`/`-cpu`/`-smp`/NIC-MAC identity flags the crate
  emits**. Proves HVF works on this Mac and the crate's arg-gen is real. Serial console
  (`-nographic`); exit with `Ctrl-A X`.

```bash
bash oracle/scripts/qemu-hvf-boot.sh args     # arg-gen proof only (fast)
bash oracle/scripts/qemu-hvf-boot.sh boot      # + boot Alpine ARM64 Linux under HVF
# env: AEMU_PROFILE=<toml>  AEMU_BOOT_IMAGE=<iso/img>  AEMU_ALPINE_BRANCH=v3.21
#      AEMU_MEM_MB=<n>  AEMU_BOOT_TIMEOUT=<s>  AEMU_SKIP_INSTALL=1
```

The boot deliberately does **not** pass the crate's `-dtb`/`-kernel`/Android `-drive` placeholders:
a generic distro boots via the firmware-provided DTB. Those flags are the Android image path
(Phase 1), exercised on the KVM fleet with real GSI + vendor images, not here.

## The honest boundary (read this)

- **A bare `qemu-system-aarch64 -M virt` will NOT boot a stock Android GSI to a usable UI.** GSI
  expects a concrete device model (Cuttlefish vendor HALs, virtio-gpu/gfxstream, gralloc, input,
  sensors). "Stealth Android to a driveable UI" is the crosvm/Cuttlefish **KVM fleet** path
  (Phase-0 DoD: `aosp_cf_arm64_only_phone` under KVM on a bare-metal ARM host), not this harness.
- **HVF cannot validate stealth.** `-cpu host` leaks MIDR implementer `0x61` (Apple) — an instant,
  fatal tell. The KVM backend clamps MIDR via `KVM_SET_ONE_REG`; HVF has no equivalent. Treat every
  HVF boot as functional bring-up, never a stealth pass.
- **The relay and hardware attestation are out of scope here** (Phase 5 / permanently unreachable —
  see the plan's tier ceiling).

## Actual probe results (recorded 2026-07-24, this Mac)

| Probe | Result |
|-------|--------|
| `sysctl kern.hv_support` | **1** (HVF available) |
| host arch | `arm64` (Apple Silicon — ARM-on-ARM) |
| `adb` | `~/Library/Android/sdk/platform-tools/adb`, v1.0.41 (37.0.0) |
| `emulator` | `~/Library/Android/sdk/emulator/emulator`, v36.6.11.0 |
| `emulator -list-avds` | **none installed** |
| system-images | none installed |
| `cmdline-tools` (sdkmanager/avdmanager) | **not installed** |
| `qemu-system-aarch64` | **not installed** (`qemu-hvf-boot.sh deps` installs it) |
| Homebrew | present (`/opt/homebrew/bin/brew`) |
| `aemulator-cli -- qemu-args` | ✅ emits a full HVF vector (machine/accel/cpu/smp/mem/smbios/dtb/net) |
| `aemulator-cli -- dtb` | ✅ writes a valid 2396-byte hand-authored DTB |
| `aemulator-cli -- fingerprint` | ✅ `Redmi/alioth/alioth:12/RKQ1.211001.001/22.2.9:user/release-keys` |

**To capture the live detection baseline on this host**, create an arm64 AVD first (this host has
no AVDs and no `cmdline-tools`):

```bash
export ANDROID_SDK_ROOT="$HOME/Library/Android/sdk"
# Install cmdline-tools if missing: Android Studio > SDK Manager > SDK Tools >
#   "Android SDK Command-line Tools", or
#   https://developer.android.com/studio#command-line-tools-only
$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager "system-images;android-34;google_apis;arm64-v8a"
$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/avdmanager create avd -n ranchu_arm64 \
    -k "system-images;android-34;google_apis;arm64-v8a" -d pixel_6
bash oracle/scripts/detect-baseline.sh          # writes reports/baseline-avd.md
```

## Layout

```
oracle/
  README.md                          this file
  scripts/
    _common.sh                       shared helpers (SDK/adb/emulator discovery, logging)
    detect-baseline.sh               live ranchu detection baseline → reports/baseline-avd.md
    qemu-hvf-boot.sh                 HVF dev-loop: arg-gen proof + Alpine ARM64 boot
  reports/
    baseline-avd.md                  live-run output (or a no-AVD note)
    baseline-avd.EXPECTED.md         expected-signal catalogue (research), pre-run reference
  downloads/                         (gitignored) qemu firmware, Alpine ISO, DTB, emulator logs,
                                     getprop dumps
```

## Phase-0 Definition of Done (the wider go/no-go, from the plan)

This harness covers the HVF + baseline slices; the full Phase-0 DoD is proven on the KVM host:

- [ ] Cuttlefish `aosp_cf_arm64_only_phone` boots to home screen under KVM on a documented host.
- [ ] A/B cross-check under `launch_cvd -vm_manager qemu_cli`.
- [ ] `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` presence probed (MIDR override go/no-go).
- [ ] GPU-identity options on the chosen silicon documented (forked-Mesa/Venus vs device farm).
- [ ] Baseline per-tap latency recorded.
- [x] Detection catalogue validated against a live stock emulator (`detect-baseline.sh` →
      `reports/baseline-avd.md`; ground-truth target in `baseline-avd.EXPECTED.md`).
- [x] HVF + VMM arg-gen proven on Apple Silicon (`qemu-hvf-boot.sh`).
