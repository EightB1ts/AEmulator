# aemulator-hvf — Phase 1 Status

**Phase 1 goal:** a from-scratch aarch64 VMM in Rust on Apple Silicon (M2) via Apple's
Hypervisor.framework (HVF), booting a real Linux kernel to an interactive userspace shell with a
spoofed (non-Apple) CPU identity.

**Status: COMPLETE — highest milestone reached = interactive shell (`shell-prompt`).**
Observed end-to-end in captured console output (`testdata/integration-run-console.log`): the guest
boots Alpine 3.20 (kernel `6.6.134-0-virt`), drops to the busybox rescue shell on `ttyAMA0`, and
executes commands typed on host stdin, echoing results back over the console. Build is green and
warning-clean; no VMM-level faults or unexpected exits occurred during the run.

---

## What was verified this run (all from captured output, not claimed)

Boot header (VMM side):

```
[ok] hv_vm_create
[ok] hv_vm_map: 1024 MiB RWX @ IPA 0x80000000
[ok] virtio slot 0: type 3 @ IPA 0xa000000 SPI 16 (INTID 48)   # console
[ok] virtio slot 1: type 4 @ IPA 0xa000200 SPI 17 (INTID 49)   # rng
[ok] virtio slot 2: type 2 @ IPA 0xa000400 SPI 18 (INTID 50)   # block
[ok] initrd: 8791628 bytes @ IPA 0x98000000..0x9886264c
[ok] kernel entry IPA 0x80000000, FDT IPA 0x90000000
MIDR_EL1: real 0x...610f0000 (impl 0x61) -> set 0x...511f8041 accepted=true readback impl 0x51
[ok] boot regs: PC=0x80000000 X0(fdt)=0x90000000 CPSR=0x3c5
```

Guest side (interactive shell, live command execution):

| Check | Command | Result observed |
|---|---|---|
| Shell alive | `echo AEMULATOR_SHELL_OK` | `AEMULATOR_SHELL_OK` |
| Kernel identity | `uname -a` | `Linux (none) 6.6.134-0-virt ... aarch64` |
| virtio-block RW roundtrip | write then read `/dev/vda` | `AVNIR-BLOCK-RW-TEST` read back verbatim |
| GIC SPI injection | `grep virtio /proc/interrupts` | `virtio0 INTID 48` (console), `virtio2 INTID 50, 210 IRQs` |
| **CPU spoof to userspace** | `cat /proc/cpuinfo` | `CPU implementer: 0x51` (Qualcomm), `CPU part: 0x804` |

The MIDR spoof (Apple `0x61` → Qualcomm `0x51`) is accepted by HVF and propagates all the way to
guest userspace `/proc/cpuinfo` on the boot CPU (and on secondaries when `--cpus 2`).

---

## Module map (`src/`)

| File | Responsibility |
|---|---|
| `lib.rs` | Raw `extern "C"` HVF FFI (`hv_vm_*`, `hv_vcpu_*`, sys/GP reg enums, exit structs, return-code names) + ESR/EC helpers. Dependency-free. |
| `memory.rs` | `GuestRam`: host-anonymous mmap of guest DRAM + `hv_vm_map` RWX at IPA `0x8000_0000`; unmap on teardown. |
| `boot.rs` | `stage_boot`: loads the arm64 `Image` at DRAM base, copies the initramfs high, builds the FDT (GIC + generic-timer + N cpus + PSCI + virtio-mmio nodes + `/chosen` bootargs/initrd) and stages it. Returns `BootPlan { entry_ipa, fdt_ipa }`. |
| `mmio.rs` | `Pl011`: the built-in PL011 UART at `0x0900_0000` (QEMU-virt address, so stock earlycon just works). TX to host stdout, host-stdin RX buffer, INTID 33. |
| `gic.rs` | Userspace **GICv3**: distributor (`0x3fff_0000`) + per-CPU redistributor (`0x3fde_0000`, `0x20000` stride) MMIO, trapped `ICC_*_EL1` CPU-interface sysregs, SPI/PPI/SGI routing, vtimer PPI 27 injection, per-CPU pending-IRQ evaluation. |
| `vcpu.rs` | `Vcpu` + `VmContext`: vCPU create/reset (spoofed MIDR + MPIDR), the `hv_vcpu_run` exit-decode loop (data abort → PL011/virtio/GIC dispatch, sysreg trap → GIC CPU interface, HVC/SMC → PSCI, WFx park, `VTIMER_ACTIVATED` → PPI 27), and PSCI `CPU_ON` secondary-vCPU spawning. Shared state (GIC/UART/virtio/RAM/cpu-ids) behind mutexes in `VmContext`. |
| `virtio/mod.rs` | virtio-mmio layout constants (`0x0a00_0000`, `0x200` stride, first SPI 16), slot→IPA/SPI/INTID helpers, `locate(ipa)` dispatch. |
| `virtio/mmio.rs` | `MmioTransport`: the virtio-mmio register file + virtqueue (split-ring) walking; raises an SPI when a used buffer is added. |
| `virtio/{console,rng,block}.rs` | `Backend` device implementations: console (TX/RX over rings), rng (fills a buffer), block (read/write a backing file at 512-byte LBAs). |
| `machine.rs` | `run_linux`: wires it all together — create VM, map RAM, build the virtio fleet, stage boot, create the boot vCPU, spawn the host-stdin input thread, run, reap secondaries, report. |
| `bin/aemulator-hvf.rs` | CLI: `run-linux --kernel … [--initrd …] [--disk …] [--mem-mb N] [--cpus N] [--cmdline …]`. |
| `bin/hvf-midr-probe.rs` | Standalone empirical MIDR-spoof probe (the original proof that HVF accepts a fake `MIDR_EL1`). |

**IPA map** (mirrors crosvm/QEMU-virt): DRAM `0x8000_0000`; GIC dist `0x3fff_0000`, redist
`0x3fde_0000`; PL011 `0x0900_0000`; virtio-mmio `0x0a00_0000` (`0x200` stride). FDT at DRAM+`0x1000_0000`,
initrd at DRAM+`0x1800_0000`.

---

## What works

- HVF VM + RAM map + entitled vCPU on Apple M2.
- Full arm64 Linux boot: decompress/reloc, FDT parse, earlycon PL011, memory/NUMA/zones, PSCI over
  our HVC handler, userspace GICv3 init, generic timer, SMP bring-up (with `--cpus 2`, a real
  secondary vCPU thread via PSCI `CPU_ON`), driver init, PID 1, interactive busybox shell.
- **CPU identity spoof**: MIDR_EL1 → Qualcomm `0x51`/part `0x804`, visible in guest `/proc/cpuinfo`.
- **virtio-mmio**: console (enumerated `/dev/hvc0`), block (`/dev/vda`, RW roundtrip verified,
  210 IRQs serviced through GIC SPI injection), rng (present + enumerated).
- Bidirectional interactive console: host stdin → guest shell → host stdout.

## What remains / known gaps

- **Shell carrier is PL011 (`ttyAMA0`), not virtio-console.** `hvc0` is a kernel *module* that
  registers after PID 1's initial console opens, so with this stock Alpine kernel it can't be the
  initial/interactive console. virtio-console is fully implemented and enumerated, just not the
  shell carrier. (Not a bug — a stock-kernel ordering constraint.)
- **virtio-rng is never driver-bound**: this Alpine kernel ships no `virtio_rng` driver (no
  `/dev/hwrng`), so the (implemented + wired) backend can't be exercised by this kernel.
- **No raw-tty on host**: the host terminal stays in cooked/line-buffered mode this phase (host
  echoes, line-at-a-time). Piping commands into stdin works directly; interactive typing is
  line-buffered.
- **No wall-clock guard inside the VMM**: post-shell the guest idles (WFI parks); an uncapped run
  keeps running until the process is killed. The run harness bounds it externally (see repro).
- **No initramfs root/rootfs mount** beyond the rescue shell — Phase 3 (Android/Cuttlefish) territory.

---

## Repro

Prereqs staged in `testdata/` (all git-ignored): `Image` (gunzipped Alpine aarch64 kernel),
`initramfs-virt` (Alpine netboot initramfs), `scratch.img` (64 MiB, auto-created).

One-shot (build + sign + interactive run):

```bash
./crates/aemulator-hvf/run-linux.sh
```

Manual:

```bash
cd crates/aemulator-hvf
cargo build --manifest-path Cargo.toml
codesign --sign - --entitlements hv.entitlements --force target/debug/aemulator-hvf
target/debug/aemulator-hvf run-linux \
  --kernel testdata/Image --initrd testdata/initramfs-virt \
  --disk testdata/scratch.img --mem-mb 1024 --cpus 1
```

Bounded/non-interactive (pipe commands, kill after N s — macOS has no `timeout`):

```bash
( sleep 20; printf 'echo AEMULATOR_SHELL_OK\nuname -a\ncat /proc/cpuinfo | grep implementer\n'; sleep 6 ) \
  | ( target/debug/aemulator-hvf run-linux --kernel testdata/Image --initrd testdata/initramfs-virt \
        --disk testdata/scratch.img --mem-mb 1024 --cpus 1 & \
      P=$!; ( perl -e 'sleep 32; kill 9, shift' $P & ); wait $P )
```

Build discipline: this crate is workspace-**excluded**; always build it with
`--manifest-path crates/aemulator-hvf/Cargo.toml`, never via the root `cargo build`.

---

## Next concrete steps (toward Phase 2)

1. **Raw-tty host mode** (termios cbreak, disable host echo) for true interactive typing, plus a
   PTY option so the console can be driven programmatically without cooked-mode line buffering.
2. **Wall-clock / idle guard inside the VMM** (`--run-secs`) so an uncapped run self-terminates
   instead of relying on an external killer.
3. **virtio-console as the primary console** by shipping a kernel with `virtio_console` built-in
   (`console=hvc0`), or wiring an earlycon→hvc0 handoff — removes the PL011 dependency.
4. **virtio-net** (rust-vmm `virtio-queue` + a host tap/user-net backend) — first real I/O beyond
   block, needed before Android.
5. **Phase 2 (graphics + input to a window)**: virtio-gpu + virtio-input backends rendering to a
   host window; this is the next Task milestone.
6. **Snapshot/inspection ergonomics**: dump ESR/PC/FAR on any unexpected exit into a structured
   log for faster future debugging (the `StopReason::Fault` tuple is already carried — surface it).
