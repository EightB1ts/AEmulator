//! Host capability probing — REAL, best-effort, macOS-native. Nothing here
//! fabricates a value: every field is sysctl / vm_stat / codesign / file-stat,
//! with a documented fallback when a probe is unavailable.

use std::path::Path;
use std::process::Command;

use crate::paths;
use crate::types::{HostCaps, ImageCatalogEntry};

/// The per-VM RAM floor (MiB) — Cuttlefish needs ~4 GiB to clear init reliably.
pub const VM_RAM_FLOOR_MB: u32 = 4096;

/// Fraction of host RAM we allow across all running VMs before refusing a start.
const RAM_BUDGET_FRACTION: f64 = 0.6;

/// Total physical RAM in MiB via `sysctl -n hw.memsize` (bytes). Fallback 16 GiB.
pub fn total_ram_mb() -> u32 {
    run_stdout("sysctl", &["-n", "hw.memsize"])
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| (bytes / (1024 * 1024)) as u32)
        .unwrap_or(16_384)
}

/// Approximate free RAM (MiB) from `vm_stat`: (free + inactive + speculative)
/// pages × page size. This is the reclaimable-memory view Activity Monitor shows
/// as "available". Fallback: half of total.
pub fn free_ram_mb() -> u32 {
    if let Some(out) = run_stdout("vm_stat", &[]) {
        let page = parse_vm_stat_page_size(&out).unwrap_or(16384);
        let free = parse_vm_stat_field(&out, "Pages free:").unwrap_or(0);
        let inactive = parse_vm_stat_field(&out, "Pages inactive:").unwrap_or(0);
        let spec = parse_vm_stat_field(&out, "Pages speculative:").unwrap_or(0);
        let bytes = (free + inactive + spec) * page;
        return (bytes / (1024 * 1024)) as u32;
    }
    total_ram_mb() / 2
}

fn parse_vm_stat_page_size(out: &str) -> Option<u64> {
    // Header: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let marker = "page size of ";
    let i = out.find(marker)? + marker.len();
    let rest = &out[i..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn parse_vm_stat_field(out: &str, label: &str) -> Option<u64> {
    let line = out.lines().find(|l| l.trim_start().starts_with(label))?;
    let val = line.split(label).nth(1)?.trim().trim_end_matches('.');
    val.parse().ok()
}

/// Is `libslirp` present (the run scripts need `brew install libslirp` for the
/// `--features net` NAT backend)? Proxy: the dylib exists under Homebrew.
pub fn libslirp_present() -> bool {
    ["/opt/homebrew/lib", "/usr/local/lib"].iter().any(|d| {
        let dir = Path::new(d);
        dir.join("libslirp.dylib").exists()
            || dir.join("libslirp.4.dylib").exists()
            || dir.join("libslirp.0.dylib").exists()
    })
}

/// Is the emulator binary present AND ad-hoc signed with the hypervisor
/// entitlement? `codesign -d --entitlements :- <bin>` prints the entitlements
/// plist; we look for `com.apple.security.hypervisor`.
pub fn binary_signed() -> bool {
    let Ok(bin) = paths::hvf_binary() else {
        return false;
    };
    let out = Command::new("codesign")
        .args(["-d", "--entitlements", ":-", &bin.display().to_string()])
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            text.contains("hypervisor") || err.contains("hypervisor")
        }
        Err(_) => false,
    }
}

/// The RAM budget cap (MiB) across all running VMs.
pub fn ram_budget_mb() -> u32 {
    ((total_ram_mb() as f64) * RAM_BUDGET_FRACTION) as u32
}

/// Assemble the real host-capabilities snapshot.
pub fn capabilities() -> HostCaps {
    let total = total_ram_mb();
    HostCaps {
        total_ram_mb: total,
        free_ram_mb: free_ram_mb(),
        libslirp: libslirp_present(),
        binary_signed: binary_signed(),
        max_vms: (ram_budget_mb() / VM_RAM_FLOOR_MB).max(1),
    }
}

/// Scan `testdata/` for bootable base images; report each with its logical size.
pub fn base_images() -> Vec<ImageCatalogEntry> {
    let mut out = Vec::new();
    let Ok(testdata) = paths::testdata_dir() else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&testdata) else {
        // testdata missing → report the canonical cf64 as not-present.
        return vec![ImageCatalogEntry {
            key: "cf64".into(),
            path: testdata.join("cf64").display().to_string(),
            logical_bytes: 0,
            present: false,
        }];
    };
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let bootable = paths::is_bootable_image(&dir);
        let key = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let logical = if bootable {
            dir_logical_bytes(&dir.join("img"))
        } else {
            0
        };
        out.push(ImageCatalogEntry {
            key,
            path: dir.display().to_string(),
            logical_bytes: logical,
            present: bootable,
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    if out.is_empty() {
        out.push(ImageCatalogEntry {
            key: "cf64".into(),
            path: testdata.join("cf64").display().to_string(),
            logical_bytes: 0,
            present: false,
        });
    }
    out
}

/// Sum of the apparent (logical) file sizes directly under `dir`.
pub fn dir_logical_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// On-disk (actual block) usage of `dir` in bytes via `du -sk` (KiB). This is
/// the COW-aware number — a clonefile'd image reports only its private delta.
pub fn dir_on_disk_bytes(dir: &Path) -> u64 {
    run_stdout("du", &["-sk", &dir.display().to_string()])
        .and_then(|s| s.split_whitespace().next().map(|t| t.to_string()))
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib * 1024)
        .unwrap_or(0)
}

fn run_stdout(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}
