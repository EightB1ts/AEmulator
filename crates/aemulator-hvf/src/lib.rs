//! `aemulator-hvf` — a thin, honest Rust binding to Apple's Hypervisor.framework (HVF) for
//! aarch64, plus the primitives the AEmulator VMM needs to stand up a guest vCPU on Apple Silicon.
//!
//! This is a *skeleton*: raw `extern "C"` FFI (no `applevisor`/`libc` dependency, so it builds
//! fully offline) with a small set of `#[repr(C)]` structs and constants transcribed **verbatim**
//! from the macOS 27.0 SDK headers:
//!
//!   * `Hypervisor.framework/Headers/hv_vcpu.h`         — function signatures
//!   * `Hypervisor.framework/Headers/hv_vcpu_types.h`   — `hv_sys_reg_t`, `hv_reg_t`, exit structs
//!   * `Hypervisor.framework/Headers/hv_vm.h`           — `hv_vm_create` / `hv_vm_map`
//!   * `usr/include/arm64/hv/hv_kern_types.h`           — `hv_return_t` codes + `hv_memory_flags_t`
//!
//! Everything is `#[cfg(target_os = "macos")]`. On any other OS the crate compiles to (almost)
//! nothing so it can never break a cross-platform build; the crate is additionally kept out of the
//! workspace via `[workspace].exclude`.
//!
//! # Why this crate exists
//! The make-or-break question for a *stealth* ARM64 Android on this host is whether HVF lets the
//! VMM lie to the guest about the CPU identity register `MIDR_EL1`. HVF exposes `MIDR_EL1` as a
//! settable system register (`HV_SYS_REG_MIDR_EL1 = 0xc000`), which on Apple Silicon is backed by
//! the hardware `VPIDR_EL2` virtualization register the host EL2 hypervisor programs — so in
//! principle a guest `MRS x0, MIDR_EL1` at EL1 returns whatever the VMM set. `hvf-midr-probe`
//! tests that *empirically* rather than trusting the header. See that binary for the experiment.

#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)]

use std::os::raw::c_void;

// VMM core modules (the `run-linux` path). Each is macOS/aarch64 and depends on the FFI above.
pub mod boot;
/// Single-writer lock on a cf-dir — two VMMs on one instance corrupt the guest filesystem.
pub mod cf_lock;
/// Phase 6: the host-driven **control plane** (framebuffer capture + virtio-input injection exposed
/// as a scriptable line protocol over a Unix socket / stdin REPL). No adb, no in-guest a11y service.
#[cfg(target_os = "macos")]
pub mod control;
#[cfg(target_os = "macos")]
pub mod display;
pub mod gic;
pub mod machine;
pub mod memory;
pub mod mmio;
/// Host-side software `secure_env` (KeyMint/Gatekeeper/OemLock) reached over the HAL HVC channels.
pub mod secure_env;
pub mod vcpu;
pub mod virtio;

// Host user-mode NAT networking backend (libslirp) for virtio-net. Behind the `net` feature so the
// default build never links libslirp/glib.
pub mod net;

// gfxstream host-renderer C ABI bindings (Phase 3a). Bindings only — NOT wired into the VMM yet
// (that is Phase 3c). Behind the `gfxstream` feature so the default build is unaffected.
pub mod gfxstream;

/// `hv_return_t` is `mach_error_t`, a 32-bit signed int. We treat it as a raw code and format it
/// as unsigned hex to match the header comments (e.g. `HV_DENIED == 0xfae94007`).
pub type HvReturn = i32;

// ---------------------------------------------------------------------------------------------
// hv_return_t codes — arm64/hv/hv_kern_types.h (the arm64 copy; hv_error.h is x86-only!)
// ---------------------------------------------------------------------------------------------
pub const HV_SUCCESS: HvReturn = 0;
pub const HV_ERROR: u32 = 0xfae9_4001;
pub const HV_BUSY: u32 = 0xfae9_4002;
pub const HV_BAD_ARGUMENT: u32 = 0xfae9_4003;
pub const HV_ILLEGAL_GUEST_STATE: u32 = 0xfae9_4004;
pub const HV_NO_RESOURCES: u32 = 0xfae9_4005;
pub const HV_NO_DEVICE: u32 = 0xfae9_4006;
pub const HV_DENIED: u32 = 0xfae9_4007;
pub const HV_EXISTS: u32 = 0xfae9_4008;
pub const HV_UNSUPPORTED: u32 = 0xfae9_400f;

/// Human-readable name for an `hv_return_t`, for honest error reporting.
pub fn hv_return_name(code: HvReturn) -> &'static str {
    match code as u32 {
        0 => "HV_SUCCESS",
        0xfae9_4001 => "HV_ERROR",
        0xfae9_4002 => "HV_BUSY",
        0xfae9_4003 => "HV_BAD_ARGUMENT",
        0xfae9_4004 => "HV_ILLEGAL_GUEST_STATE",
        0xfae9_4005 => "HV_NO_RESOURCES",
        0xfae9_4006 => "HV_NO_DEVICE",
        0xfae9_4007 => "HV_DENIED",
        0xfae9_4008 => "HV_EXISTS",
        0xfae9_400f => "HV_UNSUPPORTED",
        _ => "HV_<unknown>",
    }
}

// ---------------------------------------------------------------------------------------------
// hv_memory_flags_t — arm64/hv/hv_kern_types.h
// ---------------------------------------------------------------------------------------------
pub const HV_MEMORY_READ: u64 = 1 << 0;
pub const HV_MEMORY_WRITE: u64 = 1 << 1;
pub const HV_MEMORY_EXEC: u64 = 1 << 2;

// ---------------------------------------------------------------------------------------------
// hv_reg_t — hv_vcpu_types.h (uint32_t enum). Index order is X0..X30, then PC/FPCR/FPSR/CPSR.
// ---------------------------------------------------------------------------------------------
pub const HV_REG_X0: u32 = 0;
pub const HV_REG_X1: u32 = 1;
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_CPSR: u32 = 34;

// ---------------------------------------------------------------------------------------------
// hv_sys_reg_t — hv_vcpu_types.h (uint16_t enum). Only the ones we touch.
// ---------------------------------------------------------------------------------------------
pub const HV_SYS_REG_MIDR_EL1: u16 = 0xc000;
pub const HV_SYS_REG_MPIDR_EL1: u16 = 0xc005;
/// Virtual timer control register `CNTV_CTL_EL0` (used to decide WFI parking + vtimer sync).
pub const HV_SYS_REG_CNTV_CTL_EL0: u16 = 0xdf19;
/// Virtual timer compare value `CNTV_CVAL_EL0`.
pub const HV_SYS_REG_CNTV_CVAL_EL0: u16 = 0xdf1a;

// ---------------------------------------------------------------------------------------------
// hv_interrupt_type_t — hv_vcpu_types.h (uint32_t enum). The virtual IRQ/FIQ lines HVF asserts
// into the guest. Pending state is CLEARED on every `hv_vcpu_run`, so an active line must be
// re-asserted before each run (this is how the userspace GIC drives the guest — see `vcpu::run`).
// ---------------------------------------------------------------------------------------------
pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;
pub const HV_INTERRUPT_TYPE_FIQ: u32 = 1;

// ---------------------------------------------------------------------------------------------
// hv_exit_reason_t — hv_vcpu_types.h (uint32_t enum, declaration order).
// ---------------------------------------------------------------------------------------------
pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

/// `hv_vcpu_exit_exception_t` — hv_vcpu_types.h.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HvVcpuExitException {
    pub syndrome: u64,        // ESR_ELx
    pub virtual_address: u64, // FAR_ELx
    pub physical_address: u64,
}

/// `hv_vcpu_exit_t` — hv_vcpu_types.h. `reason` is a u32; the following u64 fields force 8-byte
/// alignment, so `#[repr(C)]` inserts the same 4 bytes of padding the C compiler does.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HvVcpuExit {
    pub reason: u32,
    pub exception: HvVcpuExitException,
}

/// Extract the Exception Class (ESR_ELx.EC, bits [31:26]) from an exception syndrome.
#[inline]
pub fn esr_ec(syndrome: u64) -> u64 {
    (syndrome >> 26) & 0x3f
}

/// ESR EC for an `HVC` executed in AArch64 state (0b010110). This is how the guest signals a
/// synchronous exit to the VMM in the default (EL1-only) HVF configuration.
pub const EC_AA64_HVC: u64 = 0x16;

// ---------------------------------------------------------------------------------------------
// ESR_ELx.EC exception classes the VMM run loop decodes (ARM ARM D17.2.37, values verbatim).
// ---------------------------------------------------------------------------------------------
/// WFI/WFE trapped (0b000001).
pub const EC_WFX_TRAP: u64 = 0x01;
/// AArch64 SMC (0b010111).
pub const EC_AA64_SMC: u64 = 0x17;
/// Trapped MSR/MRS/system-register access (0b011000).
pub const EC_SYSTEMREGISTERTRAP: u64 = 0x18;
/// Data abort from a lower EL (0b100100) — this is the MMIO exit for a stage-2 fault.
pub const EC_DATAABORT_LOWER: u64 = 0x24;
/// AArch64 breakpoint (BRK) (0b111100).
pub const EC_AA64_BKPT: u64 = 0x3c;

// ---------------------------------------------------------------------------------------------
// Additional hv_reg_t indices (hv_vcpu_types.h). X0..X30 are 0..30 in order.
// ---------------------------------------------------------------------------------------------
pub const HV_REG_X2: u32 = 2;
pub const HV_REG_X3: u32 = 3;
pub const HV_REG_FP: u32 = 29;
pub const HV_REG_LR: u32 = 30;

/// hv_reg_t for general register `Xn` (0..=30). X0 is index 0, so the index *is* `n`.
#[inline]
pub fn hv_reg_x(n: u32) -> u32 {
    debug_assert!(n <= 30);
    n
}

// ---------------------------------------------------------------------------------------------
// Raw FFI — Hypervisor.framework. Signatures transcribed from hv_vm.h / hv_vcpu.h.
//   hv_vcpu_t   = uint64_t
//   hv_reg_t    = uint32_t
//   hv_sys_reg_t= uint16_t
//   hv_return_t = mach_error_t (int)
// ---------------------------------------------------------------------------------------------
extern "C" {
    pub fn hv_vm_create(config: *mut c_void) -> HvReturn;
    pub fn hv_vm_destroy() -> HvReturn;
    pub fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> HvReturn;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> HvReturn;

    pub fn hv_vcpu_create(
        vcpu: *mut u64,
        exit: *mut *mut HvVcpuExit,
        config: *mut c_void,
    ) -> HvReturn;
    pub fn hv_vcpu_destroy(vcpu: u64) -> HvReturn;
    pub fn hv_vcpu_run(vcpu: u64) -> HvReturn;

    pub fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> HvReturn;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> HvReturn;
    pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> HvReturn;
    pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> HvReturn;

    /// Assert/deassert a virtual interrupt line (IRQ/FIQ) into the guest. Pending state is
    /// cleared on every `hv_vcpu_run`, so a persistently-asserted line is re-set before each run.
    pub fn hv_vcpu_set_pending_interrupt(vcpu: u64, itype: u32, pending: bool) -> HvReturn;
    /// Mask/unmask HVF's virtual timer. HVF auto-masks the vtimer when it fires
    /// (`HV_EXIT_REASON_VTIMER_ACTIVATED`); we unmask it again once the guest has serviced the
    /// tick so the next deadline can raise a fresh exit.
    pub fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> HvReturn;
    /// Force one or more vCPUs out of `hv_vcpu_run` (used to kick a sibling vCPU so it observes a
    /// freshly-queued cross-core interrupt / SGI).
    pub fn hv_vcpus_exit(vcpus: *mut u64, count: u32) -> HvReturn;
}

extern "C" {
    fn getpagesize() -> std::os::raw::c_int;
    /// Monotonic host tick counter. On Apple Silicon this shares the guest's `CNTVCT` timebase, so
    /// it can be compared directly against `CNTV_CVAL_EL0` to size a WFI park.
    pub fn mach_absolute_time() -> u64;
}

/// Host page size (16 KiB on Apple Silicon). HVF requires `hv_vm_map` host addr / IPA / size to be
/// aligned to this granule.
pub fn host_page_size() -> usize {
    // Safety: getpagesize() is a pure libc call with no preconditions.
    unsafe { getpagesize() as usize }
}

/// Encoded aarch64 instructions for the probe stub, verified against the macOS `clang` assembler:
///   `mrs x0, MIDR_EL1` == 0xd538_0000
///   `str x0, [x1]`     == 0xf900_0020
///   `hvc #0`           == 0xd400_0002
pub mod stub {
    pub const MRS_X0_MIDR_EL1: u32 = 0xd538_0000;
    pub const STR_X0_X1: u32 = 0xf900_0020;
    pub const HVC_0: u32 = 0xd400_0002;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_struct_has_c_layout_with_padding() {
        // reason(4) + pad(4) + 3*u64(24) == 32.
        assert_eq!(std::mem::size_of::<HvVcpuExit>(), 32);
        assert_eq!(std::mem::align_of::<HvVcpuExit>(), 8);
        // exception must start at offset 8 (after the 4-byte pad), matching the C struct.
        let base = std::mem::MaybeUninit::<HvVcpuExit>::uninit();
        let p = base.as_ptr();
        let ex = unsafe { std::ptr::addr_of!((*p).exception) } as usize;
        let root = p as usize;
        assert_eq!(ex - root, 8);
    }

    #[test]
    fn esr_ec_extracts_hvc() {
        // A synthetic ESR with EC=0x16 in bits[31:26].
        let syndrome = 0x16u64 << 26;
        assert_eq!(esr_ec(syndrome), EC_AA64_HVC);
    }

    #[test]
    fn stub_encodings_match_assembler() {
        assert_eq!(stub::MRS_X0_MIDR_EL1, 0xd538_0000);
        assert_eq!(stub::STR_X0_X1, 0xf900_0020);
        assert_eq!(stub::HVC_0, 0xd400_0002);
    }

    #[test]
    fn return_code_names() {
        assert_eq!(hv_return_name(0), "HV_SUCCESS");
        assert_eq!(hv_return_name(HV_DENIED as i32), "HV_DENIED");
        assert_eq!(hv_return_name(HV_UNSUPPORTED as i32), "HV_UNSUPPORTED");
    }
}
