//! The System Control Space: the part of the address space, at
//! `0xE000_E000`, that is the processor rather than the chip.
//!
//! Four things a kernel talks to live here, and Hubris talks to all four:
//!
//! - [`SysTick`], the 24-bit down-counter the kernel's notion of time is
//!   built on;
//! - [`Nvic`], the pending and active sets and every priority, which
//!   between them decide which handler runs next;
//! - the SCB registers — ICSR to pend a context switch, SHPR to put the
//!   kernel's entry points at one priority, AIRCR to reset, VTOR, and on
//!   ARMv7-M the fault enables and the fault status registers a fault
//!   handler reads to find out what happened;
//! - [`Mpu`], the PMSAv7 regions task isolation is made of.
//!
//! These are the registers the Hubris FAQ says QEMU gets wrong, so the
//! rules below are written from the ARMv6-M and ARMv7-M Architecture
//! Reference Manuals rather than from what a kernel happens to need, and
//! each says which section it comes from where that is not obvious. The
//! two profiles differ in how much of each there is — four priority levels
//! or sixteen, eight MPU regions or sixteen, thirty-two interrupts or up to
//! two hundred and forty — and in whether faults have exceptions of their
//! own; [`Core`] decides.

use crate::Access;
use crate::state::Core;

/// The lowest execution priority there is: Thread mode with nothing
/// active. Every configurable priority, on either profile, preempts it.
pub const BASE_PRIORITY: i32 = 256;

/// The exception numbers the architecture fixes.
pub mod exception {
    pub const RESET: u32 = 1;
    pub const NMI: u32 = 2;
    pub const HARD_FAULT: u32 = 3;
    /// ARMv7-M's configurable faults.
    pub const MEM_MANAGE: u32 = 4;
    pub const BUS_FAULT: u32 = 5;
    pub const USAGE_FAULT: u32 = 6;
    pub const SVCALL: u32 = 11;
    pub const DEBUG_MONITOR: u32 = 12;
    pub const PENDSV: u32 = 14;
    pub const SYSTICK: u32 = 15;
    /// External interrupt 0.
    pub const IRQ0: u32 = 16;
    /// The most external interrupts either profile allows.
    pub const IRQS: u32 = 240;
    /// How many exception numbers there are.
    pub const COUNT: usize = (IRQ0 + IRQS) as usize;

    pub fn name(n: u32) -> String {
        match n {
            RESET => "Reset".into(),
            NMI => "NMI".into(),
            HARD_FAULT => "HardFault".into(),
            MEM_MANAGE => "MemManage".into(),
            BUS_FAULT => "BusFault".into(),
            USAGE_FAULT => "UsageFault".into(),
            SVCALL => "SVCall".into(),
            DEBUG_MONITOR => "DebugMonitor".into(),
            PENDSV => "PendSV".into(),
            SYSTICK => "SysTick".into(),
            n if n >= IRQ0 => format!("IRQ{}", n - IRQ0),
            n => format!("exception {n}"),
        }
    }
}

use exception::*;

/// A set of exception numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Exceptions(pub [u64; 4]);

impl Exceptions {
    pub fn set(&mut self, n: u32) {
        self.0[(n / 64) as usize] |= 1 << (n % 64);
    }

    pub fn clear(&mut self, n: u32) {
        self.0[(n / 64) as usize] &= !(1 << (n % 64));
    }

    pub fn contains(&self, n: u32) -> bool {
        self.0[(n / 64) as usize] & 1 << (n % 64) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    pub fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }

    /// The members, in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let n = bits.trailing_zeros();
                bits &= bits - 1;
                Some(64 * i as u32 + n)
            })
        })
    }

    /// The 32 external interrupts starting at `irq`, as a word of the
    /// NVIC's set and clear registers lays them out.
    pub fn irq_word(&self, irq: u32) -> u32 {
        (0..32).filter(|&i| self.contains(IRQ0 + irq + i)).fold(0, |w, i| w | 1 << i)
    }

    pub fn set_irq_word(&mut self, irq: u32, word: u32, value: bool) {
        for i in (0..32).filter(|&i| word & 1 << i != 0) {
            if IRQ0 + irq + i < COUNT as u32 {
                if value {
                    self.set(IRQ0 + irq + i);
                } else {
                    self.clear(IRQ0 + irq + i);
                }
            }
        }
    }
}

/// Pending and active state for every exception, and their priorities.
#[derive(Clone, Debug)]
pub struct Nvic {
    pub pending: Exceptions,
    pub active: Exceptions,
    /// ISER: which external interrupts may be taken at all. The system
    /// exceptions are always enabled.
    pub enabled: [u32; 8],
    /// IPR, one byte per external interrupt, masked to the implemented
    /// bits.
    pub ipr: [u8; IRQS as usize],
    /// SHPR1–SHPR3, one byte per system exception from MemManage (4) to
    /// SysTick (15), masked to the implemented bits; the ones a profile
    /// does not have stay zero.
    pub shpr: [u8; 12],
    /// AIRCR.PRIGROUP: how many low bits of a priority are subpriority,
    /// which orders exceptions without letting one preempt another.
    pub prigroup: u32,
    /// The implemented priority bits: the top two on ARMv6-M, the top four
    /// on an STM32H7's Cortex-M7.
    pub mask: u8,
}

impl Default for Nvic {
    fn default() -> Self {
        Nvic {
            pending: Exceptions::default(),
            active: Exceptions::default(),
            enabled: [0; 8],
            ipr: [0; IRQS as usize],
            shpr: [0; 12],
            prigroup: 0,
            mask: 0xC0,
        }
    }
}

impl Nvic {
    /// An exception's priority, subpriority bits and all.
    pub fn priority(&self, n: u32) -> i32 {
        match n {
            RESET => -3,
            NMI => -2,
            HARD_FAULT => -1,
            4..=15 => self.shpr[(n - 4) as usize] as i32,
            n if (IRQ0..IRQ0 + IRQS).contains(&n) => self.ipr[(n - IRQ0) as usize] as i32,
            _ => BASE_PRIORITY,
        }
    }

    /// A priority with its subpriority bits cleared: what preemption
    /// compares (B1.5.4).
    pub fn group(&self, priority: i32) -> i32 {
        if priority < 0 || priority >= BASE_PRIORITY {
            return priority;
        }
        priority & !((2 << self.prigroup) - 1)
    }

    pub fn pend(&mut self, n: u32) {
        self.pending.set(n);
    }

    pub fn unpend(&mut self, n: u32) {
        self.pending.clear(n);
    }

    pub fn is_pending(&self, n: u32) -> bool {
        self.pending.contains(n)
    }

    fn irq_enabled(&self, irq: u32) -> bool {
        self.enabled[(irq / 32) as usize] & 1 << (irq % 32) != 0
    }

    /// The pending exception that would be taken next if priority allowed,
    /// and its group priority: the most urgent priority, then the lowest
    /// exception number (B1.5.4). External interrupts count only while
    /// enabled.
    pub fn highest_pending(&self) -> Option<(u32, i32)> {
        let mut best: Option<(u32, i32)> = None;
        for n in self.pending.iter() {
            if n >= IRQ0 && !self.irq_enabled(n - IRQ0) {
                continue;
            }
            let p = self.priority(n);
            if best.is_none_or(|(_, bp)| p < bp) {
                best = Some((n, p));
            }
        }
        best.map(|(n, p)| (n, self.group(p)))
    }

    /// The priority the processor is running at: the most urgent active
    /// exception's group priority, boosted by BASEPRI, to 0 by PRIMASK, and
    /// to -1 by FAULTMASK (B1.5.4).
    pub fn execution_priority(&self, primask: bool, basepri: u8, faultmask: bool) -> i32 {
        let mut p = BASE_PRIORITY;
        for n in self.active.iter() {
            p = p.min(self.group(self.priority(n)));
        }
        if basepri != 0 {
            p = p.min(self.group((basepri & self.mask) as i32));
        }
        if primask {
            p = p.min(0);
        }
        if faultmask {
            p = p.min(-1);
        }
        p
    }
}

/// The system timer.
///
/// Its clock is the processor clock, and in this machine the processor
/// clock is retired instructions: one instruction, one tick. That is what
/// makes a run deterministic — when the timer fires is a pure function of
/// what executed — and it is also what lets `wfi` skip straight to the
/// next tick rather than spinning through it.
#[derive(Clone, Debug, Default)]
pub struct SysTick {
    pub csr: u32,
    pub rvr: u32,
    pub cvr: u32,
}

const CSR_ENABLE: u32 = 1 << 0;
const CSR_TICKINT: u32 = 1 << 1;
const CSR_CLKSOURCE: u32 = 1 << 2;
const CSR_COUNTFLAG: u32 = 1 << 16;

impl SysTick {
    fn enabled(&self) -> bool {
        self.csr & CSR_ENABLE != 0
    }

    /// How many clocks until the counter next reaches zero, or `None` if it
    /// never will.
    pub fn cycles_to_fire(&self) -> Option<u64> {
        if !self.enabled() {
            None
        } else if self.cvr != 0 {
            Some(self.cvr as u64)
        } else if self.rvr != 0 {
            // One clock to reload, then `rvr` down to zero.
            Some(self.rvr as u64 + 1)
        } else {
            None
        }
    }

    /// Runs the counter for `n` clocks; true if it reached zero on the way
    /// with the interrupt enabled.
    ///
    /// Each clock either reloads a counter at zero from RVR or decrements
    /// it, and a decrement that lands on zero sets COUNTFLAG and pends the
    /// interrupt (B3.3.1). Several wraps within one call pend it once, which
    /// is what the hardware would show a handler that could not run.
    pub fn advance(&mut self, mut n: u64) -> bool {
        let mut fired = false;
        while n > 0 && self.enabled() {
            if self.cvr == 0 {
                if self.rvr == 0 {
                    break;
                }
                self.cvr = self.rvr;
                n -= 1;
                continue;
            }
            if n < self.cvr as u64 {
                self.cvr -= n as u32;
                break;
            }
            n -= self.cvr as u64;
            self.cvr = 0;
            fired = true;
            self.csr |= CSR_COUNTFLAG;
            // Whole periods past the first change nothing a handler could
            // see; only the remainder positions the counter.
            n %= self.rvr as u64 + 1;
        }
        fired && self.csr & CSR_TICKINT != 0
    }
}

/// One PMSAv7 region, as RBAR and RASR hold it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Region {
    pub rbar: u32,
    pub rasr: u32,
}

impl Region {
    /// What this region says about an access: `None` if it does not cover
    /// the address (disabled, elsewhere, or a disabled subregion), so the
    /// next region down gets to answer.
    fn decide(&self, addr: u32, access: Access, privileged: bool) -> Option<bool> {
        if self.rasr & 1 == 0 {
            return None;
        }
        let size_field = self.rasr >> 1 & 0x1F;
        if size_field < 4 {
            // Under 32 bytes is UNPREDICTABLE; treat it as no region.
            return None;
        }
        let size = 1u64 << (size_field + 1);
        let mask = (size - 1) as u32;
        if addr & !mask != self.rbar & !mask {
            return None;
        }
        // Regions of 256 bytes and up split into eight subregions, any of
        // which SRD can switch off (B3.5.8).
        if size >= 256 {
            let sub = ((addr & mask) as u64 / (size / 8)) as u32;
            if self.rasr >> 8 & 1 << sub != 0 {
                return None;
            }
        }
        let read = access != Access::Write;
        let allowed = match self.rasr >> 24 & 7 {
            0b001 => privileged,
            0b010 => privileged || read,
            0b011 => true,
            0b101 => privileged && read,
            0b110 | 0b111 => read,
            _ => false,
        };
        let xn = self.rasr & 1 << 28 != 0;
        Some(allowed && !(access == Access::Execute && xn))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Mpu {
    pub ctrl: u32,
    pub rnr: u32,
    /// Eight on a Cortex-M0+, sixteen on an STM32H7's Cortex-M7; the rest
    /// stay disabled.
    pub regions: [Region; 16],
    /// How many regions there are, as MPU_TYPE.DREGION says.
    pub count: usize,
    /// Counts writes to the MPU's registers, so that a verdict cached
    /// against one configuration is never used under another.
    pub epoch: u32,
}

const MPU_ENABLE: u32 = 1 << 0;
const MPU_HFNMIENA: u32 = 1 << 1;
const MPU_PRIVDEFENA: u32 = 1 << 2;

impl Mpu {
    /// Whether an access is allowed. `negative_priority` is whether the
    /// processor is running at a negative priority — in HardFault or NMI,
    /// or with FAULTMASK set — where the MPU is off unless HFNMIENA says
    /// otherwise.
    pub fn permits(&self, addr: u32, access: Access, privileged: bool, negative_priority: bool) -> bool {
        // The PPB is never subject to the MPU, and never available to
        // unprivileged code (B3.5.1).
        if (0xE000_0000..0xE010_0000).contains(&addr) {
            return privileged && access != Access::Execute;
        }
        let enabled =
            self.ctrl & MPU_ENABLE != 0 && !(negative_priority && self.ctrl & MPU_HFNMIENA == 0);
        if !enabled {
            return default_map_permits(addr, access);
        }
        // Higher-numbered regions take precedence where they overlap.
        for region in self.regions[..self.count].iter().rev() {
            if let Some(allowed) = region.decide(addr, access, privileged) {
                return allowed;
            }
        }
        privileged && self.ctrl & MPU_PRIVDEFENA != 0 && default_map_permits(addr, access)
    }
}

/// The default memory map's one restriction that matters here: the
/// peripheral, device and system regions are execute-never (B3.1).
fn default_map_permits(addr: u32, access: Access) -> bool {
    !(access == Access::Execute && ((0x4000_0000..0x6000_0000).contains(&addr) || addr >= 0xA000_0000))
}

/// What a fault handler on ARMv7-M reads to find out what happened.
#[derive(Clone, Copy, Debug, Default)]
pub struct FaultStatus {
    /// CFSR: MMFSR in bits 7:0, BFSR in 15:8, UFSR in 31:16.
    pub cfsr: u32,
    pub hfsr: u32,
    pub mmfar: u32,
    pub bfar: u32,
}

/// CFSR's bits (B3.2.15).
pub mod cfsr {
    pub const IACCVIOL: u32 = 1 << 0;
    pub const DACCVIOL: u32 = 1 << 1;
    pub const MUNSTKERR: u32 = 1 << 3;
    pub const MSTKERR: u32 = 1 << 4;
    pub const MLSPERR: u32 = 1 << 5;
    pub const MMARVALID: u32 = 1 << 7;
    pub const IBUSERR: u32 = 1 << 8;
    pub const PRECISERR: u32 = 1 << 9;
    pub const UNSTKERR: u32 = 1 << 11;
    pub const STKERR: u32 = 1 << 12;
    pub const LSPERR: u32 = 1 << 13;
    pub const BFARVALID: u32 = 1 << 15;
    pub const UNDEFINSTR: u32 = 1 << 16;
    pub const INVSTATE: u32 = 1 << 17;
    pub const INVPC: u32 = 1 << 18;
    pub const NOCP: u32 = 1 << 19;
    pub const UNALIGNED: u32 = 1 << 24;
    pub const DIVBYZERO: u32 = 1 << 25;
}

/// HFSR.VECTTBL and HFSR.FORCED.
pub const HFSR_VECTTBL: u32 = 1 << 1;
pub const HFSR_FORCED: u32 = 1 << 30;

#[derive(Clone, Debug, Default)]
pub struct Scs {
    pub nvic: Nvic,
    pub systick: SysTick,
    pub mpu: Mpu,
    pub vtor: u32,
    pub scr: u32,
    /// ICSR.VECTACTIVE, which is IPSR. Kept here because a read of ICSR
    /// must answer it and the bus cannot see the processor.
    pub vectactive: u32,
    /// Set by a write of SYSRESETREQ to AIRCR; the run loop answers it.
    pub reset_requested: bool,
    /// Which processor these registers belong to.
    pub core: Core,
    /// CCR, as ARMv7-M lets software set it. ARMv6-M's is fixed.
    pub ccr: u32,
    /// SHCSR's enables for MemManage, BusFault and UsageFault, in bits
    /// 18:16 as SHCSR holds them. The rest of SHCSR is the NVIC's state.
    pub fault_enables: u32,
    pub faults: FaultStatus,
    /// CPACR: coprocessor access. CP10 and CP11, the FPU, in bits 23:20.
    pub cpacr: u32,
    /// How many external interrupts the part has, rounded up to a multiple
    /// of 32 as ICTR reports them.
    pub irq_lines: u32,
    /// FPCCR, FPCAR and FPDSCR: the FPU's context control.
    pub fpccr: u32,
    pub fpcar: u32,
    pub fpdscr: u32,
    /// Where the part boots, which VTOR comes out of reset holding.
    pub boot: u32,
    /// CSSELR: which cache CCSIDR describes.
    pub csselr: u32,
}

/// CCR.UNALIGN_TRP: fault every unaligned access, as ARMv6-M always does.
pub const CCR_UNALIGN_TRP: u32 = 1 << 3;
/// CCR.DIV_0_TRP: fault a divide by zero rather than answer zero.
pub const CCR_DIV_0_TRP: u32 = 1 << 4;
/// CCR.STKALIGN: exception frames are eight-byte aligned.
pub const CCR_STKALIGN: u32 = 1 << 9;
/// CCR.NONBASETHRDENA: an exception may return to Thread mode with others
/// still active.
pub const CCR_NONBASETHRDENA: u32 = 1 << 0;
/// SHCSR's fault enables.
pub const SHCSR_MEMFAULTENA: u32 = 1 << 16;
pub const SHCSR_BUSFAULTENA: u32 = 1 << 17;
pub const SHCSR_USGFAULTENA: u32 = 1 << 18;

const CPUID_CORTEX_M0PLUS: u32 = 0x410C_C601;
/// Cortex-M7 r1p2, which is what an STM32H7 has.
const CPUID_CORTEX_M7: u32 = 0x411F_C272;
const AIRCR_VECTKEY: u32 = 0x05FA;

/// SHCSR's active and pending bits, and the exception each stands for
/// (B3.2.13).
const SHCSR_ACTIVE: [(u32, u32); 7] =
    [(0, MEM_MANAGE), (1, BUS_FAULT), (3, USAGE_FAULT), (7, SVCALL), (8, DEBUG_MONITOR), (10, PENDSV), (11, SYSTICK)];
const SHCSR_PENDED: [(u32, u32); 4] = [(12, USAGE_FAULT), (13, MEM_MANAGE), (14, BUS_FAULT), (15, SVCALL)];

impl Scs {
    pub fn reset(&mut self) {
        // The epoch survives: a verdict cached before the reset was made
        // under a configuration the reset just threw away.
        let epoch = self.mpu.epoch.wrapping_add(1);
        let core = self.core;
        let irq_lines = self.irq_lines.max(32);
        let boot = self.boot;
        *self = Scs::default();
        self.mpu.epoch = epoch;
        self.core = core;
        self.irq_lines = irq_lines;
        self.boot = boot;
        self.vtor = boot;
        match core {
            Core::CortexM0Plus => {
                self.ccr = CCR_STKALIGN | CCR_UNALIGN_TRP;
                self.nvic.mask = 0xC0;
                self.mpu.count = 8;
            }
            Core::CortexM7 => {
                self.ccr = CCR_STKALIGN;
                self.nvic.mask = 0xF0;
                self.mpu.count = 16;
                self.fpccr = crate::fpu::FPCCR_ASPEN | crate::fpu::FPCCR_LSPEN;
            }
        }
    }

    fn v7(&self) -> bool {
        self.core != Core::CortexM0Plus
    }

    /// Runs the clock for `n` cycles.
    pub fn tick(&mut self, n: u64) {
        if self.systick.advance(n) {
            self.nvic.pend(SYSTICK);
        }
    }

    /// Whether a configurable fault's exception is enabled; disabled, a
    /// fault escalates to HardFault.
    pub fn fault_enabled(&self, n: u32) -> bool {
        let bit = match n {
            MEM_MANAGE => SHCSR_MEMFAULTENA,
            BUS_FAULT => SHCSR_BUSFAULTENA,
            USAGE_FAULT => SHCSR_USGFAULTENA,
            _ => return true,
        };
        self.fault_enables & bit != 0
    }

    /// A word read. `None` is a bus fault. Reading SysTick's CSR clears
    /// COUNTFLAG, which is the one read in here with an effect; [`peek`]
    /// is the same read without it.
    ///
    /// [`peek`]: Scs::peek
    pub fn read(&mut self, addr: u32) -> Option<u32> {
        let value = self.peek(addr);
        if addr == 0xE000_E010 {
            self.systick.csr &= !CSR_COUNTFLAG;
        }
        value
    }

    pub fn peek(&self, addr: u32) -> Option<u32> {
        let nvic = &self.nvic;
        let v7 = self.v7();
        Some(match addr {
            // ICTR: lines in blocks of 32, less one.
            0xE000_E004 if v7 => self.irq_lines / 32 - 1,
            0xE000_E010 => self.systick.csr | CSR_CLKSOURCE,
            0xE000_E014 => self.systick.rvr,
            0xE000_E018 => self.systick.cvr,
            // CALIB: NOREF, and no calibration value.
            0xE000_E01C => 1 << 31,
            0xE000_E100..=0xE000_E33F => {
                let word = (addr & 0x7F) / 4;
                if word >= self.irq_lines / 32 || !v7 && word > 0 {
                    return Some(0);
                }
                match addr & !0x7F {
                    0xE000_E100 | 0xE000_E180 => nvic.enabled[word as usize],
                    0xE000_E200 | 0xE000_E280 => nvic.pending.irq_word(32 * word),
                    0xE000_E300 if v7 => nvic.active.irq_word(32 * word),
                    _ => 0,
                }
            }
            0xE000_E400..=0xE000_E4EF => {
                let i = (addr - 0xE000_E400) as usize;
                u32::from_le_bytes([nvic.ipr[i], nvic.ipr[i + 1], nvic.ipr[i + 2], nvic.ipr[i + 3]])
            }
            0xE000_ED00 => match self.core {
                Core::CortexM0Plus => CPUID_CORTEX_M0PLUS,
                Core::CortexM7 => CPUID_CORTEX_M7,
            },
            0xE000_ED04 => {
                let vectpending = nvic.highest_pending().map_or(0, |(n, _)| n);
                let irq_pending = nvic.pending.iter().any(|n| n >= IRQ0);
                // RETTOBASE: the handler running is the only one active.
                let rettobase = v7 && nvic.active.count() <= 1;
                (nvic.is_pending(NMI) as u32) << 31
                    | (nvic.is_pending(PENDSV) as u32) << 28
                    | (nvic.is_pending(SYSTICK) as u32) << 26
                    | (irq_pending as u32) << 22
                    | (vectpending & 0x1FF) << 12
                    | (rettobase as u32) << 11
                    | self.vectactive & 0x1FF
            }
            0xE000_ED08 => self.vtor,
            0xE000_ED0C => 0xFA05_0000 | if v7 { nvic.prigroup << 8 } else { 0 },
            0xE000_ED10 => self.scr,
            0xE000_ED14 => self.ccr,
            0xE000_ED18..=0xE000_ED23 => {
                let i = (addr - 0xE000_ED18) as usize;
                u32::from_le_bytes([nvic.shpr[i], nvic.shpr[i + 1], nvic.shpr[i + 2], nvic.shpr[i + 3]])
            }
            // SHCSR exists, but on a Cortex-M0+ it reads as zero from the
            // processor — the Hubris fault handler is written around
            // exactly this, so the model keeps it.
            0xE000_ED24 if v7 => {
                let mut value = self.fault_enables;
                for (bit, n) in SHCSR_ACTIVE {
                    value |= (nvic.active.contains(n) as u32) << bit;
                }
                for (bit, n) in SHCSR_PENDED {
                    value |= (nvic.pending.contains(n) as u32) << bit;
                }
                value
            }
            0xE000_ED28 if v7 => self.faults.cfsr,
            0xE000_ED2C if v7 => self.faults.hfsr,
            0xE000_ED34 if v7 => self.faults.mmfar,
            0xE000_ED38 if v7 => self.faults.bfar,
            0xE000_ED88 if v7 => self.cpacr,
            // The caches' identification (B4.8): CLIDR, a level of separate
            // instruction and data caches; CTR, 32-byte lines; CCSIDR, the
            // M7's 16 KiB of each as an H7 has them, the data cache 4-way
            // and write-back, the instruction cache 2-way.
            0xE000_ED78 if v7 => 0x0900_0003,
            0xE000_ED7C if v7 => 0x8303_C003,
            0xE000_ED80 if v7 => if self.csselr & 1 == 0 { 0xF00F_E019 } else { 0x201F_E009 },
            0xE000_ED84 if v7 => self.csselr,
            // ITM's stimulus ports, whose reads say whether the FIFO can take
            // a write. It always can: what is written goes nowhere.
            0xE000_0000..=0xE000_007F if v7 => 1,
            0xE000_EF34 if v7 => self.fpccr,
            0xE000_EF38 if v7 => self.fpcar,
            0xE000_EF3C if v7 => self.fpdscr,
            // MVFR0–2: an FPv5 FPU with single and double precision, as an
            // STM32H7's Cortex-M7 has.
            0xE000_EF40 if v7 => 0x1011_0221,
            0xE000_EF44 if v7 => 0x1200_0011,
            0xE000_EF48 if v7 => 0x0000_0040,
            // MPU_TYPE: DREGION, separate instruction and data maps not.
            0xE000_ED90 => (self.mpu.count as u32) << 8,
            0xE000_ED94 => self.mpu.ctrl,
            0xE000_ED98 => self.mpu.rnr,
            // RBAR and RASR, and on ARMv7-M their three aliases.
            0xE000_ED9C..=0xE000_EDA3 | 0xE000_EDA4..=0xE000_EDBB if v7 || addr <= 0xE000_EDA3 => {
                let region = &self.mpu.regions[self.mpu.rnr as usize];
                if (addr - 0xE000_ED9C) % 8 == 0 { region.rbar | self.mpu.rnr } else { region.rasr }
            }
            // The rest of the PPB — debug, the ROM table — reads as zero.
            _ => 0,
        })
    }

    /// Whether a sub-word write to `addr` should merge into the register's
    /// current value. True only of the byte-wise priority registers; for
    /// the rest a merged write would re-assert whatever set-bits it read
    /// back, which is not what a byte store to ICSR means.
    pub fn merges_subword(addr: u32) -> bool {
        matches!(addr, 0xE000_E400..=0xE000_E4EF | 0xE000_ED18..=0xE000_ED23)
    }

    pub fn write(&mut self, addr: u32, value: u32) -> Option<()> {
        let v7 = self.v7();
        let irq_lines = self.irq_lines;
        let nvic = &mut self.nvic;
        match addr {
            0xE000_E010 => self.systick.csr = value & (CSR_ENABLE | CSR_TICKINT) | self.systick.csr & CSR_COUNTFLAG,
            0xE000_E014 => self.systick.rvr = value & 0x00FF_FFFF,
            // Any write clears the counter and COUNTFLAG.
            0xE000_E018 => {
                self.systick.cvr = 0;
                self.systick.csr &= !CSR_COUNTFLAG;
            }
            0xE000_E100..=0xE000_E2BF => {
                let word = (addr & 0x7F) / 4;
                if word >= irq_lines / 32 || !v7 && word > 0 {
                    return Some(());
                }
                let at = 32 * word;
                match addr & !0x7F {
                    0xE000_E100 => nvic.enabled[word as usize] |= value,
                    0xE000_E180 => nvic.enabled[word as usize] &= !value,
                    0xE000_E200 => nvic.pending.set_irq_word(at, value, true),
                    _ => nvic.pending.set_irq_word(at, value, false),
                }
            }
            0xE000_E400..=0xE000_E4EF => {
                let i = (addr - 0xE000_E400) as usize;
                for (k, byte) in value.to_le_bytes().into_iter().enumerate() {
                    if i + k < (irq_lines as usize).min(IRQS as usize) {
                        nvic.ipr[i + k] = byte & nvic.mask;
                    }
                }
            }
            0xE000_ED04 => {
                if value & 1 << 31 != 0 {
                    nvic.pend(NMI);
                }
                if value & 1 << 28 != 0 {
                    nvic.pend(PENDSV);
                } else if value & 1 << 27 != 0 {
                    nvic.unpend(PENDSV);
                }
                if value & 1 << 26 != 0 {
                    nvic.pend(SYSTICK);
                } else if value & 1 << 25 != 0 {
                    nvic.unpend(SYSTICK);
                }
            }
            0xE000_ED08 => self.vtor = value & !0x7F,
            0xE000_ED0C => {
                if value >> 16 == AIRCR_VECTKEY {
                    if value & 1 << 2 != 0 {
                        self.reset_requested = true;
                    }
                    if v7 {
                        nvic.prigroup = value >> 8 & 7;
                    }
                }
            }
            0xE000_ED10 => self.scr = value & 0b10110,
            // ARMv6-M's CCR is fixed. ARMv7-M's takes the traps, BFHFNMIGN,
            // USERSETMPEND and NONBASETHRDENA, and keeps STKALIGN, which the
            // M7 has as one; its cache and branch-predictor enables read back.
            0xE000_ED14 if v7 => self.ccr = value & 0x0007_011B | CCR_STKALIGN,
            0xE000_ED18..=0xE000_ED23 => {
                let i = (addr - 0xE000_ED18) as usize;
                for (k, byte) in value.to_le_bytes().into_iter().enumerate() {
                    let n = 4 + i + k;
                    // ARMv6-M has only SVCall, PendSV and SysTick's.
                    let implemented = if v7 { !matches!(n, 7..=10 | 13) } else { matches!(n, 11 | 14 | 15) };
                    if implemented {
                        nvic.shpr[i + k] = byte & nvic.mask;
                    }
                }
            }
            0xE000_ED24 if v7 => {
                self.fault_enables = value & (SHCSR_MEMFAULTENA | SHCSR_BUSFAULTENA | SHCSR_USGFAULTENA);
                // The active and pending bits are writable, and software
                // that has to tidy up after a derived fault — Hubris's
                // fault handler — writes them.
                for (bit, n) in SHCSR_ACTIVE {
                    if value & 1 << bit != 0 {
                        nvic.active.set(n);
                    } else {
                        nvic.active.clear(n);
                    }
                }
                for (bit, n) in SHCSR_PENDED {
                    if value & 1 << bit != 0 {
                        nvic.pending.set(n);
                    } else {
                        nvic.pending.clear(n);
                    }
                }
            }
            0xE000_ED28 if v7 => self.faults.cfsr &= !value,
            0xE000_ED2C if v7 => self.faults.hfsr &= !value,
            0xE000_ED34 if v7 => self.faults.mmfar = value,
            0xE000_ED38 if v7 => self.faults.bfar = value,
            0xE000_ED88 if v7 => self.cpacr = value & 0x00F0_0000,
            0xE000_ED84 if v7 => self.csselr = value & 1,
            // ASPEN, LSPEN and the context bits; LSPACT stays clear, since
            // stacking is never lazy here.
            0xE000_EF34 if v7 => self.fpccr = value & 0xC000_013A,
            0xE000_EF38 if v7 => self.fpcar = value & !7,
            0xE000_EF3C if v7 => self.fpdscr = value & 0x07C0_0000,
            0xE000_ED94..=0xE000_EDBB => {
                self.mpu.epoch = self.mpu.epoch.wrapping_add(1);
                self.write_mpu(addr, value);
            }
            // STIR: pends an interrupt by number.
            0xE000_EF00 if v7 => {
                let irq = value & 0x1FF;
                if irq < irq_lines.min(IRQS) {
                    nvic.pend(IRQ0 + irq);
                }
            }
            _ => {}
        }
        Some(())
    }

    fn write_mpu(&mut self, addr: u32, value: u32) {
        let regions = self.mpu.count as u32;
        match addr {
            0xE000_ED94 => self.mpu.ctrl = value & 0b111,
            0xE000_ED98 => self.mpu.rnr = value & (regions - 1),
            _ if addr > 0xE000_EDA3 && !self.v7() => {}
            _ if (addr - 0xE000_ED9C) % 8 == 0 => {
                // VALID selects the region from the write itself.
                if value & 1 << 4 != 0 {
                    self.mpu.rnr = value & 0xF & (regions - 1);
                }
                self.mpu.regions[self.mpu.rnr as usize].rbar = value & !0x1F;
            }
            _ => self.mpu.regions[self.mpu.rnr as usize].rasr = value,
        }
    }
}
