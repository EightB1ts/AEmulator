//! Link against Apple's Hypervisor.framework — but ONLY on macOS.
//!
//! Guarding the link directive on the target OS keeps a hypothetical non-macOS build (e.g. a Linux
//! CI runner) from failing on a missing framework. On macOS this emits
//! `-framework Hypervisor` so the `extern "C"` symbols in `src/lib.rs` resolve.
use std::path::PathBuf;

/// Resolve the Phase 3a host-renderer build dir (holds `libgfxstream_backend.dylib`).
///
/// NEVER hardcode an absolute path here. This tree gets relocated (it is a subtree of the
/// avnir-beta monorepo), and a stale constant is uniquely nasty: the dir only has to exist at
/// BUILD time for the `exists()` check below, but the same string is also baked in as an `-rpath`.
/// Move the tree and you still get a green build — then a `dyld: Library not loaded` crash at
/// launch, with nothing in the build output pointing at the cause.
///
/// Resolution order:
///   1. `$GFXSTREAM_BUILD_HOST` — explicit override (CI, or a renderer built out-of-tree). This is
///      the same variable `third_party/paths.sh` honours, so one export steers build and run alike.
///   2. `<repo-root>/third_party/gfxstream/build-host` — what `third_party/bootstrap.sh` produces,
///      derived from `CARGO_MANIFEST_DIR` so it follows the tree wherever it moves.
///
/// Every other consumer resolves this by sourcing `third_party/paths.sh`; cargo runs build.rs
/// directly, so this is the one place the default is spelled twice. It must track
/// `TP_GFXSTREAM_BUILD_HOST`.
fn gfxstream_build_host() -> PathBuf {
    if let Some(dir) = std::env::var_os("GFXSTREAM_BUILD_HOST") {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );
    // <repo-root>/crates/aemulator-hvf → <repo-root>
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("aemulator-hvf must live at <repo-root>/crates/aemulator-hvf")
        .join("third_party/gfxstream/build-host")
}

/// Homebrew lib dir — where the Vulkan **loader** (`libvulkan.dylib`) lives at runtime. Asked of
/// `brew` rather than hardcoded so this resolves on Intel (`/usr/local`) as well as Apple Silicon.
///
/// The loader is the only Vulkan component that comes from Homebrew. The ICD behind it is Mesa
/// KosmicKrisp, which is built from source (`third_party/bootstrap.sh mesa`), never linked, and
/// selected at run time through `VK_DRIVER_FILES` — so nothing about the driver is a build-time
/// concern here.
fn brew_lib() -> String {
    brew_prefix_root()
        .map(|p| format!("{p}/lib"))
        .unwrap_or_else(|| "/opt/homebrew/lib".to_string())
}

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Hypervisor");
    }

    // Networking: compile the libslirp bridge shim and link libslirp (user-mode NAT for virtio-net).
    // UNCONDITIONAL — see the note in Cargo.toml. Homebrew libslirp: headers under
    // <prefix>/include/slirp, dylib under <prefix>/lib (glib is pulled transitively via
    // libslirp.dylib's own install-name dependency).
    if target_os == "macos" {
        let slirp_prefix =
            brew_prefix("libslirp").unwrap_or_else(|| "/opt/homebrew/opt/libslirp".to_string());
        if !std::path::Path::new(&format!("{slirp_prefix}/lib")).exists() {
            panic!(
                "libslirp not found at {slirp_prefix} — networking is a hard requirement of this \
                 VMM, not an optional feature. Install it:\n  brew install libslirp"
            );
        }
        println!("cargo:rerun-if-changed=src/net/slirp_shim.c");
        cc::Build::new()
            .file("src/net/slirp_shim.c")
            .include(format!("{slirp_prefix}/include"))
            .compile("aemu_slirp_shim");
        println!("cargo:rustc-link-search=native={slirp_prefix}/lib");
        println!("cargo:rustc-link-lib=dylib=slirp");
        // The dylib's install_name is absolute (<prefix>/lib/libslirp.0.dylib) so it resolves at
        // run time without an rpath; add one anyway for robustness against a relocated cellar.
        println!("cargo:rustc-link-arg=-Wl,-rpath,{slirp_prefix}/lib");
    }

    // Everything below is the GPU path (gfxstream → Vulkan/KosmicKrisp → Metal). UNCONDITIONAL on macOS — see
    // the note in Cargo.toml. Only a non-macOS target skips it, since none of these frameworks exist
    // there and the crate cannot run on one anyway.
    if target_os != "macos" {
        return;
    }
    println!("cargo:rerun-if-env-changed=GFXSTREAM_BUILD_HOST");
    let gfxstream_build_host = gfxstream_build_host();
    let gfxstream_build_host = gfxstream_build_host.display();
    if !std::path::Path::new(&gfxstream_build_host.to_string())
        .join("libgfxstream_backend.dylib")
        .exists()
    {
        panic!(
            "libgfxstream_backend.dylib not found in {gfxstream_build_host} — the GPU path is a hard \
             requirement of this VMM, not an optional feature. Build the host renderer first:\n  \
             third_party/bootstrap.sh gfxstream\n\
             (clones the pinned revision, applies our tracked patch series, and verifies the dylib \
             exports the present path). Or point GFXSTREAM_BUILD_HOST at an existing build."
        );
    }

    // 1. Compile the Objective-C CAMetalLayer presenter shim (ARC).
    println!("cargo:rerun-if-changed=src/display/metal_shim.m");
    cc::Build::new()
        .file("src/display/metal_shim.m")
        .flag("-fobjc-arc")
        .compile("aemu_metal_shim");

    // 2. Frameworks the shim uses (Metal present path) + the AppKit window it messages.
    println!("cargo:rustc-link-lib=framework=Metal");
    println!("cargo:rustc-link-lib=framework=QuartzCore");
    println!("cargo:rustc-link-lib=framework=AppKit");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // 3. Link the gfxstream host renderer dylib (install_name is @rpath/…), with an rpath so it
    //    resolves at run time, plus an rpath to the Homebrew lib dir for the Vulkan loader.
    //
    //    The Homebrew rpath is NOT dead weight even though nothing links libvulkan.dylib (it is
    //    absent from `otool -L` on both this binary and libgfxstream_backend.dylib): gfxstream
    //    `dlopen`s the loader, and `VulkanDispatch::getPossibleLoaderPaths()` returns the bare
    //    leafname "libvulkan.dylib" whenever ANDROID_EMU_VK_ICD is EMPTY — a leafname dyld resolves
    //    through this process's rpath list. Every run script sets ANDROID_EMU_VK_ICD=kosmickrisp,
    //    which switches that search to <programDir>/lib64/vulkan instead (hence the staged loader
    //    symlink there), so this rpath is what keeps a bare `cargo run` — no run script, no env —
    //    finding a loader at all. The ICD it then loads is Mesa KosmicKrisp, not MoltenVK.
    println!("cargo:rustc-link-search=native={gfxstream_build_host}");
    println!("cargo:rustc-link-lib=dylib=gfxstream_backend");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{gfxstream_build_host}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", brew_lib());
}

/// `brew --prefix` (the Homebrew root, no formula). `None` if brew is absent.
fn brew_prefix_root() -> Option<String> {
    let out = std::process::Command::new("brew")
        .arg("--prefix")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Resolve `brew --prefix <formula>` at build time (so the shim finds libslirp headers/lib wherever
/// Homebrew placed them). Returns `None` if brew is absent or the formula isn't installed.
fn brew_prefix(formula: &str) -> Option<String> {
    let out = std::process::Command::new("brew")
        .args(["--prefix", formula])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
