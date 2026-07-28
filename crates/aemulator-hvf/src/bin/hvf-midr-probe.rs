//! `hvf-midr-probe` — the empirical make-or-break test for MIDR_EL1 spoofing under Apple's
//! Hypervisor.framework on this host.
//!
//! What it does, end to end:
//!   1. `hv_vm_create(NULL)`               — default VM (guest runs at EL1, no EL2).
//!   2. `hv_vm_map`                        — one RWX guest page (host heap page → guest IPA).
//!   3. Writes a 3-instruction aarch64 stub into that page:
//!      `mrs x0, MIDR_EL1 ; str x0, [x1] ; hvc #0`
//!   4. `hv_vcpu_create`, set PC=code, x1=scratch IPA, CPSR=EL1h.
//!   5. `hv_vcpu_set_sys_reg(HV_SYS_REG_MIDR_EL1, <fake Qualcomm MIDR>)`.
//!   6. `hv_vcpu_run` until the guest `hvc #0` exits (HV_EXIT_REASON_EXCEPTION, ESR.EC=0x16).
//!   7. Reads back what the GUEST actually observed — from the scratch memory it stored to AND
//!      from x0 — and prints it beside the real Apple MIDR and the value we asked for.
//!
//! The verdict: if the guest read our fake `0x51`-implementer (Qualcomm) MIDR, spoofing WORKS.
//! If it read the real Apple `0x61`-implementer MIDR, HVF ignored our write and spoofing FAILS.
//!
//! Requires the `com.apple.security.hypervisor` entitlement (see hv.entitlements + README).

fn main() {
    #[cfg(target_os = "macos")]
    {
        std::process::exit(macos::run());
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "hvf-midr-probe: requires macOS + Apple Hypervisor.framework (this host is not macOS)."
        );
        std::process::exit(2);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use aemulator_hvf::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};
    use std::os::raw::c_void;
    use std::ptr;

    /// Guest-physical layout. Both the code and the scratch cell live in one mapped page.
    const CODE_IPA: u64 = 0x1_0000; // 64 KiB — a multiple of the 16 KiB granule.
    const SCRATCH_OFF: u64 = 0x800; // offset of the store target within the page
    /// Fake Qualcomm MIDR. Implementer 0x51 ('Q'), variant 0x1, arch 0xF, PartNum 0x804 (Kryo),
    /// revision 0x1 — a plausible Snapdragon-class MIDR. The load-bearing bit is the top byte
    /// `0x51`; the real Apple implementer is `0x61`.
    const FAKE_QUALCOMM_MIDR: u64 = 0x5100_0000 | (0x1 << 20) | (0xF << 16) | (0x804 << 4) | 0x1; // 0x511F_8041
    /// PSTATE for the reset vector: EL1h (M=0b0101) with D/A/I/F masked.
    const PSTATE_EL1H: u64 = 0x3c5;

    macro_rules! hv_try {
        ($label:expr, $call:expr) => {{
            let rc: HvReturn = unsafe { $call };
            if rc != HV_SUCCESS {
                eprintln!(
                    "[FAIL] {}: rc={} (0x{:08x})",
                    $label,
                    hv_return_name(rc),
                    rc as u32
                );
                return Err(rc);
            }
        }};
    }

    pub fn run() -> i32 {
        match run_inner() {
            Ok(()) => 0,
            Err(_rc) => 1,
        }
    }

    fn run_inner() -> Result<(), HvReturn> {
        let page = host_page_size();
        println!("== hvf-midr-probe ==");
        println!("host page size: {} bytes", page);
        println!(
            "fake MIDR we will set: 0x{FAKE_QUALCOMM_MIDR:016x}  (implementer 0x{:02x})",
            (FAKE_QUALCOMM_MIDR >> 24) & 0xff
        );

        // --- 1. Create the VM (needs the hypervisor entitlement). ---
        hv_try!("hv_vm_create", hv_vm_create(ptr::null_mut()));
        println!("[ok] hv_vm_create");

        // Ensure we always tear the VM down.
        let result = with_vm(page);

        let _ = unsafe { hv_vm_destroy() };
        result
    }

    fn with_vm(page: usize) -> Result<(), HvReturn> {
        // --- 2. Allocate one page-aligned host page and map it RWX into the guest. ---
        let layout = Layout::from_size_align(page, page).expect("valid page layout");
        // Safety: nonzero size; we free it on every exit path below.
        let host = unsafe { alloc_zeroed(layout) };
        if host.is_null() {
            eprintln!("[FAIL] alloc_zeroed returned null");
            return Err(HV_ERROR as HvReturn);
        }

        let res = with_mapping(host, page);

        // Unmap + free regardless of outcome.
        let _ = unsafe { hv_vm_unmap(CODE_IPA, page) };
        unsafe { dealloc(host, Layout::from_size_align(page, page).unwrap()) };
        res
    }

    fn with_mapping(host: *mut u8, page: usize) -> Result<(), HvReturn> {
        // --- 3. Write the stub into the page (little-endian u32 words). ---
        let words = [stub::MRS_X0_MIDR_EL1, stub::STR_X0_X1, stub::HVC_0];
        // Safety: host points to `page` (>= 12) writable bytes.
        unsafe {
            let code = host as *mut u32;
            for (i, w) in words.iter().enumerate() {
                ptr::write(code.add(i), *w);
            }
        }

        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;
        hv_try!(
            "hv_vm_map",
            hv_vm_map(host as *mut c_void, CODE_IPA, page, flags)
        );
        println!("[ok] hv_vm_map: host {host:p} -> IPA 0x{CODE_IPA:x} size {page} flags RWX");

        // --- 4. Create the vCPU on this thread. ---
        let mut vcpu: u64 = 0;
        let mut exit: *mut HvVcpuExit = ptr::null_mut();
        hv_try!(
            "hv_vcpu_create",
            hv_vcpu_create(&mut vcpu, &mut exit, ptr::null_mut())
        );
        println!("[ok] hv_vcpu_create: vcpu id {vcpu}");
        if exit.is_null() {
            eprintln!("[FAIL] hv_vcpu_create returned a null exit pointer");
            return Err(HV_ERROR as HvReturn);
        }

        let res = drive_vcpu(vcpu, exit, host);
        let _ = unsafe { hv_vcpu_destroy(vcpu) };
        res
    }

    fn drive_vcpu(vcpu: u64, exit: *mut HvVcpuExit, host: *mut u8) -> Result<(), HvReturn> {
        // Read the REAL Apple MIDR HVF exposes by default (before we touch it).
        let mut real_midr: u64 = 0;
        hv_try!(
            "hv_vcpu_get_sys_reg(MIDR default)",
            hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_MIDR_EL1, &mut real_midr)
        );
        println!(
            "real (default) MIDR_EL1 HVF exposes: 0x{real_midr:016x}  (implementer 0x{:02x})",
            (real_midr >> 24) & 0xff
        );

        // --- 5. Try to overwrite MIDR_EL1 with the fake Qualcomm value. ---
        let set_rc: HvReturn =
            unsafe { hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_MIDR_EL1, FAKE_QUALCOMM_MIDR) };
        println!(
            "hv_vcpu_set_sys_reg(MIDR_EL1, 0x{FAKE_QUALCOMM_MIDR:016x}) -> {} (0x{:08x})",
            hv_return_name(set_rc),
            set_rc as u32
        );
        // A non-success here is itself a finding — record it but keep going so we can still see
        // what the guest reads.
        let set_ok = set_rc == HV_SUCCESS;

        // Read it straight back at the API level: did HVF store our value?
        let mut api_readback: u64 = 0;
        let _ = unsafe { hv_vcpu_get_sys_reg(vcpu, HV_SYS_REG_MIDR_EL1, &mut api_readback) };
        println!(
            "hv_vcpu_get_sys_reg(MIDR_EL1) after set: 0x{api_readback:016x}  (implementer 0x{:02x})",
            (api_readback >> 24) & 0xff
        );

        // --- Program the reset state: PC=code, x1=scratch IPA, CPSR=EL1h. ---
        hv_try!("set PC", hv_vcpu_set_reg(vcpu, HV_REG_PC, CODE_IPA));
        hv_try!(
            "set X1 (scratch IPA)",
            hv_vcpu_set_reg(vcpu, HV_REG_X1, CODE_IPA + SCRATCH_OFF)
        );
        hv_try!(
            "set CPSR (EL1h)",
            hv_vcpu_set_reg(vcpu, HV_REG_CPSR, PSTATE_EL1H)
        );

        // --- 6. Run until the guest hvc #0 exits. ---
        let mut hit_hvc = false;
        for iter in 0..16 {
            hv_try!("hv_vcpu_run", hv_vcpu_run(vcpu));
            // Safety: exit pointer is valid for the life of the vcpu and written by hv_vcpu_run.
            let e = unsafe { *exit };
            match e.reason {
                r if r == HV_EXIT_REASON_EXCEPTION => {
                    let ec = esr_ec(e.exception.syndrome);
                    println!(
                        "exit[{iter}]: EXCEPTION  ESR=0x{:016x} EC=0x{:02x} FAR=0x{:016x} IPA=0x{:016x}",
                        e.exception.syndrome, ec, e.exception.virtual_address, e.exception.physical_address
                    );
                    if ec == EC_AA64_HVC {
                        hit_hvc = true;
                        break;
                    }
                    // Any other synchronous exception (e.g. instruction/data abort) means the
                    // guest never reached hvc — stop and report honestly.
                    eprintln!("[FAIL] unexpected exception class 0x{ec:02x} before hvc; guest did not complete the stub");
                    return Err(HV_ERROR as HvReturn);
                }
                r if r == HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    println!("exit[{iter}]: VTIMER_ACTIVATED (masking, re-running)");
                    // Nothing to do; loop again. (VTimer auto-masks on this exit.)
                    continue;
                }
                r if r == HV_EXIT_REASON_CANCELED => {
                    println!("exit[{iter}]: CANCELED (re-running)");
                    continue;
                }
                other => {
                    eprintln!("[FAIL] exit[{iter}]: unexpected reason {other}");
                    return Err(HV_ERROR as HvReturn);
                }
            }
        }

        if !hit_hvc {
            eprintln!("[FAIL] never reached hvc within iteration budget");
            return Err(HV_ERROR as HvReturn);
        }
        println!("[ok] guest executed the stub and trapped via hvc #0");

        // --- 7a. What did the guest STORE to scratch memory (str x0,[x1])? ---
        // Safety: SCRATCH_OFF+8 is within the mapped page; the guest wrote through to this host buf.
        let stored = unsafe { ptr::read((host.add(SCRATCH_OFF as usize)) as *const u64) };
        println!(
            "guest STORED (from scratch mem): 0x{stored:016x}  (implementer 0x{:02x})",
            (stored >> 24) & 0xff
        );

        // --- 7b. And what is in x0 (the MRS result)? ---
        let mut x0: u64 = 0;
        let _ = unsafe { hv_vcpu_get_reg(vcpu, HV_REG_X0, &mut x0) };
        println!(
            "guest READ into x0 (MRS result): 0x{x0:016x}  (implementer 0x{:02x})",
            (x0 >> 24) & 0xff
        );

        // --- Verdict ---
        // Prefer what the guest stored to memory; fall back to x0 if they disagree (they must not).
        let observed = if stored != 0 { stored } else { x0 };
        let observed_impl = (observed >> 24) & 0xff;
        let real_impl = (real_midr >> 24) & 0xff;
        println!("\n---------------- VERDICT ----------------");
        println!("set_sys_reg success:  {set_ok}");
        println!(
            "we asked for:         0x{FAKE_QUALCOMM_MIDR:016x}  (impl 0x{:02x} Qualcomm)",
            (FAKE_QUALCOMM_MIDR >> 24) & 0xff
        );
        println!("real Apple MIDR:      0x{real_midr:016x}  (impl 0x{real_impl:02x} Apple)");
        println!("guest actually saw:   0x{observed:016x}  (impl 0x{observed_impl:02x})");
        if observed == FAKE_QUALCOMM_MIDR {
            println!("RESULT: SPOOF WORKS — guest read our fake Qualcomm MIDR exactly.");
        } else if observed_impl == 0x51 {
            println!("RESULT: SPOOF WORKS (implementer) — guest read a 0x51/Qualcomm implementer, value 0x{observed:016x}.");
        } else if observed == real_midr || observed_impl == real_impl {
            println!(
                "RESULT: SPOOF FAILS — guest read the REAL Apple MIDR; HVF ignored our write."
            );
        } else {
            println!("RESULT: INCONCLUSIVE — guest read 0x{observed:016x}, neither our fake nor the plain Apple default.");
        }
        println!("-----------------------------------------");

        Ok(())
    }
}
