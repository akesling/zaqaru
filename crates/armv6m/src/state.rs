//! The processor's architectural state, as a struct.
//!
//! Everything ARMv6-M says a Cortex-M0+ holds and nothing else: thirteen
//! general registers, a banked stack pointer, the link register, the
//! program counter, the four condition flags, and the handful of special
//! registers that decide privilege and which stack is current. The NVIC's
//! pending and active sets are not here — they are memory-mapped, so they
//! live with the System Control Space in [`crate::scs`].

/// Which of the two execution modes the processor is in.
///
/// Handler mode is "inside an exception": always privileged, always on the
/// main stack. Thread mode is everything else, and is where `CONTROL`
/// decides both privilege and stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Thread,
    Handler,
}

#[derive(Clone, Debug, Default)]
pub struct Cpu {
    /// r0–r12 and lr. Index 13 is never read or written: the stack pointer
    /// is banked, and lives in `msp` and `psp`.
    pub r: [u32; 15],
    pub msp: u32,
    pub psp: u32,
    /// The address of the instruction executing, or — between
    /// instructions — of the next one to execute. A read of `pc` *as an
    /// operand* is this plus four; see [`Cpu::reg`].
    pub pc: u32,
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    /// EPSR.T. M-profile has no ARM state, so the only thing this bit
    /// does is fault: an interworking branch to an even address clears it,
    /// and the *next* instruction is an INVSTATE UsageFault (HardFault, on
    /// this profile) rather than the branch itself.
    pub t: bool,
    pub mode: Mode,
    /// The exception number being handled; zero in Thread mode.
    pub ipsr: u32,
    pub primask: bool,
    /// CONTROL.nPRIV: Thread mode runs unprivileged.
    pub npriv: bool,
    /// CONTROL.SPSEL: Thread mode runs on the process stack.
    pub spsel: bool,
}

impl Cpu {
    pub fn privileged(&self) -> bool {
        self.mode == Mode::Handler || !self.npriv
    }

    /// Whether the current stack is the process stack.
    pub fn on_psp(&self) -> bool {
        self.mode == Mode::Thread && self.spsel
    }

    pub fn sp(&self) -> u32 {
        if self.on_psp() { self.psp } else { self.msp }
    }

    /// ARMv6-M has no unaligned stack pointer: bits 1:0 read as zero and
    /// ignore writes, whichever instruction writes them.
    pub fn set_sp(&mut self, value: u32) {
        if self.on_psp() {
            self.psp = value & !3;
        } else {
            self.msp = value & !3;
        }
    }

    /// Register `n` as an operand.
    pub fn reg(&self, n: usize) -> u32 {
        match n {
            13 => self.sp(),
            15 => self.pc.wrapping_add(4),
            _ => self.r[n],
        }
    }

    /// Writes register `n`. A write to the pc is a branch, and which kind
    /// of branch depends on the instruction, so the executor handles those
    /// itself and never asks this for one.
    pub fn set_reg(&mut self, n: usize, value: u32) {
        debug_assert!(n != 15, "a pc write is a branch");
        if n == 13 {
            self.set_sp(value);
        } else {
            self.r[n] = value;
        }
    }

    /// APSR: the flags in bits 31:28.
    pub fn apsr(&self) -> u32 {
        (self.n as u32) << 31 | (self.z as u32) << 30 | (self.c as u32) << 29 | (self.v as u32) << 28
    }

    pub fn set_apsr(&mut self, value: u32) {
        self.n = value & 1 << 31 != 0;
        self.z = value & 1 << 30 != 0;
        self.c = value & 1 << 29 != 0;
        self.v = value & 1 << 28 != 0;
    }

    /// xPSR as an exception frame stacks it: flags, T, and the exception
    /// number.
    pub fn xpsr(&self) -> u32 {
        self.apsr() | (self.t as u32) << 24 | self.ipsr & 0x3F
    }
}
