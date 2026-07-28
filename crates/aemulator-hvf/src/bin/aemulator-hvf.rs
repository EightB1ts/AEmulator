//! `aemulator-hvf` — the VMM CLI. Phase 1 subcommand: `run-linux` boots an aarch64 Linux kernel
//! under Apple's Hypervisor.framework and streams its earlycon output.
//!
//! Usage:
//!   aemulator-hvf run-linux --kernel <Image> [--mem-mb N] [--max-iters N] [--cmdline "..."]
//!
//! Requires the `com.apple.security.hypervisor` entitlement — ad-hoc sign the built binary with
//! hv.entitlements before running (see the crate README / run-probe.sh).

fn main() {
    #[cfg(target_os = "macos")]
    {
        std::process::exit(macos::main());
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "aemulator-hvf: requires macOS + Apple Hypervisor.framework (this host is not macOS)."
        );
        std::process::exit(2);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use aemulator_hvf::control::{self, ControlOpts};
    use aemulator_hvf::display;
    use aemulator_hvf::machine::{
        run_linux, run_linux_display, Config, DisplayBridge, DEFAULT_CMDLINE,
        DEFAULT_DISPLAY_CMDLINE, DEFAULT_MAX_ITERS, DEFAULT_NUM_CPUS, DEFAULT_RAM_BYTES,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        if args.len() < 2 {
            usage(&args[0]);
            return 2;
        }
        match args[1].as_str() {
            "run-linux" => run_linux_cmd(&args[2..]),
            "run-android" => android::run_android_cmd(&args[2..]),
            // Phase 6: boot Android and serve the host-driven control plane (framebuffer capture +
            // virtio-input injection) over a Unix socket / stdin REPL. Equivalent to
            // `run-android --control`.
            "control" => {
                let mut a = args[2..].to_vec();
                a.push("--control".to_string());
                android::run_android_cmd(&a)
            }
            // Phase 6 one-shot control CLIENTS: connect to a running control server's socket, send
            // one command, print the response. This is the exact seam a thin MCP server wraps.
            "tap" | "swipe" | "text" | "key" | "shot" | "screenshot" | "wait" | "wait_stable"
            | "launch" | "sh" | "info" | "hash" => client_cmd(&args[1..]),
            "-h" | "--help" | "help" => {
                usage(&args[0]);
                0
            }
            other => {
                eprintln!("unknown subcommand: {other}");
                usage(&args[0]);
                2
            }
        }
    }

    fn usage(prog: &str) {
        eprintln!(
            "usage: {prog} run-linux  --kernel <Image> [--initrd <cpio.gz>] [--disk <file>]... [--net] [--mem-mb N] [--cpus N] [--max-iters N] [--cmdline \"...\"] [--display] [--gpu3d]\n       {prog} run-android [--cf-dir <dir>] [--mem-mb N] [--cpus N] [--max-iters N] [--display] [--gpu3d] [--selinux permissive|enforcing] [--extra-cmdline \"...\"]\n       {prog} control    [<run-android flags>] [--socket <path>] [--script <file>] [--repl] [--exit-after-script]   (boot + serve the control plane)\n       {prog} <tap X Y|swipe X1 Y1 X2 Y2 [ms]|text <s>|key <code>|screenshot [path]|wait_stable [ms]|launch <pkg>|info|hash> [--socket <path>]   (one-shot client to a running control server)"
        );
    }

    /// One-shot control client: `aemulator-hvf tap 500 400 [--socket <path>]` etc. Extracts an
    /// optional `--socket`, reconstructs the command line, sends it to the running control server,
    /// prints the single-line response, and maps `ERR …` to a non-zero exit.
    fn client_cmd(args: &[String]) -> i32 {
        let mut socket: Option<PathBuf> = None;
        let mut parts: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--socket" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => socket = Some(PathBuf::from(v)),
                        None => {
                            eprintln!("--socket requires a path");
                            return 2;
                        }
                    }
                }
                other => parts.push(other.to_string()),
            }
            i += 1;
        }
        let sock = socket.unwrap_or_else(|| PathBuf::from(control::DEFAULT_SOCKET));
        let command = parts.join(" ");
        match control::client_send(Path::new(&sock), &command) {
            Ok(resp) => {
                println!("{resp}");
                if resp.starts_with("ERR") {
                    1
                } else {
                    0
                }
            }
            Err(e) => {
                eprintln!(
                    "[client] connect {}: {e} — is `aemulator-hvf control` running?",
                    sock.display()
                );
                1
            }
        }
    }

    /// Control orchestration (parallel to [`run_windowed_bc`]): the VMM runs on a worker thread and
    /// hands a [`DisplayBridge`] back; THIS (main) thread runs the control server. No window is opened
    /// — screenshots come from the guest scanout directly (works headless/remote). Sets
    /// `AEMU_CONTROL_NO_CONSOLE_STDIN` so stdin is the command REPL and the guest console is driven
    /// over the bridge's `console_tx` instead.
    #[allow(clippy::too_many_arguments)]
    pub fn run_control_bc(
        kernel: PathBuf,
        initrd: Option<PathBuf>,
        disks: Vec<PathBuf>,
        composite: Option<Vec<aemulator_hvf::virtio::block_composite::PartitionSpec>>,
        bootconfig: Option<Vec<u8>>,
        ram_bytes: usize,
        max_iters: u64,
        num_cpus: u64,
        cmdline: String,
        gpu3d: bool,
        netdev: bool,
        dt_identity: Option<(Vec<String>, String)>,
        opts: ControlOpts,
    ) -> i32 {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc::sync_channel;
        use std::sync::Arc;

        // Free stdin from the guest console so it can be the control REPL (read on the worker thread
        // during boot — must be set before the worker spawns).
        std::env::set_var("AEMU_CONTROL_NO_CONSOLE_STDIN", "1");

        let (bridge_tx, bridge_rx) = sync_channel::<DisplayBridge>(1);
        let vmm_done = Arc::new(AtomicBool::new(false));
        let vmm_done_worker = vmm_done.clone();

        let worker = std::thread::Builder::new()
            .name("vmm".into())
            .spawn(move || {
                let cfg = Config {
                    kernel_path: &kernel,
                    ram_bytes,
                    cmdline: &cmdline,
                    max_iters,
                    num_cpus,
                    initrd_path: initrd.as_deref(),
                    disk_paths: &disks,
                    composite_partitions: composite.as_deref(),
                    bootconfig: bootconfig.as_deref(),
                    gpu3d,
                    netdev,
                    dt_identity,
                };
                match run_linux_display(&cfg, bridge_tx, vmm_done_worker) {
                    Ok(s) => println!("cpu0 stop reason: {s:?}"),
                    Err(e) => eprintln!("[FAIL] {e}"),
                }
            })
            .expect("spawn vmm thread");

        match bridge_rx.recv() {
            Ok(bridge) => control::run_control(bridge, opts),
            Err(_) => eprintln!("[FAIL] VMM stopped before the control bridge was ready"),
        }

        if vmm_done.load(std::sync::atomic::Ordering::Acquire) {
            let _ = worker.join();
        }
        0
    }

    fn run_linux_cmd(args: &[String]) -> i32 {
        let mut kernel: Option<PathBuf> = None;
        let mut initrd: Option<PathBuf> = None;
        let mut disks: Vec<PathBuf> = Vec::new();
        let mut ram_bytes = DEFAULT_RAM_BYTES;
        let mut max_iters = DEFAULT_MAX_ITERS;
        let mut num_cpus = DEFAULT_NUM_CPUS;
        // `None` = user did not pass --cmdline, so the default is chosen by mode (display vs headless).
        let mut cmdline: Option<String> = None;
        let mut display = false;
        // Phase 3d: attach the gfxstream 3D renderer to virtio-gpu (advertise VIRGL). Only effective
        // on the `--features gfxstream` build; a no-op flag on the default build (warns).
        let mut gpu3d = false;
        // Phase 4: attach a virtio-net NIC (enumerate-only stub for now).
        let mut netdev = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--kernel" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => kernel = Some(PathBuf::from(v)),
                        None => {
                            eprintln!("--kernel requires a path");
                            return 2;
                        }
                    }
                }
                "--initrd" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => initrd = Some(PathBuf::from(v)),
                        None => {
                            eprintln!("--initrd requires a path");
                            return 2;
                        }
                    }
                }
                "--disk" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => disks.push(PathBuf::from(v)),
                        None => {
                            eprintln!("--disk requires a path");
                            return 2;
                        }
                    }
                }
                "--mem-mb" => {
                    i += 1;
                    match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                        Some(mb) => ram_bytes = mb * 1024 * 1024,
                        None => {
                            eprintln!("--mem-mb requires a positive integer");
                            return 2;
                        }
                    }
                }
                "--max-iters" => {
                    i += 1;
                    match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                        Some(n) => max_iters = n,
                        None => {
                            eprintln!("--max-iters requires a positive integer");
                            return 2;
                        }
                    }
                }
                "--cpus" => {
                    i += 1;
                    match args.get(i).and_then(|v| v.parse::<u64>().ok()) {
                        Some(n) if n >= 1 => num_cpus = n,
                        _ => {
                            eprintln!("--cpus requires a positive integer");
                            return 2;
                        }
                    }
                }
                "--cmdline" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => cmdline = Some(v.clone()),
                        None => {
                            eprintln!("--cmdline requires a string");
                            return 2;
                        }
                    }
                }
                "--display" => display = true,
                "--gpu3d" => gpu3d = true,
                "--net" => netdev = true,
                other => {
                    eprintln!("unknown flag: {other}");
                    return 2;
                }
            }
            i += 1;
        }

        let kernel = match kernel {
            Some(k) => k,
            None => {
                eprintln!("--kernel is required");
                return 2;
            }
        };

        // Resolve the cmdline default by mode: display mode routes /dev/console to the framebuffer
        // (tty0) so the shell renders in the window; headless keeps the exact Phase-1 serial cmdline.
        let cmdline = cmdline.unwrap_or_else(|| {
            if display {
                DEFAULT_DISPLAY_CMDLINE.to_string()
            } else {
                DEFAULT_CMDLINE.to_string()
            }
        });

        if display {
            return run_windowed(
                kernel, initrd, disks, ram_bytes, max_iters, num_cpus, cmdline, gpu3d, netdev,
            );
        }

        let cfg = Config {
            kernel_path: &kernel,
            ram_bytes,
            cmdline: &cmdline,
            max_iters,
            num_cpus,
            initrd_path: initrd.as_deref(),
            disk_paths: &disks,
            composite_partitions: None,
            bootconfig: None,
            gpu3d,
            netdev,
            dt_identity: None,
        };
        match run_linux(&cfg) {
            Ok(_) => 0,
            Err(e) => {
                eprintln!("[FAIL] {e}");
                1
            }
        }
    }

    /// `--display` orchestration. macOS requires the winit event loop on the **main** thread, so the
    /// VMM runs on a spawned worker thread and hands a [`DisplayBridge`] back over a channel; this
    /// (main) thread then owns the window event loop until the window closes or the guest stops.
    #[allow(clippy::too_many_arguments)]
    fn run_windowed(
        kernel: PathBuf,
        initrd: Option<PathBuf>,
        disks: Vec<PathBuf>,
        ram_bytes: usize,
        max_iters: u64,
        num_cpus: u64,
        cmdline: String,
        gpu3d: bool,
        netdev: bool,
    ) -> i32 {
        run_windowed_bc(
            kernel, initrd, disks, None, None, ram_bytes, max_iters, num_cpus, cmdline, gpu3d,
            netdev, None,
        )
    }

    /// Windowed run with an optional Android bootconfig blob (shared by `run-linux --display` and
    /// `run-android --display`). macOS requires the winit event loop on the **main** thread, so the
    /// VMM runs on a spawned worker thread and hands a [`DisplayBridge`] back over a channel.
    #[allow(clippy::too_many_arguments)]
    pub fn run_windowed_bc(
        kernel: PathBuf,
        initrd: Option<PathBuf>,
        disks: Vec<PathBuf>,
        composite: Option<Vec<aemulator_hvf::virtio::block_composite::PartitionSpec>>,
        bootconfig: Option<Vec<u8>>,
        ram_bytes: usize,
        max_iters: u64,
        num_cpus: u64,
        cmdline: String,
        gpu3d: bool,
        netdev: bool,
        dt_identity: Option<(Vec<String>, String)>,
    ) -> i32 {
        let (bridge_tx, bridge_rx) = sync_channel::<DisplayBridge>(1);
        let vmm_done = Arc::new(AtomicBool::new(false));
        let vmm_done_worker = vmm_done.clone();

        let worker = std::thread::Builder::new()
            .name("vmm".into())
            .spawn(move || {
                let cfg = Config {
                    kernel_path: &kernel,
                    ram_bytes,
                    cmdline: &cmdline,
                    max_iters,
                    num_cpus,
                    initrd_path: initrd.as_deref(),
                    disk_paths: &disks,
                    composite_partitions: composite.as_deref(),
                    bootconfig: bootconfig.as_deref(),
                    gpu3d,
                    netdev,
                    dt_identity,
                };
                match run_linux_display(&cfg, bridge_tx, vmm_done_worker) {
                    Ok(s) => println!("cpu0 stop reason: {s:?}"),
                    Err(e) => eprintln!("[FAIL] {e}"),
                }
            })
            .expect("spawn vmm thread");

        // Wait for the VMM to publish the scanout + input channel, then run the window event loop.
        match bridge_rx.recv() {
            Ok(bridge) => display::run_event_loop(bridge),
            Err(_) => eprintln!("[FAIL] VMM stopped before the display bridge was ready"),
        }

        // The window closed (or the guest stopped). The VMM keeps running until the guest halts; the
        // process exits when this returns, tearing it down. Join briefly if it's already finishing.
        if vmm_done.load(std::sync::atomic::Ordering::Acquire) {
            let _ = worker.join();
        }
        0
    }

    /// `run-android` — boot the Cuttlefish (aosp_cf_arm64) images under HVF and stream the guest
    /// console so we can diagnose exactly how far Android gets. See PHASE4-STATUS.md.
    pub mod android {
        use aemulator_hvf::machine::{run_linux, Config};
        use aemulator_hvf::virtio::block_composite::PartitionSpec;
        use std::path::{Path, PathBuf};

        /// Default guest DRAM for Android (MiB). Android's zygote/system_server want real memory;
        /// 4 GiB is the smallest that reliably clears first/second-stage init on Cuttlefish.
        const DEFAULT_ANDROID_MEM_MB: usize = 4096;
        /// Cuttlefish is happiest with 2 vCPUs (exercises PSCI SMP bring-up through our GIC).
        const DEFAULT_ANDROID_CPUS: u64 = 2;
        /// A generous iteration budget — an Android boot issues orders of magnitude more MMIO exits
        /// than an idle Linux shell. Wall-clock is bounded externally (`timeout`), not by this.
        const DEFAULT_ANDROID_MAX_ITERS: u64 = 40_000_000_000;

        /// The Cuttlefish partitions, assembled into a SINGLE GPT composite virtio-blk disk
        /// (`/dev/vda`) whose partition entries carry these by-name labels — so Android first-stage
        /// init's `BlockDevInitializer` resolves `/dev/block/by-name/<label>`. Labels are already
        /// slot-suffixed (`_a`) for the A/B-slotted partitions (boot/init_boot/vbmeta*/vendor_boot);
        /// `super`, `metadata`, `userdata`, `misc` are not slotted. `writable` routes guest writes to
        /// the backing file (userdata/misc/metadata). `create_empty` fabricates a zeroed 16 MiB image
        /// if absent. Order = GPT entry order.
        const PARTS: &[Part] = &[
            // (filename under <cf>/img, GPT by-name label, writable, create-empty-if-missing)
            Part("super.raw.img", "super", false, false),
            Part("vbmeta.img", "vbmeta_a", false, false),
            Part("vbmeta_system.img", "vbmeta_system_a", false, false),
            Part(
                "vbmeta_vendor_dlkm.img",
                "vbmeta_vendor_dlkm_a",
                false,
                false,
            ),
            // The 64-bit-only android16 image (aosp_cf_arm64_only_phone) adds a `vbmeta_system_dlkm`
            // chained vbmeta; its first-stage `BlockDevInitializer` blocks 10s then fatal-reboots when
            // `/dev/block/by-name/vbmeta_system_dlkm_a` is absent. Skip-if-missing keeps the older
            // dual-ABI aosp_cf_arm64_phone image (no such file) unaffected.
            Part(
                "vbmeta_system_dlkm.img",
                "vbmeta_system_dlkm_a",
                false,
                false,
            ),
            Part("boot.img", "boot_a", false, false),
            Part("init_boot.img", "init_boot_a", false, false),
            Part("vendor_boot.img", "vendor_boot_a", false, false),
            Part("userdata.img", "userdata", true, false),
            Part("misc.img", "misc", true, true),
            Part("metadata.img", "metadata", true, true),
            // `frp` (Factory Reset Protection / persistent data block). system_server's
            // PersistentDataBlockService reads `ro.frp.pst=/dev/block/by-name/frp` on a background
            // init thread; if that device is absent the read throws ("persistent partition not
            // readable"), the init-done signal never fires, and onBootPhase(500) times out →
            // `FATAL EXCEPTION IN SYSTEM PROCESS: PersistentDataBlockService init timeout` →
            // system_server crash-loops and boot never reaches boot_completed. Cuttlefish's
            // assemble_cvd normally creates this partition; the raw image set ships no frp.img, so we
            // fabricate an empty writable one — PDB formats the uninitialized block itself on first
            // boot (deactivateFrp), signals init-done, and boot proceeds past phase 500 to the launcher.
            Part("frp.img", "frp", true, true),
        ];

        /// (filename, GPT by-name label, writable, create-empty-if-missing).
        struct Part(&'static str, &'static str, bool, bool);

        pub fn run_android_cmd(args: &[String]) -> i32 {
            let mut cf_dir: Option<PathBuf> = None;
            let mut mem_mb = DEFAULT_ANDROID_MEM_MB;
            let mut cpus = DEFAULT_ANDROID_CPUS;
            let mut max_iters = DEFAULT_ANDROID_MAX_ITERS;
            let mut display = false;
            let mut gpu3d = false;
            let mut selinux = String::from("permissive");
            let mut extra_cmdline = String::new();
            // Opt-in: drop first-stage init into an interactive debug shell BEFORE it mounts the
            // fstab. Off by default so boot proceeds to first_stage_mount (the real diagnosis point).
            let mut first_stage_console = false;
            // `ro.boot.fstab_suffix`: first-stage init's `ReadDefaultFstab` looks for a file
            // `fstab.<suffix>` on the ramdisk. Cuttlefish ships `fstab.cf.{ext4,f2fs}.{cts,hctr2}`
            // (crosvm normally selects one and feeds it via a DT node we don't emit); pointing the
            // suffix straight at the ext4/cts file lets init load the fstab from the ramdisk instead.
            let mut fstab_suffix = String::from("cf.ext4.cts");
            // Phase 6 control-plane flags (only meaningful with `--control` / the `control` subcommand).
            let mut control = false;
            let mut socket: Option<PathBuf> = None;
            let mut script: Option<PathBuf> = None;
            let mut repl = false;
            let mut exit_after_script = false;

            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--cf-dir" => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => cf_dir = Some(PathBuf::from(v)),
                            None => return err("--cf-dir requires a path"),
                        }
                    }
                    "--mem-mb" => {
                        i += 1;
                        match args.get(i).and_then(|v| v.parse().ok()) {
                            Some(v) => mem_mb = v,
                            None => return err("--mem-mb requires a positive integer"),
                        }
                    }
                    "--cpus" => {
                        i += 1;
                        match args.get(i).and_then(|v| v.parse().ok()) {
                            Some(v) if v >= 1 => cpus = v,
                            _ => return err("--cpus requires a positive integer"),
                        }
                    }
                    "--max-iters" => {
                        i += 1;
                        match args.get(i).and_then(|v| v.parse().ok()) {
                            Some(v) => max_iters = v,
                            None => return err("--max-iters requires a positive integer"),
                        }
                    }
                    "--selinux" => {
                        i += 1;
                        match args.get(i).map(|s| s.as_str()) {
                            Some(v @ ("permissive" | "enforcing")) => selinux = v.to_string(),
                            _ => return err("--selinux requires permissive|enforcing"),
                        }
                    }
                    "--extra-cmdline" => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => extra_cmdline = v.clone(),
                            None => return err("--extra-cmdline requires a string"),
                        }
                    }
                    "--fstab-suffix" => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => fstab_suffix = v.clone(),
                            None => return err("--fstab-suffix requires a string"),
                        }
                    }
                    "--display" => display = true,
                    "--gpu3d" => gpu3d = true,
                    "--first-stage-console" => first_stage_console = true,
                    "--control" => control = true,
                    "--socket" => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => socket = Some(PathBuf::from(v)),
                            None => return err("--socket requires a path"),
                        }
                    }
                    "--script" => {
                        i += 1;
                        match args.get(i) {
                            Some(v) => script = Some(PathBuf::from(v)),
                            None => return err("--script requires a path"),
                        }
                    }
                    "--repl" => repl = true,
                    "--exit-after-script" => exit_after_script = true,
                    other => return err(&format!("unknown flag: {other}")),
                }
                i += 1;
            }

            // Resolve <cf> dir: --cf-dir, else <crate>/testdata/cf.
            let cf = cf_dir.unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("testdata")
                    .join("cf")
            });
            let img = cf.join("img");
            let kernel = cf.join("kernel");
            let initrd = cf.join("initramfs.cf.gz");

            for (p, what) in [(&kernel, "kernel"), (&initrd, "initramfs.cf.gz")] {
                if !p.exists() {
                    return err(&format!(
                        "missing {what} at {p:?} — run the Cuttlefish acquire step (see testdata/cf/SOURCE.txt)"
                    ));
                }
            }

            // Claim the cf-dir BEFORE touching any partition. `img/` is mutated in place, so a
            // second VMM here means two kernels writing one ext4 device — which reliably ends in
            // "EXT4-fs error … checksum invalid" + kernel panic a minute into boot. Held for the
            // process lifetime; the kernel releases it even on SIGKILL. See `cf_lock`.
            if let Err(e) = aemulator_hvf::cf_lock::hold_for_process_lifetime(&cf) {
                return err(&e);
            }

            // Assemble the composite-disk partition list (fabricating empty misc/metadata when
            // absent). All partitions land in ONE GPT composite virtio-blk disk (/dev/vda).
            let mut parts: Vec<PartitionSpec> = Vec::new();
            for Part(fname, label, writable, create_empty) in PARTS {
                let mut path = img.join(fname);
                if *create_empty {
                    // Fabricated scratch partitions. `metadata` is formatted **f2fs** by fs_mgr on
                    // first boot, and `make_f2fs` refuses a volume < ~50 MiB ("Device size is not
                    // sufficient for F2FS volume"); a too-small /metadata never mounts RW, which
                    // breaks vold's metadata-encryption key store and cascades into the /data mount,
                    // keystore2's DB, and apexd. So metadata gets 128 MiB; misc stays small.
                    let target: u64 = if *label == "metadata" {
                        128 * 1024 * 1024
                    } else {
                        16 * 1024 * 1024
                    };
                    match ensure_size(&path, target) {
                        Ok(true) => println!(
                            "[android] fabricated/grew {fname} → {} MiB",
                            target / (1024 * 1024)
                        ),
                        Ok(false) => {}
                        Err(e) => return err(&format!("size {path:?}: {e}")),
                    }
                } else if !path.exists() {
                    println!("[android][warn] partition {fname} not present — skipping");
                    continue;
                } else if *label == "userdata" {
                    // The shipped `userdata.img` is a ~2 MB placeholder with no valid superblock;
                    // vold's `cryptfs mountFstab` can't format a real (metadata-encrypted) /data into
                    // it, so /data mounts read-only and keystore2's DB + `vdc cryptfs init_user0` +
                    // apexd's decompression all fail. Grow it to a usable size so /data can format.
                    match ensure_size(&path, 6 * 1024 * 1024 * 1024) {
                        Ok(true) => {
                            println!("[android] grew userdata → 6144 MiB (formattable /data)")
                        }
                        Ok(false) => {}
                        Err(e) => return err(&format!("size {path:?}: {e}")),
                    }
                }
                // AVB bringup: patch the TOP-LEVEL vbmeta with the VERIFICATION_DISABLED flag so
                // first-stage fs_mgr skips both signature verification (we lack the Cuttlefish signing
                // key + the expected `androidboot.vbmeta.digest`) and dm-verity hashtree setup, and
                // mounts the logical partitions (system/vendor/product) directly. Written to a copy —
                // the original image is untouched.
                if *label == "vbmeta_a" {
                    match patch_vbmeta_verification_disabled(&path, &img) {
                        Ok(p) => {
                            println!("[android] vbmeta_a: VERIFICATION_DISABLED (flag=2) → {p:?}");
                            path = p;
                        }
                        Err(e) => {
                            println!("[android][warn] vbmeta patch failed ({e}); using original")
                        }
                    }
                }
                parts.push(PartitionSpec {
                    label: (*label).to_string(),
                    path,
                    writable: *writable,
                });
            }

            // boot_devices = the composite disk's virtio-PCI address. The fleet is built (in
            // machine.rs) as console(fleet 0 → 00:01.0), rng(1 → 00:02.0), composite-block
            // (2 → 00:03.0), … on the `pci-host-cam-generic` bridge whose CAM base is
            // `pci::PCI_CAM_BASE` (0x3000_0000 → sysfs `30000000.pci`). Android's fs_mgr /
            // BlockDevInitializer resolve `/dev/block/by-name/<label>` for partitions whose DEVPATH
            // contains one of `androidboot.boot_devices`.
            // Android's ueventd creates `/dev/block/by-name/<label>` only for partitions whose
            // resolved *platform-device* string equals one of `boot_devices`. For a generic PCI
            // host bridge the PCI BDFs are NOT platform devices — only the host bridge is — so
            // `FindPlatformDevice` resolves the vda partitions to `<cam_base>.pci` (the bridge),
            // with `/devices/platform/` stripped. That exact string is what must be in boot_devices.
            let boot_devices = format!("{:x}.pci", aemulator_hvf::virtio::pci::PCI_CAM_BASE);

            // --- Phase 5 (stealth / de-brand): the DeviceProfile identity we emit from the VMM. ---
            // The `aemulator-identity` reference profile is Xiaomi 13 `fuxi` / Snapdragon SM8550.
            // We own three identity bits directly (no partition edit needed): the DT root
            // compatible/model, `androidboot.serialno`, and `androidboot.verifiedbootstate`.
            let profile = aemulator_identity::profiles::reference_profile();
            let dt_compatible = aemulator_dtb::root_compatible(&profile); // ["xiaomi,fuxi","qcom,kalama"]
            let dt_model = aemulator_dtb::machine_name(&profile); // "Qualcomm Technologies, Inc SM8550"
            let profile_bc = profile.emit_bootconfig();
            let serialno = profile_bc
                .iter()
                .find(|(k, _)| k == "androidboot.serialno")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "AEMULATORHVF01".to_string());
            // verifiedbootstate: STAYS `orange` (unlocked). This is a STRUCTURAL constraint, not a
            // choice — the image's top-level vbmeta is patched VERIFICATION_DISABLED (we lack the
            // Cuttlefish AVB signing key), so first-stage `fs_mgr` only tolerates the resulting
            // ERROR_VERIFICATION / PUBLIC_KEY_REJECTED when the device reports UNLOCKED
            // (`ro.boot.flash.locked=0`). Forcing `verifiedbootstate=green` + `flash.locked=1` flips
            // libfs_avb into strict enforcement → "PUBLIC_KEY_REJECTED isn't allowed" → `/system`
            // fails to mount → `InitFatalReboot: signal 6` (empirically confirmed). Flipping to green
            // therefore requires re-signing the image under our own AVB key + embedding the pubkey —
            // the deep verified-boot pass. `veritymode` stays `disabled` for the same reason.
            // `androidboot.hardware=cutf_cvm` is likewise KEPT: load-bearing for vendor-HAL
            // resolution (init.<hw>.rc / fstab.<hw> / <mod>.<hw>.so); renaming it is the deep pass.
            let vb_state = "orange";

            // Base bootconfig (the vendor bootconfig.txt Cuttlefish ships) + the launch-time
            // androidboot.* Cuttlefish's host tool would normally inject. Appended to the initrd.
            let vendor_bc = std::fs::read(cf.join("bootconfig.txt")).unwrap_or_default();
            let mut bootconfig = String::from_utf8_lossy(&vendor_bc).into_owned();
            if !bootconfig.is_empty() && !bootconfig.ends_with('\n') {
                bootconfig.push('\n');
            }
            let fsc = if first_stage_console { 1 } else { 0 };
            bootconfig.push_str(&format!(
                "androidboot.hardware=cutf_cvm\n\
                 androidboot.serialno={serialno}\n\
                 androidboot.boot_devices={boot_devices}\n\
                 androidboot.slot_suffix=_a\n\
                 androidboot.force_normal_boot=1\n\
                 androidboot.verifiedbootstate={vb_state}\n\
                 androidboot.veritymode=disabled\n\
                 androidboot.selinux={selinux}\n\
                 androidboot.first_stage_console={fsc}\n\
                 androidboot.fstab_suffix={fstab_suffix}\n"
            ));

            // Kernel command line. androidboot.* are ALSO placed here (init reads both /proc/cmdline
            // and /proc/bootconfig) as belt-and-suspenders; `bootconfig` tells the kernel to scan the
            // initrd tail for the appended blob. Serial console on ttyAMA0 so we capture the boot log.
            let mut cmdline = format!(
                "earlycon=pl011,0x9000000 console=ttyAMA0 nokaslr audit=0 loglevel=3 \
                 printk.devkmsg=on loop.max_part=7 bootconfig init=/init \
                 androidboot.hardware=cutf_cvm androidboot.console=ttyAMA0 \
                 androidboot.boot_devices={boot_devices} androidboot.selinux={selinux} \
                 androidboot.serialno={serialno} androidboot.force_normal_boot=1 \
                 androidboot.slot_suffix=_a androidboot.verifiedbootstate={vb_state} \
                 androidboot.veritymode=disabled androidboot.first_stage_console={fsc} \
                 androidboot.fstab_suffix={fstab_suffix} \
                 modules=virtio_mmio,virtio_blk,virtio_console,virtio_net,virtio_gpu"
            );
            if !extra_cmdline.is_empty() {
                cmdline.push(' ');
                cmdline.push_str(&extra_cmdline);
            }

            println!("== aemulator-hvf run-android ==");
            println!("cf dir       : {cf:?}");
            println!("kernel       : {kernel:?}");
            println!("initramfs    : {initrd:?}");
            println!(
                "disk         : GPT composite /dev/vda ({} partitions) @ virtio-pci 00:03.0",
                parts.len(),
            );
            for p in &parts {
                println!(
                    "  by-name/{:<20} {} {}",
                    p.label,
                    if p.writable { "rw" } else { "ro" },
                    p.path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                );
            }
            println!("boot_devices : {boot_devices}");
            println!("mem / cpus   : {mem_mb} MiB / {cpus}");
            println!("selinux      : {selinux}");
            println!("identity     : model={dt_model:?} compatible={dt_compatible:?} serialno={serialno} verifiedbootstate={vb_state}");
            println!("cmdline      : {cmdline}");
            println!("bootconfig   :\n{bootconfig}");

            let ram_bytes = mem_mb * 1024 * 1024;
            let bc_bytes = bootconfig.into_bytes();
            let dt_identity = (dt_compatible, dt_model);

            // Phase 6: control plane (no window; screenshots come straight from the guest scanout,
            // so it works on a headless/remote session). Takes precedence over `--display`.
            if control {
                println!(
                    "control      : socket={} script={:?} repl={} (host-driven input + framebuffer capture; no adb)",
                    socket
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| aemulator_hvf::control::DEFAULT_SOCKET.to_string()),
                    script,
                    repl,
                );
                return super::run_control_bc(
                    kernel,
                    Some(initrd),
                    Vec::new(),
                    Some(parts),
                    Some(bc_bytes),
                    ram_bytes,
                    max_iters,
                    cpus,
                    cmdline,
                    gpu3d,
                    true, // Android always gets a NIC
                    Some(dt_identity),
                    aemulator_hvf::control::ControlOpts {
                        socket,
                        script,
                        repl,
                        exit_after_script,
                    },
                );
            }

            if display {
                return super::run_windowed_bc(
                    kernel,
                    Some(initrd),
                    Vec::new(),
                    Some(parts),
                    Some(bc_bytes),
                    ram_bytes,
                    max_iters,
                    cpus,
                    cmdline,
                    gpu3d,
                    true, // Android always gets a NIC
                    Some(dt_identity),
                );
            }

            let cfg = Config {
                kernel_path: &kernel,
                ram_bytes,
                cmdline: &cmdline,
                max_iters,
                num_cpus: cpus,
                initrd_path: Some(&initrd),
                disk_paths: &[],
                composite_partitions: Some(&parts),
                bootconfig: Some(&bc_bytes),
                gpu3d,
                netdev: true,
                dt_identity: Some(dt_identity),
            };
            match run_linux(&cfg) {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("[FAIL] {e}");
                    1
                }
            }
        }

        fn make_empty(path: &Path, bytes: u64) -> std::io::Result<()> {
            let f = std::fs::File::create(path)?;
            f.set_len(bytes)?;
            Ok(())
        }

        /// Ensure `path` exists and is at least `bytes` long (a sparse grow — the tail reads as
        /// zeros, which is what a wiped/formattable partition looks like). Never shrinks an existing
        /// image. Returns `true` if it created or grew the file.
        fn ensure_size(path: &Path, bytes: u64) -> std::io::Result<bool> {
            if !path.exists() {
                make_empty(path, bytes)?;
                return Ok(true);
            }
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            let cur = f.metadata()?.len();
            if cur < bytes {
                f.set_len(bytes)?;
                return Ok(true);
            }
            Ok(false)
        }

        /// Write a copy of the AVB `vbmeta` image with `AVB_VBMETA_IMAGE_FLAGS_VERIFICATION_DISABLED`
        /// (flags = 2) set in its header, so libavb short-circuits verification and fs_mgr mounts the
        /// partitions without dm-verity. The `flags` field is a **big-endian u32 at offset 120** of
        /// `AvbVBMetaImageHeader`. Returns the patched copy's path (`vbmeta.verifydisabled.img`).
        fn patch_vbmeta_verification_disabled(
            src: &Path,
            img_dir: &Path,
        ) -> std::io::Result<PathBuf> {
            let mut data = std::fs::read(src)?;
            if data.len() < 124 || &data[0..4] != b"AVB0" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "not an AVB vbmeta image (bad magic)",
                ));
            }
            // flags (BE u32) @ 120 → 2 (VERIFICATION_DISABLED).
            data[120..124].copy_from_slice(&2u32.to_be_bytes());
            let out = img_dir.join("vbmeta.verifydisabled.img");
            std::fs::write(&out, &data)?;
            Ok(out)
        }

        fn err(msg: &str) -> i32 {
            eprintln!("{msg}");
            2
        }
    }
}
