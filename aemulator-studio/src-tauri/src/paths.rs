//! Filesystem resolution: locate the `aemulator-hvf` crate (its signed binary,
//! entitlements, and base-image `testdata/` dir) and the per-VM instance dirs.
//!
//! The Studio is a STANDALONE crate that spawns the emulator binary as a child,
//! so at runtime it must find that binary on disk. Resolution order:
//!   1. `AEMU_HVF_DIR` env override (an absolute path to the aemulator-hvf crate).
//!   2. Walk the ancestors of the current exe looking for `crates/aemulator-hvf`.
//!   3. Walk the ancestors of the CWD for the same.
//!
//! In `tauri dev` the exe lives at
//! `AEmulator/aemulator-studio/src-tauri/target/debug/aemulator-studio`, whose
//! ancestor `AEmulator/` contains `crates/aemulator-hvf` — so dev resolves with
//! zero config. A relocated bundle sets `AEMU_HVF_DIR`.

use std::path::{Path, PathBuf};

/// The aemulator-hvf crate dir, relative to a repo root that contains `crates/`.
const HVF_REL: &str = "crates/aemulator-hvf";

/// Resolve the aemulator-hvf crate directory, or an error string if not found.
pub fn hvf_dir() -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var("AEMU_HVF_DIR") {
        let p = PathBuf::from(v);
        if p.join("Cargo.toml").exists() {
            return Ok(p);
        }
        return Err(format!(
            "AEMU_HVF_DIR={} does not contain a Cargo.toml",
            p.display()
        ));
    }

    // Search anchors: the current exe path and the CWD.
    let mut anchors: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        anchors.push(exe);
    }
    if let Ok(cwd) = std::env::current_dir() {
        anchors.push(cwd);
    }

    for anchor in &anchors {
        for base in anchor.ancestors() {
            let cand = base.join(HVF_REL);
            if cand.join("Cargo.toml").exists() {
                return Ok(cand);
            }
        }
    }

    Err(format!(
        "could not locate {HVF_REL} from the exe or CWD — set AEMU_HVF_DIR to the aemulator-hvf crate path"
    ))
}

/// The pre-built emulator binary — `<hvf>/target/{release,debug}/aemulator-hvf`.
///
/// **Prefers `release`**, falling back to `debug`. Release is not a nicety here: the per-frame work
/// is scalar pixel loops (Metal readback swizzle, BGRA→RGBA, the scanout scaler) and an unoptimized
/// build is many times slower — a debug VMM cannot feed a 60fps viewer, so "runs at all" and "runs
/// at the frame rate this app is built around" are different binaries.
///
/// This deliberately does NOT pick the newer of the two. That heuristic looks helpful and is a trap:
/// rebuilding debug for any reason silently downgrades every subsequent launch, and because both
/// binaries work, the only symptom is that the picture got slow. Explicit beats fresh. Override with
/// `AEMU_HVF_PROFILE=debug` when you actually want to run a debug VMM (lldb, a `dbg!`, etc.).
///
/// (`spawn::ensure_signed` re-signs the chosen binary on every launch, which rewrites its mtime —
/// another reason mtime is a bad signal here.)
pub fn hvf_binary() -> Result<PathBuf, String> {
    let dir = hvf_dir()?;
    let candidate = |profile: &str| dir.join(format!("target/{profile}/aemulator-hvf"));

    if let Some(pin) = std::env::var_os("AEMU_HVF_PROFILE") {
        let pin = pin.to_string_lossy().to_string();
        let p = candidate(&pin);
        return if p.exists() {
            println!(
                "[studio] emulator binary (AEMU_HVF_PROFILE): {}",
                p.display()
            );
            Ok(p)
        } else {
            Err(format!(
                "AEMU_HVF_PROFILE={pin} but {} does not exist",
                p.display()
            ))
        };
    }

    for profile in ["release", "debug"] {
        let p = candidate(profile);
        if p.exists() {
            if profile == "debug" {
                eprintln!(
                    "[studio] WARNING: using the DEBUG emulator binary — expect a slow picture. \
                     Build the fast one: cargo build --release --manifest-path {}/Cargo.toml \
                     --features net,gfxstream --bin aemulator-hvf",
                    dir.display()
                );
            }
            println!("[studio] emulator binary: {}", p.display());
            return Ok(p);
        }
    }
    Err(format!(
        "emulator binary not found under {}/target/{{release,debug}} — build it first: cargo build --release --manifest-path {}/Cargo.toml --features net,gfxstream --bin aemulator-hvf",
        dir.display(),
        dir.display()
    ))
}

/// The ad-hoc hypervisor entitlements the binary must be signed with.
pub fn hv_entitlements() -> Result<PathBuf, String> {
    let p = hvf_dir()?.join("hv.entitlements");
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("hv.entitlements not found at {}", p.display()))
    }
}

/// A base image directory, e.g. key `cf64` → `<hvf>/testdata/cf64`.
pub fn base_image_dir(key: &str) -> Result<PathBuf, String> {
    Ok(hvf_dir()?.join("testdata").join(key))
}

/// The `testdata/` root that holds base images.
pub fn testdata_dir() -> Result<PathBuf, String> {
    Ok(hvf_dir()?.join("testdata"))
}

/// Does a base-image dir look bootable (has an `img/` with the composite parts)?
pub fn is_bootable_image(dir: &Path) -> bool {
    dir.join("img").join("super.raw.img").exists() || dir.join("img").join("super.img").exists()
}
