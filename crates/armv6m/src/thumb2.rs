//! Thumb-2: the 32-bit encodings ARMv7E-M adds, and what each means.
//!
//! Decoding follows the manual's tables (ARMv7-M ARM A5.3), one function
//! per group, in the manual's order: load/store multiple, dual and
//! exclusive, data processing with a shifted register, a modified
//! immediate and a plain one, branches and miscellaneous control, single
//! loads and stores, data processing with registers, and the multiplies
//! and divides. The DSP extension's instructions — saturating and parallel
//! arithmetic, the halfword multiplies — sit in the groups the manual puts
//! them in. Coprocessor space is the FPU's, in [`crate::fpu`].
//!
//! An encoding the manual calls UNDEFINED is a [`Fault::Undefined`], and so
//! is one with a should-be bit wrong, as QEMU decodes them. Where the manual
//! says UNPREDICTABLE because of *which* register an instruction names — a
//! pc or sp as an operand or a destination — the instruction simply uses
//! it, which is what QEMU does and what the corpus in `tests/thumb2.rs`
//! holds this model to: reading the pc gives the address four on, and
//! writing it is a branch.

use crate::exec::{Flow, Shift, add_with_carry, shift_c};
use crate::{Fault, Machine};

/// `ThumbExpandImm_C` (A5.3.2): a modified immediate, and the carry its
/// rotation produced.
///
/// A replicated pattern of a zero byte is UNPREDICTABLE; it expands to zero
/// here, as in QEMU.
pub fn thumb_expand_imm_c(imm12: u32, carry: bool) -> Option<(u32, bool)> {
    let imm8 = imm12 & 0xFF;
    if imm12 >> 10 == 0 {
        let value = match imm12 >> 8 & 3 {
            0 => imm8,
            1 => imm8 << 16 | imm8,
            2 => imm8 << 24 | imm8 << 8,
            _ => imm8 * 0x0101_0101,
        };
        return Some((value, carry));
    }
    let value = (0x80 | imm12 & 0x7F).rotate_right(imm12 >> 7);
    Some((value, value >> 31 != 0))
}

/// `DecodeImmShift` (A7.4.2): a shift encoded as two type bits and five of
/// amount, where an amount of zero means 32 for the right shifts and RRX
/// for a rotate.
pub fn decode_imm_shift(kind: u32, imm5: u32) -> (Shift, u32) {
    match kind {
        0 => (Shift::Lsl, imm5),
        1 => (Shift::Lsr, if imm5 == 0 { 32 } else { imm5 }),
        2 => (Shift::Asr, if imm5 == 0 { 32 } else { imm5 }),
        _ if imm5 == 0 => (Shift::Rrx, 1),
        _ => (Shift::Ror, imm5),
    }
}

/// `SignedSatQ`: `value` saturated to an `n`-bit signed range, and whether
/// it had to be.
pub fn signed_sat(value: i64, n: u32) -> (u32, bool) {
    let max = (1i64 << (n - 1)) - 1;
    let min = -(1i64 << (n - 1));
    if value > max {
        (max as u32, true)
    } else if value < min {
        (min as u32, true)
    } else {
        (value as u32, false)
    }
}

/// `UnsignedSatQ`: `value` saturated to `0..2^n`.
pub fn unsigned_sat(value: i64, n: u32) -> (u32, bool) {
    let max = (1i64 << n) - 1;
    if value > max {
        (max as u32, true)
    } else if value < 0 {
        (0, true)
    } else {
        (value as u32, false)
    }
}

/// Bits `hi..=lo` of `x`.
fn bits(x: u32, hi: u32, lo: u32) -> u32 {
    x >> lo & ((1u64 << (hi - lo + 1)) - 1) as u32
}

fn bit(x: u32, n: u32) -> bool {
    x >> n & 1 != 0
}

/// The four halves of a parallel add or subtract (A5.3.16).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lanes {
    Add16,
    Asx,
    Sax,
    Sub16,
    Add8,
    Sub8,
}

/// How a parallel instruction treats each lane's result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parallel {
    /// SADD16 and friends: wrap, and set GE.
    Plain,
    /// QADD16: saturate, no GE.
    Saturating,
    /// SHADD16: halve, no GE.
    Halving,
}

impl Machine {
    /// Executes a 32-bit Thumb-2 instruction, `hw1` its first halfword.
    pub(crate) fn execute_thumb2(&mut self, hw1: u32, hw2: u32) -> Result<Flow, Fault> {
        let undefined = Fault::Undefined { instr: hw1 << 16 | hw2 };
        let op1 = bits(hw1, 12, 11);
        let op2 = bits(hw1, 10, 4);
        let next = self.cpu.pc.wrapping_add(4);
        self.branch_to = None;
        let flow = match op1 {
            0b01 => {
                if op2 & 0b1100100 == 0b0000000 {
                    self.load_store_multiple(hw1, hw2)
                } else if op2 & 0b1100100 == 0b0000100 {
                    self.load_store_dual(hw1, hw2)
                } else if op2 & 0b1100000 == 0b0100000 {
                    self.data_shifted_register(hw1, hw2)
                } else {
                    self.coprocessor(hw1, hw2)
                }
            }
            0b10 if bit(hw2, 15) => return self.branches_and_control(hw1, hw2),
            0b10 if op2 & 0b0100000 == 0 => self.data_modified_immediate(hw1, hw2),
            0b10 => self.data_plain_immediate(hw1, hw2),
            _ => {
                if op2 & 0b1000000 != 0 {
                    self.coprocessor(hw1, hw2)
                } else if op2 & 0b1110001 == 0b0000000 {
                    self.store_single(hw1, hw2)
                } else if op2 & 0b1100111 == 0b0000001
                    || op2 & 0b1100111 == 0b0000011
                    || op2 & 0b1100111 == 0b0000101
                {
                    self.load_single(hw1, hw2)
                } else if op2 & 0b1110000 == 0b0100000 {
                    self.data_register(hw1, hw2)
                } else if op2 & 0b1111000 == 0b0110000 {
                    self.multiply(hw1, hw2)
                } else if op2 & 0b1111000 == 0b0111000 {
                    self.long_multiply(hw1, hw2)
                } else {
                    Err(undefined)
                }
            }
        }?;
        match (flow, self.branch_to.take()) {
            (_, Some(target)) => {
                self.cpu.t = target & 1 != 0;
                self.cpu.pc = target & !1;
            }
            (Step::Next, None) => self.cpu.pc = next,
            (Step::Branched, None) => {}
        }
        Ok(Flow::Next)
    }

    fn undefined32(hw1: u32, hw2: u32) -> Fault {
        Fault::Undefined { instr: hw1 << 16 | hw2 }
    }

    /// Writes a result. A result written to the pc is a branch, taken when
    /// the instruction completes. ARMv7-M calls a 32-bit one UNPREDICTABLE;
    /// QEMU makes it interworking, so bit 0 decides T, and so does this.
    fn put(&mut self, d: u32, value: u32) {
        if d == 15 {
            self.branch_to = Some(value);
        } else {
            self.w(d, value);
        }
    }

    /// Flags for a logical result: N and Z from it, C from the shifter.
    fn logical_flags(&mut self, result: u32, carry: bool) {
        self.cpu.n = result >> 31 != 0;
        self.cpu.z = result == 0;
        self.cpu.c = carry;
    }

    pub(crate) fn arith(&mut self, x: u32, y: u32, carry: bool, setflags: bool) -> u32 {
        let (result, c, v) = add_with_carry(x, y, carry);
        if setflags {
            self.cpu.n = result >> 31 != 0;
            self.cpu.z = result == 0;
            self.cpu.c = c;
            self.cpu.v = v;
        }
        result
    }

    /// The data-processing operations two groups share (A5.3.1, A5.3.11):
    /// `op` with its first operand and a second already shifted or
    /// expanded, and the carry that produced it. Answers whether the
    /// result goes to `d`.
    #[allow(clippy::too_many_arguments)]
    fn data_operation(
        &mut self,
        hw1: u32,
        hw2: u32,
        op: u32,
        setflags: bool,
        n: u32,
        d: u32,
        y: u32,
        carry: bool,
    ) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        // MOV and MVN have no Rn: read none, lest following values take
        // the pc for what the result was made of.
        let x = if n == 15 && matches!(op, 0b0010 | 0b0011) { 0 } else { self.r(n) };
        let c = self.cpu.c;
        // TST, TEQ, CMN and CMP: the operation, flags only.
        let test = d == 15 && setflags && matches!(op, 0b0000 | 0b0100 | 0b1000 | 0b1101);
        let logical = |m: &mut Machine, result: u32| -> Result<Step, Fault> {
            if setflags {
                m.logical_flags(result, carry);
            }
            if !test {
                // MOV.W may set the sp; nothing else logical may.
                
                m.put(d, result);
            }
            Ok(Step::Next)
        };
        match op {
            0b0000 => logical(self, x & y),
            0b0001 => logical(self, x & !y),
            0b0010 => logical(self, if n == 15 { y } else { x | y }),
            0b0011 => logical(self, if n == 15 { !y } else { x | !y }),
            0b0100 => logical(self, x ^ y),
            0b1000 | 0b1010 | 0b1011 | 0b1101 | 0b1110 => {
                let (a, b, cin) = match op {
                    0b1000 => (x, y, false),
                    0b1010 => (x, y, c),
                    0b1011 => (x, !y, c),
                    0b1101 => (x, !y, true),
                    _ => (!x, y, true),
                };
                let result = self.arith(a, b, cin, setflags);
                if test && matches!(op, 0b1000 | 0b1101) {
                    return Ok(Step::Next);
                }
                self.put(d, result);
                Ok(Step::Next)
            }
            _ => Err(undefined),
        }
    }

    /// A5.3.11, with a shifted register for the second operand.
    fn data_shifted_register(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let op = bits(hw1, 8, 5);
        let setflags = bit(hw1, 4);
        let n = bits(hw1, 3, 0);
        let d = bits(hw2, 11, 8);
        let m = bits(hw2, 3, 0);
        let imm5 = bits(hw2, 14, 12) << 2 | bits(hw2, 7, 6);
        let kind = bits(hw2, 5, 4);
        if bit(hw2, 15) {
            return Err(Self::undefined32(hw1, hw2));
        }
        if op == 0b0110 {
            // PKHBT and PKHTB: a halfword of each, the second shifted.
            if setflags || kind & 1 != 0 {
                return Err(Self::undefined32(hw1, hw2));
            }
            let (shift, amount) = decode_imm_shift(kind, imm5);
            let (y, _) = shift_c(shift, self.r(m), amount, self.cpu.c);
            let x = self.r(n);
            let result = if kind == 0b10 { x & 0xFFFF_0000 | y & 0xFFFF } else { y & 0xFFFF_0000 | x & 0xFFFF };
            self.put(d, result);
            return Ok(Step::Next);
        }
        let (shift, amount) = decode_imm_shift(kind, imm5);
        let (y, carry) = shift_c(shift, self.r(m), amount, self.cpu.c);
        self.data_operation(hw1, hw2, op, setflags, n, d, y, carry)
    }

    /// A5.3.1, with a modified immediate.
    fn data_modified_immediate(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let op = bits(hw1, 8, 5);
        let setflags = bit(hw1, 4);
        let n = bits(hw1, 3, 0);
        let d = bits(hw2, 11, 8);
        let imm12 = (hw1 >> 10 & 1) << 11 | bits(hw2, 14, 12) << 8 | hw2 & 0xFF;
        let (y, carry) = thumb_expand_imm_c(imm12, self.cpu.c).ok_or(Self::undefined32(hw1, hw2))?;
        self.data_operation(hw1, hw2, op, setflags, n, d, y, carry)
    }

    /// A5.3.3: twelve- and sixteen-bit immediates, saturation, bitfields.
    fn data_plain_immediate(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let op = bits(hw1, 8, 4);
        let n = bits(hw1, 3, 0);
        let d = bits(hw2, 11, 8);
        let imm12 = (hw1 >> 10 & 1) << 11 | bits(hw2, 14, 12) << 8 | hw2 & 0xFF;
        let imm5 = bits(hw2, 14, 12) << 2 | bits(hw2, 7, 6);
        // MOVW's and MOVT's Rn bits are their immediate's top four: no
        // register is read.
        let x = if matches!(op, 0b00100 | 0b01100) { 0 } else { self.r(n) };
        match op {
            // ADDW and SUBW; from the pc, ADR.
            0b00000 | 0b01010 => {
                let base = if n == 15 { self.cpu.pc.wrapping_add(4) & !3 } else { x };
                let result = if op == 0 { base.wrapping_add(imm12) } else { base.wrapping_sub(imm12) };
                self.put(d, result);
            }
            0b00100 | 0b01100 => {
                let imm16 = n << 12 | imm12;
                let value = if op == 0b00100 { imm16 } else { self.r(d) & 0xFFFF | imm16 << 16 };
                self.put(d, value);
            }
            // The rest have no i bit, and a zero in hw2's bit 5.
            _ if op >= 0b10000 && (bit(hw1, 10) || bit(hw2, 5)) => return Err(undefined),
            // SSAT, SSAT16, USAT, USAT16.
            0b10000 | 0b10010 | 0b11000 | 0b11010 => {
                let signed = op & 0b01000 == 0;
                let sat = bits(hw2, 4, 0);
                if op & 0b00010 != 0 && imm5 == 0 {
                    // The sixteen-bit forms: both halves, no shift.
                    let (lo, hi) = (x as u16 as i16 as i64, (x >> 16) as u16 as i16 as i64);
                    let (rl, ql, rh, qh) = if signed {
                        let (rl, ql) = signed_sat(lo, sat + 1);
                        let (rh, qh) = signed_sat(hi, sat + 1);
                        (rl, ql, rh, qh)
                    } else {
                        let (rl, ql) = unsigned_sat(lo, sat);
                        let (rh, qh) = unsigned_sat(hi, sat);
                        (rl, ql, rh, qh)
                    };
                    self.put(d, rh << 16 | rl & 0xFFFF);
                    self.cpu.q |= ql || qh;
                } else {
                    let kind = if op & 0b00010 != 0 { 2 } else { 0 };
                    let (shift, amount) = decode_imm_shift(kind, imm5);
                    let (shifted, _) = shift_c(shift, x, amount, self.cpu.c);
                    let (result, q) =
                        if signed { signed_sat(shifted as i32 as i64, sat + 1) } else { unsigned_sat(shifted as i32 as i64, sat) };
                    self.put(d, result);
                    self.cpu.q |= q;
                }
            }
            // SBFX and UBFX.
            0b10100 | 0b11100 => {
                let lsb = imm5;
                let width = bits(hw2, 4, 0) + 1;
                if lsb + width > 32 {
                    return Err(undefined);
                }
                let field = (x >> lsb) & ((1u64 << width) - 1) as u32;
                let value = if op == 0b10100 { ((field << (32 - width)) as i32 >> (32 - width)) as u32 } else { field };
                self.put(d, value);
            }
            // BFI, and with no source, BFC.
            0b10110 => {
                let lsb = imm5;
                let msb = bits(hw2, 4, 0);
                if msb < lsb {
                    return Err(undefined);
                }
                let mask = (((1u64 << (msb - lsb + 1)) - 1) as u32) << lsb;
                let source = if n == 15 { 0 } else { x << lsb };
                self.put(d, self.r(d) & !mask | source & mask);
            }
            _ => return Err(undefined),
        }
        Ok(Step::Next)
    }

    /// A5.3.4: the wide branches, BL, MSR and MRS, hints and barriers.
    fn branches_and_control(&mut self, hw1: u32, hw2: u32) -> Result<Flow, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let next = self.cpu.pc.wrapping_add(4);
        let s = hw1 >> 10 & 1;
        let j1 = hw2 >> 13 & 1;
        let j2 = hw2 >> 11 & 1;
        let op = bits(hw1, 10, 4);
        match bits(hw2, 14, 12) {
            0b000 | 0b010 => {
                if op & 0b0111000 != 0b0111000 {
                    // B<cond>.W: S:J2:J1:imm6:imm11:'0'.
                    if self.cpu.in_it_block() {
                        return Err(undefined);
                    }
                    let imm = s << 20 | j2 << 19 | j1 << 18 | bits(hw1, 5, 0) << 12 | (hw2 & 0x7FF) << 1;
                    let target = next.wrapping_add(((imm << 11) as i32 >> 11) as u32);
                    self.cpu.pc = if self.condition(bits(hw1, 9, 6)) { target } else { next };
                    return Ok(Flow::Next);
                }
                match op {
                    0b0111000 | 0b0111001 if hw2 & 0x2000 == 0 => {
                        let mask = bits(hw2, 11, 10);
                        self.msr_masked(bits(hw1, 3, 0), hw2 & 0xFF, mask)?;
                    }
                    0b0111010 => {
                        if bits(hw2, 10, 8) != 0 {
                            // The wide CPS, which M-profile does not have.
                            return Err(undefined);
                        }
                        // NOP, YIELD, WFE, WFI, SEV, DBG: only WFI does
                        // anything, as in the 16-bit forms.
                        if hw2 & 0xFF == 3 {
                            self.sleeping = true;
                        }
                    }
                    0b0111011 => match bits(hw2, 7, 4) {
                        0b0010 => self.exclusive = None,
                        // DSB, DMB, ISB: every access here completes
                        // before the next instruction.
                        0b0100..=0b0110 => {}
                        _ => return Err(undefined),
                    },
                    0b0111110 | 0b0111111 => {
                        let d = bits(hw2, 11, 8);
                        let value = self.mrs(hw2 & 0xFF);
                        self.put(d, value);
                    }
                    _ => return Err(undefined),
                }
                self.cpu.pc = next;
                Ok(Flow::Next)
            }
            0b001 | 0b011 | 0b101 | 0b111 => {
                // B.W and BL: S:I1:I2:imm10:imm11:'0', I = NOT(J XOR S).
                let i1 = !(j1 ^ s) & 1;
                let i2 = !(j2 ^ s) & 1;
                let imm = s << 24 | i1 << 23 | i2 << 22 | (hw1 & 0x3FF) << 12 | (hw2 & 0x7FF) << 1;
                let target = next.wrapping_add(((imm << 7) as i32 >> 7) as u32);
                if self.cpu.in_it_block() && self.cpu.itstate & 0xF != 0b1000 {
                    // A branch inside an IT block has to be its last.
                    return Err(undefined);
                }
                if bit(hw2, 14) {
                    self.cpu.set_reg(14, next | 1);
                }
                self.cpu.pc = target;
                Ok(Flow::Next)
            }
            // UDF.W, permanently undefined; and BLX, which would switch to
            // an ARM state M-profile does not have.
            _ => Err(undefined),
        }
    }

    /// LDM, STM, and the wide PUSH and POP (A5.3.5). Every one is a word
    /// access that must be aligned.
    fn load_store_multiple(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let op = bits(hw1, 8, 7);
        let wback = bit(hw1, 5);
        let load = bit(hw1, 4);
        let n = bits(hw1, 3, 0);
        let list = hw2 & 0xFFFF;
        // SRS and RFE are A-profile; a list of one register has its own
        // encoding, as an LDR or STR; and one that writes back cannot also
        // transfer its base.
        if !matches!(op, 0b01 | 0b10) || n == 15 || list.count_ones() < 2 || wback && list & 1 << n != 0 {
            return Err(undefined);
        }
        let base = self.r(n);
        let size = 4 * list.count_ones();
        let start = if op == 0b01 { base } else { base.wrapping_sub(size) };
        let after = if op == 0b01 { base.wrapping_add(size) } else { start };
        if start & 3 != 0 {
            return Err(Fault::Unaligned { addr: start });
        }
        let mut addr = start;
        let mut target = None;
        for reg in 0..16 {
            if list & 1 << reg == 0 {
                continue;
            }
            if load {
                let value = self.load(addr, 4)?;
                if reg == 15 {
                    target = Some(value);
                } else if reg == 13 {
                    self.cpu.set_sp(value);
                } else {
                    self.cpu.set_reg(reg, value);
                }
            } else {
                self.store(addr, 4, self.r(reg as u32))?;
            }
            addr = addr.wrapping_add(4);
        }
        if wback && !(load && list & 1 << n != 0) {
            self.put(n, after);
        }
        if let Some(target) = target {
            self.bx(target)?;
            return Ok(Step::Branched);
        }
        Ok(Step::Next)
    }

    /// LDRD and STRD, the exclusives, and TBB and TBH (A5.3.6).
    fn load_store_dual(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let op1 = bits(hw1, 8, 7);
        let op2 = bits(hw1, 5, 4);
        let n = bits(hw1, 3, 0);
        let t = bits(hw2, 15, 12);
        let t2 = bits(hw2, 11, 8);
        let imm8 = hw2 & 0xFF;
        // The exclusives may not transfer the pc or sp, nor be based on
        // the pc, and a store's status may not be its source or base. TBB
        // and TBH are in this group too, and may use the pc.
        let table = op1 == 0b01 && op2 == 0b01 && bits(hw2, 7, 4) < 2;
        if op1 < 0b10 && op2 < 0b10 && !table {
            if n == 15 || t == 13 || t == 15 {
                return Err(undefined);
            }
            if op2 == 0b00 {
                let status = if op1 == 0b00 { t2 } else { hw2 & 0xF };
                if status == 13 || status == 15 || status == t || status == n {
                    return Err(undefined);
                }
            }
        }
        match (op1, op2) {
            (0b00, 0b00) => {
                // STREX: the status is Rd, in bits 11:8.
                let addr = self.r(n).wrapping_add(imm8 << 2);
                let d = t2;
                self.store_exclusive(d, t, addr, 4)?;
            }
            (0b00, 0b01) => {
                if t2 != 0b1111 {
                    return Err(undefined);
                }
                let addr = self.r(n).wrapping_add(imm8 << 2);
                let value = self.load_exclusive(addr, 4)?;
                self.put(t, value);
            }
            (0b01, 0b00) => {
                if t2 != 0b1111 {
                    return Err(undefined);
                }
                let size = match bits(hw2, 7, 4) {
                    0b0100 => 1,
                    0b0101 => 2,
                    _ => return Err(undefined),
                };
                let d = hw2 & 0xF;
                self.store_exclusive(d, t, self.r(n), size)?;
            }
            (0b01, 0b01) => match bits(hw2, 7, 4) {
                0b0000 | 0b0001 if t != 0b1111 || t2 != 0 => return Err(undefined),
                0b0100 | 0b0101 if t2 != 0b1111 || hw2 & 0xF != 0b1111 => return Err(undefined),
                0b0000 | 0b0001 => {
                    if self.cpu.in_it_block() && self.cpu.itstate & 0xF != 0b1000 {
                        return Err(undefined);
                    }
                    let m = hw2 & 0xF;
                    let base = self.r(n);
                    let offset = if bit(hw2, 4) {
                        self.load_unaligned(base.wrapping_add(self.r(m) << 1), 2)?
                    } else {
                        self.load_unaligned(base.wrapping_add(self.r(m)), 1)?
                    };
                    self.cpu.pc = self.cpu.pc.wrapping_add(4).wrapping_add(offset << 1);
                    return Ok(Step::Branched);
                }
                0b0100 | 0b0101 => {
                    let size = if bit(hw2, 4) { 2 } else { 1 };
                    let value = self.load_exclusive(self.r(n), size)?;
                    self.put(t, value);
                }
                _ => return Err(undefined),
            },
            _ => {
                // LDRD and STRD, immediate (and LDRD literal).
                let index = bit(hw1, 8);
                let add = bit(hw1, 7);
                let wback = bit(hw1, 5);
                let load = bit(hw1, 4);
                if !index && !wback {
                    return Err(undefined);
                }
                let base = if n == 15 { self.cpu.pc.wrapping_add(4) & !3 } else { self.r(n) };
                if n == 15 && (wback || !load) {
                    return Err(undefined);
                }
                let offset = if add { base.wrapping_add(imm8 << 2) } else { base.wrapping_sub(imm8 << 2) };
                let addr = if index { offset } else { base };
                if addr & 3 != 0 {
                    return Err(Fault::Unaligned { addr });
                }
                if load {
                    let first = self.load(addr, 4)?;
                    let second = self.load(addr.wrapping_add(4), 4)?;
                    self.put(t, first);
                    self.put(t2, second);
                } else {
                    self.store(addr, 4, self.r(t))?;
                    self.store(addr.wrapping_add(4), 4, self.r(t2))?;
                }
                if wback {
                    self.put(n, offset);
                }
            }
        }
        Ok(Step::Next)
    }

    /// LDREX: a load that marks its address for a STREX to find.
    fn load_exclusive(&mut self, addr: u32, size: u32) -> Result<u32, Fault> {
        if addr & (size - 1) != 0 {
            return Err(Fault::Unaligned { addr });
        }
        let value = self.load(addr, size)?;
        self.exclusive = Some(addr);
        Ok(value)
    }

    /// STREX: a store that happens only if nothing cleared the monitor
    /// since the LDREX — an exception entry or return, a CLREX — and that
    /// says in `d` whether it did.
    fn store_exclusive(&mut self, d: u32, t: u32, addr: u32, size: u32) -> Result<(), Fault> {
        if addr & (size - 1) != 0 {
            return Err(Fault::Unaligned { addr });
        }
        // A store the monitor refuses is still checked: the MPU's verdict
        // does not depend on whether the store would have happened.
        if self.exclusive.take() == Some(addr) {
            self.store(addr, size, self.r(t))?;
            self.put(d, 0);
        } else {
            self.check(addr, crate::Access::Write)?;
            self.put(d, 1);
        }
        Ok(())
    }

    /// The address of a single load or store, and what to write back
    /// (A5.3.7–A5.3.10). `None` for an encoding the group leaves undefined.
    fn single_address(&mut self, hw1: u32, hw2: u32) -> Option<(u32, Option<u32>, bool)> {
        let n = bits(hw1, 3, 0);
        if n == 15 {
            // Literal: the pc, word-aligned, and a twelve-bit offset either
            // way.
            let base = self.cpu.pc.wrapping_add(4) & !3;
            let imm12 = hw2 & 0xFFF;
            return Some((if bit(hw1, 7) { base.wrapping_add(imm12) } else { base.wrapping_sub(imm12) }, None, false));
        }
        let base = self.r(n);
        if bit(hw1, 7) {
            return Some((base.wrapping_add(hw2 & 0xFFF), None, false));
        }
        if bits(hw2, 11, 6) == 0 {
            let m = hw2 & 0xF;
            return Some((base.wrapping_add(self.r(m) << bits(hw2, 5, 4)), None, false));
        }
        let imm8 = hw2 & 0xFF;
        match bits(hw2, 11, 8) {
            // LDRT and friends: the access is made as if unprivileged.
            0b1110 => Some((base.wrapping_add(imm8), None, true)),
            0b1100 => Some((base.wrapping_sub(imm8), None, false)),
            puw if puw & 0b1001 == 0b1001 => {
                let index = puw & 0b0100 != 0;
                let add = puw & 0b0010 != 0;
                let offset = if add { base.wrapping_add(imm8) } else { base.wrapping_sub(imm8) };
                Some((if index { offset } else { base }, Some(offset), false))
            }
            _ => None,
        }
    }

    fn store_single(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let size = match bits(hw1, 6, 5) {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return Err(undefined),
        };
        let n = bits(hw1, 3, 0);
        let t = bits(hw2, 15, 12);
        if n == 15 {
            return Err(undefined);
        }
        let (addr, wback, unprivileged) = self.single_address(hw1, hw2).ok_or(undefined)?;
        let value = self.r(t);
        if unprivileged {
            self.store_unprivileged(addr, size, value)?;
        } else {
            self.store_unaligned(addr, size, value)?;
        }
        if let Some(offset) = wback {
            self.put(n, offset);
        }
        Ok(Step::Next)
    }

    fn load_single(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let signed = bit(hw1, 8);
        let size = match bits(hw1, 6, 5) {
            0 => 1,
            1 => 2,
            2 if !signed => 4,
            _ => return Err(undefined),
        };
        let n = bits(hw1, 3, 0);
        let t = bits(hw2, 15, 12);
        let (addr, wback, unprivileged) = self.single_address(hw1, hw2).ok_or(undefined)?;
        if t == 15 && size != 4 && wback.is_none() && !unprivileged {
            // PLD and PLI: hints, and the model has no cache to warm. The
            // forms that write back, and the unprivileged ones, are not
            // hints but loads to the pc.
            return Ok(Step::Next);
        }
        let raw = if unprivileged { self.load_unprivileged(addr, size)? } else { self.load_unaligned(addr, size)? };
        let value = match (size, signed) {
            (1, true) => raw as u8 as i8 as u32,
            (2, true) => raw as u16 as i16 as u32,
            _ => raw,
        };
        if let Some(offset) = wback {
            self.put(n, offset);
        }
        if t == 15 {
            if self.cpu.in_it_block() && self.cpu.itstate & 0xF != 0b1000 {
                return Err(undefined);
            }
            self.bx(value)?;
            return Ok(Step::Branched);
        }
        self.put(t, value);
        Ok(Step::Next)
    }

    /// A5.3.12: shifts by a register, extends, parallel arithmetic, and
    /// the miscellaneous operations.
    fn data_register(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        if bits(hw2, 15, 12) != 0b1111 {
            return Err(undefined);
        }
        let op1 = bits(hw1, 7, 4);
        let op2 = bits(hw2, 7, 4);
        let n = bits(hw1, 3, 0);
        let d = bits(hw2, 11, 8);
        let m = hw2 & 0xF;
        let y = self.r(m);
        if op1 & 0b1000 == 0 && op2 == 0 {
            // LSL, LSR, ASR, ROR by a register.
            let shift = [Shift::Lsl, Shift::Lsr, Shift::Asr, Shift::Ror][(op1 >> 1) as usize];
            let (result, carry) = shift_c(shift, self.r(n), y & 0xFF, self.cpu.c);
            if op1 & 1 != 0 {
                self.logical_flags(result, carry);
            }
            self.put(d, result);
            return Ok(Step::Next);
        }
        if op1 & 0b1000 == 0 && op2 & 0b1100 == 0b1000 {
            // The extends, with an add when there is a first operand.
            let rotated = y.rotate_right(8 * bits(hw2, 5, 4));
            let add = if n == 15 { 0 } else { self.r(n) };
            let value = match op1 {
                0b0000 => add.wrapping_add(rotated as u16 as i16 as u32),
                0b0001 => add.wrapping_add(rotated & 0xFFFF),
                0b0010 | 0b0011 => {
                    let (lo, hi) = if op1 == 0b0010 {
                        (rotated as u8 as i8 as u32, (rotated >> 16) as u8 as i8 as u32)
                    } else {
                        (rotated & 0xFF, rotated >> 16 & 0xFF)
                    };
                    (add as u16).wrapping_add(lo as u16) as u32 | (((add >> 16) as u16).wrapping_add(hi as u16) as u32) << 16
                }
                0b0100 => add.wrapping_add(rotated as u8 as i8 as u32),
                0b0101 => add.wrapping_add(rotated & 0xFF),
                _ => return Err(undefined),
            };
            self.put(d, value);
            return Ok(Step::Next);
        }
        let x = self.r(n);
        if op1 & 0b1000 != 0 && op2 & 0b1000 == 0 {
            let lanes = match op1 & 0b111 {
                0b001 => Lanes::Add16,
                0b010 => Lanes::Asx,
                0b110 => Lanes::Sax,
                0b101 => Lanes::Sub16,
                0b000 => Lanes::Add8,
                0b100 => Lanes::Sub8,
                _ => return Err(undefined),
            };
            let kind = match op2 & 0b11 {
                0b00 => Parallel::Plain,
                0b01 => Parallel::Saturating,
                0b10 => Parallel::Halving,
                _ => return Err(undefined),
            };
            let result = self.parallel(lanes, kind, op2 & 0b100 == 0, x, y);
            self.put(d, result);
            return Ok(Step::Next);
        }
        if op1 & 0b1100 == 0b1000 && op2 & 0b1100 == 0b1000 {
            let value = match (op1 & 3, op2 & 3) {
                (0b00, op) => {
                    // QADD, QDADD, QSUB, QDSUB: Rm is the first operand.
                    let (first, second) = (y as i32 as i64, x as i32 as i64);
                    let second = if op & 1 != 0 {
                        let (doubled, q) = signed_sat(2 * second, 32);
                        self.cpu.q |= q;
                        doubled as i32 as i64
                    } else {
                        second
                    };
                    let (result, q) = signed_sat(if op & 2 == 0 { first + second } else { first - second }, 32);
                    self.cpu.q |= q;
                    result
                }
                (0b01, 0b00) => y.swap_bytes(),
                (0b01, 0b01) => (y & 0x00FF_00FF) << 8 | (y >> 8 & 0x00FF_00FF),
                (0b01, 0b10) => y.reverse_bits(),
                (0b01, _) => (y as u16).swap_bytes() as i16 as u32,
                (0b10, 0b00) => {
                    // SEL: each byte from the first operand where its GE
                    // bit is set.
                    let mut result = 0;
                    for lane in 0..4 {
                        let from = if self.cpu.ge & 1 << lane != 0 { x } else { y };
                        result |= from & 0xFF << (8 * lane);
                    }
                    result
                }
                (0b11, 0b00) => y.leading_zeros(),
                _ => return Err(undefined),
            };
            self.put(d, value);
            return Ok(Step::Next);
        }
        Err(undefined)
    }

    /// The parallel adds and subtracts: two halfwords or four bytes at
    /// once, signed or unsigned, wrapping, saturating or halving.
    fn parallel(&mut self, lanes: Lanes, kind: Parallel, signed: bool, x: u32, y: u32) -> u32 {
        let (width, count) = if matches!(lanes, Lanes::Add8 | Lanes::Sub8) { (8, 4) } else { (16, 2) };
        let lane = |v: u32, i: u32| -> i64 {
            let raw = v >> (width * i) & ((1 << width) - 1);
            if signed { ((raw << (32 - width)) as i32 >> (32 - width)) as i64 } else { raw as i64 }
        };
        let mut result = 0u32;
        let mut ge = 0u8;
        for i in 0..count {
            let (a, b, add) = match lanes {
                Lanes::Add16 | Lanes::Add8 => (lane(x, i), lane(y, i), true),
                Lanes::Sub16 | Lanes::Sub8 => (lane(x, i), lane(y, i), false),
                // ASX: the low half subtracts the high, the high adds the
                // low. SAX the other way round.
                Lanes::Asx => (lane(x, i), lane(y, 1 - i), i == 1),
                Lanes::Sax => (lane(x, i), lane(y, 1 - i), i == 0),
            };
            let sum = if add { a + b } else { a - b };
            let value = match kind {
                Parallel::Plain => {
                    let set = if signed { sum >= 0 } else if add { sum >= 1 << width } else { sum >= 0 };
                    if set {
                        let per = if width == 16 { 0b11 } else { 0b1 };
                        ge |= per << (i * (4 / count));
                    }
                    sum as u32
                }
                Parallel::Saturating => {
                    if signed { signed_sat(sum, width).0 } else { unsigned_sat(sum, width).0 }
                }
                Parallel::Halving => (sum >> 1) as u32,
            };
            result |= (value & ((1u64 << width) - 1) as u32) << (width * i);
        }
        if kind == Parallel::Plain {
            self.cpu.ge = ge;
        }
        result
    }

    /// A5.3.15: the 32-bit multiplies, and the DSP's halfword ones.
    fn multiply(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let op1 = bits(hw1, 6, 4);
        let op2 = bits(hw2, 5, 4);
        let n = bits(hw1, 3, 0);
        let a = bits(hw2, 15, 12);
        let d = bits(hw2, 11, 8);
        let m = hw2 & 0xF;
        if bits(hw2, 7, 6) != 0 {
            return Err(undefined);
        }
        let x = self.r(n);
        let y = self.r(m);
        // Ra of 1111 means "no accumulator" — except to MLS, which has no
        // form without one, and reads the pc.
        let acc = if a == 15 && !(op1 == 0b000 && op2 == 0b01) { None } else { Some(self.r(a)) };
        let half = |v: u32, top: bool| -> i64 { (if top { v >> 16 } else { v }) as u16 as i16 as i64 };
        let value = match op1 {
            0b000 => match (op2, acc) {
                (0b00, None) => x.wrapping_mul(y),
                (0b00, Some(acc)) => acc.wrapping_add(x.wrapping_mul(y)),
                (0b01, Some(acc)) => acc.wrapping_sub(x.wrapping_mul(y)),
                _ => return Err(undefined),
            },
            0b001 => {
                // SMUL<x><y> and SMLA<x><y>.
                let product = half(x, bit(hw2, 5)) * half(y, bit(hw2, 4));
                match acc {
                    None => product as u32,
                    Some(acc) => {
                        let sum = product + acc as i32 as i64;
                        self.cpu.q |= sum != sum as i32 as i64;
                        sum as u32
                    }
                }
            }
            0b010 | 0b100 if op2 & 2 == 0 => {
                // SMUAD, SMLAD, SMUSD, SMLSD, the X forms swapping y's
                // halves.
                let y = if bit(hw2, 4) { y.rotate_right(16) } else { y };
                let lo = half(x, false) * half(y, false);
                let hi = half(x, true) * half(y, true);
                let sum = if op1 == 0b010 { lo + hi } else { lo - hi } + acc.map_or(0, |acc| acc as i32 as i64);
                if op1 == 0b010 || acc.is_some() {
                    self.cpu.q |= sum != sum as i32 as i64;
                }
                sum as u32
            }
            0b011 if op2 & 2 == 0 => {
                // SMULW<y> and SMLAW<y>: a word by a halfword, the top 32
                // of the 48.
                let product = (x as i32 as i64 * half(y, bit(hw2, 4))) >> 16;
                match acc {
                    None => product as u32,
                    Some(acc) => {
                        let sum = product + acc as i32 as i64;
                        self.cpu.q |= sum != sum as i32 as i64;
                        sum as u32
                    }
                }
            }
            0b101 | 0b110 if op2 & 2 == 0 => {
                // SMMUL, SMMLA, SMMLS: the top word of a 64-bit product,
                // rounded when R is set.
                let product = x as i32 as i64 * y as i32 as i64;
                let base = acc.map_or(0, |acc| (acc as i32 as i64) << 32);
                // An SMMLS with no accumulator is UNPREDICTABLE, and QEMU
                // makes it an SMMUL.
                let wide = if op1 == 0b101 || acc.is_none() { base.wrapping_add(product) } else { base.wrapping_sub(product) };
                let rounded = if bit(hw2, 4) { wide.wrapping_add(0x8000_0000) } else { wide };
                (rounded >> 32) as u32
            }
            0b111 if op2 == 0 => {
                // USAD8 and USADA8.
                let sum: u32 = (0..4).map(|i| ((x >> (8 * i) & 0xFF) as i32 - (y >> (8 * i) & 0xFF) as i32).unsigned_abs()).sum();
                sum.wrapping_add(acc.unwrap_or(0))
            }
            _ => return Err(undefined),
        };
        self.put(d, value);
        Ok(Step::Next)
    }

    /// A5.3.16: the 64-bit multiplies and the divides.
    fn long_multiply(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        let undefined = Self::undefined32(hw1, hw2);
        let op1 = bits(hw1, 6, 4);
        let op2 = bits(hw2, 7, 4);
        let n = bits(hw1, 3, 0);
        let lo = bits(hw2, 15, 12);
        let hi = bits(hw2, 11, 8);
        let m = hw2 & 0xF;
        let x = self.r(n);
        let y = self.r(m);
        if matches!((op1, op2), (0b001, 0b1111) | (0b011, 0b1111)) {
            if lo != 0b1111 {
                return Err(undefined);
            }
            let d = hi;
            if y == 0 {
                if self.bus.scs.ccr & crate::scs::CCR_DIV_0_TRP != 0 {
                    return Err(Fault::DivideByZero);
                }
                self.put(d, 0);
                return Ok(Step::Next);
            }
            let value = if op1 == 0b001 { (x as i32).wrapping_div(y as i32) as u32 } else { x / y };
            let magnitude = |v: u32| if op1 == 0b001 { (v as i32).unsigned_abs() } else { v };
            self.stall += crate::timing::divide(magnitude(x), magnitude(y));
            self.put(d, value);
            return Ok(Step::Next);
        }
        let acc = (self.r(hi) as u64) << 32 | self.r(lo) as u64;
        let half = |v: u32, top: bool| -> i64 { (if top { v >> 16 } else { v }) as u16 as i16 as i64 };
        let value: u64 = match (op1, op2) {
            (0b000, 0b0000) => (x as i32 as i64 * y as i32 as i64) as u64,
            (0b010, 0b0000) => x as u64 * y as u64,
            (0b100, 0b0000) => acc.wrapping_add((x as i32 as i64 * y as i32 as i64) as u64),
            (0b110, 0b0000) => acc.wrapping_add(x as u64 * y as u64),
            (0b110, 0b0110) => {
                // UMAAL: both halves of the accumulator added.
                (x as u64 * y as u64).wrapping_add(self.r(lo) as u64).wrapping_add(self.r(hi) as u64)
            }
            (0b100, op) if op & 0b1100 == 0b1000 => {
                // SMLAL<x><y>.
                acc.wrapping_add((half(x, op & 2 != 0) * half(y, op & 1 != 0)) as u64)
            }
            (0b100 | 0b101, op) if op & 0b1110 == 0b1100 => {
                // SMLALD and SMLSLD.
                let y = if op & 1 != 0 { y.rotate_right(16) } else { y };
                let a = half(x, false) * half(y, false);
                let b = half(x, true) * half(y, true);
                acc.wrapping_add((if op1 == 0b100 { a + b } else { a - b }) as u64)
            }
            _ => return Err(undefined),
        };
        self.put(lo, value as u32);
        self.put(hi, (value >> 32) as u32);
        Ok(Step::Next)
    }

    /// Coprocessor space: the FPU's, when there is one.
    fn coprocessor(&mut self, hw1: u32, hw2: u32) -> Result<Step, Fault> {
        match crate::fpu::execute(self, hw1, hw2)? {
            true => Ok(Step::Next),
            false => Err(Self::undefined32(hw1, hw2)),
        }
    }

    /// MSR with the mask bits ARMv7E-M reads: for the APSR, bit 1 writes
    /// N, Z, C, V and Q and bit 0 writes GE.
    fn msr_masked(&mut self, n: u32, sysm: u32, mask: u32) -> Result<(), Fault> {
        if n == 13 || n == 15 {
            return Err(Fault::Undefined { instr: 0xF380_8000 | n << 16 | mask << 10 | sysm });
        }
        if sysm < 4 {
            let value = self.r(n);
            if mask & 2 != 0 {
                self.cpu.set_apsr(value);
            }
            if mask & 1 != 0 {
                self.cpu.ge = (value >> 16 & 0xF) as u8;
            }
            return Ok(());
        }
        self.msr(n, sysm);
        Ok(())
    }
}

/// Where an instruction leaves the pc: at the next one, or wherever it
/// branched.
pub(crate) enum Step {
    Next,
    Branched,
}
