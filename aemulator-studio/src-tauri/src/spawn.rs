//! Build-adjacent launch of the `aemulator-hvf` binary: ad-hoc codesign it with
//! the hypervisor entitlement (required or `hv_vm_create` fails), compose the
//! argv + env, and spawn the supervised child with piped stdout/stderr.
//!
//! We invoke the `control` subcommand (which the binary rewrites to
//! `run-android --control`), pointing it at a per-VM cf-dir + Unix socket:
//!   aemulator-hvf control --cf-dir <dir> --socket <sock> --mem-mb N --cpus N
//!       [--display --gpu3d] --selinux <mode> [--extra-cmdline "…"]
//! env: SWRENDER=1 (SwRender mode), AEMU_HOSTFWD="tcp:<adb_port>:5555".

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::paths;
use crate::types::{EmulatorProfile, RenderMode, SelinuxMode};

/// gfxstream GPU-accel gralloc/vulkan bootconfig, mirrored from run-android64.sh.
/// NOTE `egl=angle`, NOT `egl=emulation`. There is no host OpenGL on macOS, so gfxstream's host GL
/// emulation is unusable; `egl=emulation` makes the guest's first EGL call abort the VMM
/// (`FATAL in getEmulationGl`), and enabling host GLES to satisfy it instead dies deeper in the same
/// path (`std::bad_function_call` after SurfaceFlinger's boot animation). ANGLE translates GLES→
/// Vulkan inside the GUEST, so the host only ever sees Vulkan → KosmicKrisp → Metal. This pairing
/// boots Android to the lock screen with gpu3d; keep it in sync with the VK-only `init_gfxstream`
/// call in the VMM's `machine.rs::build_gpu`.
///
/// `hwcomposer.display_finder_mode=drm` + `hwcomposer.mode=client` point the ranchu composer at our
/// virtio-gpu KMS scanout instead of making it look for a Cuttlefish vsock config server we do not
/// run. These were previously missing here while `run-android64.sh` set them, so the Studio was
/// launching a configuration nobody had validated.
const GPU3D_EXTRA_CMDLINE: &str = "androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=ranchu androidboot.hardware.gltransport=virtio-gpu-asg androidboot.hardware.gltransport.drawFlushInterval=800 androidboot.opengles.version=196609";
/// SwiftShader/ANGLE guest-CPU render bootconfig (SWRENDER=1), from run-control64.sh.
const SWRENDER_EXTRA_CMDLINE: &str = "androidboot.hardware.gralloc=minigbm androidboot.hardware.hwcomposer=ranchu androidboot.hardware.hwcomposer.display_finder_mode=drm androidboot.hardware.hwcomposer.mode=client androidboot.hardware.egl=angle androidboot.hardware.vulkan=pastel androidboot.opengles.version=196609";

/// A fully-composed launch plan: argv + env for `aemulator-hvf control …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// (key, value) env pairs layered onto the child.
    pub env: Vec<(String, String)>,
}

impl LaunchPlan {
    /// A copy-pasteable shell preview (env VAR=… prog args…) for logs + the UI.
    pub fn preview(&self) -> String {
        let env: String = self
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect();
        format!("{env}{} {}", self.program.display(), self.args.join(" "))
    }
}

/// Compose the argv + env for a profile + resolved per-VM allocations. Pure —
/// no I/O, no spawn — so it can be asserted in tests and previewed in the UI.
pub fn compose_launch(
    binary: PathBuf,
    profile: &EmulatorProfile,
    cf_dir: &Path,
    socket: &Path,
    adb_port: u16,
) -> LaunchPlan {
    let mut args: Vec<String> = vec![
        "control".into(),
        "--cf-dir".into(),
        cf_dir.display().to_string(),
        "--socket".into(),
        socket.display().to_string(),
        "--mem-mb".into(),
        profile.mem_mb.to_string(),
        "--cpus".into(),
        profile.cpus.to_string(),
    ];

    // Render pipeline → flags. Gpu3d shows a native window + Metal scanout;
    // SwRender / Headless run without --gpu3d (SwRender adds SWRENDER=1 below).
    let mut env: Vec<(String, String)> = vec![("AEMU_HOSTFWD".into(), format!("tcp:{adb_port}:5555"))];
    let extra_render_cmdline = match profile.render_mode {
        RenderMode::Gpu3d => {
            args.push("--display".into());
            args.push("--gpu3d".into());
            // NOTE deliberately NOT setting `AEMU_PRESENT_READBACK=1`.
            //
            // That flag is an all-or-nothing switch back to the legacy CPU-readback present:
            // `display::run` publishes no `CAMetalLayer` under it, so `flush_3d` never calls
            // `stream_renderer_present_to_metal_layer` and instead does a
            // `stream_renderer_transfer_read` into the CPU scanout framebuffer on EVERY guest
            // flush, which softbuffer then paints. Setting it here would take the validated
            // in-Vulkan present path (VkSurfaceKHR on the layer → vkCmdBlitImage →
            // vkQueuePresentKHR, entirely on the GPU) out of the configuration Studio ships and
            // leave nobody exercising it.
            //
            // Why the in-app live view still has pixels, which is not obvious: we pass `--display`,
            // but the `control` subcommand short-circuits it (bin/aemulator-hvf.rs routes
            // `--control` first and returns), so NO window and therefore no CAMetalLayer is ever
            // created. `ScanoutHandle::metal_layer` stays 0 for the whole run, so `flush_3d` takes
            // the readback branch and fills the CPU scanout framebuffer — exactly what the `frame`
            // / `screenshot` / `hash` verbs read. Verified: `shot` over the control socket returns a
            // correct 1024x768 Android frame with `nonzero=2359296`.
            //
            // The consequence is a cost, not a blank view: one `stream_renderer_transfer_read` per
            // GUEST flush rather than per frame the VIEWER asks for. Making that demand-driven needs
            // a VMM-side on-demand capture path; it is NOT what this flag does — setting
            // AEMU_PRESENT_READBACK would force the same readback while additionally retiring the
            // in-Vulkan present path in the windowed configuration.
            Some(GPU3D_EXTRA_CMDLINE)
        }
        RenderMode::SwRender => {
            env.push(("SWRENDER".into(), "1".into()));
            Some(SWRENDER_EXTRA_CMDLINE)
        }
        RenderMode::Headless => None,
    };

    args.push("--selinux".into());
    args.push(
        match profile.selinux {
            SelinuxMode::Permissive => "permissive",
            SelinuxMode::Enforcing => "enforcing",
        }
        .into(),
    );

    // Merge the render bootconfig with any user extra-cmdline into one flag.
    let mut extra = String::new();
    if let Some(rc) = extra_render_cmdline {
        extra.push_str(rc);
    }
    if !profile.extra_cmdline.trim().is_empty() {
        if !extra.is_empty() {
            extra.push(' ');
        }
        extra.push_str(profile.extra_cmdline.trim());
    }
    if !extra.is_empty() {
        args.push("--extra-cmdline".into());
        args.push(extra);
    }

    LaunchPlan { program: binary, args, env }
}

/// Ad-hoc codesign the binary with the hypervisor entitlement:
/// `codesign --sign - --entitlements <ent> --force <bin>`. Idempotent; required
/// before every run or `hv_vm_create` returns `HV_DENIED`.
pub fn ensure_signed(binary: &Path) -> Result<(), String> {
    let ent = paths::hv_entitlements()?;
    let out = Command::new("codesign")
        .args([
            "--sign",
            "-",
            "--entitlements",
            &ent.display().to_string(),
            "--force",
            &binary.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("codesign spawn: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Does the emulator binary actually carry the libslirp NAT backend?
///
/// As of 2026-07-27 networking is **not optional** in `aemulator-hvf` — the `net` cargo feature was
/// deleted, so every build links libslirp and this returns true. What it still catches is a **stale
/// binary**: one built before that change (or from an older checkout) sitting in `target/release`,
/// which Studio would happily spawn. That is the exact shape of the original failure — a
/// `--features gfxstream`-only release binary, indistinguishable from here (same path, same argv,
/// same `AEMU_HOSTFWD`) right up until the guest got a NIC that dropped every frame. The VMM still
/// enumerates `eth0`, still assigns `10.0.2.15`, still starts adbd, so the guest *looks* networked
/// from the inside; the only symptom is that nothing crosses the wire.
///
/// Detection is a byte scan for the `libslirp` dylib path linked in as an `LC_LOAD_DYLIB` string; a
/// binary without the NAT backend carries the name nowhere. Dependency-free (no `otool` shell-out)
/// and sub-millisecond on a release binary.
pub fn binary_has_net(binary: &Path) -> bool {
    std::fs::read(binary).is_ok_and(|image| image_links_libslirp(&image))
}

fn image_links_libslirp(image: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"libslirp";
    image.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

/// Refuse to launch a binary that cannot do networking, naming the cause and the fix.
///
/// Fatal rather than a warning because Studio does not merely *offer* networking — every launch
/// allocates a host adb port, sets `AEMU_HOSTFWD`, and shows the user an `adb connect` hint. On a
/// net-less binary all three are lies, and the failure surfaces far from its cause (a timeout in
/// adb, minutes after a successful-looking boot). Same reasoning as the Vulkan-ICD check above:
/// turn an unattributable failure into a message that names it. The escape hatch is one rebuild.
pub fn ensure_net_support(binary: &Path) -> Result<(), String> {
    if binary_has_net(binary) {
        return Ok(());
    }
    Err(format!(
        "the emulator binary at {} carries no NAT backend, so its NIC is the enumerate-only stub: \
         it links up and takes an IP but drops every packet, and the adb port this launch would \
         hand you could never connect. This means the binary is STALE — networking is no longer \
         optional in aemulator-hvf, so any current build has it. Rebuild:\n  \
         cargo build --release --manifest-path <aemulator-hvf>/Cargo.toml --bin aemulator-hvf\n\
         (needs `brew install libslirp`.)",
        binary.display()
    ))
}

/// Where `third_party/bootstrap.sh` installs the KosmicKrisp driver manifest, relative to the repo
/// root that holds `crates/` — i.e. `TP_KOSMICKRISP_ICD_DIR` as `third_party/paths.sh` resolves it
/// for every shell consumer. Studio is Rust and cannot source that file, so this constant is the
/// Rust-side mirror of it and [`kosmickrisp_icd`] is the only place the path is spelled.
const KOSMICKRISP_ICD_DIR_REL: &str = "third_party/kosmickrisp-install/share/vulkan/icd.d";

/// Mesa names the manifest `kosmickrisp_mesa_icd.<meson host_machine.cpu()>.json`, and meson's cpu
/// name is not `uname -m`'s (`aarch64`, not `arm64`), so the installed file is DISCOVERED by this
/// prefix rather than reassembled from the host architecture.
const KOSMICKRISP_ICD_PREFIX: &str = "kosmickrisp_mesa_icd.";

/// The one command that produces it, named in the failure because it is the entire fix.
const BOOTSTRAP_REL: &str = "third_party/bootstrap.sh";

/// The staged manifest's basename is not cosmetic: gfxstream's `initIcdPaths` falls back to
/// `setIcdPaths("kosmickrisp_mesa_icd.aarch64.json")` and looks for *that* name under
/// `<programDir>/lib64/vulkan`, so staging it as anything else is the same as not staging it.
const KOSMICKRISP_ICD_BASENAME: &str = "kosmickrisp_mesa_icd.aarch64.json";

/// The Vulkan LOADER (not the driver). Homebrew first, then the Intel-era prefix.
const VULKAN_LOADER_CANDIDATES: [&str; 2] =
    ["/opt/homebrew/lib/libvulkan.dylib", "/usr/local/lib/libvulkan.dylib"];

/// Resolve the KosmicKrisp driver manifest that this machine's bootstrap installed, erroring with
/// the command that produces it when it is absent.
///
/// KosmicKrisp (Mesa's Khronos-conformant Vulkan-on-Metal driver) is the ONLY host Vulkan driver
/// this build supports — MoltenVK is gone from the render path and from the repo, and there is no
/// fallback, so a missing manifest is a hard launch failure rather than a degraded mode.
pub fn kosmickrisp_icd() -> Result<PathBuf, String> {
    let hvf = paths::hvf_dir()?;
    // <repo>/crates/aemulator-hvf → <repo>.
    let repo = hvf
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{} has no repo root above it", hvf.display()))?;
    let dir = repo.join(KOSMICKRISP_ICD_DIR_REL);
    let found = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.flatten().map(|e| e.path()).find(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(KOSMICKRISP_ICD_PREFIX) && n.ends_with(".json"))
        })
    });
    if let Some(icd) = found {
        return Ok(icd);
    }
    Err(format!(
        "no KosmicKrisp Vulkan driver manifest ({KOSMICKRISP_ICD_PREFIX}*.json) in {} — bootstrap \
         the out-of-tree dependencies:\n  {}\n\
         (KosmicKrisp is Mesa's conformant Vulkan-on-Metal driver and needs macOS 26 or newer on \
         Apple Silicon.)",
        dir.display(),
        repo.join(BOOTSTRAP_REL).display()
    ))
}

/// Stage everything a gfxstream (`--gpu3d`) launch needs next to the emulator binary.
///
/// Two distinct artifacts land in `<bindir>/lib64/vulkan/`, and both are required:
///
///   * the Vulkan **loader** (`libvulkan.dylib`), symlinked from Homebrew. gfxstream's
///     `VulkanDispatch::getPossibleLoaderPaths()` stops searching the system default paths and
///     looks ONLY in `<programDir>/lib64/vulkan` + `<launcherDir>/lib64/vulkan` as soon as
///     `ANDROID_EMU_VK_ICD` is non-empty — which `spawn_child` sets — so an unstaged loader means
///     no Vulkan at all, not a system-loader fallback.
///   * the KosmicKrisp **driver** manifest, under the exact basename `initIcdPaths` falls back to.
///     `spawn_child` also exports `VK_DRIVER_FILES`, which takes strict loader precedence over this
///     copy; the copy is what a relocated bundle (no env) would load, and re-copying every launch
///     is what keeps it from going stale against a rebuilt driver.
///
/// Every step is fatal. It used to swallow the manifest copy (`let _ = fs::copy`), which meant a
/// missing or unreadable manifest sailed through the caller's preflight and produced exactly the
/// unattributable SEGFAULT that preflight exists to prevent: gfxstream finds no ICD, logs "Failed
/// to initialize global Vulkan emulation", `stream_renderer_init` still returns 0, and the VMM dies
/// a moment later with no mention of Vulkan.
pub fn ensure_vulkan_icd(binary: &Path) -> Result<(), String> {
    stage_vulkan_icd(binary, &kosmickrisp_icd()?, &VULKAN_LOADER_CANDIDATES)
}

/// The I/O half of [`ensure_vulkan_icd`], with both inputs injected so it can be tested against a
/// fabricated source tree instead of whatever this machine happens to have installed.
fn stage_vulkan_icd(
    binary: &Path,
    icd: &Path,
    loader_candidates: &[&str],
) -> Result<(), String> {
    let bindir = binary.parent().ok_or("binary has no parent dir")?;
    let vkdir = bindir.join("lib64/vulkan");
    std::fs::create_dir_all(&vkdir).map_err(|e| format!("mkdir {}: {e}", vkdir.display()))?;

    let loader = loader_candidates
        .iter()
        .find(|cand| Path::new(cand).exists())
        .ok_or_else(|| {
            format!(
                "no Vulkan loader (libvulkan.dylib) at any of {loader_candidates:?} — \
                 install one: brew install vulkan-loader"
            )
        })?;
    let link = vkdir.join("libvulkan.dylib");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(loader, &link)
        .map_err(|e| format!("symlink {loader} -> {}: {e}", link.display()))?;

    let staged = vkdir.join(KOSMICKRISP_ICD_BASENAME);
    std::fs::copy(icd, &staged)
        .map_err(|e| format!("stage {} -> {}: {e}", icd.display(), staged.display()))?;
    Ok(())
}

/// Spawn the composed plan as a child with piped stdout/stderr. The caller owns
/// the returned `Child` (its pipes feed the supervisor thread).
pub fn spawn_child(plan: &LaunchPlan, gpu3d: bool) -> Result<Child, String> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    if gpu3d {
        // Matches run-android64.sh exactly — the only gpu3d configuration validated end to end.
        //
        // `ANDROID_EMU_VK_ICD=kosmickrisp` selects the driver AND (see `stage_vulkan_icd`) narrows
        // gfxstream's loader search to `<programDir>/lib64/vulkan`; `current_dir` is what makes
        // `<programDir>` the binary's own directory, i.e. the tree we just staged into.
        cmd.env("ANDROID_EMU_VK_ICD", "kosmickrisp");
        // Authoritative driver selection: `VK_DRIVER_FILES` beats `VK_ICD_FILENAMES` in the Khronos
        // loader, and gfxstream's `initIcdPaths` now honors a caller-supplied value instead of
        // overwriting it — so naming the same manifest kosmickrisp/run-gate.sh drives makes the
        // Studio and the conformance gate provably load the same driver binary. The staged copy
        // stays as the bundle-relative fallback.
        cmd.env("VK_DRIVER_FILES", kosmickrisp_icd()?);
        if let Some(dir) = plan.program.parent() {
            cmd.current_dir(dir);
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
        .map_err(|e| format!("spawn {}: {e}", plan.program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DeviceIdentity, RenderMode, SelinuxMode};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique scratch dir. This crate carries no `tempfile` dev-dependency and two staging tests
    /// do not justify adding one; pid + a counter is enough to keep parallel test threads (and
    /// concurrent `cargo test` runs) from colliding.
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "aemu-studio-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn prof(render: RenderMode) -> EmulatorProfile {
        EmulatorProfile {
            id: "x".into(),
            name: "t".into(),
            base_image: "cf64".into(),
            mem_mb: 4096,
            cpus: 2,
            render_mode: render,
            selinux: SelinuxMode::Permissive,
            extra_cmdline: String::new(),
            device_identity: DeviceIdentity {
                soc: "s".into(),
                manufacturer: "m".into(),
                model: "mo".into(),
                advertised_sensors: vec![],
                telephony: None,
            },
            created_at: 0,
        }
    }

    #[test]
    fn swrender_sets_env_and_no_gpu3d() {
        let p = compose_launch(
            PathBuf::from("/bin/x"),
            &prof(RenderMode::SwRender),
            Path::new("/i/cf"),
            Path::new("/i/s.sock"),
            5556,
        );
        assert!(p.env.iter().any(|(k, v)| k == "SWRENDER" && v == "1"));
        assert!(p.env.iter().any(|(k, v)| k == "AEMU_HOSTFWD" && v == "tcp:5556:5555"));
        assert!(!p.args.iter().any(|a| a == "--gpu3d"));
        assert!(p.args.iter().any(|a| a == "--cf-dir"));
    }

    #[test]
    fn gpu3d_adds_display_flags() {
        let p = compose_launch(
            PathBuf::from("/bin/x"),
            &prof(RenderMode::Gpu3d),
            Path::new("/i/cf"),
            Path::new("/i/s.sock"),
            5560,
        );
        assert!(p.args.iter().any(|a| a == "--display"));
        assert!(p.args.iter().any(|a| a == "--gpu3d"));
        assert!(!p.env.iter().any(|(k, _)| k == "SWRENDER"));
        // The guest must use ANGLE, never gfxstream's GLES: there is no host OpenGL on
        // macOS, so `egl=emulation` aborts the VMM mid-boot.
        assert!(p.args.iter().any(|a| a.contains("androidboot.hardware.egl=angle")));
        assert!(!p.args.iter().any(|a| a.contains("egl=emulation")));
    }

    /// The Gpu3d cmdline must stay identical to the one `run-android64.sh` boots, because that is the
    /// only combination anyone verifies end-to-end. It had already drifted once — the Studio was
    /// missing the two `hwcomposer.*` props that point the ranchu composer at our virtio-gpu KMS
    /// scanout instead of a Cuttlefish vsock config server we do not run — so pin every prop here.
    #[test]
    fn gpu3d_cmdline_matches_the_validated_harness_configuration() {
        let p = compose_launch(
            PathBuf::from("/bin/x"),
            &prof(RenderMode::Gpu3d),
            Path::new("/i/cf"),
            Path::new("/i/s.sock"),
            5561,
        );
        let cmdline = p
            .args
            .iter()
            .find(|a| a.contains("androidboot.hardware.gralloc"))
            .expect("Gpu3d must pass a graphics cmdline");
        for prop in [
            "androidboot.hardware.gralloc=minigbm",
            "androidboot.hardware.hwcomposer=ranchu",
            "androidboot.hardware.hwcomposer.display_finder_mode=drm",
            "androidboot.hardware.hwcomposer.mode=client",
            "androidboot.hardware.egl=angle",
            "androidboot.hardware.vulkan=ranchu",
            "androidboot.hardware.gltransport=virtio-gpu-asg",
            "androidboot.opengles.version=196609",
        ] {
            assert!(cmdline.contains(prop), "Gpu3d cmdline is missing {prop}: {cmdline}");
        }
        // `vulkan=ranchu` is what selects gfxstream_vk (the real host GPU); `pastel` is SwiftShader.
        assert!(!cmdline.contains("vulkan=pastel"), "Gpu3d must not select SwiftShader");
    }

    /// Staging must leave the MANIFEST on disk, not merely the directory that should hold it.
    ///
    /// This is the exact gap that let the MoltenVK-era bug hide: the copy was `let _ =
    /// fs::copy(..)`, so `create_dir_all` succeeding was the only thing preflight ever observed.
    /// `<bindir>/lib64/vulkan` existed, `ensure_vulkan_icd` returned Ok, and gfxstream then found
    /// no ICD and took the VMM down with a SEGFAULT that named nothing.
    #[test]
    fn staging_writes_the_icd_manifest_itself_not_just_its_directory() {
        let root = scratch("icd-ok");
        let icd = root.join(KOSMICKRISP_ICD_BASENAME);
        std::fs::write(&icd, br#"{"ICD":{"library_path":"/x/libvulkan_kosmickrisp.dylib"}}"#)
            .unwrap();
        // Stand-in for the Vulkan loader, so the test never depends on this machine's Homebrew.
        let loader = root.join("libvulkan.dylib");
        std::fs::write(&loader, b"\xcf\xfa\xed\xfe").unwrap();
        let bin = root.join("bin/aemulator-hvf");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();

        stage_vulkan_icd(&bin, &icd, &[&loader.display().to_string()]).expect("staging must succeed");

        let vkdir = bin.parent().unwrap().join("lib64/vulkan");
        let staged = vkdir.join(KOSMICKRISP_ICD_BASENAME);
        assert!(staged.is_file(), "no ICD manifest staged at {}", staged.display());
        assert_eq!(
            std::fs::read(&staged).unwrap(),
            std::fs::read(&icd).unwrap(),
            "staged manifest must be a copy of the built driver's, not a stale leftover"
        );
        // The loader belongs in the SAME dir: ANDROID_EMU_VK_ICD being set makes gfxstream look
        // for libvulkan.dylib here and nowhere else, so a staged ICD alone still finds no Vulkan.
        assert!(vkdir.join("libvulkan.dylib").exists(), "Vulkan loader not staged");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_icd_manifest_fails_staging_instead_of_passing_preflight() {
        let root = scratch("icd-missing");
        let loader = root.join("libvulkan.dylib");
        std::fs::write(&loader, b"\xcf\xfa\xed\xfe").unwrap();
        let bin = root.join("bin/aemulator-hvf");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();

        let err = stage_vulkan_icd(
            &bin,
            &root.join("kosmickrisp-not-built.json"),
            &[&loader.display().to_string()],
        )
        .expect_err("a missing driver manifest must be fatal — there is no MoltenVK fallback left");
        assert!(err.contains("kosmickrisp-not-built.json"), "error must name the manifest: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn net_probe_keys_on_the_libslirp_load_command() {
        // A `--features net` build embeds the dylib path as an LC_LOAD_DYLIB string…
        assert!(image_links_libslirp(
            b"\xcf\xfa\xed\xfe...../opt/homebrew/opt/libslirp/lib/libslirp.0.dylib....."
        ));
        // …and a default (stub-NIC) build names it nowhere. Guard the false-negative direction
        // specifically: reporting a net binary as net-less is a spurious hard launch failure.
        assert!(!image_links_libslirp(
            b"\xcf\xfa\xed\xfe...../usr/lib/libSystem.B.dylib..virtio-net stub (enumerate + link-up"
        ));
    }

    #[test]
    fn never_forces_present_readback() {
        // The flag switches the VMM back to the legacy CPU-readback present wholesale: no
        // CAMetalLayer is published, so the in-Vulkan swapchain present never runs and every
        // GUEST frame pays a stream_renderer_transfer_read. Setting it would quietly retire the
        // only present path Studio ships.
        for mode in [RenderMode::Gpu3d, RenderMode::SwRender, RenderMode::Headless] {
            let p = compose_launch(
                PathBuf::from("/bin/x"),
                &prof(mode),
                Path::new("/i/cf"),
                Path::new("/i/s.sock"),
                5556,
            );
            assert!(
                !p.env.iter().any(|(k, _)| k == "AEMU_PRESENT_READBACK"),
                "{mode:?} must not force present readback"
            );
        }
    }
}
