# A Emulator

A **Rust-first ARM64 Android emulator** whose purpose is to let an autonomous LLM agent
install and operate **real Android applications**. The hard product constraint: **every device
identifier must read as genuine hardware — it must not be discoverable that virtualization is
taking place.**

AEmulator presents a *coherent, real-hardware identity end-to-end* (build props, per-cluster
MIDR, hand-authored device tree, GPU strings, sensors, verified-boot state, network) and is
driveable through a clean, versioned **host-side control API** (perception + input live outside
the guest so there is no in-guest automation footprint).

## Where the design lives

- **Architecture + rationale + phased roadmap:** `~/.claude/plans/i-would-like-to-majestic-willow.md`
  (the approved plan). Read it first.
- **Per-crate public API, the dependency DAG, and the implementer rules:** [`SCAFFOLD.md`](./SCAFFOLD.md).
- **Verified research facts** (exact detection strings, blocklist entries, real-device values):
  the research dossier referenced by the plan. Use the *exact* strings from there, not recall.

## The hardening ceiling (set expectations)

Three tiers; the B→C boundary is a **cryptographic wall**, not a difficulty gradient:

- **Tier A — heuristic / software introspection** (strings, props, files, sockets, HAL names,
  sysfs, DTB, GL strings, root/Frida artifacts, network defaults): **the v1 target — fully
  winnable.** Choosing ARM64-on-ARM64 hardware virtualization (never TCG) structurally deletes
  whole detection classes.
- **Tier B — commercial RASP + software `MEETS_BASIC_INTEGRITY`:** achievable but never "done";
  a permanent adversarial-CI cost. On Android 13+ the integrity half needs the relay.
- **Tier C — hardware-backed remote attestation** (Play Integrity DEVICE/STRONG, Key Attestation
  `TrustedEnvironment`/`StrongBox`, Widevine L1, anti-cheat): **permanently unreachable by the VM
  itself.** The only partial path is the optional, per-flow **attestation relay** to real devices.

## Live device driving on macOS (HVF) — proven

On Apple Silicon (`kern.hv_support=1`), `aemulator` boots and drives a **real ARM64 Android VM in a
window** via Hypervisor.framework, and audits it against the detection catalogue live:

```bash
cargo build
target/debug/aemulator run --writable        # boots a windowed ARM64 Android VM (Metal GPU), injects identity
target/debug/aemulator ui                     # our control plane perceives the live UI (index-first nodes)
target/debug/aemulator audit                  # runs the detection catalogue against the LIVE device
target/debug/aemulator harden                 # rewrites the image build.prop identity in place, reboots
target/debug/aemulator tap 12 / launch <pkg> / shot out.png / gps 37.77 -122.41
```

**What this proves (measured, not claimed):**

- The control loop is real: `run` reaches `Ready` and `observe()` captures a live 1080×2400
  framebuffer + the structured UI tree (e.g. 33 home-screen nodes), so an agent can drive apps now.
- `-prop` injection **cannot** rewrite the ranchu image's baked tells (empirically: `ro.product.model`
  stayed `sdk_gphone64_arm64`). The audit measured **113 tells** on the stock substrate.
- `harden` (root + remount + in-place `build.prop` rewrite — the *legitimate* build-time-identity
  mechanism, applied live) flipped the device to `brand=Redmi, model=M2012K11AC, device=alioth` and
  dropped the audit to **81 tells** (−32). Those identity values are what the reference profile
  carried at measurement time; it has since moved to Xiaomi `fuxi` / SM8550, so a re-run flips to
  `brand=Xiaomi, model=2211133C, device=fuxi` — the tell count is unaffected.
- The remaining ~81 are **structural** `ranchu`/`qemu`/`goldfish` tells from the emulator
  kernel/device-model (`ro.hardware=ranchu`, `ro.boot.qemu`, `/vendor/etc/fstab.ranchu`, the Goldfish
  sensor HAL). **Zeroing these is not possible on the stock emulator** — it requires the custom
  de-branded guest image + our crosvm/KVM VMM (the Linux ARM64 fleet path). See
  [`docs/PHASE0-RUNBOOK.md`](./docs/PHASE0-RUNBOOK.md).

> macOS/HVF is the **dev loop + detection oracle**. The zero-tell stealth device is the
> crosvm/KVM + custom-image milestone (`aemulator-vmm::crosvm_backend`, `feature = "kvm"`, Linux).

## Workspace layout

A single Cargo workspace of host-side Rust crates. See [`SCAFFOLD.md`](./SCAFFOLD.md) for the DAG.

```
crates/
  aemulator-identity     the DeviceProfile — single source of identity truth (CORE)
  aemulator-detection    the adversary's-eye view: blocklist, probe catalogue, linters
  aemulator-dtb          hand-authored device tree (vm-fdt), no-VM-tells assertion
  aemulator-control      LLM control-plane contract + in-memory MockController
  aemulator-vmm          identity injection + launcher-arg generation (HVF now, KVM/crosvm seam)
  aemulator-supervisor   device lifecycle wiring vmm + control
  attestation-relay      OPTIONAL Phase-5 per-flow relay to real rooted devices
  aemulator-cli          `aemulator` binary — profile-show/validate/props/bootconfig/…
oracle/                  Phase-0 bring-up + go/no-go host spike (scripts land here)
```

## Build

Default features build green on **macOS / Apple Silicon** (`aarch64-apple-darwin`). Anything
requiring Linux/KVM (the crosvm backend) is behind the non-default `kvm` feature **and**
`cfg(target_os = "linux")`, so the default macOS build skips it.

### Quickstart

```bash
cargo build                                       # whole workspace, default features
cargo test                                        # unit tests (120 pass on macOS/arm64)

# Drive the identity layer through the CLI (defaults to the bundled reference profile):
cargo run -p aemulator-cli -- profile-show        # resolved DeviceProfile as JSON
cargo run -p aemulator-cli -- profile-validate    # recompute fingerprint + coherence asserts
cargo run -p aemulator-cli -- fingerprint         # ConstructBuildFingerprint
cargo run -p aemulator-cli -- props               # Android system-property map
cargo run -p aemulator-cli -- bootconfig          # androidboot.* (never androidboot.qemu)
cargo run -p aemulator-cli -- qemu-args           # HVF launcher argument vector
cargo run -p aemulator-cli -- dtb out.dtb         # hand-authored device tree
cargo run -p aemulator-cli -- lint <image-dir>    # scan a built image for virtualization tells

# The merge gate: fmt --check + clippy -D warnings + tests + the identity-validator blocklist
# grep over props/bootconfig/DTB (see ci/check.sh):
bash ci/check.sh

# Phase-0 oracle (Apple Silicon dev loop):
bash oracle/scripts/qemu-hvf-boot.sh args         # prove the VMM crate's arg-gen (no boot)
bash oracle/scripts/qemu-hvf-boot.sh boot         # + boot ARM64 Linux under -accel hvf
bash oracle/scripts/detect-baseline.sh            # detection baseline vs a live stock emulator
```

The Linux/KVM crosvm backend is Linux-only and gated behind the `kvm` feature:
`cargo build -p aemulator-vmm --features kvm` (builds green everywhere; the module body is
`cfg(target_os = "linux")`, so it is absent on macOS).

### Repo-root task runner (pnpm)

The cargo workspace, the two *excluded* crates (`aemulator-hvf`, `aemulator-studio/src-tauri`) and
the Studio frontend each need a different invocation. The root `package.json` fronts all of them so
every task is one command from the repo root — it declares **no dependencies** and only delegates
(`pnpm -C aemulator-studio …` for Studio, `cargo …` for Rust). `ci/check.sh` remains the source of
truth for the merge gate; `pnpm gate` just calls it.

```bash
pnpm setup                # install Studio deps (--frozen-lockfile)

# AEmulator Studio (Tauri GUI)
pnpm tauri:dev            # the main dev loop — Vite + Tauri window   (alias: pnpm dev)
pnpm tauri:build          # bundle the .app / .dmg
pnpm tauri:check          # cargo check the excluded src-tauri crate
pnpm tauri:lint           # clippy -D warnings over src-tauri
pnpm tauri info           # any tauri-cli subcommand passes through
pnpm studio:dev           # Vite alone, no Tauri window
pnpm studio:build         # tsc --noEmit && vite build
pnpm studio:typecheck     # tsc --noEmit only

# Rust workspace
pnpm rust:build           # cargo build --workspace       (pnpm rust:build:release for --release)
pnpm rust:test            # cargo test --workspace        (alias: pnpm test)
pnpm rust:lint            # clippy --all-targets -D warnings, same flags as the gate
pnpm rust:fmt             # cargo fmt --all               (pnpm rust:fmt:check to verify only)
pnpm rust:check           # cargo check --all-targets
pnpm rust:doc             # cargo doc --no-deps

# Excluded macOS-only HVF spike (the emulator binary Studio spawns)
pnpm hvf:build            # release build + ad-hoc codesign — ALWAYS use this, never a bare
                          # `cargo build`: that drops the hypervisor entitlement and every
                          # hv_vm_create then fails with HV_DENIED.
pnpm hvf:sign             # re-apply the entitlement to an existing binary
pnpm hvf:doctor           # verify the whole host dependency chain before hitting Start
pnpm hvf:doctor:fix       # …and re-sign if that is the only thing missing
pnpm hvf:test

# CLI + gates
pnpm cli profile-validate # any aemulator-cli subcommand: pnpm cli props, pnpm cli dtb out.dtb, …
pnpm gate                 # bash ci/check.sh — the full merge gate
pnpm gate:fast            # blocklist gate only (AEMU_SKIP_BUILD_CHECKS=1)
pnpm ci                   # gate + studio typecheck + studio build
pnpm build                # rust:build + studio:build
pnpm lint                 # rust:lint + studio:typecheck
pnpm clean                # cargo clean + Studio dist/ + src-tauri/target/
```

Pass arguments straight after the script name (`pnpm cli props`, `pnpm tauri info`) — **not** after a
`--` separator, which pnpm 11 forwards literally to the underlying binary.

The root `pnpm-workspace.yaml` (`packages: []`) is load-bearing: AEmulator is a subtree of the
avnir-beta monorepo, and it stops pnpm climbing out of this directory to resolve that parent
workspace. `aemulator-studio/` stays its own pnpm root and owns its lockfile and `node_modules`.

### Crate map

| Crate | Role |
|-------|------|
| `aemulator-identity` | the `DeviceProfile` — single source of identity truth (CORE); emits props/bootconfig/MIDR regs, recomputes `ConstructBuildFingerprint`, coherence validator |
| `aemulator-detection` | the adversary's-eye view (leaf): blocklist + rnidbg probe catalogue, `scan_dir`/`lint_properties` |
| `aemulator-dtb` | hand-authored device tree via `vm-fdt`; `assert_no_vm_tells` byte-scan |
| `aemulator-control` | LLM control-plane contract (`UiTree`/`Observation`/`Action` + generation counter) + a fully-tested in-memory `MockController` |
| `aemulator-hal` | host-side **guest-HAL protocol state machines** (leaf, dependency-free): sensors (goldfish ASCII + a coherent motion model), NFC (NCI), Bluetooth (HCI H4), GNSS (NMEA), RIL (AT/27.007), and virtio-vsock packets + a host service registry. Pure bytes-in/bytes-out, so `aemulator-hvf` and the future crosvm backend share one implementation |
| `aemulator-vmm` | identity injection + HVF launcher-arg generation; the KVM/crosvm seam behind `feature=kvm` + `cfg(linux)` |
| `aemulator-supervisor` | device lifecycle (state machine) wiring vmm + control |
| `attestation-relay` | OPTIONAL Phase-5 per-flow relay to real rooted devices (leaf) |
| `aemulator-cli` | the `aemulator` binary — profile-show/validate/props/bootconfig/fingerprint/lint/qemu-args/dtb |
| `oracle/` | Phase-0 bring-up: HVF dev loop + live detection baseline (shell + reports) |

Dependency DAG + frozen per-crate APIs: [`SCAFFOLD.md`](./SCAFFOLD.md). Bring-up go/no-go:
[`docs/PHASE0-RUNBOOK.md`](./docs/PHASE0-RUNBOOK.md).

## Status

The host-side Rust workspace **builds and tests green on macOS / Apple Silicon with default
features** (`cargo build` + `cargo test`: 120 tests pass; `cargo clippy -D warnings` clean). The
identity layer, detection blocklist/catalogue, hand-authored DTB, control-plane contract +
`MockController`, HVF arg-gen, supervisor state machine, CLI, and the Phase-0 oracle scripts are
implemented and unit-tested.

**Honest ceiling** (the wall to hold with stakeholders):

- **Tier A — heuristic / software introspection** (strings, props, files, DTB, GL strings, root
  artifacts, network): **the v1 target — reachable and largely done at the identity layer.**
- **Tier B — commercial RASP + software `MEETS_BASIC_INTEGRITY`**: reachable but never "done" — a
  permanent adversarial-CI cost; on Android 13+ the integrity half needs the relay.
- **Tier C — hardware-backed remote attestation** (Play Integrity DEVICE/STRONG, Key Attestation
  `TrustedEnvironment`/`StrongBox`, Widevine L1, anti-cheat): **permanently unreachable by the VM
  itself — a cryptographic wall, not a difficulty gradient.** Banking/DRM/wallets/anti-cheat are out
  of scope by construction; the only partial path is the optional, per-flow attestation relay.

**Left as documented seams / TODO** (not required for the green default build): the Linux/KVM
`crosvm_backend` body (Phase 1, behind `feature=kvm`), the real guest-image build pipeline (GSI +
de-branded vendor HALs), swapping the `MockController` for a real VMM launch + guest control
channel in `Supervisor::boot`, and the `attestation-relay` `relay()` body (Phase 5). The KVM-fleet
Phase-0 go/no-go gates (Cuttlefish boot, `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS` probe, GPU-identity
decision) require a bare-metal ARM host — see the runbook.

## License

Host-side Rust is BSD-3-Clause (the crosvm base is BSD-3). Guest-image components carry their own
licenses (GSI: Apache-2.0; kernel: GPLv2). GMS/Play is a separate licensing wall — not redistributed.
