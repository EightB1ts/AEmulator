//! A userspace GICv3 for HVF — ported from libkrun's fallback interrupt controller
//! (`src/devices/src/legacy/gicv3.rs` + `vcpu.rs`), the path libkrun uses on macOS versions
//! without an in-kernel `hv_gic_create`. We deliberately take the userspace route (rather than
//! calling `hv_gic_create`) so the VMM has full control over everything the guest observes — the
//! stealth posture the emulator is built around.
//!
//! Two halves make up a GICv3:
//!   * **Distributor + Redistributor MMIO** — memory-mapped register banks the guest driver probes
//!     and programs. We emulate them at fixed IPAs (`GICD_BASE` / `GICR_BASE`). Most config
//!     registers are accepted as no-ops; the identification registers (`*_TYPER`, `*_IIDR`,
//!     `*_PIDR2`, `GICR_WAKER`) return the exact values the Linux `gic-v3` driver expects so its
//!     probe succeeds.
//!   * **CPU interface** — the `ICC_*_EL1` system registers the guest uses to acknowledge/complete
//!     interrupts. On HVF (with no in-kernel GIC) these MSR/MRS accesses trap out to the VMM as
//!     `EC == 0x18`; `vcpu::run` forwards them here.
//!
//! Interrupt delivery is **queue-based**, exactly as libkrun's fallback does it: an injected IRQ is
//! pushed onto the target vCPU's pending queue, the run loop asserts the HVF IRQ line while the
//! queue is non-empty, and the guest drains it by reading `ICC_IAR1_EL1`. This is sufficient for a
//! Linux/Android boot because the only sources this phase are the virtual timer (PPI 27) and,
//! later, SPIs — we never inject an interrupt the guest hasn't armed.

use std::collections::VecDeque;

// ---------------------------------------------------------------------------------------------
// IPA map (mirrors crosvm's aarch64 layout, below the 0x4000_0000 MMIO hole, far from DRAM).
// Redistributor frame is 2 * 64 KiB per vCPU: RD_base (0x10000) + SGI_base (0x10000).
// ---------------------------------------------------------------------------------------------
pub const GICD_BASE: u64 = 0x3fff_0000;
pub const GICD_SIZE: u64 = 0x0001_0000;
pub const GICR_BASE: u64 = 0x3fde_0000;
pub const GICR_FRAME_SIZE: u64 = 0x0002_0000;

/// INTID of the EL1 virtual timer (PPI 11 → 16 + 11). HVF's vtimer maps here.
pub const VTIMER_IRQ: u32 = 27;

/// Returned by `ICC_IAR1_EL1` when no interrupt is pending (spurious).
const GIC_INTID_SPURIOUS: u32 = 1023;

const IRQ_NUM: u32 = 288;
const MAXIRQ: usize = 1020;
const BITMAP_SZ: usize = MAXIRQ.div_ceil(32);
const GIC_INTERNAL: u32 = 32;

// Distributor register offsets (from RD_base 0). The full bank is listed to document the register
// map; the enable/pending/active/priority banks are accepted as no-ops (delivery is queue-based),
// so several are unreferenced — kept deliberately for readers cross-checking the GICv3 spec.
#[allow(dead_code)]
const GICD_CTLR: u64 = 0x0000;
const GICD_TYPER: u64 = 0x0004;
const GICD_IIDR: u64 = 0x0008;
#[allow(dead_code)]
const GICD_IGROUPR: u64 = 0x0080;
#[allow(dead_code)]
const GICD_ISENABLER: u64 = 0x0100;
#[allow(dead_code)]
const GICD_ICENABLER: u64 = 0x0180;
#[allow(dead_code)]
const GICD_ISPENDR: u64 = 0x0200;
#[allow(dead_code)]
const GICD_ICPENDR: u64 = 0x0280;
#[allow(dead_code)]
const GICD_ISACTIVER: u64 = 0x0300;
#[allow(dead_code)]
const GICD_ICACTIVER: u64 = 0x0380;
#[allow(dead_code)]
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0C00;
const GICD_IROUTER: u64 = 0x6000;
const GICD_IDREGS: u64 = 0xFFD0;

const GICD_CTLR_EN_GRP0: u32 = 1 << 0;
const GICD_CTLR_EN_GRP1NS: u32 = 1 << 1;
const GICD_CTLR_ARE: u32 = 1 << 4;
const GICD_CTLR_DS: u32 = 1 << 6;

const GICD_TYPER_RSS_SHIFT: u64 = 26;
const GICD_TYPER_NO1N_SHIFT: u64 = 25;
const GICD_TYPER_A3V_SHIFT: u64 = 24;
const GICD_TYPER_ID_BITS_SHIFT: u64 = 19;
const GICD_TYPER_LPIS_SHIFT: u64 = 17;

// Redistributor register offsets (from RD_base).
const GICR_CTLR: u64 = 0x0000;
const GICR_IIDR: u64 = 0x0004;
const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
const GICR_IDREGS: u64 = 0xFFD0;
const GICR_SGI_OFFSET: u64 = 0x10000;
const GICR_IGROUPR0: u64 = GICR_SGI_OFFSET + 0x0080;
const GICR_ISENABLER0: u64 = GICR_SGI_OFFSET + 0x0100;
const GICR_ICENABLER0: u64 = GICR_SGI_OFFSET + 0x0180;
const GICR_ICACTIVER0: u64 = GICR_SGI_OFFSET + 0x0380;
const GICR_IPRIORITYR: u64 = GICR_SGI_OFFSET + 0x0400;
const GICR_ICFGR1: u64 = GICR_SGI_OFFSET + 0x0C04;

const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;

const GICR_TYPER_AFFINITY_VALUE: u64 = 32;
const GICR_TYPER_COMMON_LPI_AFF_SHIFT: u64 = 24;
const GICR_TYPER_PROCESSOR_NUMBER_SHIFT: u64 = 8;
const GICR_TYPER_LAST_SHIFT: u64 = 4;

const GICV3_PIDR0_DIST: u8 = 0x92;
const GICV3_PIDR0_REDIST: u8 = 0x93;

// ---------------------------------------------------------------------------------------------
// ICC_*_EL1 CPU-interface sysreg encodings. The trap syndrome exposes op0/op1/op2/CRn/CRm packed
// exactly like this; `vcpu::run` masks the syndrome with `SYSREG_MASK` and matches these.
// Encoding order matches libkrun's `arm64_sys_reg!(name, op0, op1, op2, crn, crm)`.
// ---------------------------------------------------------------------------------------------
const fn sysreg(op0: u32, op1: u32, op2: u32, crn: u32, crm: u32) -> u32 {
    (op0 << 20) | (op2 << 17) | (op1 << 14) | (crn << 10) | (crm << 1)
}

/// Mask isolating the op0/op1/op2/CRn/CRm bits of a system-register trap syndrome.
pub const SYSREG_MASK: u32 = sysreg(0x3, 0x7, 0x7, 0xf, 0xf);

const SYSREG_ICC_PMR_EL1: u32 = sysreg(3, 0, 0, 4, 6);
const SYSREG_ICC_IAR1_EL1: u32 = sysreg(3, 0, 0, 12, 12);
const SYSREG_ICC_EOIR1_EL1: u32 = sysreg(3, 0, 1, 12, 12);
const SYSREG_ICC_DIR_EL1: u32 = sysreg(3, 0, 1, 12, 11);
const SYSREG_ICC_BPR1_EL1: u32 = sysreg(3, 0, 3, 12, 12);
const SYSREG_ICC_CTLR_EL1: u32 = sysreg(3, 0, 4, 12, 12);
const SYSREG_ICC_SGI1R_EL1: u32 = sysreg(3, 0, 5, 12, 11);
const SYSREG_ICC_SRE_EL1: u32 = sysreg(3, 0, 5, 12, 12);
const SYSREG_ICC_IGRPEN1_EL1: u32 = sysreg(3, 0, 7, 12, 12);
const SYSREG_ICC_AP1R0_EL1: u32 = sysreg(3, 0, 0, 12, 9);

const ICC_CTLR_EL1_RSS_SHIFT: u64 = 18;
const ICC_CTLR_EL1_A3V_SHIFT: u64 = 15;
const ICC_CTLR_EL1_ID_BITS_SHIFT: u64 = 11;
const ICC_CTLR_EL1_PRI_BITS_SHIFT: u64 = 8;

/// What to do after a CPU-interface sysreg access (returned to the run loop).
pub enum SgiRequest {
    /// The write generated software interrupts targeting `(cpu, intid)` pairs; the run loop
    /// queues them and kicks the target vCPUs.
    Sgi(Vec<(u64, u32)>),
}

/// The userspace GICv3. Guarded by a `Mutex` in `VmContext` and shared across vCPU threads.
pub struct Gic {
    num_cpus: u64,
    gicd_ctlr: u32,
    gicr_waker: u32,
    revision: u8,
    edge_trigger: [u32; BITMAP_SZ],
    gicd_irouter: [u64; MAXIRQ],
    /// Per-vCPU pending-interrupt queue (SGIs/PPIs are per-core; SPIs are routed here by affinity).
    pending: Vec<VecDeque<u32>>,
}

impl Gic {
    pub fn new(num_cpus: u64) -> Self {
        Self {
            num_cpus,
            gicd_ctlr: GICD_CTLR_DS | GICD_CTLR_ARE,
            gicr_waker: GICR_WAKER_PROCESSOR_SLEEP | GICR_WAKER_CHILDREN_ASLEEP,
            revision: 3,
            edge_trigger: [0; BITMAP_SZ],
            gicd_irouter: [0; MAXIRQ],
            pending: (0..num_cpus).map(|_| VecDeque::new()).collect(),
        }
    }

    /// Total size of the redistributor region (one 128 KiB frame per vCPU).
    pub fn redist_region_size(&self) -> u64 {
        GICR_FRAME_SIZE * self.num_cpus
    }

    /// Is `ipa` inside the distributor MMIO window?
    pub fn dist_contains(ipa: u64) -> bool {
        ipa >= GICD_BASE && ipa < GICD_BASE + GICD_SIZE
    }

    /// Is `ipa` inside the redistributor MMIO window?
    pub fn redist_contains(&self, ipa: u64) -> bool {
        ipa >= GICR_BASE && ipa < GICR_BASE + self.redist_region_size()
    }

    // --- Injection ----------------------------------------------------------------------------

    /// Queue an interrupt on a specific vCPU (PPI/SGI, or an already-routed SPI).
    pub fn set_irq(&mut self, cpu: u64, intid: u32) {
        if let Some(q) = self.pending.get_mut(cpu as usize) {
            q.push_back(intid);
        }
    }

    /// Queue the virtual-timer PPI on a vCPU, de-duplicating so repeated `sync_vtimer` passes
    /// across many WFI exits can't grow the queue without bound.
    pub fn inject_vtimer(&mut self, cpu: u64) {
        if let Some(q) = self.pending.get_mut(cpu as usize) {
            if !q.contains(&VTIMER_IRQ) {
                q.push_back(VTIMER_IRQ);
            }
        }
    }

    /// Route + queue a shared peripheral interrupt (SPI, intid ≥ 32) by its `GICD_IROUTER` Aff0.
    /// De-duplicated (virtio SPIs are level-triggered — one pending entry per intid is enough until
    /// the guest acks), and returns the target cpu so the caller can cross-core kick it.
    pub fn set_spi(&mut self, intid: u32) -> u64 {
        let cpu = self
            .gicd_irouter
            .get(intid as usize)
            .map(|r| r & 0xff)
            .unwrap_or(0);
        let cpu = if cpu < self.num_cpus { cpu } else { 0 };
        if let Some(q) = self.pending.get_mut(cpu as usize) {
            if !q.contains(&intid) {
                q.push_back(intid);
            }
        }
        cpu
    }

    /// Does this vCPU have an interrupt waiting? Drives IRQ-line assertion in the run loop.
    pub fn has_pending(&self, cpu: u64) -> bool {
        self.pending
            .get(cpu as usize)
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }

    // --- CPU interface (ICC_*_EL1 traps) ------------------------------------------------------

    /// Service a trapped `MRS Xt, ICC_*_EL1`. `Some(val)` is written to Xt; `None` means we don't
    /// model this register (the run loop returns 0, which is safe for boot).
    pub fn sysreg_read(&mut self, cpu: u64, reg: u32) -> Option<u64> {
        match reg {
            SYSREG_ICC_IAR1_EL1 => Some(
                self.pending
                    .get_mut(cpu as usize)
                    .and_then(|q| q.pop_front())
                    .unwrap_or(GIC_INTID_SPURIOUS) as u64,
            ),
            SYSREG_ICC_PMR_EL1 | SYSREG_ICC_BPR1_EL1 | SYSREG_ICC_AP1R0_EL1
            | SYSREG_ICC_IGRPEN1_EL1 => Some(0),
            SYSREG_ICC_SRE_EL1 => Some(0x7), // SRE | DFB | DIB — system-register interface enabled
            SYSREG_ICC_CTLR_EL1 => Some(
                (1 << ICC_CTLR_EL1_RSS_SHIFT)
                    | (1 << ICC_CTLR_EL1_A3V_SHIFT)
                    | (1 << ICC_CTLR_EL1_ID_BITS_SHIFT)
                    | (4 << ICC_CTLR_EL1_PRI_BITS_SHIFT),
            ),
            _ => None,
        }
    }

    /// Service a trapped `MSR ICC_*_EL1, Xt`. Returns an [`SgiRequest`] when the write generated
    /// software interrupts; otherwise `None` (the write is accepted as a no-op).
    pub fn sysreg_write(&mut self, _cpu: u64, reg: u32, val: u64) -> Option<SgiRequest> {
        if reg == SYSREG_ICC_SGI1R_EL1 {
            let target_list = val & 0xffff;
            let intid = ((val >> 24) & 0xf) as u32;
            let irm = (val >> 40) & 1;
            let rs = (val >> 44) & 0xf;
            let mut targets = Vec::new();
            if irm == 1 {
                // Broadcast to all cores except self.
                for c in 0..self.num_cpus {
                    if c != _cpu {
                        targets.push((c, intid));
                    }
                }
            } else {
                for bit in 0u64..16 {
                    if (target_list >> bit) & 1 == 1 {
                        let target = rs * 16 + bit;
                        if target < self.num_cpus {
                            targets.push((target, intid));
                        }
                    }
                }
            }
            return Some(SgiRequest::Sgi(targets));
        }
        // ICC_EOIR1/DIR/PMR/BPR1/CTLR/IGRPEN1/AP1R0/SRE and anything else: accept, no state.
        let _ = (SYSREG_ICC_EOIR1_EL1, SYSREG_ICC_DIR_EL1);
        None
    }

    // --- MMIO ---------------------------------------------------------------------------------

    /// Distributor/redistributor MMIO read. `len` ∈ {1,2,4,8}.
    pub fn mmio_read(&self, ipa: u64, len: usize) -> u64 {
        if Self::dist_contains(ipa) {
            self.dist_read(ipa - GICD_BASE, len)
        } else {
            let rel = ipa - GICR_BASE;
            let cpu = rel / GICR_FRAME_SIZE;
            self.redist_read(cpu, rel % GICR_FRAME_SIZE, len)
        }
    }

    /// Distributor/redistributor MMIO write. `len` ∈ {1,2,4,8}.
    pub fn mmio_write(&mut self, ipa: u64, val: u64, len: usize) {
        if Self::dist_contains(ipa) {
            self.dist_write(ipa - GICD_BASE, val, len);
        } else {
            let rel = ipa - GICR_BASE;
            let cpu = rel / GICR_FRAME_SIZE;
            self.redist_write(cpu, rel % GICR_FRAME_SIZE, val, len);
        }
    }

    fn dist_read(&self, off: u64, _len: usize) -> u64 {
        match off {
            GICD_CTLR => self.gicd_ctlr as u64,
            GICD_TYPER => {
                let itlinesnumber = ((IRQ_NUM / 32) - 1) as u64;
                (1 << GICD_TYPER_RSS_SHIFT)
                    | (1 << GICD_TYPER_NO1N_SHIFT)
                    | (1 << GICD_TYPER_A3V_SHIFT)
                    | (1 << GICD_TYPER_LPIS_SHIFT)
                    | (0xf << GICD_TYPER_ID_BITS_SHIFT)
                    | itlinesnumber
            }
            GICD_IIDR => 0x43b,
            _ if (GICD_ICFGR..GICD_ICFGR + 0x100).contains(&off) => {
                let irq = ((off - GICD_ICFGR) * 4) as u32;
                if !(GIC_INTERNAL..IRQ_NUM).contains(&irq) {
                    0
                } else {
                    let value = self.edge_trigger[((irq & !0x1f) / 32) as usize];
                    let value = extract32(value, if (irq & 0x1f) != 0 { 16 } else { 0 }, 16);
                    (half_shuffle32(value) << 1) as u64
                }
            }
            _ if (GICD_IDREGS..GICD_IDREGS + 0x30).contains(&off) => {
                id_reg(off - GICD_IDREGS, self.revision, GICV3_PIDR0_DIST)
            }
            // Enable/pending/active/priority/group/route banks: reads return 0 (we track delivery
            // via the pending queue, not these banks). Keeps the driver probe happy.
            _ => 0,
        }
    }

    fn dist_write(&mut self, off: u64, val: u64, len: usize) {
        match off {
            GICD_CTLR => {
                let mask = GICD_CTLR_EN_GRP0 | GICD_CTLR_EN_GRP1NS;
                self.gicd_ctlr = (self.gicd_ctlr & !mask) | (val as u32 & mask);
            }
            _ if (GICD_ICFGR..GICD_ICFGR + 0x100).contains(&off) => {
                let irq = ((off - GICD_ICFGR) * 4) as u32;
                if !(GIC_INTERNAL..IRQ_NUM).contains(&irq) {
                    return;
                }
                let mut value = half_unshuffle32((val as u32) >> 1);
                let mut mask = 0xffff_ffffu32;
                if irq & 0x1f != 0 {
                    value <<= 16;
                    mask &= 0xffff_0000;
                } else {
                    mask &= 0xffff;
                }
                let idx = ((irq & !0x1f) / 32) as usize;
                self.edge_trigger[idx] = (self.edge_trigger[idx] & !mask) | (value & mask);
            }
            _ if (GICD_IROUTER..GICD_IROUTER + 0x2000).contains(&off) => {
                let intid = ((off - GICD_IROUTER) / 8) as usize;
                if len == 8 {
                    if let Some(r) = self.gicd_irouter.get_mut(intid) {
                        *r = val;
                    }
                } else if let Some(r) = self.gicd_irouter.get_mut(intid) {
                    // 32-bit halves of a 64-bit IROUTER.
                    let lo = (off - GICD_IROUTER) & 0x4 != 0;
                    if lo {
                        *r = (*r & 0x0000_0000_ffff_ffff) | (val << 32);
                    } else {
                        *r = (*r & 0xffff_ffff_0000_0000) | (val & 0xffff_ffff);
                    }
                }
            }
            // ISENABLER/ICENABLER/ISPENDR/IPRIORITYR/IGROUPR/... accepted as no-ops.
            _ => {}
        }
    }

    fn redist_read(&self, cpu: u64, off: u64, _len: usize) -> u64 {
        match off {
            GICR_CTLR => 0,
            GICR_IIDR => 0x43b,
            GICR_WAKER => self.gicr_waker as u64,
            GICR_TYPER => {
                let mut typer = (cpu << GICR_TYPER_AFFINITY_VALUE)
                    | (1 << GICR_TYPER_COMMON_LPI_AFF_SHIFT)
                    | (cpu << GICR_TYPER_PROCESSOR_NUMBER_SHIFT);
                if cpu == self.num_cpus - 1 {
                    typer |= 1 << GICR_TYPER_LAST_SHIFT;
                }
                typer
            }
            _ if (GICR_IDREGS..GICR_IDREGS + 0x30).contains(&off) => {
                id_reg(off - GICR_IDREGS, self.revision, GICV3_PIDR0_REDIST)
            }
            _ => 0,
        }
    }

    fn redist_write(&mut self, _cpu: u64, off: u64, val: u64, _len: usize) {
        match off {
            GICR_WAKER => {
                let mut v = val as u32 & GICR_WAKER_PROCESSOR_SLEEP;
                if v & GICR_WAKER_PROCESSOR_SLEEP != 0 {
                    v |= GICR_WAKER_CHILDREN_ASLEEP;
                }
                self.gicr_waker = v;
            }
            // IGROUPR0/ISENABLER0/ICENABLER0/ICACTIVER0/IPRIORITYR/ICFGR1: accepted as no-ops.
            GICR_IGROUPR0
            | GICR_ISENABLER0
            | GICR_ICENABLER0
            | GICR_ICACTIVER0
            | GICR_ICFGR1 => {}
            _ if (GICR_IPRIORITYR..GICR_IPRIORITYR + 0x400).contains(&off) => {}
            _ => {}
        }
    }
}

/// CoreSight ID register value at `regoffset*4` from the first ID register — the byte pattern that
/// identifies an ARM GICv3 to the guest driver (PIDR2[7:4] carries the architecture revision).
fn id_reg(byte_off: u64, revision: u8, pidr0: u8) -> u64 {
    const GICD_IDS: [u8; 12] = [
        0x44, 0x00, 0x00, 0x00, 0x92, 0xB4, 0x0B, 0x00, 0x0D, 0xF0, 0x05, 0xB1,
    ];
    let regoffset = (byte_off / 4) as usize;
    if regoffset == 4 {
        pidr0 as u64
    } else {
        let mut id = *GICD_IDS.get(regoffset).unwrap_or(&0) as u64;
        if regoffset == 6 {
            id |= (revision as u64) << 4;
        }
        id
    }
}

fn half_shuffle32(val: u32) -> u32 {
    let mut x = val;
    x = ((x & 0xFF00) << 8) | (x & 0x00FF);
    x = ((x << 4) | x) & 0x0F0F_0F0F;
    x = ((x << 2) | x) & 0x3333_3333;
    x = ((x << 1) | x) & 0x5555_5555;
    x
}

fn half_unshuffle32(val: u32) -> u32 {
    let mut x = val & 0x5555_5555;
    x = ((x >> 1) | x) & 0x3333_3333;
    x = ((x >> 2) | x) & 0x0F0F_0F0F;
    x = ((x >> 4) | x) & 0x00FF_00FF;
    x = ((x >> 8) | x) & 0x0000_FFFF;
    x
}

fn extract32(value: u32, start: u32, length: u32) -> u32 {
    (value >> start) & (!0u32 >> (32 - length))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysreg_encodings_match_reference() {
        // Values transcribed from libkrun's arm64_sys_reg! expansions.
        assert_eq!(SYSREG_ICC_IAR1_EL1, sysreg(3, 0, 0, 12, 12));
        assert_eq!(SYSREG_ICC_SGI1R_EL1, sysreg(3, 0, 5, 12, 11));
        assert_eq!(SYSREG_ICC_PMR_EL1, sysreg(3, 0, 0, 4, 6));
    }

    #[test]
    fn iar_pops_pending_then_spurious() {
        let mut gic = Gic::new(1);
        gic.set_irq(0, VTIMER_IRQ);
        assert!(gic.has_pending(0));
        assert_eq!(gic.sysreg_read(0, SYSREG_ICC_IAR1_EL1), Some(VTIMER_IRQ as u64));
        assert!(!gic.has_pending(0));
        assert_eq!(gic.sysreg_read(0, SYSREG_ICC_IAR1_EL1), Some(1023));
    }

    #[test]
    fn sgi_write_targets_listed_cores() {
        let mut gic = Gic::new(4);
        // target_list bits 0b1010 (cpus 1 and 3), intid 3.
        let val = (3u64 << 24) | 0b1010;
        match gic.sysreg_write(0, SYSREG_ICC_SGI1R_EL1, val) {
            Some(SgiRequest::Sgi(t)) => assert_eq!(t, vec![(1, 3), (3, 3)]),
            None => panic!("expected SGI request"),
        }
    }

    #[test]
    fn dist_typer_and_pidr2_identify_gicv3() {
        let gic = Gic::new(1);
        assert_eq!(gic.mmio_read(GICD_BASE + GICD_IIDR, 4), 0x43b);
        // PIDR2 is IDREGS regoffset 6; low nibble [7:4] must be the revision (3).
        let pidr2 = gic.mmio_read(GICD_BASE + GICD_IDREGS + 6 * 4, 4);
        assert_eq!((pidr2 >> 4) & 0xf, 3);
    }

    #[test]
    fn last_redistributor_sets_last_bit() {
        let gic = Gic::new(2);
        let typer0 = gic.mmio_read(GICR_BASE + GICR_TYPER, 8);
        let typer1 = gic.mmio_read(GICR_BASE + GICR_FRAME_SIZE + GICR_TYPER, 8);
        assert_eq!((typer0 >> GICR_TYPER_LAST_SHIFT) & 1, 0);
        assert_eq!((typer1 >> GICR_TYPER_LAST_SHIFT) & 1, 1);
    }
}
