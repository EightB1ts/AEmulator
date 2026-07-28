#!/usr/bin/env bash
#
# qemu-hvf-boot.sh — HVF dev-loop + arg-generation proof for AEmulator.
#
# WHAT THIS PROVES
#   1. Our VMM crate's argument generation works:
#      `cargo run -p aemulator-cli -- qemu-args` + `-- dtb` produce a real, coherent
#      qemu-system-aarch64 argument vector and a hand-authored DTB (zero VM-tell strings).
#   2. Hypervisor.framework (HVF) works on this Mac: qemu boots a plain ARM64 Linux
#      near-native under `-accel hvf -cpu host`, reusing the CPU/machine/NIC identity
#      flags our crate emits.
#
# ┌──────────────────────────────────────────────────────────────────────────┐
# │ HONEST NOTE — read before assuming this boots Android:                     │
# │                                                                            │
# │ A bare `qemu-system-aarch64 -M virt` WILL NOT boot a stock Android GSI to  │
# │ a usable UI. Stock GSI expects a specific device model (Cuttlefish's       │
# │ vendor HALs, virtio-gpu/gfxstream, gralloc, input, sensors). Standing up   │
# │ "stealth Android to a driveable UI" is the crosvm/Cuttlefish **Linux/KVM   │
# │ fleet** path (Phase 0 DoD: `aosp_cf_arm64_only_phone` under KVM on a       │
# │ bare-metal ARM host), NOT this script.                                     │
# │                                                                            │
# │ This script is the **CPU/identity dev loop + HVF proof** only. It boots a  │
# │ generic ARM64 Linux (Alpine `virt`) to prove HVF + our arg-gen; it does    │
# │ not — and cannot, on macOS/HVF alone — deliver the full stealth device.    │
# └──────────────────────────────────────────────────────────────────────────┘
#
# USAGE
#   qemu-hvf-boot.sh [deps|args|boot]     (default: boot)
#     deps   brew-install qemu if missing; locate/fetch edk2 aarch64 UEFI firmware.
#     args   generate + print the VMM crate's qemu arg vector and DTB (no boot).
#     boot   deps + args, then boot a plain ARM64 Linux under HVF.
#
# ENV KNOBS
#   AEMU_PROFILE=<path.toml>  DeviceProfile TOML (default: bundled reference profile)
#   AEMU_BOOT_IMAGE=<path>    a local bootable arm64 .iso/.img to boot instead of Alpine
#   AEMU_ALPINE_BRANCH=v3.21  Alpine branch to fetch the virt ISO from (default v3.21)
#   AEMU_MEM_MB=<n>           guest RAM (default 2048 for the Alpine smoke boot)
#   AEMU_SKIP_INSTALL=1       do not run `brew install qemu`
#   AEMU_BOOT_TIMEOUT=<s>     auto-kill the boot after N seconds (default: 0 = interactive)
#
# Exit the interactive serial console with:  Ctrl-A  then  X

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=_common.sh
source "$SCRIPT_DIR/_common.sh"

MODE="${1:-boot}"
PROFILE="${AEMU_PROFILE:-}"
MEM_MB="${AEMU_MEM_MB:-2048}"
ALPINE_BRANCH="${AEMU_ALPINE_BRANCH:-v3.21}"
BOOT_TIMEOUT="${AEMU_BOOT_TIMEOUT:-0}"
mkdir -p "$DOWNLOADS_DIR"

profile_args() { [ -n "$PROFILE" ] && printf -- '--profile\n%s\n' "$PROFILE"; }

# ---------------------------------------------------------------------------
# HVF sanity
# ---------------------------------------------------------------------------
require_hvf() {
    if [ "$(uname -s)" != "Darwin" ]; then
        die "This is the HVF (macOS) dev backend. On Linux use the crosvm/KVM path (see oracle/README.md)."
    fi
    local hv; hv="$(sysctl -n kern.hv_support 2>/dev/null || echo 0)"
    [ "$hv" = "1" ] || die "kern.hv_support != 1 — Hypervisor.framework unavailable on this host."
    ok "kern.hv_support=1 (HVF available)"
    case "$(uname -m)" in
        arm64) ok "host arch arm64 (ARM-on-ARM, near-native)";;
        *) warn "host arch $(uname -m) is not arm64 — HVF ARM guest needs Apple Silicon";;
    esac
}

# ---------------------------------------------------------------------------
# deps: qemu + firmware
# ---------------------------------------------------------------------------
QEMU_BIN=""
EDK2_CODE=""
EDK2_VARS=""

ensure_qemu() {
    if command -v qemu-system-aarch64 >/dev/null 2>&1; then
        QEMU_BIN="$(command -v qemu-system-aarch64)"
        ok "qemu-system-aarch64: $QEMU_BIN ($("$QEMU_BIN" --version | head -1))"
        return 0
    fi
    if [ "${AEMU_SKIP_INSTALL:-0}" = "1" ]; then
        die "qemu-system-aarch64 not found and AEMU_SKIP_INSTALL=1."
    fi
    command -v brew >/dev/null 2>&1 || die "Homebrew not found — install from https://brew.sh, then re-run."
    step "Installing qemu via Homebrew (first run only; ~a few minutes)"
    brew install qemu || die "brew install qemu failed."
    QEMU_BIN="$(command -v qemu-system-aarch64)" || die "qemu still not on PATH after install."
    ok "qemu-system-aarch64: $QEMU_BIN"
}

ensure_firmware() {
    # Prefer the edk2 aarch64 firmware Homebrew's qemu already ships.
    local share=""
    if command -v brew >/dev/null 2>&1; then
        share="$(brew --prefix qemu 2>/dev/null)/share/qemu"
    fi
    local code_candidates=(
        "$share/edk2-aarch64-code.fd"
        "/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
        "/usr/local/share/qemu/edk2-aarch64-code.fd"
    )
    local vars_candidates=(
        "$share/edk2-arm-vars.fd"
        "/opt/homebrew/share/qemu/edk2-arm-vars.fd"
        "/usr/local/share/qemu/edk2-arm-vars.fd"
    )
    local c
    for c in "${code_candidates[@]}"; do [ -f "$c" ] && { EDK2_CODE="$c"; break; }; done
    for c in "${vars_candidates[@]}"; do [ -f "$c" ] && { EDK2_VARS_TEMPLATE="$c"; break; }; done

    if [ -z "$EDK2_CODE" ]; then
        step "Bundled edk2 firmware not found — downloading QEMU_EFI (aarch64 UEFI)"
        EDK2_CODE="$DOWNLOADS_DIR/QEMU_EFI.fd"
        if [ ! -f "$EDK2_CODE" ]; then
            curl -fL --retry 3 -o "$EDK2_CODE" \
                "https://releases.linaro.org/components/kernel/uefi-linaro/latest/release/qemu64/QEMU_EFI.fd" \
                || die "Failed to download QEMU_EFI.fd. Install qemu via brew (ships edk2-aarch64-code.fd)."
        fi
        ok "firmware: $EDK2_CODE (downloaded)"
    else
        ok "firmware (code): $EDK2_CODE"
    fi

    # pflash needs a WRITABLE 64 MiB vars image. Copy the template, or make a blank one.
    EDK2_VARS="$DOWNLOADS_DIR/edk2-vars.fd"
    if [ -n "${EDK2_VARS_TEMPLATE:-}" ] && [ -f "$EDK2_VARS_TEMPLATE" ]; then
        cp -f "$EDK2_VARS_TEMPLATE" "$EDK2_VARS"
    else
        # 64 MiB zero-filled vars store (matches the -code.fd flash size).
        dd if=/dev/zero of="$EDK2_VARS" bs=1m count=64 >/dev/null 2>&1
    fi
    # The pflash backend requires code+vars to each be 64 MiB. Pad into the writable
    # downloads dir (the brew-owned source is read-only).
    local code_padded="$DOWNLOADS_DIR/edk2-aarch64-code.fd"
    pad_to_64m "$code_padded" "$EDK2_CODE"; EDK2_CODE="$code_padded"
    ok "firmware (code, 64MiB): $EDK2_CODE"
    ok "firmware (vars): $EDK2_VARS"
}

# Copy src to dst padded to exactly 64 MiB (idempotent; pflash size requirement).
pad_to_64m() {
    local dst="$1" src="$2"
    local want=$((64*1024*1024))
    local have; have=$(wc -c < "$src" | tr -d ' ')
    if [ "$have" -eq "$want" ]; then cp -f "$src" "$dst"; return; fi
    dd if=/dev/zero of="$dst" bs=1m count=64 >/dev/null 2>&1
    dd if="$src" of="$dst" conv=notrunc >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# args: drive the VMM crate's arg generation
# ---------------------------------------------------------------------------
VMM_ARGS=""          # full generated vector (minus the leading binary name)
generate_vmm_args() {
    step "Generating VMM arg vector + DTB from the aemulator-cli (our crate)"
    command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust (https://rustup.rs)."

    # DTB — hand-authored, zero VM-tell strings.
    local dtb="$DOWNLOADS_DIR/aemulator.dtb"
    ( cd "$REPO_ROOT" && cargo run -q -p aemulator-cli -- $(profile_args) dtb "$dtb" ) \
        || die "cargo run -- dtb failed."
    ok "DTB: $dtb"

    # Full qemu arg vector.
    local line
    line="$(cd "$REPO_ROOT" && cargo run -q -p aemulator-cli -- $(profile_args) qemu-args 2>/dev/null)" \
        || die "cargo run -- qemu-args failed."
    # Strip the leading "qemu-system-aarch64 " the CLI prints.
    VMM_ARGS="${line#qemu-system-aarch64 }"

    echo
    log "── VMM-generated qemu invocation (this is what the crate emits) ──"
    printf '%s\n' "$line" | fold -s -w 100 | sed 's/^/    /'
    echo
    log "Note: the vector uses placeholder Android asset names (kernel.img/system.img/"
    log "vendor.img) + our hand-authored -dtb. Feeding it real GSI/vendor images + KVM"
    log "MIDR overrides is the Phase-1 image-build + fleet work, not this dev loop."
}

# Pull a single-value flag out of the generated vector, e.g. vmm_flag -cpu -> host
vmm_flag() {
    printf '%s' "$VMM_ARGS" | tr ' ' '\n' | grep -A1 -x -- "$1" 2>/dev/null | tail -1
}
# Pull the NIC mac= token our crate emits (real OEM OUI).
vmm_mac() { printf '%s' "$VMM_ARGS" | grep -oE 'mac=[0-9a-fA-F:]+' | head -1 | cut -d= -f2; }

# ---------------------------------------------------------------------------
# boot: plain ARM64 Linux under HVF, reusing the crate's identity flags
# ---------------------------------------------------------------------------
fetch_alpine() {
    local base="https://dl-cdn.alpinelinux.org/alpine/${ALPINE_BRANCH}/releases/aarch64"
    step "Resolving current Alpine virt aarch64 ISO from $base"
    local iso_name
    iso_name="$(curl -fsSL "$base/" 2>/dev/null \
        | grep -oE 'alpine-virt-[0-9][0-9.]*-aarch64\.iso' | sort -V | uniq | tail -1)"
    [ -n "$iso_name" ] || die "Could not list Alpine ISOs at $base (network? branch $ALPINE_BRANCH valid?)."
    local dst="$DOWNLOADS_DIR/$iso_name"
    if [ -f "$dst" ]; then
        ok "Alpine ISO cached: $dst"
    else
        log "Downloading $iso_name"
        curl -fL --retry 3 -o "$dst" "$base/$iso_name" || die "Alpine ISO download failed."
        # Best-effort integrity check.
        if curl -fsSL -o "$dst.sha256" "$base/$iso_name.sha256" 2>/dev/null; then
            ( cd "$DOWNLOADS_DIR" && shasum -a 256 -c "$iso_name.sha256" >/dev/null 2>&1 ) \
                && ok "sha256 verified" || die "sha256 mismatch for $iso_name — refusing to boot."
        else
            warn "No .sha256 published for $iso_name — skipping integrity check."
        fi
    fi
    BOOT_MEDIA="$dst"
}

do_boot() {
    local media="$1"
    local machine cpu smp mac
    machine="$(vmm_flag -machine)"; [ -n "$machine" ] || machine="virt,gic-version=3,highmem=on"
    cpu="$(vmm_flag -cpu)";         [ -n "$cpu" ] || cpu="host"
    smp="$(vmm_flag -smp)";         [ -n "$smp" ] || smp="4"
    mac="$(vmm_mac)";               [ -n "$mac" ] || mac="f0:b4:29:00:00:01"

    step "Booting ARM64 Linux under HVF (Ctrl-A X to quit)"
    log "media=$media"
    log "reusing VMM-emitted identity flags → -machine $machine  -cpu $cpu  -smp $smp  mac=$mac"
    warn "MIDR caveat: -cpu host leaks Apple implementer 0x61 (fatal for stealth). The KVM"
    warn "fleet path clamps MIDR via KVM_SET_ONE_REG; HVF has no such override yet, so this"
    warn "boot is FUNCTIONAL bring-up only, not a stealth-validation run."

    # Attach ISO media as a CD-ROM; a raw/qcow2 disk image as a virtio-blk drive.
    local media_args
    case "$media" in
        *.iso) media_args="-cdrom $media -boot d" ;;
        *)     media_args="-drive file=$media,if=virtio,format=raw" ;;
    esac

    # Boot-plumbing flags below are distro-generic (UEFI + media), NOT emitted by the
    # Android-oriented crate vector: a generic distro boots via firmware-provided DTB,
    # so we deliberately DO NOT pass our -dtb / -kernel / Android -drive placeholders here.
    set -x
    # shellcheck disable=SC2086
    "$QEMU_BIN" \
        -machine "$machine" \
        -accel hvf \
        -cpu "$cpu" \
        -smp "$smp" \
        -m "$MEM_MB" \
        -drive "if=pflash,format=raw,readonly=on,file=$EDK2_CODE" \
        -drive "if=pflash,format=raw,file=$EDK2_VARS" \
        -device virtio-net-pci,netdev=net0,mac="$mac" \
        -netdev user,id=net0 \
        -device virtio-rng-pci \
        $media_args \
        -nographic &
    local qpid=$!
    set +x

    if [ "$BOOT_TIMEOUT" -gt 0 ] 2>/dev/null; then
        ( sleep "$BOOT_TIMEOUT"; kill "$qpid" 2>/dev/null ) &
        local killer=$!
        wait "$qpid" 2>/dev/null || true
        kill "$killer" 2>/dev/null || true
        warn "Auto-killed after ${BOOT_TIMEOUT}s (AEMU_BOOT_TIMEOUT). Alpine virt reaches a login"
        warn "prompt (user 'root', no password) on the serial console within ~15-30s."
    else
        wait "$qpid" 2>/dev/null || true
    fi
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
case "$MODE" in
    deps)
        require_hvf; ensure_qemu; ensure_firmware
        ok "deps ready."
        ;;
    args)
        generate_vmm_args
        ;;
    boot)
        require_hvf
        ensure_qemu
        ensure_firmware
        generate_vmm_args
        if [ -n "${AEMU_BOOT_IMAGE:-}" ]; then
            [ -f "$AEMU_BOOT_IMAGE" ] || die "AEMU_BOOT_IMAGE not found: $AEMU_BOOT_IMAGE"
            BOOT_MEDIA="$AEMU_BOOT_IMAGE"
            ok "Using AEMU_BOOT_IMAGE: $BOOT_MEDIA"
        else
            fetch_alpine
        fi
        do_boot "$BOOT_MEDIA"
        ;;
    *)
        die "Unknown mode '$MODE'. Use: deps | args | boot"
        ;;
esac
