//! # aemulator — the AEmulator CLI
//!
//! A thin operator front-end over the real crates. Static (host-only) verbs load a
//! [`DeviceProfile`] and drive one crate; **live** verbs boot and drive a real ARM64 Android VM
//! through [`aemulator_supervisor`] + [`aemulator_adb`].
//!
//! | subcommand         | what it does                                                        |
//! |--------------------|---------------------------------------------------------------------|
//! | `profile-show`     | serialize the profile to JSON                                       |
//! | `profile-validate` | [`DeviceProfile::validate`]                                          |
//! | `props`            | [`DeviceProfile::emit_properties`]                                   |
//! | `bootconfig`       | [`DeviceProfile::emit_bootconfig`]                                   |
//! | `fingerprint`      | [`DeviceProfile::fingerprint`]                                       |
//! | `lint <dir>`       | [`aemulator_detection::scan_dir`] over a built-image directory       |
//! | `qemu-args`        | [`aemulator_vmm::qemu_hvf_args`]                                     |
//! | `dtb <out>`        | [`aemulator_dtb::build_fdt`]                                         |
//! | `run`              | boot a real ARM64 Android VM (HVF window) + inject identity          |
//! | `stop`             | kill the running VM                                                  |
//! | `audit`            | run the detection catalogue against the **live** device             |
//! | `ui`               | dump the live UI tree (index-first, as the agent sees it)           |
//! | `shot <out>`       | capture a live screenshot                                           |
//! | `tap <index>`      | tap a UI node by index (generation-guarded)                         |
//! | `tapxy <x> <y>`    | tap raw device coordinates                                          |
//! | `launch <pkg>`     | launch an app                                                       |
//! | `install <apk>`    | install an APK                                                      |
//! | `gps <lat> <lng>`  | set the simulated GNSS fix                                          |

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use aemulator_adb::AdbController;
use aemulator_control::{Action, DeviceController};
use aemulator_identity::{profiles, DeviceProfile};
use aemulator_supervisor::Supervisor;
use aemulator_vmm::BootAssets;

/// AEmulator identity + device CLI.
#[derive(Parser, Debug)]
#[command(name = "aemulator", version, about, long_about = None)]
struct Cli {
    /// Path to a DeviceProfile TOML. Defaults to the bundled reference profile.
    #[arg(long, global = true)]
    profile: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the resolved DeviceProfile as pretty JSON.
    ProfileShow,
    /// Run the coherence validator over the profile.
    ProfileValidate,
    /// Emit the Android system-property map (key=value per line).
    Props,
    /// Emit the bootconfig `androidboot.*` lines (key=value per line).
    Bootconfig,
    /// Print ConstructBuildFingerprint for the profile.
    Fingerprint,
    /// Scan a built-image directory for virtualization tells.
    Lint {
        /// Directory to scan (a mounted/extracted built image).
        dir: PathBuf,
    },
    /// Parse `key=value` property files (build.prop / getprop dumps) into one merged map and run
    /// the property-map linter (`lint_properties`). Every `*.prop`, `*.build.prop`, and `*.txt`
    /// file directly inside <dir> is merged (later files win on key conflict); lines starting with
    /// `#` and blank lines are ignored. Prints the tell count + one line per finding.
    LintProps {
        /// Directory holding the property files to merge + lint.
        dir: PathBuf,
    },
    /// Print the QEMU/HVF launcher argument vector.
    QemuArgs,
    /// Build the device-tree blob and write it to <out>.
    Dtb {
        /// Output path for the .dtb.
        out: PathBuf,
    },
    /// Boot a real ARM64 Android VM (HVF window on Apple Silicon) and inject the writable identity.
    Run {
        /// AVD to boot (must exist; see docs/PHASE0-RUNBOOK.md to create one).
        #[arg(long, default_value = "aemu_ranchu_arm64")]
        avd: String,
        /// Boot without a window.
        #[arg(long)]
        headless: bool,
        /// Do NOT inject the profile identity via -prop (boot the AVD as-is).
        #[arg(long)]
        no_inject: bool,
        /// Boot with -writable-system so `aemulator harden` can rewrite build.prop in place.
        #[arg(long)]
        writable: bool,
    },
    /// Kill the running VM.
    Stop,
    /// Rewrite the live image's build.prop identity to the profile (root + remount, then reboots).
    /// Requires the VM booted with `run --writable`.
    Harden,
    /// Run the detection catalogue against the live device and report which tells fire.
    Audit,
    /// Dump the live UI tree the agent perceives.
    Ui,
    /// Capture a live screenshot to <out> (PNG).
    Shot { out: PathBuf },
    /// Tap the UI node at <index> (generation-guarded, index-first).
    Tap { index: u32 },
    /// Tap raw device coordinates (escape hatch).
    #[command(allow_negative_numbers = true)]
    Tapxy { x: i32, y: i32 },
    /// Launch an app by package name.
    Launch { pkg: String },
    /// Install an APK.
    Install { apk: PathBuf },
    /// Set the simulated GNSS fix.
    #[command(allow_negative_numbers = true)]
    Gps { lat: f64, lng: f64 },
}

fn load_profile(cli: &Cli) -> Result<DeviceProfile> {
    match &cli.profile {
        Some(path) => DeviceProfile::from_toml(path)
            .with_context(|| format!("loading profile from {}", path.display())),
        None => Ok(profiles::reference_profile()),
    }
}

/// The device paths whose mere existence reveals the goldfish/ranchu emulator.
const NODE_TELLS: &[&str] = &[
    "/dev/qemu_pipe",
    "/dev/goldfish_pipe",
    "/dev/goldfish_sync",
    "/dev/goldfish_dma",
    "/dev/socket/qemud",
    "/sys/qemu_trace",
    "/vendor/etc/fstab.ranchu",
    "/system/bin/qemu-props",
];

fn run_live_audit(ctl: &AdbController) -> Result<()> {
    println!("== Live detection audit (serial {}) ==\n", ctl.serial());

    let props = ctl.getprop_all().context("reading getprop")?;
    let show = [
        "ro.product.brand",
        "ro.product.model",
        "ro.product.device",
        "ro.hardware",
        "ro.build.fingerprint",
        "ro.kernel.qemu",
        "ro.boot.qemu",
        "ro.build.characteristics",
    ];
    println!("-- live identity --");
    for k in show {
        println!(
            "  {k} = {}",
            props.get(k).map(String::as_str).unwrap_or("<unset>")
        );
    }

    // 1) Property tells via the detection crate (blocklist + build-prop signals).
    let prop_findings = aemulator_detection::lint_properties(&props);

    // 2) Node/socket tells by live existence check.
    let mut node_fires = Vec::new();
    for p in NODE_TELLS {
        if ctl.exists(p) {
            node_fires.push(*p);
        }
    }

    // 3) Sensor HAL tell.
    let sensors = ctl.shell(&["dumpsys", "sensorservice"]).unwrap_or_default();
    let sensor_fires = sensors.contains("Goldfish") || sensors.contains("goldfish");

    let total = prop_findings.len() + node_fires.len() + usize::from(sensor_fires);
    println!("\n-- signals firing: {total} --");
    for f in &prop_findings {
        println!(
            "  🔴 [{}] {} @ {} matched `{}`",
            f.signal.category, f.signal.pattern, f.location, f.matched
        );
    }
    for p in &node_fires {
        println!("  🔴 [file-node] {p} exists");
    }
    if sensor_fires {
        println!("  🔴 [sensor] Goldfish sensor HAL present (dumpsys sensorservice)");
    }
    if total == 0 {
        println!("  🟢 none — device presents as real hardware to this catalogue");
    } else {
        println!(
            "\nNote: these are image-baked ranchu tells a runtime -prop cannot rewrite; clearing them\n\
             to zero is the custom de-branded guest image + crosvm/KVM milestone (docs/PHASE0-RUNBOOK.md)."
        );
    }
    Ok(())
}

fn connect() -> Result<AdbController> {
    AdbController::connect(None).map_err(|e| anyhow::anyhow!("connect to device: {e}"))
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match &cli.command {
        Command::ProfileShow => {
            let profile = load_profile(&cli)?;
            println!("{}", serde_json::to_string_pretty(&profile)?);
        }
        Command::ProfileValidate => {
            let profile = load_profile(&cli)?;
            match profile.validate() {
                Ok(()) => println!("OK: profile is coherent"),
                Err(errors) => {
                    for e in &errors {
                        eprintln!("coherence error: {e}");
                    }
                    anyhow::bail!("{} coherence error(s)", errors.len());
                }
            }
        }
        Command::Props => {
            let profile = load_profile(&cli)?;
            for (key, value) in profile.emit_properties() {
                println!("{key}={value}");
            }
        }
        Command::Bootconfig => {
            let profile = load_profile(&cli)?;
            for (key, value) in profile.emit_bootconfig() {
                println!("{key}={value}");
            }
        }
        Command::Fingerprint => {
            let profile = load_profile(&cli)?;
            println!("{}", profile.fingerprint());
        }
        Command::Lint { dir } => {
            let findings = aemulator_detection::scan_dir(dir);
            if findings.is_empty() {
                println!(
                    "no findings ({} signals in blocklist)",
                    aemulator_detection::blocklist().len()
                );
            } else {
                for f in &findings {
                    println!(
                        "[{}] {} @ {} matched `{}`",
                        f.signal.category, f.signal.pattern, f.location, f.matched
                    );
                }
                std::process::exit(1);
            }
        }
        Command::LintProps { dir } => {
            let mut props: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
                .with_context(|| format!("reading {}", dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.is_file()
                        && p.file_name()
                            .and_then(|s| s.to_str())
                            .map(|n| {
                                n.ends_with(".prop") || n.ends_with(".txt") || n.contains("prop")
                            })
                            .unwrap_or(false)
                })
                .collect();
            files.sort();
            for path in &files {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        props.insert(k.trim().to_string(), v.trim().to_string());
                    }
                }
            }
            let findings = aemulator_detection::lint_properties(&props);
            println!(
                "merged {} keys from {} file(s); {} tell(s)",
                props.len(),
                files.len(),
                findings.len()
            );
            for f in &findings {
                println!(
                    "[{}] {} @ {} matched `{}`",
                    f.signal.category, f.signal.pattern, f.location, f.matched
                );
            }
            if !findings.is_empty() {
                std::process::exit(1);
            }
        }
        Command::QemuArgs => {
            let profile = load_profile(&cli)?;
            let assets = aemulator_vmm::BootAssets::default();
            let args = aemulator_vmm::qemu_hvf_args(&profile, &assets);
            println!("qemu-system-aarch64 {}", args.join(" "));
        }
        Command::Dtb { out } => {
            let profile = load_profile(&cli)?;
            let blob = aemulator_dtb::build_fdt(&profile).context("building device tree")?;
            std::fs::write(out, &blob).with_context(|| format!("writing {}", out.display()))?;
            println!("wrote {} bytes to {}", blob.len(), out.display());
        }
        Command::Run {
            avd,
            headless,
            no_inject,
            writable,
        } => {
            let profile = load_profile(&cli)?;
            let launcher = aemulator_adb::AndroidHvfLauncher {
                avd: avd.clone(),
                gpu: "host".to_string(),
                window: !headless,
                inject_identity: !no_inject,
                writable_system: *writable,
                boot_timeout: std::time::Duration::from_secs(300),
            };
            let injected = launcher.injectable_props(&profile);
            println!(
                "Booting AVD `{}` under HVF ({}), injecting {} identity props:",
                avd,
                if *headless { "headless" } else { "windowed" },
                injected.len()
            );
            for (k, v) in &injected {
                println!("  -prop {k}={v}");
            }
            println!("(this blocks until sys.boot_completed — cold boot ~30-60s)");
            let mut sup = Supervisor::with_launcher(profile, Box::new(launcher));
            sup.boot(&BootAssets::default())
                .map_err(|e| anyhow::anyhow!("boot failed: {e}"))?;
            println!("\n✅ VM state: {:?}", sup.state());
            if let Ok(obs) = sup.observe() {
                println!(
                    "control channel live: framebuffer {}x{}, {} UI nodes",
                    obs.width,
                    obs.height,
                    obs.ui_tree.nodes.len()
                );
            }
            println!("\nThe VM window is up and driven by aemulator. Try: `aemulator audit`, `aemulator ui`, `aemulator shot /tmp/s.png`.");
        }
        Command::Stop => {
            let ctl = connect()?;
            let adb = aemulator_adb::adb_path();
            let st = std::process::Command::new(&adb)
                .arg("-s")
                .arg(ctl.serial())
                .arg("emu")
                .arg("kill")
                .status()
                .context("adb emu kill")?;
            println!("stop ({}): {st}", ctl.serial());
        }
        Command::Harden => {
            let profile = load_profile(&cli)?;
            let ctl = connect()?;
            println!(
                "Hardening identity of live device {} to `{}` (root + remount + build.prop rewrite)...",
                ctl.serial(),
                profile.fingerprint()
            );
            let (files, lines) = aemulator_adb::harden_identity(&ctl, &profile)
                .map_err(|e| anyhow::anyhow!("harden failed: {e}"))?;
            println!("rewrote {lines} identity line(s) across {files} build.prop file(s); rebooting to apply...");
            ctl.reboot_and_wait(std::time::Duration::from_secs(300))
                .map_err(|e| anyhow::anyhow!("reboot: {e}"))?;
            println!("✅ rebooted. Run `aemulator audit` to see the reduced tell surface.");
        }
        Command::Audit => {
            let ctl = connect()?;
            run_live_audit(&ctl)?;
        }
        Command::Ui => {
            let ctl = connect()?;
            let obs = ctl.observe().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "UI tree (generation {}, {}x{}, {} nodes):",
                obs.generation,
                obs.width,
                obs.height,
                obs.ui_tree.nodes.len()
            );
            for n in &obs.ui_tree.nodes {
                let label = if !n.text.is_empty() {
                    n.text.clone()
                } else if !n.content_desc.is_empty() {
                    n.content_desc.clone()
                } else {
                    String::new()
                };
                if n.clickable || !label.is_empty() {
                    println!(
                        "  [{:>3}] {:<34} {:<6} {:?} {}",
                        n.index,
                        truncate(&label, 34),
                        if n.clickable { "click" } else { "" },
                        (n.bounds.x, n.bounds.y, n.bounds.width, n.bounds.height),
                        truncate(&n.class, 40)
                    );
                }
            }
        }
        Command::Shot { out } => {
            let ctl = connect()?;
            let obs = ctl.observe().map_err(|e| anyhow::anyhow!("{e}"))?;
            std::fs::write(out, &obs.screenshot_png)
                .with_context(|| format!("writing {}", out.display()))?;
            println!(
                "wrote {} bytes ({}x{}) to {}",
                obs.screenshot_png.len(),
                obs.width,
                obs.height,
                out.display()
            );
        }
        Command::Tap { index } => {
            let mut ctl = connect()?;
            let obs = ctl.observe().map_err(|e| anyhow::anyhow!("{e}"))?;
            let gen = obs.generation;
            let post = ctl
                .act(Action::Tap {
                    index: *index,
                    generation: gen,
                })
                .map_err(|e| anyhow::anyhow!("tap failed: {e}"))?;
            println!(
                "tapped node {index}; post-action generation {}",
                post.generation
            );
        }
        Command::Tapxy { x, y } => {
            let mut ctl = connect()?;
            ctl.act(Action::TapXy { x: *x, y: *y })
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("tapped ({x},{y})");
        }
        Command::Launch { pkg } => {
            let mut ctl = connect()?;
            ctl.act(Action::LaunchApp { pkg: pkg.clone() })
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("launched {pkg}");
        }
        Command::Install { apk } => {
            let mut ctl = connect()?;
            ctl.act(Action::InstallApk { path: apk.clone() })
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("installed {}", apk.display());
        }
        Command::Gps { lat, lng } => {
            let mut ctl = connect()?;
            ctl.act(Action::SetGps {
                lat: *lat,
                lng: *lng,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("set GPS fix to {lat},{lng}");
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    //! Arg-parsing tests. These exercise the `clap` derive wiring only — they never dispatch a
    //! subcommand body, so they are independent of whether a device is connected.
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn bare_subcommand_defaults_profile_to_none() {
        let cli = parse(&["aemulator", "fingerprint"]).expect("should parse");
        assert!(cli.profile.is_none());
        assert!(matches!(cli.command, Command::Fingerprint));
    }

    #[test]
    fn global_profile_flag_parses_before_the_subcommand() {
        let cli = parse(&["aemulator", "--profile", "/tmp/a.toml", "props"]).expect("should parse");
        assert_eq!(cli.profile, Some(PathBuf::from("/tmp/a.toml")));
        assert!(matches!(cli.command, Command::Props));
    }

    #[test]
    fn lint_captures_the_directory_argument() {
        let cli = parse(&["aemulator", "lint", "/mnt/img"]).expect("should parse");
        match cli.command {
            Command::Lint { dir } => assert_eq!(dir, PathBuf::from("/mnt/img")),
            other => panic!("expected Lint, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_flags() {
        let cli = parse(&[
            "aemulator",
            "run",
            "--avd",
            "foo",
            "--headless",
            "--no-inject",
        ])
        .expect("should parse");
        match cli.command {
            Command::Run {
                avd,
                headless,
                no_inject,
                writable,
            } => {
                assert_eq!(avd, "foo");
                assert!(headless);
                assert!(no_inject);
                assert!(!writable);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn tap_and_gps_parse() {
        assert!(matches!(
            parse(&["aemulator", "tap", "5"]).unwrap().command,
            Command::Tap { index: 5 }
        ));
        match parse(&["aemulator", "gps", "37.77", "-122.41"])
            .unwrap()
            .command
        {
            Command::Gps { lat, lng } => {
                assert!((lat - 37.77).abs() < 1e-6);
                assert!((lng + 122.41).abs() < 1e-6);
            }
            other => panic!("expected Gps, got {other:?}"),
        }
    }

    #[test]
    fn missing_subcommand_is_an_error() {
        assert!(parse(&["aemulator"]).is_err());
    }
}
