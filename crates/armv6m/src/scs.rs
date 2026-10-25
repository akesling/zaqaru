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
//!   kernel's entry points at one priority, AIRCR to reset, VTOR;
//! - [`Mpu`], the eight PMSAv7 regions task isolation is made of.
//!
//! These are the registers the Hubris FAQ says QEMU gets wrong, so the
//! rules below are written from the ARMv6-M Architecture Reference Manual
//! rather than from what a kernel happens to need, and each says which
//! section it comes from where that is not obvious.

use crate::Access;
use crate::state::Core;

/// The lowest execution priority there is: Thread mode with nothing
/// active. ARMv6-M priorities are the top two bits of a byte, so every
/// configurable priority is one of 0, 64, 128, 192 and all of them preempt
/// this.
pub const BASE_PRIORITY: i32 = 256;

/// The exception numbers the architecture fixes.
pub mod exception {
    pub const RESET: u32 = 1;
    pub const NMI: u32 = 2;
    pub const HARD_FAULT: u32 = 3;
    pub const SVCALL: u32 = 11;
    pub const PENDSV: u32 = 14;
    pub const SYSTICK: u32 = 15;
    /// External interrupt 0. A Cortex-M0+ has at most 32.
    pub const IRQ0: u32 = 16;
    pub const IRQS: u32 = 32;

    pub fn name(n: u32) -> String {
        match n {
            RESET => "Reset".into(),
            NMI => "NMI".into(),
            HARD_FAULT => "HardFault".into(),
            SVCALL => "SVCall".into(),
            PENDSV => "PendSV".into(),
            SYSTICK => "SysTick".into(),
            n if n >= IRQ0 => format!("IRQ{}", n - IRQ0),
            n => format!("exception {n}"),
        }
    }
}

use exception::*;

/// Pending and active state for every exception, and their priorities.
///
/// Bit `n` of `pending` and `active` is exception `n`, so the whole of
/// "what could run next" is two words and a scan.
#[derive(Clone, Debug, Default)]
pub struct Nvic {
    pub pending: u64,
    pub active: u64,
    /// ISER: which external interrupts may be taken at all. The system
    /// exceptions are always enabled.
    pub enabled: u32,
    /// IPR, one byte per external interrupt, only bits 7:6 implemented.
    pub ipr: [u8; IRQS as usize],
    /// SHPR2 (SVCall in 31:30) and SHPR3 (PendSV in 23:22, SysTick in
    /// 31:30), already masked to their implemented bits.
    pub shpr2: u32,
    pub shpr3: u32,
}

impl Nvic {
    pub fn priority(&self, n: u32) -> i32 {
        match n {
            RESET => -3,
            NMI => -2,
            HARD_FAULT => -1,
            SVCALL => (self.shpr2 >> 24) as i32,
            PENDSV => (self.shpr3 >> 16 & 0xFF) as i32,
            SYSTICK => (self.shpr3 >> 24) as i32,
            n if (IRQ0..IRQ0 + IRQS).contains(&n) => self.ipr[(n - IRQ0) as usize] as i32,
            _ => BASE_PRIORITY,
        }
    }

    pub fn pend(&mut self, n: u32) {
        self.pending |= 1 << n;
    }

    pub fn unpend(&mut self, n: u32) {
        self.pending &= !(1 << n);
    }

    pub fn is_pending(&self, n: u32) -> bool {
        self.pending & 1 << n != 0
    }

    /// The pending exception that would be taken next if priority allowed:
    /// the lowest priority value, and among equals the lowest exception
    /// number (B1.5.4). External interrupts count only while enabled.
    pub fn highest_pending(&self) -> Option<(u32, i32)> {
        let mut best: Option<(u32, i32)> = None;
        let mut bits = self.pending;
        while bits != 0 {
            let n = bits.trailing_zeros();
            bits &= bits - 1;
            if n >= IRQ0 && self.enabled & 1 << (n - IRQ0) == 0 {
                continue;
            }
            let p = self.priority(n);
            if best.is_none_or(|(_, bp)| p < bp) {
                best = Some((n, p));
            }
        }
        best
    }

    /// The priority the processor is running at: the most urgent active
    /// exception's, boosted to 0 by PRIMASK (B1.5.4).
    pub fn execution_priority(&self, primask: bool) -> i32 {
        let mut p = BASE_PRIORITY;
        let mut bits = self.active;
        while bits != 0 {
            let n = bits.trailing_zeros();
            bits &= bits - 1;
            p = p.min(self.priority(n));
        }
        if primask { p.min(0) } else { p }
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
    pub regions: [Region; 8],
    /// Counts writes to the MPU's registers, so that a verdict cached
    /// against one configuration is never used under another.
    pub epoch: u32,
}

const MPU_ENABLE: u32 = 1 << 0;
const MPU_HFNMIENA: u32 = 1 << 1;
const MPU_PRIVDEFENA: u32 = 1 << 2;

impl Mpu {
    /// Whether an access is allowed. `negative_priority` is whether the
    /// processor is in HardFault or NMI, where the MPU is off unless
    /// HFNMIENA says otherwise.
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
        for region in self.regions.iter().rev() {
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
}

/// CCR.UNALIGN_TRP: fault every unaligned access, as ARMv6-M always does.
pub const CCR_UNALIGN_TRP: u32 = 1 << 3;
/// CCR.DIV_0_TRP: fault a divide by zero rather than answer zero.
pub const CCR_DIV_0_TRP: u32 = 1 << 4;
/// CCR.STKALIGN: exception frames are eight-byte aligned.
pub const CCR_STKALIGN: u32 = 1 << 9;

const CPUID_CORTEX_M0PLUS: u32 = 0x410C_C601;
/// Cortex-M7 r1p2, which is what an STM32H7 has.
const CPUID_CORTEX_M7: u32 = 0x411F_C272;
const AIRCR_VECTKEY: u32 = 0x05FA;

impl Scs {
    pub fn reset(&mut self) {
        // The epoch survives: a verdict cached before the reset was made
        // under a configuration the reset just threw away.
        let epoch = self.mpu.epoch.wrapping_add(1);
        let core = self.core;
        *self = Scs::default();
        self.mpu.epoch = epoch;
        self.core = core;
        self.ccr = match core {
            Core::CortexM0Plus => CCR_STKALIGN | CCR_UNALIGN_TRP,
            Core::CortexM7 => CCR_STKALIGN,
        };
    }

    /// Runs the clock for `n` cycles.
    pub fn tick(&mut self, n: u64) {
        if self.systick.advance(n) {
            self.nvic.pend(SYSTICK);
        }
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
        Some(match addr {
            0xE000_E010 => self.systick.csr | CSR_CLKSOURCE,
            0xE000_E014 => self.systick.rvr,
            0xE000_E018 => self.systick.cvr,
            // CALIB: NOREF, and no calibration value.
            0xE000_E01C => 1 << 31,
            0xE000_E100 | 0xE000_E180 => nvic.enabled,
            0xE000_E200 | 0xE000_E280 => (nvic.pending >> IRQ0) as u32,
            0xE000_E400..=0xE000_E41F => {
                let i = (addr - 0xE000_E400) as usize;
                u32::from_le_bytes([nvic.ipr[i], nvic.ipr[i + 1], nvic.ipr[i + 2], nvic.ipr[i + 3]])
            }
            0xE000_ED00 => match self.core {
                Core::CortexM0Plus => CPUID_CORTEX_M0PLUS,
                Core::CortexM7 => CPUID_CORTEX_M7,
            },
            0xE000_ED04 => {
                let vectpending = nvic.highest_pending().map_or(0, |(n, _)| n);
                (nvic.is_pending(NMI) as u32) << 31
                    | (nvic.is_pending(PENDSV) as u32) << 28
                    | (nvic.is_pending(SYSTICK) as u32) << 26
                    | ((nvic.pending >> IRQ0 != 0) as u32) << 22
                    | (vectpending & 0x1FF) << 12
                    | self.vectactive & 0x1FF
            }
            0xE000_ED08 => self.vtor,
            0xE000_ED0C => 0xFA05_0000,
            0xE000_ED10 => self.scr,
            0xE000_ED14 => self.ccr,
            0xE000_ED1C => nvic.shpr2,
            0xE000_ED20 => nvic.shpr3,
            // SHCSR exists, but on a Cortex-M0+ it reads as zero from the
            // processor — the Hubris fault handler is written around
            // exactly this, so the model keeps it.
            0xE000_ED24 => 0,
            0xE000_ED90 => 8 << 8,
            0xE000_ED94 => self.mpu.ctrl,
            0xE000_ED98 => self.mpu.rnr,
            0xE000_ED9C => self.mpu.regions[self.mpu.rnr as usize].rbar | self.mpu.rnr,
            0xE000_EDA0 => self.mpu.regions[self.mpu.rnr as usize].rasr,
            // The rest of the PPB — debug, the ROM table — reads as zero.
            _ => 0,
        })
    }

    /// Whether a sub-word write to `addr` should merge into the register's
    /// current value. True only of the byte-wise priority registers; for
    /// the rest a merged write would re-assert whatever set-bits it read
    /// back, which is not what a byte store to ICSR means.
    pub fn merges_subword(addr: u32) -> bool {
        matches!(addr, 0xE000_E400..=0xE000_E41F | 0xE000_ED1C | 0xE000_ED20)
    }

    pub fn write(&mut self, addr: u32, value: u32) -> Option<()> {
        let nvic = &mut self.nvic;
        match addr {
            0xE000_E010 => self.systick.csr = value & (CSR_ENABLE | CSR_TICKINT) | self.systick.csr & CSR_COUNTFLAG,
            0xE000_E014 => self.systick.rvr = value & 0x00FF_FFFF,
            // Any write clears the counter and COUNTFLAG.
            0xE000_E018 => {
                self.systick.cvr = 0;
                self.systick.csr &= !CSR_COUNTFLAG;
            }
            0xE000_E100 => nvic.enabled |= value,
            0xE000_E180 => nvic.enabled &= !value,
            0xE000_E200 => nvic.pending |= (value as u64) << IRQ0,
            0xE000_E280 => nvic.pending &= !((value as u64) << IRQ0),
            0xE000_E400..=0xE000_E41F => {
                let i = (addr - 0xE000_E400) as usize;
                for (k, byte) in value.to_le_bytes().into_iter().enumerate() {
                    nvic.ipr[i + k] = byte & 0xC0;
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
                if value >> 16 == AIRCR_VECTKEY && value & 1 << 2 != 0 {
                    self.reset_requested = true;
                }
            }
            0xE000_ED10 => self.scr = value & 0b10110,
            // ARMv6-M's CCR is fixed. ARMv7-M's takes the traps, STKALIGN,
            // BFHFNMIGN, USERSETMPEND and NONBASETHRDENA; the M7's cache
            // and branch-predictor enables read back.
            0xE000_ED14 if self.core != Core::CortexM0Plus => self.ccr = value & 0x0007_031B,
            0xE000_ED1C => nvic.shpr2 = value & 0xC000_0000,
            0xE000_ED20 => nvic.shpr3 = value & 0xC0C0_0000,
            0xE000_ED94..=0xE000_EDA0 => {
                self.mpu.epoch = self.mpu.epoch.wrapping_add(1);
                self.write_mpu(addr, value);
            }
            _ => {}
        }
        Some(())
    }

    fn write_mpu(&mut self, addr: u32, value: u32) {
        match addr {
            0xE000_ED94 => self.mpu.ctrl = value & 0b111,
            0xE000_ED98 => self.mpu.rnr = value & 7,
            0xE000_ED9C => {
                // VALID selects the region from the write itself.
                if value & 1 << 4 != 0 {
                    self.mpu.rnr = value & 7;
                }
                self.mpu.regions[self.mpu.rnr as usize].rbar = value & !0x1F;
            }
            0xE000_EDA0 => self.mpu.regions[self.mpu.rnr as usize].rasr = value,
            _ => {}
        }
    }
}
