//! The core's debug: halting, the debug registers, the breakpoint unit, the
//! DWT's registers, and the ROM table a debugger finds them through.
//!
//! This is the half of the Cortex-M0+ debug architecture that is the
//! processor's (ARMv6-M C1): what a debugger reaches through the AHB-AP
//! once it has a debug port (see [`crate::dap`]). A probe halts the core by
//! writing DHCSR, reads registers through DCRSR and DCRDR while it is
//! halted, steps it with C_STEP, catches it at reset and HardFault through
//! DEMCR, and stops it at an address with one of the four BPU comparators.
//! What it finds where is told by the ROM table at `0xE00FF000` and the
//! CoreSight identification registers at the top of each component.
//!
//! Two things are simplified:
//!
//! - **The debug registers answer the debugger, not the processor.**
//!   ARMv6-M lets software read some of them; Hubris never does, and a
//!   processor access to any of them reads as zero and writes nothing,
//!   as the rest of the unmodelled PPB does.
//! - **The DWT has its registers and no watchpoints.** A comparator can be
//!   programmed and read back, and never matches.
//!
//! Halting is the machine's state, not a debugger's: the GDB server and a
//! debug port see the same halted core, and a run of a halted core does
//! nothing (it returns [`Outcome::Halted`]).

use crate::state::Mode;
use crate::{Machine, Outcome};

pub const DFSR: u32 = 0xE000_ED30;
pub const DHCSR: u32 = 0xE000_EDF0;
pub const DCRSR: u32 = 0xE000_EDF4;
pub const DCRDR: u32 = 0xE000_EDF8;
pub const DEMCR: u32 = 0xE000_EDFC;
pub const DWT: u32 = 0xE000_1000;
pub const BPU: u32 = 0xE000_2000;
pub const SCS: u32 = 0xE000_E000;
pub const ROM_TABLE: u32 = 0xE00F_F000;

const DBGKEY: u32 = 0xA05F;

pub const C_DEBUGEN: u32 = 1 << 0;
pub const C_HALT: u32 = 1 << 1;
pub const C_STEP: u32 = 1 << 2;
pub const C_MASKINTS: u32 = 1 << 3;
pub const S_REGRDY: u32 = 1 << 16;
pub const S_HALT: u32 = 1 << 17;
pub const S_SLEEP: u32 = 1 << 18;
pub const S_LOCKUP: u32 = 1 << 19;
pub const S_RETIRE_ST: u32 = 1 << 24;
pub const S_RESET_ST: u32 = 1 << 25;

pub const DFSR_HALTED: u32 = 1 << 0;
pub const DFSR_BKPT: u32 = 1 << 1;
pub const DFSR_DWTTRAP: u32 = 1 << 2;
pub const DFSR_VCATCH: u32 = 1 << 3;

pub const VC_CORERESET: u32 = 1 << 0;
pub const VC_HARDERR: u32 = 1 << 10;
pub const DWTENA: u32 = 1 << 24;

const DCRSR_REGWNR: u32 = 1 << 16;

/// Comparators in the BPU and the DWT, as a Cortex-M0+ has them.
const BPU_COMPARATORS: usize = 4;
const DWT_COMPARATORS: usize = 2;

#[derive(Clone, Debug, Default)]
pub struct Debug {
    /// DHCSR.C_DEBUGEN: halting debug is enabled.
    pub enabled: bool,
    /// The core is in Debug state: S_HALT.
    pub halted: bool,
    /// C_STEP: the next run executes one instruction and halts again.
    pub stepping: bool,
    /// C_MASKINTS: PendSV, SysTick and interrupts are not taken.
    pub maskints: bool,
    /// The core locked up: S_LOCKUP.
    pub lockup: bool,
    /// S_RESET_ST: the core was reset since DHCSR was last read.
    reset_seen: bool,
    /// The instruction count DHCSR was last read at, for S_RETIRE_ST.
    retired_at_read: u64,
    pub dcrdr: u32,
    pub demcr: u32,
    pub dfsr: u32,
    /// FP_CTRL.ENABLE.
    bpu_enabled: bool,
    /// FP_COMPn, as written.
    bpu: [u32; BPU_COMPARATORS],
    /// DWT_COMPn, DWT_MASKn and DWT_FUNCTIONn.
    dwt: [[u32; 3]; DWT_COMPARATORS],
    /// Whether any BPU comparator is in force, so a run checks only when
    /// one is.
    pub(crate) breaking: bool,
}

impl Debug {
    /// Puts the core in Debug state for `reason`, a DFSR bit.
    pub fn halt(&mut self, reason: u32) {
        self.halted = true;
        self.stepping = false;
        self.dfsr |= reason;
    }

    /// Whether a BPU comparator matches an instruction at `pc` (C1.8.3):
    /// COMP holds bits 28:2 of a word address, and BP_MATCH says which of
    /// its two halfwords break.
    pub fn breaks_at(&self, pc: u32) -> bool {
        self.bpu.iter().any(|&comp| {
            let halfword = match comp >> 30 {
                0b01 => pc & 2 == 0,
                0b10 => pc & 2 != 0,
                0b11 => true,
                _ => false,
            };
            comp & 1 != 0 && halfword && comp & 0x1FFF_FFFC == pc & 0x1FFF_FFFC
        })
    }

    fn rearm(&mut self) {
        self.breaking = self.enabled && self.bpu_enabled && self.bpu.iter().any(|&c| c & 1 != 0 && c >> 30 != 0);
    }

    /// Lets go of the core as a debugger leaving does: running, nothing
    /// caught, no breakpoint set.
    pub fn release(&mut self) {
        *self = Debug { retired_at_read: self.retired_at_read, ..Debug::default() };
    }
}

/// Whether `addr` is one of the registers this module answers rather than
/// the bus.
pub fn answers(addr: u32) -> bool {
    matches!(addr & !3, DFSR | DHCSR | DCRSR | DCRDR | DEMCR)
        || (DWT..DWT + 0x1000).contains(&addr)
        || (BPU..BPU + 0x1000).contains(&addr)
        || (ROM_TABLE..ROM_TABLE + 0x1000).contains(&addr)
        || (SCS + 0xFD0..SCS + 0x1000).contains(&addr)
}

/// The CoreSight identification registers at the top of each component's
/// 4 KiB: peripheral IDs 4 and 0–3, then component IDs 0–3. A Cortex-M0+'s
/// own — ARM's JEP106 code, its part numbers, and the component class (1
/// for a ROM table, 0xE for the rest).
fn identification(addr: u32) -> Option<u32> {
    if addr & 0xFFF < 0xFD0 {
        return None;
    }
    let (part, class) = match addr & !0xFFF {
        ROM_TABLE => (0x4C0, 0x1),
        SCS => (0x008, 0xE),
        DWT => (0x00A, 0xE),
        BPU => (0x00B, 0xE),
        _ => return None,
    };
    Some(match addr & 0xFFF {
        0xFD0 => 0x04,
        0xFE0 => part & 0xFF,
        0xFE4 => part >> 8 | 0xB << 4,
        0xFE8 => 0x0B,
        0xFF0 => 0x0D,
        0xFF4 => class << 4,
        0xFF8 => 0x05,
        0xFFC => 0xB1,
        _ => 0,
    })
}

/// The ROM table's entries: the SCS, the DWT and the BPU, as offsets from
/// the table, each present and in the 32-bit format.
fn rom_entry(offset: u32) -> u32 {
    let entry = |component: u32| component.wrapping_sub(ROM_TABLE) | 3;
    match offset {
        0x000 => entry(SCS),
        0x004 => entry(DWT),
        0x008 => entry(BPU),
        // MEMTYPE: the system memory map is on this bus.
        0xFCC => 1,
        _ => 0,
    }
}

impl Machine {
    /// Halts the core, as a debugger asking does.
    pub fn halt(&mut self) {
        self.debug.enabled = true;
        if !self.debug.halted {
            self.debug.halt(DFSR_HALTED);
        }
        self.wake_for_debug();
    }

    /// A halt wakes a core out of `wfi`, as on the part: the `wfi` is done,
    /// and the core resumes after it. Without this a debugger halting an
    /// idle Hubris — whose idle task is a `wfi` loop — would find it both
    /// halted and asleep, which no core is.
    fn wake_for_debug(&mut self) {
        if self.debug.halted {
            self.sleeping = false;
        }
    }

    pub fn halted(&self) -> bool {
        self.debug.halted
    }

    /// A debug register read, `None` if `addr` is not one. Reading DHCSR
    /// clears its two sticky status bits, as the architecture says; `peek`
    /// says not to.
    pub(crate) fn debug_register(&mut self, addr: u32, peek: bool) -> Option<u32> {
        let d = &mut self.debug;
        let word = addr & !3;
        Some(match word {
            DHCSR => {
                let value = (d.enabled as u32) * C_DEBUGEN
                    | (d.halted as u32) * C_HALT
                    | (d.stepping as u32) * C_STEP
                    | (d.maskints as u32) * C_MASKINTS
                    | S_REGRDY
                    | (d.halted as u32) * S_HALT
                    | ((self.sleeping && !d.halted) as u32) * S_SLEEP
                    | (d.lockup as u32) * S_LOCKUP
                    | ((self.retired != d.retired_at_read) as u32) * S_RETIRE_ST
                    | (d.reset_seen as u32) * S_RESET_ST;
                if !peek {
                    d.retired_at_read = self.retired;
                    d.reset_seen = false;
                }
                value
            }
            DCRSR => 0,
            DCRDR => d.dcrdr,
            DEMCR => d.demcr,
            DFSR => d.dfsr,
            // FP_CTRL: four code comparators, KEY reads as one.
            BPU => (BPU_COMPARATORS as u32) << 4 | 1 << 1 | d.bpu_enabled as u32,
            a if (BPU + 8..BPU + 8 + 4 * BPU_COMPARATORS as u32).contains(&a) => d.bpu[((a - BPU - 8) / 4) as usize],
            // DWT_CTRL: two comparators.
            DWT => (DWT_COMPARATORS as u32) << 28,
            // DWT_PCSR: the pc, sampled.
            a if a == DWT + 0x1C => self.cpu.pc,
            a if (DWT + 0x20..DWT + 0x20 + 0x10 * DWT_COMPARATORS as u32).contains(&a) => {
                let (n, r) = (((a - DWT - 0x20) / 0x10) as usize, ((a - DWT) & 0xF) / 4);
                if r < 3 { d.dwt[n][r as usize] } else { 0 }
            }
            a if (ROM_TABLE..ROM_TABLE + 0xFD0).contains(&a) => rom_entry(a - ROM_TABLE),
            a => match identification(a) {
                Some(value) => value,
                None if answers(a) => 0,
                None => return None,
            },
        })
    }

    /// A debug register write; false if `addr` is not one.
    pub(crate) fn set_debug_register(&mut self, addr: u32, value: u32) -> bool {
        let d = &mut self.debug;
        match addr & !3 {
            DHCSR => {
                if value >> 16 != DBGKEY {
                    return true;
                }
                if value & C_DEBUGEN == 0 {
                    // Leaving halting debug leaves Debug state (C1.6.3).
                    d.enabled = false;
                    d.halted = false;
                    d.stepping = false;
                    d.maskints = false;
                } else {
                    d.enabled = true;
                    d.maskints = value & C_MASKINTS != 0;
                    if value & C_HALT != 0 {
                        if !d.halted {
                            d.halt(DFSR_HALTED);
                        }
                    } else if d.halted {
                        d.halted = false;
                        d.stepping = value & C_STEP != 0;
                    }
                }
                d.rearm();
            }
            DCRSR => {
                // A transfer is only defined in Debug state.
                if d.halted {
                    let selector = value & 0x7F;
                    if value & DCRSR_REGWNR != 0 {
                        let word = d.dcrdr;
                        set_register(self, selector, word);
                    } else {
                        self.debug.dcrdr = register(self, selector);
                    }
                }
            }
            DCRDR => d.dcrdr = value,
            DEMCR => d.demcr = value & (VC_CORERESET | VC_HARDERR | DWTENA),
            DFSR => d.dfsr &= !value,
            BPU => {
                if value & 1 << 1 != 0 {
                    d.bpu_enabled = value & 1 != 0;
                    d.rearm();
                }
            }
            a if (BPU + 8..BPU + 8 + 4 * BPU_COMPARATORS as u32).contains(&a) => {
                d.bpu[((a - BPU - 8) / 4) as usize] = value & 0xDFFF_FFFD;
                d.rearm();
            }
            a if (DWT + 0x20..DWT + 0x20 + 0x10 * DWT_COMPARATORS as u32).contains(&a) => {
                let (n, r) = (((a - DWT - 0x20) / 0x10) as usize, ((a - DWT) & 0xF) / 4);
                if r < 3 {
                    d.dwt[n][r as usize] = value;
                }
            }
            a => return answers(a),
        }
        self.wake_for_debug();
        true
    }

    /// A load as the AHB-AP makes it: no permission check, the debug
    /// registers in place, and the side effects a read has. `None` is a
    /// bus error.
    pub fn debug_load(&mut self, addr: u32, size: u32) -> Option<u32> {
        let shift = 8 * (addr & 3);
        let mask = if size == 4 { u32::MAX } else { (1 << (8 * size)) - 1 };
        if let Some(word) = self.debug_register(addr, false) {
            return Some(word >> shift & mask);
        }
        if Self::touches_systick(addr, size) {
            self.sync_systick();
        }
        self.bus.read(addr, size).ok()
    }

    /// A store as the AHB-AP makes it. False is a bus error.
    pub fn debug_store(&mut self, addr: u32, size: u32, value: u32) -> bool {
        if answers(addr) {
            let shift = 8 * (addr & 3);
            let mask = if size == 4 { u32::MAX } else { ((1 << (8 * size)) - 1) << shift };
            let word = self.debug_register(addr, true).unwrap_or(0) & !mask | value << shift & mask;
            return self.set_debug_register(addr, word);
        }
        if Self::touches_systick(addr, size) {
            self.sync_systick();
        }
        let ok = self.bus.write(addr, size, value).is_ok();
        self.drain_edges();
        self.sample_interrupts();
        if Self::touches_systick(addr, size) {
            self.sync_systick();
        }
        ok
    }

    /// What a run that stopped by itself means for Debug state: a
    /// breakpoint or a `bkpt` halts the core, and a lockup leaves it locked
    /// up, and halted, waiting for a debugger as a real one does.
    pub(crate) fn settle(&mut self, outcome: Outcome) -> Outcome {
        match outcome {
            Outcome::Breakpoint { .. } | Outcome::Stopped { .. } => self.debug.halt(DFSR_BKPT),
            Outcome::Lockup { .. } => {
                self.debug.lockup = true;
                self.debug.halt(DFSR_HALTED);
            }
            _ => {}
        }
        outcome
    }

    /// The reset half of vector catch, and S_RESET_ST. Debug state itself
    /// is on the debug reset domain and survives a system reset.
    pub(crate) fn debug_reset(&mut self) {
        self.debug.reset_seen = true;
        self.debug.lockup = false;
        self.debug.halted = false;
        self.debug.stepping = false;
        if self.debug.enabled && self.debug.demcr & VC_CORERESET != 0 {
            self.debug.halt(DFSR_VCATCH);
        }
    }

    /// Whether HardFault entry should stop for the debugger.
    pub(crate) fn catches_hard_fault(&self) -> bool {
        self.debug.enabled && self.debug.demcr & VC_HARDERR != 0
    }
}

/// Register `n` by its DCRSR selector (C1.6.3): r0–r12, sp, lr, the
/// return address 15, xPSR 16, MSP 17, PSP 18, and 20 packing CONTROL in
/// 31:24 with PRIMASK in 0.
pub fn register(m: &Machine, n: u32) -> u32 {
    let cpu = &m.cpu;
    match n {
        15 => cpu.pc,
        0..=14 => cpu.reg(n as usize),
        16 => cpu.xpsr(),
        17 => cpu.msp,
        18 => cpu.psp,
        // FAULTMASK and BASEPRI do not exist on this profile.
        20 => ((cpu.npriv as u32) | (cpu.spsel as u32) << 1) << 24 | cpu.primask as u32,
        _ => 0,
    }
}

/// Writes register `n` by its DCRSR selector; false if there is no such
/// register.
pub fn set_register(m: &mut Machine, n: u32, value: u32) -> bool {
    let cpu = &mut m.cpu;
    match n {
        0..=12 | 14 => cpu.r[n as usize] = value,
        13 => cpu.set_sp(value),
        15 => cpu.pc = value & !1,
        16 => {
            cpu.set_apsr(value);
            cpu.t = value & 1 << 24 != 0;
        }
        17 => cpu.msp = value & !3,
        18 => cpu.psp = value & !3,
        20 => {
            cpu.primask = value & 1 != 0;
            cpu.npriv = value >> 24 & 1 != 0;
            if cpu.mode == Mode::Thread {
                cpu.spsel = value >> 25 & 1 != 0;
            }
            m.update_priority();
        }
        _ => return false,
    }
    true
}
