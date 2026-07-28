//! The vCPU: create it on the current thread, program the arm64 reset state (with a spoofed
//! `MIDR_EL1`), and run the exit-decode loop. Beyond the earlycon PL011 + PSCI HVCs of the
//! BootCore stage, this stage services:
//!   * the **userspace GICv3** — distributor/redistributor MMIO (`crate::gic`) plus the trapped
//!     `ICC_*_EL1` CPU-interface system registers (EC 0x18), and asserts the HVF IRQ line while a
//!     vCPU has a queued interrupt;
//!   * the **generic timer** — `HV_EXIT_REASON_VTIMER_ACTIVATED` injects PPI 27 through the GIC and
//!     re-arms HVF's vtimer once the guest has serviced the tick;
//!   * **PSCI `CPU_ON`** — spawns a secondary vCPU thread so SMP bring-up completes.
//!
//! HVF pins a vCPU to its creating thread, so each secondary creates its own `Vcpu` on its own
//! thread; shared interrupt state lives behind `VmContext`'s mutexes.

use crate::gic::{Gic, SgiRequest};
use crate::mmio::Pl011;
use crate::virtio::{self, Mem, MmioTransport, PciBus};
use crate::*;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use std::{arch::asm, ptr};

/// The spoofed `MIDR_EL1` for `cpu_index`, taken straight from the claimed SoC's cluster topology
/// (`aemulator_identity::profiles::reference_profile()` — Snapdragon 8 Gen 2 `SM8550`) so
/// `/proc/cpuinfo` and `/sys/.../midr_el1` can never drift from the DeviceProfile the DTB and
/// `build.prop` already advertise. Writing `MIDR_EL1` at all is what hides Apple's real `0x61`
/// implementer; `hvf-midr-probe` proves HVF accepts the write.
///
/// Two things this deliberately does NOT do:
///   * it does not hand every vCPU the same value — real big.LITTLE parts report a different MIDR
///     per cluster, so a uniform MIDR across 8 cores is itself the anomaly;
///   * it does not claim implementer `0x51` (custom Kryo). SM8550 ships stock ARM cores and reports
///     ARM's `0x41`; `0x51` on a part this new would be the tell, not the disguise.
///
/// Cores are laid out prime → gold → silver, matching the `cpu@<mpidr>` order `aemulator-dtb`
/// emits, so cpu N here is the same core N the guest sees in the device tree. A `cpu_index` past
/// the profile's declared core count falls back to the last (silver) cluster.
pub fn spoof_midr(cpu_index: u64) -> u64 {
    let profile = aemulator_identity::profiles::reference_profile();
    let mut remaining = cpu_index;
    let mut last = None;
    for write in profile.midr_regs() {
        let cluster = &profile.soc.midr_clusters[write.cluster_index];
        let cores = u64::from(cluster.core_count.max(1));
        if remaining < cores {
            return write.value;
        }
        remaining -= cores;
        last = Some(write.value);
    }
    // More vCPUs than the profile declares cores: reuse the silver cluster. If the profile somehow
    // declares none at all, fall back to a stock Cortex-A510 MIDR rather than leaking Apple's.
    last.unwrap_or(0x411F_D460)
}

/// PSTATE for the arm64 reset vector: EL1h (M=0b0101) with D/A/I/F masked (0x3c5).
pub const PSTATE_EL1H: u64 = 0x3c5;

// CNTV_CTL_EL0 bits.
const TMR_CTL_ENABLE: u64 = 1 << 0;
const TMR_CTL_IMASK: u64 = 1 << 1;
const TMR_CTL_ISTATUS: u64 = 1 << 2;

// PSCI function IDs (SMC64/HVC calling convention) the kernel issues early.
const PSCI_0_2_FN_VERSION: u64 = 0x8400_0000;
const PSCI_0_2_FN_CPU_OFF: u64 = 0x8400_0002;
const PSCI_0_2_FN_MIGRATE_INFO_TYPE: u64 = 0x8400_0006;
const PSCI_0_2_FN_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_0_2_FN_SYSTEM_RESET: u64 = 0x8400_0009;
const PSCI_0_2_FN64_CPU_ON: u64 = 0xc400_0003;
const PSCI_0_2_FN64_AFFINITY_INFO: u64 = 0xc400_0004;
const PSCI_RET_SUCCESS: u64 = 0;
const PSCI_RET_ALREADY_ON: u64 = (-4i64) as u64;
const PSCI_RET_NOT_SUPPORTED: u64 = (-1i64) as u64;

/// Shared VM state every vCPU thread references. The interrupt controller and UART are mutex-
/// guarded because a `CPU_ON`-spawned secondary vCPU touches them concurrently with the primary.
pub struct VmContext {
    pub gic: Mutex<Gic>,
    pub pl011: Mutex<Pl011>,
    pub fdt_ipa: u64,
    pub max_iters: u64,
    pub num_cpus: u64,
    /// Guest RAM handle shared with the virtio backends (ring walking reads/writes it directly).
    pub mem: Mem,
    /// The virtio-mmio transports, in slot order (device i lives at `virtio::device_ipa(i)`).
    pub virtio: Mutex<Vec<MmioTransport>>,
    /// The virtio-**PCI** host bridge (the Android/Cuttlefish transport). Empty on the Linux path.
    pub pci: Mutex<PciBus>,
    /// HVF vCPU ids indexed by guest cpu number (for cross-core `hv_vcpus_exit` kicks).
    pub hvf_ids: Mutex<Vec<Option<u64>>>,
    /// Which guest cpu indices have been powered on (guards double `CPU_ON`).
    pub started: Mutex<Vec<bool>>,
    pub secondaries: Mutex<Vec<JoinHandle<Result<StopReason, String>>>>,
}

impl VmContext {
    pub fn new(
        num_cpus: u64,
        fdt_ipa: u64,
        max_iters: u64,
        mem: Mem,
        virtio: Vec<MmioTransport>,
        pci: PciBus,
    ) -> Arc<Self> {
        let mut started = vec![false; num_cpus as usize];
        started[0] = true; // cpu0 is the boot cpu
        Arc::new(Self {
            gic: Mutex::new(Gic::new(num_cpus)),
            pl011: Mutex::new(Pl011::new()),
            fdt_ipa,
            max_iters,
            num_cpus,
            mem,
            virtio: Mutex::new(virtio),
            pci: Mutex::new(pci),
            hvf_ids: Mutex::new(vec![None; num_cpus as usize]),
            started: Mutex::new(started),
            secondaries: Mutex::new(Vec::new()),
        })
    }

    pub fn kick(&self, cpu: u64) {
        if let Some(Some(id)) = self.hvf_ids.lock().unwrap().get(cpu as usize).copied() {
            let mut v = id;
            let _ = unsafe { hv_vcpus_exit(&mut v, 1) };
        }
    }
}

/// Why the run loop stopped.
#[derive(Debug)]
pub enum StopReason {
    /// Guest issued PSCI SYSTEM_OFF / SYSTEM_RESET.
    PoweredOff,
    /// This (secondary) vCPU issued PSCI CPU_OFF.
    CpuOff { cpu: u64 },
    /// Ran the whole iteration budget (typically parked idle in WFI).
    BudgetExhausted { iters: u64 },
    /// A synchronous exception we don't emulate. Carries the diagnostic tuple.
    Fault {
        cpu: u64,
        ec: u64,
        esr: u64,
        pc: u64,
        far: u64,
        ipa: u64,
    },
    /// An unexpected hv_vcpu_run exit reason.
    UnexpectedExit { cpu: u64, reason: u32, pc: u64 },
}

pub struct Vcpu {
    pub id: u64,
    pub cpu_index: u64,
    exit: *mut HvVcpuExit,
    /// Deferred PC += 4 after a trapped-and-emulated instruction (data abort / sysreg / WFx).
    pending_advance_pc: bool,
    /// HVF auto-masks its vtimer on activation; set while we owe the guest a re-arm.
    vtimer_masked: bool,
    /// Host counter frequency (CNTFRQ_EL0), for turning a WFI deadline into a sleep.
    cntfrq: u64,
    /// Set by a PSCI SYSTEM_OFF/RESET/CPU_OFF so the run loop can stop after servicing the call.
    psci_stop: Option<StopReason>,
}

macro_rules! hv_ok {
    ($label:expr, $call:expr) => {{
        let rc: HvReturn = unsafe { $call };
        if rc != HV_SUCCESS {
            return Err(format!(
                "{}: {} ({:#010x})",
                $label,
                hv_return_name(rc),
                rc as u32
            ));
        }
    }};
}

impl Vcpu {
    /// Create the vCPU on the calling thread (HVF pins a vCPU to its creating thread).
    pub fn new(cpu_index: u64) -> Result<Self, String> {
        let mut id: u64 = 0;
        let mut exit: *mut HvVcpuExit = ptr::null_mut();
        hv_ok!(
            "hv_vcpu_create",
            hv_vcpu_create(&mut id, &mut exit, ptr::null_mut())
        );
        if exit.is_null() {
            return Err("hv_vcpu_create returned a null exit pointer".into());
        }
        // Read the host counter frequency once (same timebase HVF exposes to the guest).
        let cntfrq: u64;
        unsafe { asm!("mrs {}, cntfrq_el0", out(reg) cntfrq) };
        Ok(Self {
            id,
            cpu_index,
            exit,
            pending_advance_pc: false,
            vtimer_masked: false,
            cntfrq: if cntfrq == 0 { 24_000_000 } else { cntfrq },
            psci_stop: None,
        })
    }

    fn get_reg(&self, reg: u32) -> Result<u64, String> {
        let mut v: u64 = 0;
        hv_ok!("hv_vcpu_get_reg", hv_vcpu_get_reg(self.id, reg, &mut v));
        Ok(v)
    }

    fn set_reg(&self, reg: u32, val: u64) -> Result<(), String> {
        hv_ok!("hv_vcpu_set_reg", hv_vcpu_set_reg(self.id, reg, val));
        Ok(())
    }

    fn get_sys(&self, reg: u16) -> Result<u64, String> {
        let mut v: u64 = 0;
        hv_ok!("hv_vcpu_get_sys_reg", hv_vcpu_get_sys_reg(self.id, reg, &mut v));
        Ok(v)
    }

    /// Spoof `MIDR_EL1`. Returns whether HVF accepted the write (a non-success is a finding, not
    /// fatal to boot).
    pub fn set_midr(&self, midr: u64) -> bool {
        let rc = unsafe { hv_vcpu_set_sys_reg(self.id, HV_SYS_REG_MIDR_EL1, midr) };
        rc == HV_SUCCESS
    }

    pub fn get_midr(&self) -> u64 {
        let mut v: u64 = 0;
        let _ = unsafe { hv_vcpu_get_sys_reg(self.id, HV_SYS_REG_MIDR_EL1, &mut v) };
        v
    }

    /// Program `MPIDR_EL1` = this cpu's affinity (flat Aff0), so the redistributor id and PSCI
    /// `target_cpu`/`CPU_ON` affinity all agree.
    pub fn set_mpidr(&self) -> Result<(), String> {
        hv_ok!(
            "hv_vcpu_set_sys_reg(MPIDR)",
            hv_vcpu_set_sys_reg(self.id, HV_SYS_REG_MPIDR_EL1, self.cpu_index)
        );
        Ok(())
    }

    /// Program the arm64 boot state: PC = entry, X0 = arg (FDT IPA for the boot cpu, context id
    /// for a secondary), X1..X3 = 0, PSTATE = EL1h.
    pub fn set_boot_regs(&self, entry_ipa: u64, x0: u64) -> Result<(), String> {
        self.set_reg(HV_REG_PC, entry_ipa)?;
        self.set_reg(HV_REG_X0, x0)?;
        self.set_reg(HV_REG_X1, 0)?;
        self.set_reg(HV_REG_X2, 0)?;
        self.set_reg(HV_REG_X3, 0)?;
        self.set_reg(HV_REG_CPSR, PSTATE_EL1H)?;
        Ok(())
    }

    /// Run until the guest powers off, this vCPU is turned off, it faults, or the iteration budget
    /// is exhausted.
    pub fn run(&mut self, ctx: &Arc<VmContext>) -> Result<StopReason, String> {
        ctx.hvf_ids.lock().unwrap()[self.cpu_index as usize] = Some(self.id);

        for _ in 0..ctx.max_iters {
            if self.pending_advance_pc {
                let pc = self.get_reg(HV_REG_PC)?;
                self.set_reg(HV_REG_PC, pc.wrapping_add(4))?;
                self.pending_advance_pc = false;
            }

            // Assert the IRQ line if this cpu has a queued interrupt. HVF clears the pending-
            // interrupt state on every run, so this must be re-evaluated each iteration.
            let pending = ctx.gic.lock().unwrap().has_pending(self.cpu_index);
            if pending {
                let _ = unsafe {
                    hv_vcpu_set_pending_interrupt(self.id, HV_INTERRUPT_TYPE_IRQ, true)
                };
            }

            hv_ok!("hv_vcpu_run", hv_vcpu_run(self.id));
            // Safety: the exit struct is valid for the vCPU's lifetime and refreshed by each run.
            let e = unsafe { *self.exit };

            match e.reason {
                r if r == HV_EXIT_REASON_EXCEPTION => {
                    self.sync_vtimer(ctx);
                    if let Some(stop) = self.handle_exception(&e.exception, ctx)? {
                        return Ok(stop);
                    }
                }
                r if r == HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    // The armed virtual timer fired; HVF auto-masked it. Inject PPI 27 and remember
                    // to re-arm once the guest has serviced the tick (see `sync_vtimer`).
                    self.vtimer_masked = true;
                    ctx.gic.lock().unwrap().inject_vtimer(self.cpu_index);
                }
                r if r == HV_EXIT_REASON_CANCELED => { /* kicked to observe a new IRQ; re-run */ }
                other => {
                    let pc = self.get_reg(HV_REG_PC).unwrap_or(0);
                    return Ok(StopReason::UnexpectedExit {
                        cpu: self.cpu_index,
                        reason: other,
                        pc,
                    });
                }
            }
        }
        Ok(StopReason::BudgetExhausted {
            iters: ctx.max_iters,
        })
    }

    /// After an exception exit, if we owe the guest a vtimer re-arm: keep PPI 27 queued while the
    /// timer is still firing, and unmask HVF's vtimer once it's no longer asserting (the guest has
    /// re-armed CVAL or masked it), so the next deadline raises a fresh `VTIMER_ACTIVATED`.
    fn sync_vtimer(&mut self, ctx: &Arc<VmContext>) {
        if !self.vtimer_masked {
            return;
        }
        let ctl = self.get_sys(HV_SYS_REG_CNTV_CTL_EL0).unwrap_or(0);
        let firing =
            (ctl & (TMR_CTL_ENABLE | TMR_CTL_IMASK | TMR_CTL_ISTATUS)) == (TMR_CTL_ENABLE | TMR_CTL_ISTATUS);
        ctx.gic.lock().unwrap().inject_vtimer(self.cpu_index);
        if !firing {
            let _ = unsafe { hv_vcpu_set_vtimer_mask(self.id, false) };
            self.vtimer_masked = false;
        }
    }

    /// Decode one synchronous exception. `Ok(None)` means "handled, keep running".
    fn handle_exception(
        &mut self,
        ex: &HvVcpuExitException,
        ctx: &Arc<VmContext>,
    ) -> Result<Option<StopReason>, String> {
        let esr = ex.syndrome;
        let ec = esr_ec(esr);
        match ec {
            EC_DATAABORT_LOWER => {
                let iswrite = ((esr >> 6) & 1) != 0;
                let sas = ((esr >> 22) & 3) as u32;
                let len = 1usize << sas;
                let srt = ((esr >> 16) & 0x1f) as u32;
                let ipa = ex.physical_address;

                let is_gic = Gic::dist_contains(ipa) || ctx.gic.lock().unwrap().redist_contains(ipa);
                let virtio_dev = virtio::locate(ipa, ctx.virtio.lock().unwrap().len());
                if Pl011::contains(ipa) {
                    if iswrite {
                        let val = if srt < 31 { self.get_reg(hv_reg_x(srt))? } else { 0 };
                        ctx.pl011.lock().unwrap().write(ipa, val, len);
                    } else {
                        let val = ctx.pl011.lock().unwrap().read(ipa, len);
                        if srt < 31 {
                            self.set_reg(hv_reg_x(srt), val)?;
                        }
                    }
                } else if let Some((dev, off)) = virtio_dev {
                    if iswrite {
                        let val = if srt < 31 { self.get_reg(hv_reg_x(srt))? } else { 0 };
                        let needs_irq = {
                            let mut vt = ctx.virtio.lock().unwrap();
                            vt[dev].write(&ctx.mem, off, val, len)
                        };
                        if needs_irq {
                            let intid = ctx.virtio.lock().unwrap()[dev].intid;
                            let target = ctx.gic.lock().unwrap().set_spi(intid);
                            if target != self.cpu_index {
                                ctx.kick(target);
                            }
                        }
                    } else {
                        let val = ctx.virtio.lock().unwrap()[dev].read(off, len);
                        if srt < 31 {
                            self.set_reg(hv_reg_x(srt), val)?;
                        }
                    }
                } else if is_gic {
                    if iswrite {
                        let val = if srt < 31 { self.get_reg(hv_reg_x(srt))? } else { 0 };
                        ctx.gic.lock().unwrap().mmio_write(ipa, val, len);
                    } else {
                        let val = ctx.gic.lock().unwrap().mmio_read(ipa, len);
                        if srt < 31 {
                            self.set_reg(hv_reg_x(srt), val)?;
                        }
                    }
                } else if PciBus::cam_contains(ipa) {
                    // PCI configuration space (CAM).
                    if iswrite {
                        let val = if srt < 31 { self.get_reg(hv_reg_x(srt))? } else { 0 };
                        ctx.pci.lock().unwrap().cfg_write(ipa, val, len);
                    } else {
                        let val = ctx.pci.lock().unwrap().cfg_read(ipa, len);
                        if srt < 31 {
                            self.set_reg(hv_reg_x(srt), val)?;
                        }
                    }
                } else if PciBus::mmio_contains(ipa) {
                    // virtio-PCI BAR MMIO (common-cfg / notify / ISR / device-cfg).
                    if iswrite {
                        let val = if srt < 31 { self.get_reg(hv_reg_x(srt))? } else { 0 };
                        let intid = ctx.pci.lock().unwrap().mmio_write(&ctx.mem, ipa, val, len);
                        if let Some(intid) = intid {
                            let target = ctx.gic.lock().unwrap().set_spi(intid);
                            if target != self.cpu_index {
                                ctx.kick(target);
                            }
                        }
                    } else {
                        let val = ctx.pci.lock().unwrap().mmio_read(ipa, len);
                        if srt < 31 {
                            self.set_reg(hv_reg_x(srt), val)?;
                        }
                    }
                } else {
                    // Stray MMIO to an unimplemented device: read as 0 / swallow the write.
                    if !iswrite && srt < 31 {
                        self.set_reg(hv_reg_x(srt), 0)?;
                    }
                }
                self.pending_advance_pc = true;
                Ok(None)
            }
            EC_SYSTEMREGISTERTRAP => {
                self.handle_sysreg_trap(esr, ctx)?;
                self.pending_advance_pc = true;
                Ok(None)
            }
            EC_AA64_HVC => {
                self.handle_psci(ctx)?;
                // HVC: HVF leaves PC at the instruction after `hvc`, so no manual advance.
                Ok(self.psci_stop.take())
            }
            EC_AA64_SMC => {
                // SMC does not auto-advance; step over it, then service like PSCI.
                self.handle_psci(ctx)?;
                self.pending_advance_pc = true;
                Ok(self.psci_stop.take())
            }
            EC_WFX_TRAP => {
                self.handle_wfx(ctx)?;
                self.pending_advance_pc = true;
                Ok(None)
            }
            _ => {
                let pc = self.get_reg(HV_REG_PC).unwrap_or(0);
                Ok(Some(StopReason::Fault {
                    cpu: self.cpu_index,
                    ec,
                    esr,
                    pc,
                    far: ex.virtual_address,
                    ipa: ex.physical_address,
                }))
            }
        }
    }

    /// Handle a trapped `ICC_*_EL1` (GICv3 CPU interface) MSR/MRS. Decoded from the syndrome:
    /// bit0 = direction (1 = read), bits[9:5] = Xt, and the op0/op1/op2/CRn/CRm bits select the
    /// register.
    fn handle_sysreg_trap(&mut self, esr: u64, ctx: &Arc<VmContext>) -> Result<(), String> {
        let isread = (esr & 1) != 0;
        let rt = ((esr >> 5) & 0x1f) as u32;
        let reg = (esr as u32) & gic::SYSREG_MASK;

        if isread {
            let val = ctx
                .gic
                .lock()
                .unwrap()
                .sysreg_read(self.cpu_index, reg)
                .unwrap_or(0);
            if rt < 31 {
                self.set_reg(hv_reg_x(rt), val)?;
            }
        } else {
            let val = if rt < 31 { self.get_reg(hv_reg_x(rt))? } else { 0 };
            let req = ctx.gic.lock().unwrap().sysreg_write(self.cpu_index, reg, val);
            if let Some(SgiRequest::Sgi(targets)) = req {
                for (cpu, intid) in targets {
                    ctx.gic.lock().unwrap().set_irq(cpu, intid);
                    if cpu != self.cpu_index {
                        ctx.kick(cpu);
                    }
                }
            }
        }
        Ok(())
    }

    /// WFI/WFE: if nothing is pending for this cpu, briefly park the thread (bounded by the armed
    /// vtimer deadline) instead of busy-spinning, then re-run so HVF can raise the timer exit.
    fn handle_wfx(&mut self, ctx: &Arc<VmContext>) -> Result<(), String> {
        if ctx.gic.lock().unwrap().has_pending(self.cpu_index) {
            return Ok(());
        }
        let ctl = self.get_sys(HV_SYS_REG_CNTV_CTL_EL0).unwrap_or(0);
        let armed = (ctl & (TMR_CTL_ENABLE | TMR_CTL_IMASK)) == TMR_CTL_ENABLE;
        let sleep = if armed {
            let cval = self.get_sys(HV_SYS_REG_CNTV_CVAL_EL0).unwrap_or(0);
            let now = unsafe { mach_absolute_time() };
            if cval <= now {
                Duration::from_millis(0)
            } else {
                let ticks = cval - now;
                let ns = (ticks as u128) * 1_000_000_000u128 / (self.cntfrq as u128);
                Duration::from_nanos(ns.min(5_000_000) as u64) // cap at 5ms so cross-core IRQs land
            }
        } else {
            // Indefinite WFI (waiting on a device IRQ this phase may never produce): nap briefly.
            Duration::from_millis(2)
        };
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
        Ok(())
    }

    /// Service a PSCI call issued via HVC/SMC (X0 = function id).
    fn handle_psci(&mut self, ctx: &Arc<VmContext>) -> Result<(), String> {
        let func = self.get_reg(HV_REG_X0)?;
        match func {
            PSCI_0_2_FN_VERSION => {
                self.set_reg(HV_REG_X0, 0x0001_0000)?; // PSCI v1.0
            }
            PSCI_0_2_FN_MIGRATE_INFO_TYPE => {
                self.set_reg(HV_REG_X0, 2)?; // migration not required
            }
            PSCI_0_2_FN64_AFFINITY_INFO => {
                let target = self.get_reg(HV_REG_X1)? & 0xff;
                let on = ctx
                    .started
                    .lock()
                    .unwrap()
                    .get(target as usize)
                    .copied()
                    .unwrap_or(false);
                // 0 = ON, 1 = OFF.
                self.set_reg(HV_REG_X0, if on { 0 } else { 1 })?;
            }
            PSCI_0_2_FN_SYSTEM_OFF | PSCI_0_2_FN_SYSTEM_RESET => {
                self.psci_stop = Some(StopReason::PoweredOff);
            }
            PSCI_0_2_FN_CPU_OFF => {
                if let Some(s) = ctx.started.lock().unwrap().get_mut(self.cpu_index as usize) {
                    *s = false;
                }
                self.psci_stop = Some(StopReason::CpuOff {
                    cpu: self.cpu_index,
                });
            }
            PSCI_0_2_FN64_CPU_ON => {
                let target = self.get_reg(HV_REG_X1)? & 0xff;
                let entry = self.get_reg(HV_REG_X2)?;
                let context = self.get_reg(HV_REG_X3)?;
                let ret = self.psci_cpu_on(ctx, target, entry, context);
                self.set_reg(HV_REG_X0, ret)?;
            }
            _ => {
                self.set_reg(HV_REG_X0, PSCI_RET_NOT_SUPPORTED)?;
            }
        }
        Ok(())
    }

    /// Spawn a secondary vCPU thread for `target`, entering the guest at `entry` with X0 = context.
    fn psci_cpu_on(&self, ctx: &Arc<VmContext>, target: u64, entry: u64, context: u64) -> u64 {
        if target >= ctx.num_cpus {
            return PSCI_RET_NOT_SUPPORTED;
        }
        {
            let mut started = ctx.started.lock().unwrap();
            if started[target as usize] {
                return PSCI_RET_ALREADY_ON;
            }
            started[target as usize] = true;
        }
        let ctx2 = Arc::clone(ctx);
        let handle = std::thread::Builder::new()
            .name(format!("vcpu{target}"))
            .spawn(move || -> Result<StopReason, String> {
                let mut vcpu = Vcpu::new(target)?;
                vcpu.set_mpidr()?;
                let real = vcpu.get_midr();
                let ok = vcpu.set_midr(spoof_midr(target));
                let after = vcpu.get_midr();
                eprintln!(
                    "[cpu{target}] up: MIDR real impl {:#04x} -> spoof {after:#010x} (impl {:#04x}) accepted={}",
                    (real >> 24) & 0xff,
                    (after >> 24) & 0xff,
                    ok
                );
                vcpu.set_boot_regs(entry, context)?;
                vcpu.run(&ctx2)
            });
        match handle {
            Ok(h) => {
                ctx.secondaries.lock().unwrap().push(h);
                PSCI_RET_SUCCESS
            }
            Err(e) => {
                eprintln!("[cpu{target}] spawn failed: {e}");
                ctx.started.lock().unwrap()[target as usize] = false;
                PSCI_RET_NOT_SUPPORTED
            }
        }
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        let _ = unsafe { hv_vcpu_destroy(self.id) };
    }
}

#[cfg(test)]
mod tests {
    use super::spoof_midr;

    /// The whole point of writing `MIDR_EL1` is that no vCPU ever reports Apple's `0x61`. Checked
    /// past the profile's core count so the out-of-range fallback is covered too.
    #[test]
    fn spoofed_midr_is_never_apple_or_null() {
        for cpu in 0..16 {
            let implementer = (spoof_midr(cpu) >> 24) as u8;
            assert_ne!(implementer, 0x61, "cpu{cpu} leaked the Apple implementer");
            assert_ne!(
                implementer, 0x00,
                "cpu{cpu} leaked a QEMU-max null implementer"
            );
        }
    }

    /// The claimed SoC is big.LITTLE, so the guest must see >=2 distinct MIDRs across its cores —
    /// a uniform value over 8 cores matches no real Android part.
    #[test]
    fn spoofed_midr_is_heterogeneous_across_clusters() {
        let seen: std::collections::BTreeSet<u64> = (0..8).map(spoof_midr).collect();
        assert!(
            seen.len() >= 2,
            "big.LITTLE must present >=2 distinct MIDR values, got {seen:?}"
        );
    }

    /// The spoof must agree with the identity crate core-for-core: cpu0 is the profile's prime
    /// core, and the last cpu is its silver core. A drift here means `/proc/cpuinfo` contradicts
    /// the DTB and `build.prop` built from the same profile.
    #[test]
    fn spoofed_midr_tracks_the_device_profile() {
        let profile = aemulator_identity::profiles::reference_profile();
        let regs = profile.midr_regs();
        let cores: u64 = profile
            .soc
            .midr_clusters
            .iter()
            .map(|c| u64::from(c.core_count))
            .sum();
        assert_eq!(spoof_midr(0), regs.first().unwrap().value);
        assert_eq!(spoof_midr(cores - 1), regs.last().unwrap().value);
        // SM8550: Cortex-X3 prime (0x411FD4E0), Cortex-A510 silver (0x411FD460).
        assert_eq!(spoof_midr(0), 0x411F_D4E0);
        assert_eq!(spoof_midr(cores - 1), 0x411F_D460);
    }
}
