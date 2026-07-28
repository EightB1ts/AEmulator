//! `aemu-cereal-gap` — generate and drift-check `host/vulkan/VkDecoderCerealGap.inc`.
//!
//! gfxstream's generated cereal code can compute the size of 524 `VkStructureType`s but can only
//! *decode* 195 of them; the `default:` arm of `reservedunmarshal_extension_struct` is a literal
//! `abort()` of the whole VMM. The 329-type difference belongs to 166 device extensions, so the fix
//! is to keep those extensions out of `vkEnumerateDeviceExtensionProperties` — the struct then never
//! reaches the wire. See `aemulator_hvf::gfxstream::cereal_gap` for why softening the `abort()` into
//! a skip is not an option.
//!
//! That list is derived, never hand-curated:
//!
//!   # write the strip list the pinned tree implies
//!   aemu-cereal-gap generate third_party/gfxstream \
//!       > third_party/gfxstream/host/vulkan/VkDecoderCerealGap.inc
//!
//!   # fail if the committed .inc no longer matches (this is the drift guard)
//!   aemu-cereal-gap verify third_party/gfxstream \
//!       third_party/gfxstream/host/vulkan/VkDecoderCerealGap.inc
//!
//! `ci/hvf-doctor.sh` runs the second form, so a gfxstream pin bump that moves the codegen fails at
//! doctor time instead of at `abort()` time, minutes into a boot.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aemulator_hvf::gfxstream::cereal_gap::{
    compute_gap, registry_extensions, render_inc, INC_REL_PATH, REGISTRY_REL_PATH,
    SIZE_TABLE_REL_PATH, UNMARSHAL_REL_PATH,
};

const USAGE: &str = "\
usage:
  aemu-cereal-gap generate <gfxstream-src-dir>              # writes the .inc to stdout
  aemu-cereal-gap verify   <gfxstream-src-dir> <inc-path>   # exit 1 if the .inc has drifted
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match refs.as_slice() {
        ["generate", src] => match render(Path::new(src)) {
            Ok(inc) => {
                print!("{inc}");
                ExitCode::SUCCESS
            }
            Err(e) => die(&e),
        },
        ["verify", src, inc_path] => verify(Path::new(src), Path::new(inc_path)),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn die(msg: &str) -> ExitCode {
    eprintln!("aemu-cereal-gap: {msg}");
    ExitCode::FAILURE
}

/// Read the two generated sources out of a gfxstream checkout and render the `.inc`.
fn render(src: &Path) -> Result<String, String> {
    let size = read(&src.join(SIZE_TABLE_REL_PATH))?;
    let unmarshal = read(&src.join(UNMARSHAL_REL_PATH))?;
    let gap = compute_gap(&size, &unmarshal);
    if gap.is_empty() {
        // Zero is not a plausible answer for any real tree: it means the parser found no switch
        // (renamed function, restructured codegen) and silently produced an empty strip list, which
        // would look like "nothing to hide" instead of "we can no longer tell".
        return Err(format!(
            "parsed an EMPTY gap from {}.\n  \
             That is not a plausible result — it means the generated switches were not found or no \
             longer look the way cereal_gap.rs expects (function renamed? codegen restructured?). \
             Refusing to emit a strip list that would silently hide nothing.",
            src.display()
        ));
    }
    // The registry is what tells us which extension names the codegen could POSSIBLY know. An
    // advertised name absent from it aborts in the SIZE table, before the gap is ever consulted.
    let registry = registry_extensions(&read(&src.join(REGISTRY_REL_PATH))?);
    if registry.is_empty() {
        return Err(format!(
            "parsed ZERO extensions from {}.\n  \
             Refusing to emit a registry whitelist that would hide every extension the host \
             advertises.",
            src.join(REGISTRY_REL_PATH).display()
        ));
    }
    Ok(render_inc(&gap, &gfxstream_sha(src), &registry))
}

fn verify(src: &Path, inc_path: &Path) -> ExitCode {
    let expected = match render(src) {
        Ok(s) => s,
        Err(e) => return die(&e),
    };
    let actual = match read(inc_path) {
        Ok(s) => s,
        Err(e) => {
            return die(&format!(
                "{e}\n  The generated strip list is missing. It is carried as a new-file hunk in \
                 third_party/patches/gfxstream/ — is the patch series applied? \
                 (third_party/bootstrap.sh gfxstream)"
            ))
        }
    };
    if actual == expected {
        let lines = expected.lines().count();
        eprintln!(
            "aemu-cereal-gap: {} is up to date ({lines} lines) for {}",
            inc_path.display(),
            gfxstream_sha(src)
        );
        return ExitCode::SUCCESS;
    }

    // Name the first difference: "the file differs" alone sends the reader to a diff tool for
    // something this already knows.
    let mut first = None;
    for (i, (a, b)) in actual.lines().zip(expected.lines()).enumerate() {
        if a != b {
            first = Some((i + 1, a.to_string(), b.to_string()));
            break;
        }
    }
    let detail = match first {
        Some((n, a, b)) => format!("first difference at line {n}:\n    have: {a}\n    want: {b}"),
        None => format!(
            "the files share a common prefix but differ in length: have {} lines, want {} lines",
            actual.lines().count(),
            expected.lines().count()
        ),
    };
    die(&format!(
        "{} does NOT match what {}'s codegen implies.\n  {detail}\n  \
         Either the gfxstream pin moved (regenerate) or the .inc was hand-edited (it says DO NOT \
         EDIT for this reason). Regenerate and re-export the patch series:\n    \
         cargo run --manifest-path crates/aemulator-hvf/Cargo.toml --bin aemu-cereal-gap -- \\\n      \
         generate {} > {}/{}",
        inc_path.display(),
        src.display(),
        src.display(),
        src.display(),
        INC_REL_PATH,
    ))
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))
}

/// The gfxstream revision the `.inc` is generated from, for its header comment.
///
/// `third_party/MANIFEST.sh` is the repo's single source of truth for the pin, so it is preferred;
/// `bootstrap.sh` guarantees the checkout is at that SHA. Falls back to asking git, which is what
/// happens when `GFXSTREAM_SRC` points at a checkout outside `third_party/`.
fn gfxstream_sha(src: &Path) -> String {
    if let Some(sha) = manifest_sha(&src.join("../MANIFEST.sh")) {
        return sha;
    }
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(src)
        .args(["rev-parse", "HEAD"])
        .output()
    {
        if out.status.success() {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !sha.is_empty() {
                return sha;
            }
        }
    }
    "unknown".to_string()
}

fn manifest_sha(manifest: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("TP_GFXSTREAM_SHA=") else {
            continue;
        };
        let sha = rest.trim().trim_matches('"').trim_matches('\'');
        if !sha.is_empty() {
            return Some(sha.to_string());
        }
    }
    None
}
