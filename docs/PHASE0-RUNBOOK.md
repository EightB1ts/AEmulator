# Phase 0 — Bring-up & Go/No-Go Runbook

Phase 0 answers one question before any budget is committed: **can a coherent, real-hardware ARM64
Android identity be presented under hardware virtualization, and does our arg-gen + identity stack
actually drive a hypervisor?** It has two host paths that must not be confused:

| | **HVF dev-loop** (this Mac) | **KVM fleet** (production go/no-go) |
|--|--|--|
| Host | Apple Silicon, Hypervisor.framework | bare-metal ARM64 (`/dev/kvm`) |
| Backend | `qemu-system-aarch64 -accel hvf` | forked crosvm over KVM |
| MIDR override | **none** (`-cpu host` leaks Apple `0x61` — fatal) | `KVM_SET_ONE_REG` + `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` |
| Guest | generic ARM64 Linux (HVF proof) | `aosp_cf_arm64_only_phone` Cuttlefish → home screen |
| Purpose | fast CPU/identity iteration; **not authoritative for stealth** | the real stealthy, driveable device |
| Cloud reality | your laptop | **no ARM64 cloud VM exposes `/dev/kvm` → bare-metal only** |

> **The HVF path is a velocity tool, never the shipping architecture.** Nothing validated for
> *stealth* on HVF counts: `-cpu host` leaks MIDR implementer `0x61` (Apple), an instant fatal
> tell, and HVF has no MIDR-override equivalent to KVM's `KVM_SET_ONE_REG`. Use HVF for "does it
> boot / does arg-gen work / does identity iterate fast"; prove stealth on KVM.

---

## Go / No-Go decision gates (Phase 0 DoD, from the plan)

Phase 0 is a **PASS** only when all of these are recorded. The `[x]` items are provable on this Mac
today via the workspace + oracle; the `[ ]` items require the bare-metal ARM host.

- [x] **Workspace merge gate green** — `bash ci/check.sh` (fmt + clippy `-D warnings` + tests + the
      identity-validator blocklist gate over props/bootconfig/DTB).
- [x] **HVF + VMM arg-gen proven on Apple Silicon** — `oracle/scripts/qemu-hvf-boot.sh`.
- [x] **Detection catalogue validated against a live stock emulator** — `oracle/scripts/detect-baseline.sh`
      (ground-truth of what "looks like an emulator" means; a green AEmulator build must trip
      **zero** of the signals a stock ranchu image trips).
- [ ] **Cuttlefish `aosp_cf_arm64_only_phone` boots to home screen under KVM** on a documented
      bare-metal ARM host (crosvm-driven).
- [ ] **A/B cross-check** of the same guest under `launch_cvd -vm_manager qemu_cli` (isolates
      crosvm-fork regressions from image bugs).
- [ ] **`KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` presence probed** on the fleet host — the MIDR-override
      go/no-go. Absent ⇒ MIDR cannot be clamped to the claimed SoC ⇒ the CPU identity leaks the host
      part number (a coherence failure the validator will not save you from at runtime).
- [ ] **GPU-identity options documented** on the chosen silicon (cosmetic forked-Mesa/Venus
      `INTEGRATED_GPU` vs a real Adreno/Mali device farm) — the single most-constraining fleet fork.
- [ ] **Baseline per-tap latency recorded** for the control surface.
- [ ] TCG shipped only as an unsupported `--slow` dev mode (never a stealth target — Proteus/EmuID
      classify TCG trivially).

---

## Path A — HVF dev loop (this Mac, `aarch64-apple-darwin`)

Fast local loop for iterating the identity layer and proving arg-gen + HVF. **Run everything from
the workspace root** (`AEmulator/`).

### A0. Sanity + build + gate

```bash
sysctl kern.hv_support                 # must print 1 (HVF available)
uname -m                               # arm64 (Apple Silicon — ARM-on-ARM)

cargo build                            # default features — must be green
cargo test                             # whole workspace
bash ci/check.sh                       # the full merge gate (fmt/clippy/test + blocklist)
```

### A1. Inspect the identity the profile emits

```bash
cargo run -p aemulator-cli -- profile-show        # resolved DeviceProfile as JSON
cargo run -p aemulator-cli -- fingerprint         # Redmi/alioth/alioth:12/RKQ1.211001.001/22.2.9:user/release-keys
cargo run -p aemulator-cli -- profile-validate    # recompute ConstructBuildFingerprint + coherence asserts → "OK: profile is coherent"
cargo run -p aemulator-cli -- props               # Android system-property map (126 lines)
cargo run -p aemulator-cli -- bootconfig          # androidboot.* — NEVER androidboot.qemu
cargo run -p aemulator-cli -- qemu-args           # the HVF launcher argument vector
cargo run -p aemulator-cli -- dtb /tmp/aem.dtb    # hand-authored device tree (zero VM-tell strings)
```

The `props`/`bootconfig`/`dtb` triple is exactly what `ci/check.sh` greps against the blocklist
(`ranchu|goldfish|qemu|cutf|vsoc|crosvm|swiftshader|dummy-virt|houdini|exagear`). `qemu-args` is the
**host** launcher and legitimately contains `qemu-system-aarch64` — it is NOT part of the guest
blocklist scan.

### A2. Prove arg-gen + boot HVF (oracle)

```bash
# Arg-gen proof only (fast — no qemu install, no boot):
bash oracle/scripts/qemu-hvf-boot.sh args

# Install qemu (if missing) + boot a plain ARM64 Linux (Alpine virt) under -accel hvf -cpu host,
# reusing the crate's -machine/-cpu/-smp/NIC-MAC identity flags. Serial console; exit Ctrl-A X:
bash oracle/scripts/qemu-hvf-boot.sh boot
# env: AEMU_PROFILE=<toml>  AEMU_BOOT_IMAGE=<iso/img>  AEMU_MEM_MB=<n>  AEMU_BOOT_TIMEOUT=<s>
```

> A bare `qemu-system-aarch64 -M virt` will **not** boot a stock Android GSI to a usable UI — GSI
> expects a concrete device model (Cuttlefish vendor HALs, virtio-gpu/gfxstream, gralloc, input,
> sensors). "Stealth Android to a driveable UI" is the KVM/Cuttlefish path (Path B), not HVF.

### A3. Capture the detection baseline (ground truth)

```bash
# Boots a stock Android Studio arm64 AVD and runs our probes against LIVE Android, writing
# oracle/reports/baseline-avd.md (🔴 fires / 🟢 clear). Safe with no AVD (prints create commands,
# exits 0). See oracle/README.md for the sdkmanager/avdmanager setup on a host with no AVDs.
bash oracle/scripts/detect-baseline.sh
# env: AEMU_AVD=<name>  AEMU_BOOT_TIMEOUT=<s>  AEMU_GPU=<mode>  AEMU_KEEP_RUNNING=1
```

---

## Path B — Linux / KVM fleet (bare-metal ARM64, the production go/no-go)

This is where stealth + the Cuttlefish boot are actually proven. **No ARM64 cloud VM exposes
`/dev/kvm`** (Graviton non-metal, Ampere on Azure/GCP/OCI): use bare metal — Graviton `*.metal`,
Ampere Altra bare metal, or a Hetzner RX ARM box.

### B1. Confirm the host can virtualize ARM-on-ARM

```bash
uname -m                               # aarch64
ls -l /dev/kvm                         # must exist and be accessible (add your user to the kvm group)
[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo "KVM r/w OK" || echo "NO /dev/kvm access"
grep -c . /sys/module/kvm/parameters/* 2>/dev/null   # kvm module loaded
```

### B2. Probe `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` — the MIDR-override go/no-go

Writing a per-cluster `MIDR_EL1` (so `/proc/cpuinfo` `CPU implementer`/`CPU part` and
`/sys/devices/system/cpu/cpu*/regs/identification/midr_el1` read as the **claimed** Qualcomm/Exynos
SoC, heterogeneous across clusters — never the host part) requires `KVM_SET_ONE_REG` on the ID
registers, which is gated by the `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` capability (~Linux **6.14**).

- Kernel ≥ 6.14 with the cap: MIDR clamp is **GO**.
- Older kernel / cap absent: MIDR override is **NO-GO** on this host — the CPU identity will leak the
  host part number. Either upgrade the kernel or pick a different fleet host.

Probe it with a tiny `KVM_CHECK_EXTENSION` call (the value of the cap constant is resolved from your
kernel headers `linux/kvm.h`):

```c
// probe_writable_impl_id.c — cc probe_writable_impl_id.c -o probe && ./probe
#include <fcntl.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <linux/kvm.h>
int main(void) {
    int kvm = open("/dev/kvm", O_RDWR);
    if (kvm < 0) { perror("open /dev/kvm"); return 2; }
#ifdef KVM_CAP_ARM_WRITABLE_IMP_ID_REGS
    int r = ioctl(kvm, KVM_CHECK_EXTENSION, KVM_CAP_ARM_WRITABLE_IMP_ID_REGS);
    printf("KVM_CAP_ARM_WRITABLE_IMP_ID_REGS = %d (%s)\n", r, r > 0 ? "GO — MIDR clamp available" : "NO-GO");
    return r > 0 ? 0 : 1;
#else
    printf("KVM_CAP_ARM_WRITABLE_IMP_ID_REGS not in kernel headers — NO-GO (need ~6.14)\n");
    return 1;
#endif
}
```

Also confirm the host SoC feature vector is a **superset** of the claimed SoC's `ID_AA64*` — you can
clamp features *down* to the claimed part, never synthesize MTE/SVE the host silicon lacks.

### B3. Boot Cuttlefish `aosp_cf_arm64_only_phone` under crosvm (the DoD boot)

Prove the guest boots to its home screen under KVM on this host — first with our forked crosvm, then
cross-checked with the QEMU A/B oracle.

```bash
# Fetch aosp_cf_arm64_only_phone (AOSP CI or a local build) + the cvd host package, then:
launch_cvd --daemon                    # default vm_manager is crosvm — the primary path
# ... adb wait-for-device; confirm the launcher UI renders ...
stop_cvd

# A/B oracle — same guest under QEMU to isolate crosvm-fork regressions from image bugs:
launch_cvd --daemon -vm_manager qemu_cli
stop_cvd
```

> Our production VMM is a **pinned fork of crosvm** (BSD-3), rebranded, with the identity patches
> concentrated in `aarch64/src/fdt.rs` + a handful of names. Pin a commit; never track `main`. In
> this workspace the KVM backend is the `aemulator-vmm::crosvm_backend` module, gated behind the
> non-default `kvm` cargo feature **and** `cfg(target_os = "linux")` — it does not exist in the
> default macOS build. Build it on the fleet host with:
>
> ```bash
> cargo build -p aemulator-vmm --features kvm     # Linux/aarch64 only
> ```

### B4. Fleet scale note

~10–20 instances per 80-core Altra at 4–8 vCPU each; requires privileged pods + a `/dev/kvm` k8s
device plugin. Whole-machine billing, no elastic autoscale (bare-metal only). Size the cost model
around this before committing.

---

## Recorded results (this Mac, 2026-07-24)

| Probe | Result |
|-------|--------|
| `sysctl kern.hv_support` | **1** (HVF available) |
| host arch | `arm64` (Apple Silicon — ARM-on-ARM) |
| `cargo build` (default) | ✅ green |
| `cargo test` (default) | ✅ **120 passed, 0 failed** |
| `cargo clippy --all-targets -- -D warnings` | ✅ clean |
| `cargo fmt --all --check` | ✅ clean |
| `bash ci/check.sh` | ✅ ALL CHECKS PASSED (props/bootconfig/DTB blocklist clean) |
| `aemulator-cli -- fingerprint` | ✅ `Redmi/alioth/alioth:12/RKQ1.211001.001/22.2.9:user/release-keys` |
| `aemulator-cli -- profile-validate` | ✅ coherent |
| Android SDK `emulator -list-avds` | none installed (see `oracle/README.md` to create an arm64 AVD) |

The KVM-fleet gates (B1–B4) are **pending a bare-metal ARM host** and are the outstanding Phase-0
go/no-go items.
