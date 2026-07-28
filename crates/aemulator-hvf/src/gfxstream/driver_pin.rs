//! Hermetic guards that **pin the host Vulkan driver choice** so a silent revert to MoltenVK is a
//! `cargo test` failure instead of a rendering mystery.
//!
//! # Why this exists
//! Until now, every claim about which driver is live has been established by hand: run the
//! emulator, read `Selecting Vulkan device: Apple M2, Version: 1.4.354` out of the log (MoltenVK
//! reported `1.1.357`, so the version alone identifies the driver), take a screenshot. Nothing in
//! CI observed it. That makes the whole KosmicKrisp cutover exactly one careless edit — a copied
//! `export`, a resurrected staging line, a "just to unblock myself" fallback — away from silently
//! coming back, and the symptom would not be a build break. It would be a subtly different frame,
//! or a crash three layers down inside gfxstream, on someone else's machine, weeks later.
//!
//! # What these tests actually check, and what they cannot
//! They read the repo's **launch surfaces** as text and assert on their content. That is the whole
//! trick, and it is what makes them hermetic: no GPU, no VM boot, no Vulkan loader, no dependency
//! on KosmicKrisp having been built on this machine. They run in a container, on CI, on a laptop
//! with the driver deleted.
//!
//! The cost of that is honest and worth stating: **these tests prove the repo asks for KosmicKrisp,
//! not that KosmicKrisp answers.** A manifest that exists but names a stale dylib, a driver that
//! regressed, a loader that picks something else — none of that is visible from here. Two other
//! artifacts cover that half, and neither is a substitute for the other:
//!
//!   * `ci/hvf-doctor.sh` — checks, on a real host, that the ICD manifest exists, that the dylib it
//!     names exists, and that macOS is 26 or newer. [`hvf_doctor_still_checks_the_vulkan_driver`]
//!     pins that the doctor keeps doing so, and keeps pointing at the same manifest these
//!     launchers load.
//!   * `kosmickrisp/run-gate.sh` — the capability gate (`vkCreateDevice` with
//!     `VK_EXT_external_memory_metal`, an `MTLHeap` export from an external-memory image, and a
//!     real queue-submit + fence). It is the only thing that proves the driver can do what
//!     gfxstream's ColorBuffer path stands on. It FAILS on MoltenVK with
//!     `VK_ERROR_INCOMPATIBLE_DRIVER`, which is itself a useful property: a machine that reverted
//!     to MoltenVK cannot pass the gate.
//!
//! Also out of scope here, deliberately: the gfxstream C++ host renderer, which lives in the
//! `third_party/gfxstream` checkout that a hermetic test of *this* crate must not require to be
//! present. The Phase 3 edits there (dropped portability requirements, the `useMetalExternalMemory`
//! capability probe, the extension strip list) are a tracked patch series under
//! `third_party/patches/gfxstream/`, and `third_party/bootstrap.sh` is what proves they reached the
//! built dylib. Nothing in this file does.

use std::path::{Path, PathBuf};

/// The driver manifest `third_party/bootstrap.sh mesa` installs, relative to the repo root.
///
/// Split into directory + basename because that is how the Studio's `spawn.rs` stores it, and the
/// basename half is load-bearing on its own: gfxstream's `initIcdPaths` falls back to
/// `setIcdPaths("kosmickrisp_mesa_icd.aarch64.json")` and looks for *that exact name* under
/// `<programDir>/lib64/vulkan`, so staging the manifest under any other name is indistinguishable
/// from not staging it at all. Mesa's meson derives that `aarch64` suffix from `host_machine.cpu()`,
/// which is NOT `uname -m` (`arm64`), so the installed name is discovered — never reassembled.
const ICD_DIR_REL: &str = "third_party/kosmickrisp-install/share/vulkan/icd.d";
const ICD_BASENAME: &str = "kosmickrisp_mesa_icd.aarch64.json";

/// The single path resolver every shell consumer sources, and the variable it exports for the ICD.
///
/// Accepting this indirection in place of the literal path is not a loosening: spelling an
/// out-of-tree path in a script is the exact defect `third_party/` removed. The same paths were
/// duplicated across seven launchers, CI and the gate, so relocating the repo broke the gfxstream
/// build while every script still read correctly. One definition, one override point.
const PATHS_SH: &str = "third_party/paths.sh";
const ICD_VAR: &str = "TP_KOSMICKRISP_ICD";

/// The one command that builds the driver. Supersedes `kosmickrisp/build-kosmickrisp.sh` (now a
/// forwarding shim): the Mesa revision is pinned in `third_party/MANIFEST.sh` and only bootstrap
/// builds from it, which matters because KK_WORKAROUND_15 is required on the macOS we run.
const DRIVER_BOOTSTRAP: &str = "third_party/bootstrap.sh";

/// Every place in the repo that chooses a host Vulkan driver for a gfxstream launch.
///
/// If you add a new way to start the emulator with `--gpu3d`, it belongs in this list. A launcher
/// that is not listed here is a launcher nothing stops from loading a different driver.
const GFXSTREAM_LAUNCHERS: &[&str] = &[
    "crates/aemulator-hvf/run-android.sh",
    "crates/aemulator-hvf/run-android64.sh",
    "crates/aemulator-hvf/run-control64.sh",
    "crates/aemulator-hvf/run-display-selftest.sh",
    "crates/aemulator-hvf/run-linux-gpu3d.sh",
    "crates/aemulator-hvf/gfxstream/run.sh",
    // The Studio's staging + spawn code. Its own unit tests cover the I/O behaviour of staging
    // (that the manifest lands on disk, that a missing one is fatal); what they cannot cover is
    // *which driver* is being staged, because they inject a fabricated path. That is this file's
    // job, and it is why the two do not overlap.
    "aemulator-studio/src-tauri/src/spawn.rs",
];

/// The capability gate. Not a gfxstream launcher — it links the Vulkan loader directly and never
/// sets `ANDROID_EMU_VK_ICD` — but it must drive the *same* manifest, or "the gate passed" says
/// nothing about the driver the emulator loaded.
const CAPABILITY_GATE: &str = "crates/aemulator-hvf/kosmickrisp/run-gate.sh";

/// Host-side code whose only stake in the driver is what it *says* about it: build wiring and
/// operator-facing error text. A revert shows up here as advice to install MoltenVK.
const DRIVER_FACING_SOURCES: &[&str] = &[
    "crates/aemulator-hvf/build.rs",
    "crates/aemulator-hvf/src/machine.rs",
    "aemulator-studio/src-tauri/src/manager.rs",
];

/// Tokens that only ever appear in a file that can actually *reach* MoltenVK.
///
/// Matched case-insensitively against the whole file. Every one of these is load-bearing — a
/// manifest name, a library name, a driver-selection value, or the MoltenVK-only configuration
/// namespace. Deliberately absent: the bare word "moltenvk". The repo is full of it, correctly, in
/// prose that explains *why it is gone* ("MoltenVK is gone; macOS 26+ required", "there is no
/// MoltenVK fallback"), and a guard that forbids naming the thing you removed would only teach
/// people to stop writing the explanation. Same reasoning for `VK_EXT_metal_objects`, which
/// `spawn.rs` names when recording why the capture bridge died.
const MOLTENVK_TOKENS: &[&str] = &[
    // The MoltenVK ICD manifest. Staging this is the whole revert, in one line.
    "moltenvk_icd.json",
    // The driver dylib itself, however it is referenced (`libMoltenVK.dylib`, `-lMoltenVK`).
    "libmoltenvk",
    // MoltenVK's private configuration namespace. `MVK_CONFIG_VK_SEMAPHORE_SUPPORT_STYLE=0` (its
    // single-queue implicit-sync mode) was the one that mattered; any `MVK_CONFIG_*` reappearing
    // means something is being tuned for a driver that is not loaded.
    "mvk_config_",
    // gfxstream's driver selector, set back to MoltenVK — shell form and Rust form.
    "android_emu_vk_icd=moltenvk",
    "\"moltenvk\"",
];

/// `<repo>/crates/aemulator-hvf` → `<repo>`. Compile-time, so the tests do not care about the
/// working directory `cargo test` happened to be invoked from.
fn repo_root() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/aemulator-hvf must sit two levels below the repo root")
        .to_path_buf()
}

/// True when `text` reaches the KosmicKrisp manifest through the single source of truth: either by
/// sourcing `third_party/paths.sh` and using `$TP_KOSMICKRISP_ICD` (every shell consumer — the
/// required form), or by naming the third_party directory *and* basename literally, which is all a
/// consumer that cannot source shell can do (`spawn.rs` is Rust; it mirrors the one constant).
fn resolves_the_kosmickrisp_icd(text: &str) -> bool {
    let via_resolver = text.contains(PATHS_SH) && text.contains(ICD_VAR);
    let literal = text.contains(ICD_DIR_REL) && text.contains(ICD_BASENAME);
    via_resolver || literal
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is pinned as a driver-selection surface but could not be read ({e}). If it moved \
             or was deleted, update the list in src/gfxstream/driver_pin.rs — do not just drop the \
             entry, or nothing checks its replacement.",
            path.display()
        )
    })
}

/// Every gfxstream launch path must name the KosmicKrisp manifest — the same one, in the same
/// place — and must select it through `ANDROID_EMU_VK_ICD`.
///
/// Both halves are required and neither implies the other. `ANDROID_EMU_VK_ICD=kosmickrisp` is what
/// makes gfxstream pick the driver *and* what confines its loader search to
/// `<programDir>/lib64/vulkan`; the manifest path is what `VK_DRIVER_FILES` points the Khronos
/// loader at, which takes strict precedence over the staged copy.
#[test]
fn every_gfxstream_launcher_selects_the_kosmickrisp_icd() {
    for rel in GFXSTREAM_LAUNCHERS {
        let text = read(rel);
        assert!(
            resolves_the_kosmickrisp_icd(&text),
            "{rel} does not resolve the KosmicKrisp manifest, so it is not loading the driver this \
             repo builds. Source {PATHS_SH} and use ${ICD_VAR}; only a consumer that cannot source \
             shell may instead name {ICD_DIR_REL} plus {ICD_BASENAME} itself."
        );
        // Staging a copy into <bindir>/lib64/vulkan is a separate obligation from pointing
        // VK_DRIVER_FILES at the original, and it is the copy's NAME that matters: that directory is
        // where gfxstream's initIcdPaths fallback looks, for one fixed basename.
        if text.contains("lib64/vulkan") {
            assert!(
                text.contains(ICD_BASENAME) || text.contains("TP_KOSMICKRISP_ICD_BASENAME"),
                "{rel} stages into lib64/vulkan under a name of its own making. gfxstream's \
                 initIcdPaths fallback looks for {ICD_BASENAME} there, so any other name stages \
                 nothing — copy to $TP_KOSMICKRISP_ICD_BASENAME, which paths.sh discovers."
            );
        }
        // Shell (`export ANDROID_EMU_VK_ICD=kosmickrisp`) or Rust (`cmd.env(..)`). Matching the
        // exact assignment forms, rather than the loose substring, keeps the many explanatory
        // comments that mention the variable from satisfying the assertion by accident.
        let selects = text.contains("ANDROID_EMU_VK_ICD=kosmickrisp")
            || text.contains("\"ANDROID_EMU_VK_ICD\", \"kosmickrisp\"");
        assert!(
            selects,
            "{rel} never sets ANDROID_EMU_VK_ICD=kosmickrisp. Leaving it empty does not mean \
             'default driver': it changes gfxstream's loader search back to the bare leafname \
             libvulkan.dylib resolved through the process rpath list, which is how a host with \
             MoltenVK still installed silently gets MoltenVK."
        );
    }
}

/// Nothing on a launch path may be able to reach MoltenVK.
///
/// This is not hypothetical tidiness. MoltenVK is still installed on the development machine
/// (`brew molten-vk 1.4.2`); the proof that it is not in use is that `vmmap` of the running VMM
/// shows zero MoltenVK images loaded. A single resurrected `MoltenVK_icd.json` staging line is
/// enough to undo that, and the emulator would still start.
#[test]
fn no_launch_path_can_reach_moltenvk() {
    let scanned = GFXSTREAM_LAUNCHERS
        .iter()
        .chain(std::iter::once(&CAPABILITY_GATE))
        .chain(DRIVER_FACING_SOURCES.iter());
    for rel in scanned {
        let lower = read(rel).to_lowercase();
        for token in MOLTENVK_TOKENS {
            assert!(
                !lower.contains(token),
                "{rel} contains {token:?}. MoltenVK was removed from the render path in Phase 4 \
                 and there is no fallback driver: it does not implement VK_EXT_metal_objects, and \
                 its VK_EXT_external_memory_metal supports only MTLHEAP, so the paths that used to \
                 depend on it no longer exist. If this is deliberate, the migration is being \
                 reversed and this test is the right place to argue about it."
            );
        }
    }
}

/// The capability gate must drive the same manifest the emulator does.
///
/// `run-gate.sh` is the only evidence that the driver can do the three things gfxstream's
/// ColorBuffer path stands on. If it drifts to a different ICD than the launchers use, a passing
/// gate becomes a statement about some other driver on the machine, and the strongest signal in
/// the whole setup quietly stops meaning anything.
#[test]
fn the_capability_gate_drives_the_same_manifest_as_the_emulator() {
    let gate = read(CAPABILITY_GATE);
    assert!(
        resolves_the_kosmickrisp_icd(&gate),
        "{CAPABILITY_GATE} no longer resolves the manifest through {PATHS_SH}/${ICD_VAR}; 'the gate \
         passed' would stop being evidence about the driver the emulator loads"
    );
}

/// Every launcher's missing-driver branch must name the script that builds the driver.
///
/// KosmicKrisp is built from source and is not installable any other way, so "no ICD at <path>" on
/// its own is a dead end. The build has three non-obvious constraints encoded in that script
/// (libclc is mandatory, the python venv must be first on PATH, every compiler pinned to Apple
/// clang), which is exactly why the remediation has to point at it rather than describe itself.
#[test]
fn every_launch_path_names_the_driver_build_script_as_remediation() {
    let scanned = GFXSTREAM_LAUNCHERS
        .iter()
        .chain(std::iter::once(&CAPABILITY_GATE));
    for rel in scanned {
        assert!(
            read(rel).contains(DRIVER_BOOTSTRAP),
            "{rel} can fail on a missing driver without telling anyone how to build it — name \
             {DRIVER_BOOTSTRAP} in the error"
        );
    }
}

/// `ci/hvf-doctor.sh` must keep checking the Vulkan driver, on the same manifest.
///
/// The doctor is the only guard that runs against a real host, and it went a full migration
/// without noticing the driver at all: it checked dyld, the hypervisor entitlement and libslirp on
/// a machine whose Vulkan driver had been swapped out from under it. Pin all four properties of
/// the check that make it worth having, so it cannot be hollowed out back to that state.
#[test]
fn hvf_doctor_still_checks_the_vulkan_driver() {
    let doctor = read("ci/hvf-doctor.sh");
    assert!(
        resolves_the_kosmickrisp_icd(&doctor),
        "ci/hvf-doctor.sh must check the manifest {PATHS_SH} resolves (${ICD_VAR}) — the exact one \
         the launchers load, or it is validating a file nothing uses"
    );
    assert!(
        doctor.contains("library_path"),
        "ci/hvf-doctor.sh must resolve the manifest's library_path and stat the dylib it names. \
         The manifest is a pointer: the Vulkan loader treats an ICD whose library is missing as no \
         ICD at all, silently, so checking only that the JSON exists misses the failure that \
         actually happens after a rebuild into a fresh prefix."
    );
    assert!(
        doctor.contains("sw_vers") && doctor.contains("26"),
        "ci/hvf-doctor.sh must report the macOS major version and state the macOS 26 floor — \
         KosmicKrisp needs it and there is no older-host configuration left"
    );
    assert!(
        doctor.contains(DRIVER_BOOTSTRAP),
        "ci/hvf-doctor.sh must name {DRIVER_BOOTSTRAP} as the remediation; the driver cannot be \
         installed any other way"
    );
}

/// The hand-managed dependency directory that `third_party/` replaced, and that is now deleted.
///
/// Assembled from two halves on purpose: this file is inside the tree the scan below walks, so
/// spelling the token whole here would make the guard report itself.
const DELETED_TREE: &str = concat!("Sample", "Repos");

/// Directories the scan does not descend into.
///
/// `third_party` is the important one: those are foreign checkouts (mesa, gfxstream, aemu) whose own
/// sources may say anything, and they are not ours to police. The rest is build output and fetched
/// fixtures.
const SCAN_SKIP_DIRS: &[&str] = &[
    "third_party",
    "target",
    "node_modules",
    "dist",
    "gen",
    "testdata",
    "screenshots",
    ".git",
    ".venv",
    DELETED_TREE,
];

/// What counts as a launcher / CI / build file: the authored, executed surfaces. Shell for the
/// launchers, CI and bootstrap; Rust for build.rs and the Studio's spawn path; TOML for the cargo
/// manifests; YAML for the CI workflow.
const SCAN_EXTS: &[&str] = &["sh", "rs", "toml", "yml", "yaml"];

fn collect_scan_targets(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !SCAN_SKIP_DIRS.contains(&name.as_ref()) {
                collect_scan_targets(&path, out);
            }
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if SCAN_EXTS.contains(&ext) {
            out.push(path);
        }
    }
}

/// No launcher, CI script or build file may still name the deleted dependency directory.
///
/// This is the regression the whole third_party migration closes, and it is worth a test because of
/// how it fails: a path that no longer exists does not break the build. It breaks a `--gpu3d` launch
/// minutes later, or — the case that actually cost days — it keeps resolving to a stale build tree
/// and the renderer silently stays at an old revision. The five reference-only checkouts
/// (crosvm, libkrun, QEMU, rnidbg, TrinityEmulator) never had a build consumer at all; the three
/// that did are now `third_party/{gfxstream,aemu,mesa}` + `third_party/kosmickrisp-install`.
///
/// Two kinds of file are outside [`SCAN_EXTS`] on purpose, and neither is a hole — neither is
/// executed, so neither can point a build or a launch at a path that is gone:
///   * `*.md` — the status docs describe the old layout deliberately: the relocation that broke the
///     gfxstream build and the patch series that was nearly lost are why `third_party/` exists, and
///     that history is only legible if it can name the directory. Same reasoning as
///     [`MOLTENVK_TOKENS`] not forbidding the bare word "moltenvk".
///   * `gfxstream/build.log` — a captured transcript of a build that really did run out of that
///     directory. Editing it would be falsifying a record, and nothing reads it.
#[test]
fn no_launcher_ci_or_build_file_references_the_deleted_dependency_tree() {
    let root = repo_root();
    let mut files = Vec::new();
    collect_scan_targets(&root, &mut files);
    // A scan that walks nothing passes vacuously; assert it found the repo before trusting it.
    assert!(
        files.len() > 40,
        "the scan reached only {} files under {} — the skip list or the walk is broken, and a \
         vacuous pass here is worse than no test",
        files.len(),
        root.display()
    );

    let offenders: Vec<String> = files
        .iter()
        .filter(|f| std::fs::read_to_string(f).is_ok_and(|t| t.contains(DELETED_TREE)))
        .map(|f| f.strip_prefix(&root).unwrap_or(f).display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "these executed files still point at the deleted {DELETED_TREE} tree: {offenders:?}. \
         Out-of-tree paths come from one place now: source {PATHS_SH} and use its TP_* variables \
         (shell), or mirror a single constant (Rust, which cannot source shell). Run \
         {DRIVER_BOOTSTRAP} to produce the checkouts they resolve to."
    );
}
