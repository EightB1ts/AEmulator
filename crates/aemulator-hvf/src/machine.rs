//! Assemble the pieces into a bootable VM: create the HVF VM, map RAM, stage the kernel + FDT,
//! create the vCPU with a spoofed MIDR, and run the earlycon loop.

use crate::boot::stage_boot;
use crate::memory::GuestRam;
use crate::secure_env::{self, SecChannel};
use crate::vcpu::{spoof_midr, StopReason, Vcpu, VmContext};
use crate::virtio::pci::{self, PciBus, PciDevice};
use crate::virtio::{
    block::Block, console::Console, gpu::Gpu, input::Input, net::Net, rng::Rng, Backend,
    MmioTransport, ScanoutHandle,
};
use crate::*;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default guest DRAM: 512 MiB (a multiple of the 16 KiB granule; big enough for a distro kernel).
pub const DEFAULT_RAM_BYTES: usize = 512 * 1024 * 1024;

/// virtio-gpu default scanout size (the mode reported by GET_DISPLAY_INFO + the initial fb0).
pub const GPU_WIDTH: u32 = 1024;
pub const GPU_HEIGHT: u32 = 768;

/// Number of guest vCPUs. 2 exercises PSCI `CPU_ON` / SMP bring-up through the userspace GIC.
pub const DEFAULT_NUM_CPUS: u64 = 2;

/// Kernel command line. The interactive rescue shell runs on the **built-in** PL011 (`ttyAMA0`):
/// it is the only console registered when the kernel opens PID 1's initial console, so the shell
/// inherits a valid, bidirectional stdio (virtio-console `hvc0` is a module — it registers later,
/// after the initramfs loads `virtio_mmio`, too late to be the initial console). `modules=` still
/// forces `virtio_mmio` + `virtio_blk` early so the guest enumerates our virtio-console (`/dev/hvc0`)
/// and virtio-block (`/dev/vda`) devices during boot.
/// `virtio_gpu` is added to `modules=` so the guest loads the virtio-gpu DRM driver (and, via its
/// module deps, `drm`/`drm_kms_helper`/`drm_shmem_helper`); with `CONFIG_FRAMEBUFFER_CONSOLE` the
/// kernel then switches `fbcon` onto `/dev/fb0` (the GPU scanout). `console=ttyAMA0` stays the
/// serial log so the boot trail is still captured headlessly.
pub const DEFAULT_CMDLINE: &str =
    "earlycon=pl011,0x9000000 console=ttyAMA0 nokaslr modules=virtio_mmio,virtio_blk,virtio_gpu";

/// Command line for `--display` mode. Identical to [`DEFAULT_CMDLINE`] except it appends
/// `console=tty0` as the **last** `console=`. In Linux the last `console=` is the one `/dev/console`
/// (and therefore PID 1 / the initramfs rescue shell) binds to, so the interactive shell renders on
/// the **framebuffer console** (`tty0` → the virtio-gpu scanout `/dev/fb0`) that the window presents,
/// and the in-kernel keyboard handler (which delivers virtio-input keys to the foreground VT = tty0)
/// feeds that same shell — so window keystrokes are echoed back as visible text in the window.
/// `console=ttyAMA0` is kept first so the serial boot log is still captured headlessly alongside the
/// window. The no-`--display` path stays on [`DEFAULT_CMDLINE`] (serial-only), exactly as Phase 1.
pub const DEFAULT_DISPLAY_CMDLINE: &str = "earlycon=pl011,0x9000000 console=ttyAMA0 console=tty0 nokaslr modules=virtio_mmio,virtio_blk,virtio_gpu";

/// How many `hv_vcpu_run` iterations to allow before declaring the guest parked. High enough that
/// the run is bounded by the wall-clock guard, not the iteration budget, once it reaches an idle
/// shell prompt.
pub const DEFAULT_MAX_ITERS: u64 = 100_000_000;

/// One host input frame headed for the guest's virtio-input device: a coherent set of
/// `(type, code, value)` events (the transport appends the trailing `EV_SYN`/`SYN_REPORT`).
pub type InputFrame = Vec<(u16, u16, i32)>;

/// The meeting point between the VMM (worker threads) and the display window (main thread), built
/// inside [`boot_and_run`] and handed to the main thread over a channel. See `display::run_event_loop`.
pub struct DisplayBridge {
    /// The virtio-gpu scanout framebuffer + flush generation — read by the window on each redraw.
    pub scanout: ScanoutHandle,
    /// Host input frames pushed by the window; the VMM's input-inject thread delivers them.
    pub input_tx: Sender<InputFrame>,
    /// Phase 6 (control plane only): host bytes pushed into the guest **serial console** (PL011 +
    /// virtio-console), so the control `launch` verb can drive `am start` without adb. `None` on the
    /// windowed display path (which leaves the guest console fed from stdin, Phase-1 behaviour).
    pub console_tx: Option<Sender<Vec<u8>>>,
    /// Initial scanout size (the window opens at this size; later resizes are handled on redraw).
    pub width: u32,
    pub height: u32,
    /// Set to `true` by the VMM thread once the guest has stopped, so the window can close itself.
    pub vmm_done: Arc<AtomicBool>,
}

pub struct Config<'a> {
    pub kernel_path: &'a Path,
    pub ram_bytes: usize,
    pub cmdline: &'a str,
    pub max_iters: u64,
    pub num_cpus: u64,
    /// Optional initramfs/initrd copied high in RAM and described to the kernel via `/chosen`.
    pub initrd_path: Option<&'a Path>,
    /// Files backing virtio-block devices, in order (`/dev/vda`, `/dev/vdb`, …). One entry per
    /// `--disk`. Empty = no block device (Phase-1 default). The Android boot path attaches the
    /// Cuttlefish partitions here (super, vbmeta*, userdata, misc, metadata, …).
    pub disk_paths: &'a [std::path::PathBuf],
    /// Android/Cuttlefish disk presentation: when set, ONE virtio-blk device is built carrying a
    /// synthetic GPT whose partition entries have the required by-name labels (`super`, `boot_a`,
    /// `vbmeta*_a`, `metadata`, `userdata`, `misc`, …), each mapped to its raw image file. This is
    /// what makes `/dev/block/by-name/<label>` resolve in the guest so first-stage init's
    /// `BlockDevInitializer` finds its partitions. Takes precedence over `disk_paths` (the two are
    /// not combined). `None` on the Linux path → the per-file `disk_paths` loop runs unchanged.
    pub composite_partitions: Option<&'a [crate::virtio::block_composite::PartitionSpec]>,
    /// Optional Android **bootconfig** blob appended after the initramfs (with the `#BOOTCONFIG`
    /// footer the kernel scans for). Carries `androidboot.hardware=cutf_cvm` + friends. `None` on
    /// the Linux path — the Phase-1/2/3 boots stay byte-identical.
    pub bootconfig: Option<&'a [u8]>,
    /// Attach a virtio-net NIC (`eth0`). Pass-1 is a drop/stub backend that enumerates + links up
    /// (TX drained-and-dropped, no inbound). Off by default so the Phase-1/2/3 device fleet — and
    /// thus their regression runs — stay byte-identical.
    pub netdev: bool,
    /// Phase 3d integration: attach the **gfxstream 3D renderer** to virtio-gpu so the device
    /// advertises `VIRTIO_GPU_F_VIRGL` and forwards the guest's 3D control stream into
    /// `libgfxstream_backend.dylib` (via [`crate::virtio::gpu3d::GfxstreamRenderer`]). Only honored
    /// on the `--features gfxstream` build; the default build ignores it and stays pure-2D. Requires
    /// the host renderer to initialize (`stream_renderer_init`) with the KosmicKrisp ICD + the Vulkan
    /// loader staged — see `run-linux-gpu3d.sh`. If the host renderer fails to init, boot falls back
    /// to the 2D device.
    pub gpu3d: bool,
    /// Phase 5 (stealth / de-brand): the real-SoC device-tree root identity `(compatible-list,
    /// model)` to stamp in place of the `linux,dummy-virt` / `aemulator-hvf` placeholders. The
    /// Android path sets this from the `aemulator-identity` DeviceProfile via `aemulator-dtb`
    /// (`["xiaomi,fuxi","qcom,kalama"]`, `"Qualcomm Technologies, Inc SM8550"`). `None` on the Linux
    /// path → the FDT keeps its Phase-1/2/3 placeholder root, so those regression boots stay
    /// byte-identical.
    pub dt_identity: Option<(Vec<String>, String)>,
}

/// Boot a Linux kernel and stream its earlycon output to stdout (headless). Returns the stop reason.
pub fn run_linux(cfg: &Config) -> Result<StopReason, String> {
    run_linux_inner(cfg, None)
}

/// Boot with the display window layer: the VMM runs on this (worker) thread and hands a
/// [`DisplayBridge`] back over `bridge_tx` once the scanout + input channel exist, so the caller's
/// **main** thread can run the winit event loop (macOS requires AppKit on the main thread). `vmm_done`
/// is set once the guest stops, signalling the window to close.
pub fn run_linux_display(
    cfg: &Config,
    bridge_tx: SyncSender<DisplayBridge>,
    vmm_done: Arc<AtomicBool>,
) -> Result<StopReason, String> {
    let done = vmm_done.clone();
    let result = run_linux_inner(cfg, Some((bridge_tx, vmm_done)));
    done.store(true, Ordering::Release);
    result
}

fn run_linux_inner(
    cfg: &Config,
    display: Option<(SyncSender<DisplayBridge>, Arc<AtomicBool>)>,
) -> Result<StopReason, String> {
    println!("== aemulator-hvf run-linux ==");
    println!("host page size : {} bytes", host_page_size());
    println!("kernel         : {}", cfg.kernel_path.display());
    println!(
        "guest RAM      : {} MiB @ {:#x}",
        cfg.ram_bytes / (1024 * 1024),
        crate::memory::DRAM_BASE
    );
    println!("vCPUs          : {}", cfg.num_cpus);
    println!(
        "GICv3          : dist @ {:#x} redist @ {:#x}",
        gic::GICD_BASE,
        gic::GICR_BASE
    );
    println!("cmdline        : {}", cfg.cmdline);

    // --- 1. Create the VM (needs the com.apple.security.hypervisor entitlement). ---
    let rc = unsafe { hv_vm_create(ptr::null_mut()) };
    if rc != HV_SUCCESS {
        return Err(format!(
            "hv_vm_create -> {} ({:#010x}) — is the binary signed with hv.entitlements?",
            hv_return_name(rc),
            rc as u32
        ));
    }
    println!("[ok] hv_vm_create");

    let result = boot_and_run(cfg, display);

    let _ = unsafe { hv_vm_destroy() };
    result
}

fn boot_and_run(
    cfg: &Config,
    display: Option<(SyncSender<DisplayBridge>, Arc<AtomicBool>)>,
) -> Result<StopReason, String> {
    // --- 2. Guest RAM + stage-2 map. ---
    // Guard the RAM-vs-PCI-shmem-window invariant BEFORE mapping anything. An overlap is otherwise
    // silent and near-undiagnosable: the guest boots, Linux fails to assign PCI resources inside the
    // overlapped 64-bit window, virtio-blk comes up without usable BARs, /dev/vda never appears, and
    // first-stage init reboot-loops with "Failed to create devices required for first stage mount" —
    // no message anywhere naming memory layout as the cause.
    {
        let ram_top = crate::memory::DRAM_BASE + cfg.ram_bytes as u64;
        if ram_top > pci::PCI_SHM_BASE {
            return Err(format!(
                "guest RAM ({} MiB at {:#x}) ends at {:#x}, which overlaps the virtio-gpu \
                 host-visible shmem window at {:#x}+{:#x}. Reduce --mem-mb to {} MiB or less, or \
                 raise PCI_SHM_BASE.",
                cfg.ram_bytes / (1024 * 1024),
                crate::memory::DRAM_BASE,
                ram_top,
                pci::PCI_SHM_BASE,
                pci::PCI_SHM_SIZE,
                (pci::PCI_SHM_BASE - crate::memory::DRAM_BASE) / (1024 * 1024),
            ));
        }
    }
    let ram = GuestRam::new(cfg.ram_bytes)?;
    ram.map_into_vm()?;
    println!(
        "[ok] hv_vm_map: {} MiB RWX @ IPA {:#x}",
        ram.size / (1024 * 1024),
        ram.base
    );

    // --- 3. Build the virtio device fleet. Two transports:
    //   * Android (Cuttlefish): virtio-**PCI** — the GKI kernel is `CONFIG_VIRTIO_MMIO=n` and ships
    //     only `virtio_pci.ko`, so every device (console, rng, composite block, gpu, input, net)
    //     lives on the PCI host bridge. The MMIO fleet stays empty.
    //   * Linux (Phase 1/2/3): virtio-**MMIO** exactly as before — the PCI bus stays empty.
    let mut transports: Vec<MmioTransport> = Vec::new();
    let mut pci = PciBus::new(Vec::new());
    let scanout: ScanoutHandle;

    if let Some(specs) = cfg.composite_partitions {
        // ---- Android: the whole fleet over virtio-PCI ----
        let mut devs: Vec<PciDevice> = Vec::new();
        // The 12 HVC console channels (hvc0..hvc11), placed FIRST so the guest's virtio_console
        // driver numbers them hvc0..hvc11 in PCI probe (slot) order. The HAL channels bridge to the
        // host secure_env; hvc11 is the KeyMint HAL the whole keystore2/apexd chain waits on.
        let env = secure_env::shared();
        for hvc in 0..12usize {
            let console = match hvc {
                0 => Console::new(),            // hvc0: interactive console
                2 => Console::log("[logcat] "), // hvc2: logd → host stdout
                3 => Console::secure(SecChannel::Keymaster, env.clone()), // hvc3: keymaster
                4 => Console::secure(SecChannel::Gatekeeper, env.clone()), // hvc4: gatekeeper
                10 => Console::secure(SecChannel::Oemlock, env.clone()), // hvc10: oemlock
                11 => Console::secure(SecChannel::Keymint, env.clone()), // hvc11: KeyMint (blocker)
                _ => Console::sink(),           // hvc1/5/6/7/8/9: drain
            };
            devs.push(pci_dev(Backend::Console(console), devs.len()));
        }
        println!(
            "[ok] virtio-pci consoles: 12 HVC channels (hvc0..hvc11); secure_env on hvc3/4/10/11"
        );
        // rng
        devs.push(pci_dev(Backend::Rng(Rng::new()), devs.len()));
        // composite GPT block (slot 3) — /dev/vda carrying the by-name partitions
        let cdisk = crate::virtio::block_composite::CompositeBlock::new(specs)
            .map_err(|e| format!("composite virtio-block init: {e}"))?;
        println!(
            "[ok] virtio-pci block[composite] {} partitions, {} MiB GPT disk → /dev/vda (00:{:02x}.0 BAR0 {:#x})",
            specs.len(),
            cdisk.total_size() / (1024 * 1024),
            pci::device_slot(devs.len()),
            pci::device_bar0(devs.len()),
        );
        for line in cdisk.describe(specs) {
            println!("        {line}");
        }
        devs.push(pci_dev(Backend::CompositeBlock(cdisk), devs.len()));
        // gpu (slot 4) — keep its scanout handle for the display layer
        let gpu = build_gpu(cfg.gpu3d);
        scanout = gpu.handle();
        // When a gfxstream renderer is attached the device advertises VIRTIO_GPU_F_RESOURCE_BLOB, so
        // give its PCI device the 64-bit host-visible shared-memory BAR (shmid 0) the guest's
        // virtio-gpu driver maps blobs into (the gfxstream ASG ring window).
        let gpu_blob = gpu.advertises_blob();
        let gpu_idx = devs.len();
        #[allow(unused_mut)]
        let mut gpu_dev = pci_dev(Backend::Gpu(gpu), gpu_idx);
        if gpu_blob {
            gpu_dev.attach_shm_bar(pci::PCI_SHM_BASE, pci::PCI_SHM_SIZE);
            println!(
                "[ok] virtio-gpu host-visible shmem: BAR2 {:#x}+{:#x} → guest IPA, shmid 1 (VIRTIO_GPU_F_RESOURCE_BLOB)",
                pci::PCI_SHM_BASE,
                pci::PCI_SHM_SIZE
            );
        }
        devs.push(gpu_dev);
        // input (slot 5)
        devs.push(pci_dev(Backend::Input(Input::new_tablet()), devs.len()));
        println!("[ok] virtio-pci input tablet (abs pointer + keyboard)");
        // net (slot 6)
        if cfg.netdev {
            devs.push(pci_dev(Backend::Net(build_net()), devs.len()));
        }
        for (i, d) in devs.iter().enumerate() {
            println!(
                "[ok] virtio-pci slot {i}: type {} @ 00:{:02x}.0 BAR0 {:#x} SPI {} (INTID {})",
                d.backend_type(),
                d.dev_num,
                pci::device_bar0(i),
                d.intid - 32,
                d.intid,
            );
        }
        pci = PciBus::new(devs);
        return boot_and_run_common(cfg, display, ram, transports, pci, scanout);
    }

    // ---- Linux (Phase 1/2/3): virtio-MMIO, unchanged ----
    let console_slot = transports.len();
    transports.push(MmioTransport::new(
        Backend::Console(Console::new()),
        virtio::device_intid(console_slot),
    ));
    let rng_slot = transports.len();
    transports.push(MmioTransport::new(
        Backend::Rng(Rng::new()),
        virtio::device_intid(rng_slot),
    ));
    if let Some(specs) = cfg.composite_partitions {
        // Single GPT composite virtio-blk disk (Android/Cuttlefish by-name partitions).
        let cdisk = crate::virtio::block_composite::CompositeBlock::new(specs)
            .map_err(|e| format!("composite virtio-block init: {e}"))?;
        let slot = transports.len();
        println!(
            "[ok] virtio-block[composite] {} partitions, {} MiB GPT disk → /dev/vda @ IPA {:#x}",
            specs.len(),
            cdisk.total_size() / (1024 * 1024),
            virtio::device_ipa(slot)
        );
        for line in cdisk.describe(specs) {
            println!("        {line}");
        }
        transports.push(MmioTransport::new(
            Backend::CompositeBlock(cdisk),
            virtio::device_intid(slot),
        ));
    } else {
        for (n, disk) in cfg.disk_paths.iter().enumerate() {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(disk)
                .map_err(|e| format!("open disk {disk:?}: {e}"))?;
            let blk = Block::new(f).map_err(|e| format!("virtio-block init: {e}"))?;
            let slot = transports.len();
            transports.push(MmioTransport::new(
                Backend::Block(blk),
                virtio::device_intid(slot),
            ));
            let vd = (b'a' + n as u8) as char;
            println!(
                "[ok] virtio-block[{n}] backed by {disk:?} → /dev/vd{vd} @ IPA {:#x}",
                virtio::device_ipa(slot)
            );
        }
    }

    // virtio-gpu — appended after the Phase-1 devices so their slots/SPIs are stable. Its scanout
    // framebuffer handle is kept for the display layer. In the default (2D) mode it is a plain
    // dumb-framebuffer device (Phase 2). Under `--features gfxstream` with `--gpu3d` it is upgraded
    // to a `VIRTIO_GPU_F_VIRGL` device whose 3D control stream forwards into the gfxstream host
    // renderer (Phase 3d integration); a flushed 3D resource is read back into the same 2D scanout
    // the window presents.
    let gpu = build_gpu(cfg.gpu3d);
    scanout = gpu.handle();
    let gpu_slot = transports.len();
    transports.push(MmioTransport::new(
        Backend::Gpu(gpu),
        virtio::device_intid(gpu_slot),
    ));

    // virtio-input (combined absolute-pointer + keyboard tablet). Host events are injected via
    // `MmioTransport::inject_input_events` from the display/control plane (a window, next task).
    let input_slot = transports.len();
    transports.push(MmioTransport::new(
        Backend::Input(Input::new_tablet()),
        virtio::device_intid(input_slot),
    ));
    println!("[ok] virtio-input tablet (abs pointer + keyboard)");

    // virtio-net — appended last so the Phase-1/2/3 device slots stay fixed when it is absent.
    // Pass-1 is an enumerate-only drop/stub NIC (`eth0`, link UP); Cuttlefish/Android expects at
    // least one NIC present. Off unless `--net` is passed.
    if cfg.netdev {
        let slot = transports.len();
        transports.push(MmioTransport::new(
            Backend::Net(build_net()),
            virtio::device_intid(slot),
        ));
    }

    for (i, t) in transports.iter().enumerate() {
        println!(
            "[ok] virtio slot {i}: type {} @ IPA {:#x} SPI {} (INTID {})",
            t.device_type(),
            virtio::device_ipa(i),
            virtio::device_spi(i),
            t.intid,
        );
    }

    boot_and_run_common(cfg, display, ram, transports, pci, scanout)
}

/// Default virtio-net MAC (locally-administered; QEMU `52:54:00` OUI).
const NET_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xae, 0x00, 0x01];

/// Build the virtio-net backend: a real libslirp user-mode NAT NIC (guest 10.0.2.15/24, gw
/// 10.0.2.2, DNS 10.0.2.3, DHCP). Host port-forwards default to TCP 5555→5555 and can be extended
/// via the `AEMU_HOSTFWD` env var (`tcp:HOST:GUEST,udp:HOST:GUEST,…`).
///
/// NAT is **not optional** — there is no build of this VMM without it (see the crate-level note in
/// `lib.rs`). The only remaining stub path is the runtime one below: if `slirp` init fails on this
/// machine we log loudly and fall back to a dead NIC so the guest still boots and the failure is
/// diagnosable, rather than aborting the VM.
fn build_net() -> Net {
    let hostfwds = crate::net::slirp::parse_hostfwds(std::env::var("AEMU_HOSTFWD").ok().as_deref());
    match crate::net::SlirpNet::new(&hostfwds) {
        Ok(slirp) => {
            println!(
                "[ok] virtio-net (libslirp user-mode NAT) → eth0: guest 10.0.2.15/24 gw 10.0.2.2 dns 10.0.2.3"
            );
            Net::with_slirp(NET_MAC, slirp)
        }
        Err(e) => {
            eprintln!(
                "[net][warn] slirp init failed ({e}); falling back to enumerate-only stub NIC"
            );
            Net::new(NET_MAC)
        }
    }
}

/// One PCI-transport virtio device at fleet index `i` (SPI/INTID + slot + BAR0 all derived from `i`).
fn pci_dev(backend: Backend, i: usize) -> PciDevice {
    PciDevice::new(
        backend,
        virtio::device_intid(i),
        pci::device_slot(i),
        pci::device_bar0(i),
    )
}

/// Shared boot tail for both transports: stage the kernel + FDT (MMIO virtio nodes OR a PCI host
/// bridge node, depending on which fleet is non-empty), build the shared VM state, and run cpu0.
fn boot_and_run_common(
    cfg: &Config,
    display: Option<(SyncSender<DisplayBridge>, Arc<AtomicBool>)>,
    ram: GuestRam,
    transports: Vec<MmioTransport>,
    pci: PciBus,
    scanout: ScanoutHandle,
) -> Result<StopReason, String> {
    let num_virtio = transports.len();
    let pci_irqs = pci.irq_map();

    // --- 4. Load kernel + initrd + FDT (GIC + timer + N cpus + virtio-mmio OR pci-host nodes). ---
    let plan = stage_boot(
        &ram,
        cfg.kernel_path,
        cfg.cmdline,
        cfg.num_cpus,
        cfg.initrd_path,
        num_virtio,
        &pci_irqs,
        cfg.bootconfig,
        cfg.dt_identity
            .as_ref()
            .map(|(c, m)| (c.as_slice(), m.as_str())),
    )?;
    println!(
        "[ok] kernel entry IPA {:#x}, FDT IPA {:#x}",
        plan.entry_ipa, plan.fdt_ipa
    );

    // --- 5. Shared VM state (GIC + UART + virtio + guest RAM) + the boot vCPU on this thread. ---
    let ctx = VmContext::new(
        cfg.num_cpus,
        plan.fdt_ipa,
        cfg.max_iters,
        ram.mem.clone(),
        transports,
        pci,
    );

    // gfxstream fence bridge: gfxstream signals fence completion asynchronously from a renderer
    // thread; this raises the virtio-gpu SPI on each completion so the guest's `gfxstream_vk` driver
    // (which waits on virtio-gpu fences to know the host drained the ASG ring) unblocks. Only active
    // on the `--features gfxstream` build.
    spawn_gpu_fence_thread(&ctx);

    // libslirp NAT: drive the slirp poll loop on a dedicated thread and flush inbound frames into
    // the guest receiveq (raising the net device's SPI). No-op if there is no NAT-backed NIC.
    spawn_slirp_poll_thread(&ctx);

    // In **control mode** (Phase 6) stdin is the command REPL, so the guest console must NOT be fed
    // from stdin (env-gated to keep the display/headless paths byte-identical). Instead the guest
    // console is driven over a channel exposed on the bridge as `console_tx` (used by `launch`).
    let control_mode = std::env::var_os("AEMU_CONTROL_NO_CONSOLE_STDIN").is_some();
    let console_tx = if control_mode {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        spawn_console_inject_thread(&ctx, rx);
        // Automatic guest network bring-up + adb-over-TCP. Only meaningful with a NIC attached and
        // in control mode (where `console_tx` drives the guest root console). Writes an idempotent
        // bring-up script to the console on a timer, re-asserting the policy route/rule to beat
        // netd reconciliation. Disable with `AEMU_NET_AUTOCONF=0`. Pure-std, so the default
        // (non-`net`) build stays hermetic; a NIC-less run simply never spawns it.
        let autoconf_on = std::env::var_os("AEMU_NET_AUTOCONF").is_none_or(|v| v != "0");
        if cfg.netdev && autoconf_on {
            spawn_net_autoconfig_thread(tx.clone());
        }
        Some(tx)
    } else {
        spawn_console_input_thread(&ctx);
        None
    };

    // Display hookup: spawn the input-inject thread (window/control → guest virtio-input) and hand
    // the scanout + input channel to the main thread's event loop via the bridge. Without --display
    // this is skipped and the run stays on the exact Phase-1 headless path.
    if let Some((bridge_tx, vmm_done)) = display {
        let (input_tx, input_rx) = std::sync::mpsc::channel::<InputFrame>();
        spawn_input_inject_thread(&ctx, input_rx);
        let bridge = DisplayBridge {
            scanout: scanout.clone(),
            input_tx,
            console_tx,
            width: GPU_WIDTH,
            height: GPU_HEIGHT,
            vmm_done,
        };
        if bridge_tx.send(bridge).is_err() {
            eprintln!("[warn] display bridge receiver gone before boot; continuing headless");
        }
    }

    let mut vcpu = Vcpu::new(0)?;
    vcpu.set_mpidr()?;
    println!("[ok] hv_vcpu_create: cpu0 id {}", vcpu.id);

    let real_midr = vcpu.get_midr();
    let spoofed = spoof_midr(0);
    let set_ok = vcpu.set_midr(spoofed);
    let after = vcpu.get_midr();
    println!(
        "MIDR_EL1: real {:#018x} (impl {:#04x}) -> set {:#018x} accepted={} readback {:#018x} (impl {:#04x})",
        real_midr,
        (real_midr >> 24) & 0xff,
        spoofed,
        set_ok,
        after,
        (after >> 24) & 0xff
    );

    vcpu.set_boot_regs(plan.entry_ipa, plan.fdt_ipa)?;
    println!(
        "[ok] boot regs: PC={:#x} X0(fdt)={:#x} CPSR=0x3c5",
        plan.entry_ipa, plan.fdt_ipa
    );

    // --- 5. Run. Guest console prints appear inline below this line. ---
    println!("---------------- GUEST CONSOLE ----------------");
    let stop = vcpu.run(&ctx)?;
    println!("\n----------------- END CONSOLE -----------------");

    // Reap any secondary vCPU threads spawned via PSCI CPU_ON.
    let handles: Vec<_> = std::mem::take(&mut *ctx.secondaries.lock().unwrap());
    for h in handles {
        match h.join() {
            Ok(Ok(s)) => println!("secondary stop: {s:?}"),
            Ok(Err(e)) => println!("secondary error: {e}"),
            Err(_) => println!("secondary thread panicked"),
        }
    }

    let bytes = ctx.pl011.lock().unwrap().bytes_out;
    println!("guest emitted {bytes} byte(s) via PL011");

    // virtio-gpu evidence: how many scanout flushes the guest issued + how much of the framebuffer
    // it filled (a non-zero, growing figure means the guest's DRM driver drew into our scanout).
    {
        let gen = scanout.generation();
        let fb = scanout.fb.lock().unwrap();
        println!(
            "virtio-gpu scanout: {}x{}, {} flush(es), {} non-zero fb byte(s) of {}",
            fb.width,
            fb.height,
            gen,
            fb.non_zero_bytes(),
            fb.bytes.len()
        );
    }
    // gfxstream evidence: how many host fence completions the async FENCE_CALLBACK observed (a
    // non-zero count means the guest's gfxstream_vk drove the ASG ring and the host fenced work).
    println!(
        "gfxstream: {} fence completion(s) signalled to the guest",
        crate::virtio::gpu3d::fence_count()
    );
    println!("cpu0 stop reason: {stop:?}");

    // Tear the host renderer down while the guest's memory is still mapped and every thread that
    // could touch it is gone. Idempotent and a no-op when `init_gfxstream` never succeeded (2D
    // runs, `--gpu3d` on a host with no ICD), so it is unconditional here.
    //
    // THIS POSITION IS LOAD-BEARING, on three counts:
    //   * AFTER the secondary-vCPU join loop above and after `vcpu.run` returned, so no vCPU thread
    //     exists and no further `flush_3d` can present into the renderer or the layer.
    //   * AFTER `retire_metal_layer` has run on the display thread, which unpublishes the
    //     `CAMetalLayer*` and calls `stream_renderer_release_metal_layer` on it — the cached
    //     `VkSurfaceKHR`/swapchain must be destroyed while both the layer AND the `VkDevice` are
    //     alive. Teardown is what destroys the device.
    //   * BEFORE `ram.unmap_from_vm()`, because teardown drains host GPU work that may still be
    //     reading guest-backed blob memory mapped into the VM.
    //
    // It is also the whole reason 1.2's null-guard in `vkDestroyInstanceImpl` had to land first:
    // teardown runs `~FrameBuffer()`, which runs any surviving process-cleanup callback, which
    // unboxes an already-deleted `VkInstance` box and hands VK_NULL_HANDLE to the destroy path.
    // Unguarded, adding this call converts a shutdown warning into a shutdown crash.
    crate::display::metal::teardown_gfxstream();

    ram.unmap_from_vm();
    Ok(stop)
}

/// Build the virtio-gpu device. `gpu3d` requests the gfxstream 3D upgrade (Phase 3d integration).
///
/// * `gpu3d == false`: a pure-2D scanout device.
/// * `gpu3d == true`: initialize the gfxstream host renderer (`stream_renderer_init`, VK on
///   KosmicKrisp) and, on success, build a `VIRTIO_GPU_F_VIRGL` device that forwards the guest 3D
///   stream into it. If init fails (e.g. the ICD or the loader is not staged) we log and fall back to
///   the 2D device so the guest still boots.
///
/// The renderer is compiled in unconditionally — there is no build of this VMM without gfxstream.
fn build_gpu(gpu3d: bool) -> Gpu {
    if !gpu3d {
        println!("[ok] virtio-gpu 2D scanout {GPU_WIDTH}x{GPU_HEIGHT} → /dev/fb0 (fbcon)");
        return Gpu::new(GPU_WIDTH, GPU_HEIGHT);
    }
    // Initialize the host renderer (surfaceless is fine — we read flushed ColorBuffers back into the
    // 2D scanout the presenter shows, rather than using gfxstream's own subwindow which aborts on
    // macOS; see gfxstream/PHASE3-STATUS.md).
    //
    // **VK only — do NOT enable GLES here.** There is no host OpenGL on macOS (the only host 3D API
    // is Metal, reached through Vulkan), so gfxstream's host GL emulation cannot work. The host
    // renderer flags and the guest's `androidboot.hardware.egl=` choice are ONE contract, and the
    // only coherent macOS pairing is: host VK-only + guest `egl=angle` (ANGLE translates GLES→Vulkan
    // *inside* the guest, then guest Vulkan rides gfxstream to host KosmicKrisp→Metal). That
    // combination boots Android to the lock screen; both alternatives abort the VMM mid-boot:
    //   * guest `egl=emulation` + host VK-only → `FATAL in getEmulationGl … GL/EGL emulation not
    //     enabled` (FrameBuffer.cpp:3418), SIGABRT ~8s in, as dlkm_loader loads
    //     goldfish_address_space.
    //   * guest `egl=emulation` + host VK+GLES → gets as far as SurfaceFlinger's boot animation,
    //     then the host GL path fails for real (`glTexImage2D: got err pre 0x500`) and dies on an
    //     empty callback: `libc++abi: uncaught std::bad_function_call`.
    // `AEMU_GFXSTREAM_GLES=1` opts into the broken-on-macOS host GLES path for A/B testing only.
    let use_gles = std::env::var_os("AEMU_GFXSTREAM_GLES").is_some();
    println!(
        "[gpu3d] stream_renderer_init (VK{} on KosmicKrisp)…",
        if use_gles { "+GLES" } else { "" }
    );
    let rc = crate::display::metal::init_gfxstream(GPU_WIDTH as i32, GPU_HEIGHT as i32, use_gles);
    println!("[gpu3d] stream_renderer_init returned {rc}");
    if rc != 0 {
        eprintln!(
            "[gpu3d] host renderer init FAILED (rc={rc}) — is KosmicKrisp staged? Falling back to \
             2D. The run scripts export VK_DRIVER_FILES=$TP_KOSMICKRISP_ICD (the ICD manifest under \
             third_party/kosmickrisp-install, resolved by third_party/paths.sh) and symlink the \
             Vulkan loader into lib64/vulkan (both are needed: ANDROID_EMU_VK_ICD=kosmickrisp \
             confines gfxstream's loader search to that dir). Build the driver with \
             third_party/bootstrap.sh mesa; it needs macOS 26+ on Apple Silicon and there is no \
             MoltenVK fallback."
        );
        println!("[ok] virtio-gpu 2D scanout {GPU_WIDTH}x{GPU_HEIGHT} → /dev/fb0 (fbcon)");
        return Gpu::new(GPU_WIDTH, GPU_HEIGHT);
    }
    // Safety: `stream_renderer_init` returned 0 above, and every touch of the renderer is serialized
    // by the transport mutex (`Mutex<PciBus>` / `Mutex<Vec<MmioTransport>>`) that guards the device
    // holding it.
    //
    // NOT by being single-threaded, which is what this comment used to claim: `DEFAULT_NUM_CPUS` is 2
    // and *either* vCPU can take the MMIO exit for the GPU's BAR notify, so two threads genuinely do
    // reach `process_controlq`. The `gpu-fence` thread is a third (see `spawn_gpu_fence_thread`).
    // What makes that sound is the mutex, so the invariant to preserve is "the renderer is only
    // touched under the transport lock" — not "only one thread exists".
    let renderer = unsafe { crate::virtio::gpu3d::GfxstreamRenderer::new() };
    println!(
        "[ok] virtio-gpu 3D (VIRGL+CONTEXT_INIT) {GPU_WIDTH}x{GPU_HEIGHT} → gfxstream host renderer, /dev/dri/renderD128"
    );
    Gpu::with_renderer(GPU_WIDTH, GPU_HEIGHT, Box::new(renderer))
}

/// How long the fence thread waits for a completion before running the watchdog sweep anyway.
///
/// It must be a *bounded* wait, not `recv()`: the sweep's whole job is to rescue commands whose host
/// fence never fires, and a renderer that has stopped signalling produces no wakeup to piggyback on.
const FENCE_SWEEP_TICK: Duration = Duration::from_millis(250);

/// Spawn the gfxstream fence-signal thread — the VMM half of the async fence bridge.
///
/// Installs the channel `gpu3d::on_fence_complete` forwards each completed host fence's identity
/// over, then for each batch it drains:
///
///   1. completes every guest command that was waiting on those fences (writes the held response into
///      its descriptor chain and `add_used`s it — see `Gpu::complete_fence`);
///   2. runs the watchdog sweep so a fence that never fires cannot wedge the control ring forever;
///   3. **sets the device ISR bit**, then raises the device's SPI and kicks the target vCPU.
///
/// Step 3's ordering is the item-2.2 fix. This thread previously raised the SPI *without* setting the
/// ISR, and Linux's `vp_interrupt` returns `IRQ_NONE` when the ISR reads zero — so every fence
/// interrupt was delivered to the guest and immediately discarded, making the whole bridge a no-op on
/// the PCI/Android path.
///
/// **Lock order — pci/virtio, then gic, never nested.** The two locks are taken *sequentially*: the
/// transport lock is dropped before `ctx.gic` is acquired. This matches `spawn_input_inject_thread`,
/// `spawn_slirp_poll_thread` and `spawn_console_inject_thread` exactly, which is the property that
/// keeps the four off-vCPU producers deadlock-free against a vCPU thread that holds `ctx.pci` for the
/// duration of an MMIO exit. The other half of the argument lives in `gpu3d::on_fence_complete`,
/// which takes **no** VMM lock at all: gfxstream may invoke it synchronously from inside
/// `stream_renderer_submit_cmd`, i.e. on the vCPU thread with `ctx.pci` already held, so it must never
/// do more than an unbounded-channel send.
///
/// Finds the GPU on whichever transport carries it (PCI on the Android path, MMIO on the Linux
/// `--gpu3d` path). No-op if there is no GPU device.
fn spawn_gpu_fence_thread(ctx: &Arc<VmContext>) {
    let mmio_idx = {
        let mmio = ctx.virtio.lock().unwrap();
        mmio.iter()
            .position(|t| matches!(t.backend, Backend::Gpu(_)))
    };
    let intid = match mmio_idx {
        Some(i) => Some(ctx.virtio.lock().unwrap()[i].intid),
        None => ctx.pci.lock().unwrap().gpu_intid(),
    };
    let Some(intid) = intid else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<crate::virtio::gpu3d::FenceReq>();
    crate::virtio::gpu3d::install_fence_wake(tx);
    let ctx = Arc::clone(ctx);
    std::thread::Builder::new()
        .name("gpu-fence".into())
        .spawn(move || loop {
            // Bounded wait, then coalesce everything else already queued: a burst of completions
            // costs one transport lock and one interrupt rather than N of each.
            let mut batch = Vec::new();
            match rx.recv_timeout(FENCE_SWEEP_TICK) {
                Ok(f) => batch.push(f),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // Sender dropped ⇒ the renderer is gone; nothing will complete again.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
            batch.extend(rx.try_iter());

            // Complete + sweep + set the ISR under the transport lock, then release it before the GIC.
            // The locked section runs on every tick, including idle ones: the sweep is a watchdog, so
            // it must not be conditional on state read under the very lock it needs. Four uncontended
            // acquisitions a second is noise next to a 60 fps guest's MMIO exits.
            let now = Instant::now();
            let published = if let Some(i) = mmio_idx {
                let mut vt = ctx.virtio.lock().unwrap();
                let t = &mut vt[i];
                let p = t
                    .with_gpu(|gpu, queues| gpu.drain_fences(&ctx.mem, queues, &batch, now))
                    .unwrap_or(false);
                if p {
                    t.raise_device_isr();
                }
                p
            } else {
                let mut pci = ctx.pci.lock().unwrap();
                let p = pci
                    .with_gpu(|gpu, queues| gpu.drain_fences(&ctx.mem, queues, &batch, now))
                    .unwrap_or(false);
                if p {
                    pci.raise_device_isr(intid);
                }
                p
            };

            // Poke the guest when we published a used entry, and also on a bare completion that
            // matched nothing pending — the guest's virtio-gpu handler re-scans fence progress on any
            // interrupt, and that belt-and-suspenders wakeup is the behaviour this thread has always
            // had. The ISR is only set when something was actually published, so a spurious poke
            // costs the guest one `IRQ_NONE` and nothing else.
            if published || !batch.is_empty() {
                let target = ctx.gic.lock().unwrap().set_spi(intid);
                ctx.kick(target);
            }
        })
        .ok();
}

/// Spawn the libslirp poll thread + RX bridge. Finds the NAT backend on whichever transport carries
/// the virtio-net device (PCI on the Android path, MMIO on the Linux `--net` path), then pumps
/// `slirp_pollfds_fill/poll` forever; after each tick it flushes any host->guest frames into the
/// guest receiveq and raises the net device's SPI so the guest reclaims/receives them. No-op if
/// there is no NAT-backed NIC.
fn spawn_slirp_poll_thread(ctx: &Arc<VmContext>) {
    let slirp = {
        let pci = ctx.pci.lock().unwrap();
        pci.net_slirp()
    }
    .or_else(|| {
        let vt = ctx.virtio.lock().unwrap();
        vt.iter().find_map(|t| t.net_slirp())
    });
    let Some(slirp) = slirp else {
        return;
    };
    let ctx = Arc::clone(ctx);
    let done = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name("slirp-poll".into())
        .spawn(move || {
            let flush_ctx = Arc::clone(&ctx);
            slirp.run_poll_thread(done, move || {
                // Flush inbound frames into the guest receiveq. Try the PCI bus first (Android), then
                // the MMIO fleet (Linux --net). A completed buffer returns the device INTID to raise.
                let intid = {
                    let r = flush_ctx.pci.lock().unwrap().flush_net_rx(&flush_ctx.mem);
                    if r.is_some() {
                        r
                    } else {
                        let mut vt = flush_ctx.virtio.lock().unwrap();
                        vt.iter_mut()
                            .find_map(|t| t.flush_net_rx(&flush_ctx.mem).then_some(t.intid))
                    }
                };
                if let Some(id) = intid {
                    let target = flush_ctx.gic.lock().unwrap().set_spi(id);
                    flush_ctx.kick(target);
                }
            });
        })
        .ok();
}

/// Spawn a detached thread that receives host input frames from the display window and injects them
/// into the guest's virtio-input device: for each frame it queues the events on the eventq, and if a
/// guest buffer was completed it raises the device SPI through the GIC and kicks the target vCPU so
/// the guest observes the input. No-ops (thread never spawns) if there is no virtio-input device.
fn spawn_input_inject_thread(ctx: &Arc<VmContext>, rx: Receiver<InputFrame>) {
    // The virtio-input device lives on ONE of the two transports: virtio-mmio (Linux path) or the
    // PCI host bridge (Android/Cuttlefish path — its MMIO fleet is empty). Find it on whichever.
    let mmio_idx = {
        let vt = ctx.virtio.lock().unwrap();
        vt.iter().position(|t| t.is_input())
    };
    let pci_input = ctx.pci.lock().unwrap().has_input();
    if mmio_idx.is_none() && !pci_input {
        return; // no input device at all
    }
    let ctx = Arc::clone(ctx);
    std::thread::Builder::new()
        .name("input-inject".into())
        .spawn(move || {
            while let Ok(frame) = rx.recv() {
                let (needs_irq, intid) = if let Some(idx) = mmio_idx {
                    let mut vt = ctx.virtio.lock().unwrap();
                    let t = &mut vt[idx];
                    (t.inject_input_events(&frame, &ctx.mem), t.intid)
                } else {
                    match ctx
                        .pci
                        .lock()
                        .unwrap()
                        .inject_input_events(&frame, &ctx.mem)
                    {
                        Some(intid) => (true, intid),
                        None => (false, 0),
                    }
                };
                if needs_irq {
                    let target = ctx.gic.lock().unwrap().set_spi(intid);
                    ctx.kick(target);
                }
            }
        })
        .ok();
}

/// Spawn a detached thread that feeds host bytes (from a channel, not stdin) into the guest serial
/// console — the same PL011 (`ttyAMA0`) + virtio-console (`hvc0`) path [`spawn_console_input_thread`]
/// uses, but sourced from the control plane's `console_tx` (Phase 6 `launch`). Each delivered chunk
/// raises the relevant device SPI and kicks the target vCPU. Used only in control mode, where stdin
/// is the command REPL rather than the guest console.
fn spawn_console_inject_thread(ctx: &Arc<VmContext>, rx: Receiver<Vec<u8>>) {
    let console_idx = {
        let vt = ctx.virtio.lock().unwrap();
        vt.iter().position(|t| t.is_console())
    };
    let ctx = Arc::clone(ctx);
    std::thread::Builder::new()
        .name("console-inject".into())
        .spawn(move || {
            while let Ok(chunk) = rx.recv() {
                if chunk.is_empty() {
                    continue;
                }
                // PL011 (ttyAMA0) — the primary Android serial console.
                if ctx.pl011.lock().unwrap().push_rx(&chunk) {
                    let target = ctx.gic.lock().unwrap().set_spi(crate::mmio::PL011_INTID);
                    ctx.kick(target);
                }
                // virtio-console (hvc0) — kept fed in parallel.
                if let Some(idx) = console_idx {
                    let (needs_irq, intid) = {
                        let mut vt = ctx.virtio.lock().unwrap();
                        let t = &mut vt[idx];
                        (t.feed_console_input(&chunk, &ctx.mem), t.intid)
                    };
                    if needs_irq {
                        let target = ctx.gic.lock().unwrap().set_spi(intid);
                        ctx.kick(target);
                    }
                }
            }
        })
        .ok();
}

/// The idempotent guest-side bring-up the auto-config thread writes to the root console each tick.
///
/// Replicates the proven manual sequence (see `PHASE6-STATUS.md` § Networking + adb sideload):
/// discover the NIC **by our virtio-net config-space MAC** `52:54:00:ae:00:01` (iterating
/// `/sys/class/net/*/address`, tolerating the `buried_eth0` rename), **rename it to `eth0` so
/// Android's EthernetService will adopt it** (without this, apps have no internet — see the inline
/// note below), bring the link up, set the
/// static slirp lease (`10.0.2.15/24`, gw `10.0.2.2`, DNS `10.0.2.3`), install the **load-bearing**
/// `ip rule … lookup main pref 31000` (without it Android's pref-32000 catch-all `unreachable` rule
/// makes ALL TCP "Network unreachable"), pin the gateway ARP, and expose adbd over TCP 5555.
///
/// Every step is guarded so a re-run is a near-silent no-op once configured — EXCEPT the default
/// route, which is `ip route replace`d unconditionally so it is re-asserted after netd reconciles it
/// away (that reconciliation is exactly why this runs on a timer rather than once). The body carries
/// **no single quotes** so it can be embedded verbatim inside the single-quoted `su 0 -c '…'`
/// wrapper; it is run as root directly when the console is already uid 0, else via `su 0 -c`.
fn net_autoconfig_script() -> String {
    // Guest-shell body (root). Semicolon-joined onto effectively one console line to stay quiet.
    let body = concat!(
        "IF=; ",
        "for f in /sys/class/net/*/address; do ",
        "grep -qi 52:54:00:ae:00:01 $f 2>/dev/null && { IF=${f%/address}; IF=${IF##*/}; break; }; ",
        "done; ",
        "[ -z \"$IF\" ] && IF=buried_eth0; ",
        // **Rename to eth0 — this is what gives APPS internet.** Android's EthernetService
        // (EthernetTracker) only adopts an interface whose name FULLY matches its filter
        // `(usb|eth)\d+`; Cuttlefish hands us `buried_eth0`, which does not match. With no match no
        // NetworkAgent is ever created, so ConnectivityService reports "Active default network:
        // none" and every app is offline — while the kernel routes fine and raw shell sockets reach
        // the internet, which is exactly why this looked like "networking works" from every host-side
        // check. Renaming (the link must be down first) makes EthernetService claim it, run its own
        // IpClient DHCP against our slirp server, and register a VALIDATED ethernet network.
        // Guarded so an already-conforming name is untouched, and `IF` only advances if the rename
        // actually succeeded (e.g. a pre-existing `eth0` would make it fail).
        "case $IF in ",
        "eth[0-9]|usb[0-9]) ;; ",
        "*) ip link set $IF down 2>/dev/null; ip link set $IF name eth0 2>/dev/null && IF=eth0;; ",
        "esac; ",
        // Link + address (idempotent: `set up` is a no-op if already up; addr guarded).
        "ip link set $IF up 2>/dev/null; ",
        "ip addr show dev $IF 2>/dev/null | grep -q 10.0.2.15/24 || ip addr replace 10.0.2.15/24 dev $IF; ",
        // The load-bearing policy rule — guarded so we never stack duplicates, re-added if netd dropped it.
        "ip rule show 2>/dev/null | grep -q \"from all lookup main\" || ip rule add from all lookup main pref 31000; ",
        // Default route + gateway ARP: re-asserted every tick (replace is idempotent) to beat netd.
        "ip route replace default via 10.0.2.2 dev $IF onlink 2>/dev/null; ",
        "ip neigh replace 10.0.2.2 lladdr 52:55:0a:00:02:02 dev $IF nud permanent 2>/dev/null; ",
        // DNS (guarded so setprop does not fire every tick).
        "[ \"$(getprop net.dns1)\" = 10.0.2.3 ] || setprop net.dns1 10.0.2.3; ",
        // adb over TCP: set the port + (re)start adbd only when the port is unset or adbd is down.
        "if [ \"$(getprop service.adb.tcp.port)\" != 5555 ] || [ \"$(getprop init.svc.adbd)\" != running ]; then ",
        "setprop service.adb.tcp.port 5555; stop adbd; start adbd; ",
        "fi",
    );
    // Wrapper: escalate only when the console shell is not already root. This cf64 image's serial
    // console runs as uid `shell` (2000), so the `su` branch is the live path. Android's toybox `su`
    // is `su <uid> <cmd> [args…]` and does NOT accept `-c` (`su 0 -c "…"` dies with
    // "su: failed to exec -c: No such file or directory"); the correct escalation is `su 0 sh -c "…"`,
    // which runs a root `sh -c <body>`. `N` holds the body literally (single-quoted); `"$N"` passes it
    // through without re-expansion, so `$IF` / `$(getprop …)` are expanded by the *executing* (root)
    // shell, not the outer one.
    format!("N='{body}'; if [ \"$(id -u)\" = 0 ]; then eval \"$N\"; else su 0 sh -c \"$N\"; fi\n")
}

/// Spawn the network auto-config thread (control mode + `--net`). It writes [`net_autoconfig_script`]
/// to the guest console on a timer: aggressively at first (every ~5 s, to catch the shell / `ip` /
/// `adbd` as soon as they exist during boot — the script itself tolerates "command not found" while
/// they don't), then a slow idempotent keepalive (every ~12 s) that re-asserts the policy route/rule
/// indefinitely so netd can't reconcile connectivity away. Exits when the console channel is dropped
/// (control server / VM gone). Quiet by construction: one console line per tick, each step guarded.
fn spawn_net_autoconfig_thread(tx: Sender<Vec<u8>>) {
    std::thread::Builder::new()
        .name("net-autoconf".into())
        .spawn(move || {
            let script = net_autoconfig_script();
            // Give the guest console shell a chance to come up before the first write (harmless if
            // it hasn't — the bytes buffer in the PL011 RX FIFO and flush when the shell reads).
            std::thread::sleep(Duration::from_secs(20));
            let mut ticks: u32 = 0;
            loop {
                if tx.send(script.clone().into_bytes()).is_err() {
                    break; // console channel gone — nothing left to drive.
                }
                ticks += 1;
                // ~2 min of 5 s ticks to catch a slow SWRENDER boot, then a 12 s keepalive.
                let delay = if ticks < 24 { 5 } else { 12 };
                std::thread::sleep(Duration::from_secs(delay));
            }
        })
        .ok();
}

/// Spawn a detached thread that reads host stdin and feeds it to BOTH the PL011 (`ttyAMA0`, the
/// interactive shell console) and the virtio-console receiveq, making the guest shell interactive.
/// Each delivered chunk raises the relevant device's SPI through the GIC and kicks the target vCPU
/// so it observes the input; bytes typed before a device's RX ring is ready are buffered and
/// flushed as soon as it is.
///
/// Note: the host terminal is left in cooked mode (no raw-tty setup this phase), so interactive use
/// is line-buffered and the host tty echoes; piping commands into stdin works directly.
fn spawn_console_input_thread(ctx: &Arc<VmContext>) {
    let console_idx = {
        let vt = ctx.virtio.lock().unwrap();
        vt.iter().position(|t| t.is_console())
    };
    let ctx = Arc::clone(ctx);
    std::thread::Builder::new()
        .name("console-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 256];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break, // EOF: no more host input (VM keeps running).
                    Ok(n) => {
                        let chunk = &buf[..n];

                        // PL011 (ttyAMA0) — the shell console.
                        if ctx.pl011.lock().unwrap().push_rx(chunk) {
                            let target = ctx.gic.lock().unwrap().set_spi(crate::mmio::PL011_INTID);
                            ctx.kick(target);
                        }

                        // virtio-console (hvc0) — kept fed in parallel.
                        if let Some(idx) = console_idx {
                            let (needs_irq, intid) = {
                                let mut vt = ctx.virtio.lock().unwrap();
                                let t = &mut vt[idx];
                                (t.feed_console_input(chunk, &ctx.mem), t.intid)
                            };
                            if needs_irq {
                                let target = ctx.gic.lock().unwrap().set_spi(intid);
                                ctx.kick(target);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The bring-up is assembled as a string and shipped to the guest over a serial console, so a
    /// syntax error is invisible here and silent there — the console just prints an error into a log
    /// nobody reads and the VM comes up with no networking. Parse it with a real shell.
    #[test]
    fn net_autoconfig_script_is_valid_shell() {
        let script = net_autoconfig_script();
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "generated bring-up script is not valid shell:\n{}\n--- script ---\n{script}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Two load-bearing properties of the guest-side bring-up, both of which cost a debugging session
    /// when absent:
    ///
    /// * the **rename to `eth0`** — Android's EthernetService only adopts interfaces matching
    ///   `(usb|eth)\d+`, and Cuttlefish presents ours as `buried_eth0`. Without the rename the kernel
    ///   routes fine and raw sockets reach the internet, but no NetworkAgent is created, so
    ///   ConnectivityService has no default network and every APP is offline.
    /// * **no single quotes** in the body — it is embedded inside a single-quoted `N='…'` wrapper, so
    ///   one quote silently truncates the script mid-command.
    #[test]
    fn net_autoconfig_script_renames_nic_and_stays_quote_free() {
        let script = net_autoconfig_script();
        assert!(
            script.contains("name eth0"),
            "bring-up must rename the NIC to eth0 or Android gives apps no internet"
        );
        assert!(
            script.contains("lookup main"),
            "bring-up must install the policy route or all TCP is unreachable"
        );
        let body = script
            .strip_prefix("N='")
            .and_then(|s| s.split_once("'; if "))
            .map(|(body, _)| body)
            .expect("script keeps the N='<body>'; if … wrapper shape");
        assert!(
            !body.contains('\''),
            "the body is embedded in a single-quoted wrapper; a quote truncates it: {body}"
        );
    }
}
