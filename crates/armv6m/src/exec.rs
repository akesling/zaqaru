//! Execution: what each 16-bit Thumb instruction means, and the IT block
//! around it.
//!
//! ARMv6-M is the whole 16-bit Thumb set less a few, plus six 32-bit
//! encodings — `bl`, `msr`, `mrs`, and the three barriers. ARMv7E-M has the
//! same 16-bit set plus CBZ, CBNZ and IT, and the rest of Thumb-2 in
//! [`crate::thumb2`]. Decoding follows the manual's own tables (A5.2), one
//! arm per group, and every encoding the architecture leaves undefined is a
//! [`Fault::Undefined`] naming its bits: a fault to the guest, never a
//! silent guess.
//!
//! Two rules hold throughout:
//!
//! - **`cpu.pc` is the executing instruction until the instruction is
//!   done.** A fault returns with it untouched, so the HardFault's stacked
//!   return address is the faulting instruction, as the architecture says.
//!   Only a completed instruction moves it.
//! - **Every guest access goes through [`Machine::load`] and
//!   [`Machine::store`]**, which is where alignment and the MPU are checked.

use crate::scs::exception::SVCALL;
use crate::state::{Core, Mode};
use crate::{Access, Fault, Machine, Outcome};

pub(crate) enum Flow {
    Next,
    Halt(Outcome),
}

/// `AddWithCarry` (A2.2.1): the sum, and the carry and overflow it
/// produced.
pub fn add_with_carry(x: u32, y: u32, carry: bool) -> (u32, bool, bool) {
    let wide = x as u64 + y as u64 + carry as u64;
    let result = wide as u32;
    let signed = x as i32 as i64 + y as i32 as i64 + carry as i64;
    (result, wide > u32::MAX as u64, signed != result as i32 as i64)
}

#[derive(Clone, Copy)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
    /// Rotate right by one through the carry; the amount is ignored.
    Rrx,
}

/// `Shift_C` (A2.2.2), for an amount of any size: an amount of zero leaves
/// both the value and the carry alone.
pub fn shift_c(kind: Shift, x: u32, amount: u32, carry: bool) -> (u32, bool) {
    if amount == 0 {
        return (x, carry);
    }
    match kind {
        Shift::Lsl => match amount {
            1..=31 => (x << amount, x >> (32 - amount) & 1 != 0),
            32 => (0, x & 1 != 0),
            _ => (0, false),
        },
        Shift::Lsr => match amount {
            1..=31 => (x >> amount, x >> (amount - 1) & 1 != 0),
            32 => (0, x >> 31 != 0),
            _ => (0, false),
        },
        Shift::Asr => {
            let amount = amount.min(32);
            let result = if amount == 32 { ((x as i32) >> 31) as u32 } else { ((x as i32) >> amount) as u32 };
            (result, (x as i32 as i64 >> (amount - 1)) & 1 != 0)
        }
        Shift::Ror => {
            let result = x.rotate_right(amount % 32);
            (result, result >> 31 != 0)
        }
        Shift::Rrx => ((carry as u32) << 31 | x >> 1, x & 1 != 0),
    }
}

fn low(i: u32, shift: u32) -> u32 {
    i >> shift & 7
}

impl Machine {
    pub(crate) fn r(&self, n: u32) -> u32 {
        self.cpu.reg(n as usize)
    }

    pub(crate) fn w(&mut self, n: u32, value: u32) {
        self.cpu.set_reg(n as usize, value);
    }

    fn nz(&mut self, value: u32) {
        self.cpu.n = value >> 31 != 0;
        self.cpu.z = value == 0;
    }

    fn add_flags(&mut self, x: u32, y: u32, carry: bool) -> u32 {
        let (result, c, v) = add_with_carry(x, y, carry);
        self.nz(result);
        self.cpu.c = c;
        self.cpu.v = v;
        result
    }

    pub(crate) fn condition(&self, cond: u32) -> bool {
        let c = &self.cpu;
        match cond {
            0x0 => c.z,
            0x1 => !c.z,
            0x2 => c.c,
            0x3 => !c.c,
            0x4 => c.n,
            0x5 => !c.n,
            0x6 => c.v,
            0x7 => !c.v,
            0x8 => c.c && !c.z,
            0x9 => !c.c || c.z,
            0xA => c.n == c.v,
            0xB => c.n != c.v,
            0xC => !c.z && c.n == c.v,
            0xD => c.z || c.n != c.v,
            _ => true,
        }
    }

    pub(crate) fn check(&mut self, addr: u32, access: Access) -> Result<(), Fault> {
        if self.permits(addr, access, self.cpu.privileged()) {
            Ok(())
        } else {
            Err(Fault::MemManage { addr, access })
        }
    }

    pub(crate) fn load(&mut self, addr: u32, size: u32) -> Result<u32, Fault> {
        if addr & (size - 1) != 0 {
            return Err(Fault::Unaligned { addr });
        }
        self.check(addr, Access::Read)?;
        if Self::touches_systick(addr, size) {
            self.sync_systick();
        }
        self.bus.read(addr, size).map_err(|_| Fault::Bus { addr })
    }

    pub(crate) fn store(&mut self, addr: u32, size: u32, value: u32) -> Result<(), Fault> {
        if addr & (size - 1) != 0 {
            return Err(Fault::Unaligned { addr });
        }
        self.check(addr, Access::Write)?;
        let systick = Self::touches_systick(addr, size);
        if systick {
            self.sync_systick();
        }
        self.bus.write(addr, size, value).map_err(|_| Fault::Bus { addr })?;
        if systick {
            // The write may have started, stopped or reloaded it.
            self.sync_systick();
        }
        if !self.bus.soc.edges.is_empty() || !self.bus.soc.serial.is_empty() {
            self.drain_edges();
        }
        // A store to a peripheral or to the NVIC can raise or unmask an
        // interrupt line; nothing below the peripherals can.
        if addr >= 0x4000_0000 {
            self.sample_interrupts();
        }
        Ok(())
    }

    fn fetch(&mut self, addr: u32) -> Result<u32, Fault> {
        self.check(addr, Access::Execute)?;
        self.bus.fetch(addr).ok_or(Fault::InstructionBus { addr })
    }

    /// `BXWritePC`: an interworking branch, which in Handler mode is how an
    /// exception returns.
    pub(crate) fn bx(&mut self, target: u32) -> Result<(), Fault> {
        if self.cpu.mode == Mode::Handler && target >> 28 == 0xF {
            return self.exception_return(target);
        }
        self.cpu.t = target & 1 != 0;
        self.cpu.pc = target & !1;
        Ok(())
    }

    /// Executes the instruction at `cpu.pc`.
    ///
    /// Inside an IT block the instruction's condition is the block's
    /// (A7.3.3): one that fails is skipped, as a NOP would be, and either
    /// way the block advances. It advances *before* the instruction runs,
    /// so that an exception return inside it — which restores a stacked IT
    /// state — is not then advanced again; a fault puts it back, so the
    /// frame records the IT state of the instruction that faulted.
    pub(crate) fn execute(&mut self) -> Result<Flow, Fault> {
        let pc = self.cpu.pc;
        let first = self.fetch(pc)?;
        let wide = first >> 11 >= 0b11101;
        let second = if wide { self.fetch(pc.wrapping_add(2))? } else { 0 };
        if self.core == Core::CortexM0Plus {
            return if wide { self.execute32(first, second) } else { self.execute16(first) };
        }
        let it = self.cpu.itstate;
        if it & 0xF != 0 {
            self.cpu.itstate = if it & 7 == 0 { 0 } else { it & 0xE0 | (it << 1) & 0x1F };
            // BKPT runs whatever the condition says.
            if !self.condition(u32::from(it >> 4)) && first & 0xFF00 != 0xBE00 {
                self.cpu.pc = pc.wrapping_add(if wide { 4 } else { 2 });
                return Ok(Flow::Next);
            }
        }
        self.it_active = it & 0xF != 0;
        let result = if wide { self.execute_thumb2(first, second) } else { self.execute16(first) };
        self.it_active = false;
        if result.is_err() {
            self.cpu.itstate = it;
        }
        result
    }

    /// Whether a 16-bit instruction that can set the flags does: outside an
    /// IT block always, inside one never (A6.1). ARMv6-M has no IT blocks.
    fn setflags(&self) -> bool {
        !self.it_active
    }

    /// A load of one register, which on ARMv7-M may be unaligned unless
    /// CCR.UNALIGN_TRP says otherwise. An unaligned access is made a byte
    /// at a time, each checked as the access it is part of.
    pub(crate) fn load_unaligned(&mut self, addr: u32, size: u32) -> Result<u32, Fault> {
        if addr & (size - 1) == 0 || !self.allows_unaligned() {
            return self.load(addr, size);
        }
        let mut value = 0;
        for i in 0..size {
            value |= self.load(addr.wrapping_add(i), 1)? << (8 * i);
        }
        Ok(value)
    }

    pub(crate) fn store_unaligned(&mut self, addr: u32, size: u32, value: u32) -> Result<(), Fault> {
        if addr & (size - 1) == 0 || !self.allows_unaligned() {
            return self.store(addr, size, value);
        }
        // Checked whole before any byte lands, so a store the MPU refuses
        // half of changes nothing.
        self.check(addr, Access::Write)?;
        self.check(addr.wrapping_add(size - 1), Access::Write)?;
        for i in 0..size {
            self.store(addr.wrapping_add(i), 1, value >> (8 * i))?;
        }
        Ok(())
    }

    fn allows_unaligned(&self) -> bool {
        self.core != Core::CortexM0Plus && self.bus.scs.ccr & crate::scs::CCR_UNALIGN_TRP == 0
    }

    /// LDRT and friends: the access is checked as unprivileged code's.
    pub(crate) fn load_unprivileged(&mut self, addr: u32, size: u32) -> Result<u32, Fault> {
        let mode = (self.cpu.mode, self.cpu.npriv);
        self.cpu.mode = Mode::Thread;
        self.cpu.npriv = true;
        let result = self.load_unaligned(addr, size);
        (self.cpu.mode, self.cpu.npriv) = mode;
        result
    }

    pub(crate) fn store_unprivileged(&mut self, addr: u32, size: u32, value: u32) -> Result<(), Fault> {
        let mode = (self.cpu.mode, self.cpu.npriv);
        self.cpu.mode = Mode::Thread;
        self.cpu.npriv = true;
        let result = self.store_unaligned(addr, size, value);
        (self.cpu.mode, self.cpu.npriv) = mode;
        result
    }

    fn execute16(&mut self, i: u32) -> Result<Flow, Fault> {
        let pc = self.cpu.pc;
        let mut next = pc.wrapping_add(2);
        let undefined = Fault::Undefined { instr: i };
        let sf = self.setflags();

        match i >> 10 {
            // Shift (immediate), add, subtract, move, compare.
            0b000000..=0b001111 => match i >> 11 & 7 {
                op @ 0b000..=0b010 => {
                    let imm5 = i >> 6 & 31;
                    let m = self.r(low(i, 3));
                    // LSL #0 is MOVS, which leaves C alone; LSR and ASR
                    // encode #32 as #0.
                    let (kind, amount) = match op {
                        0 => (Shift::Lsl, imm5),
                        1 => (Shift::Lsr, if imm5 == 0 { 32 } else { imm5 }),
                        _ => (Shift::Asr, if imm5 == 0 { 32 } else { imm5 }),
                    };
                    let (result, c) = shift_c(kind, m, amount, self.cpu.c);
                    if sf {
                        self.cpu.c = c;
                        self.nz(result);
                    }
                    self.w(low(i, 0), result);
                }
                0b011 => {
                    let n = self.r(low(i, 3));
                    let y = if i & 1 << 10 != 0 { low(i, 6) } else { self.r(low(i, 6)) };
                    let result = if i & 1 << 9 != 0 { self.arith(n, !y, true, sf) } else { self.arith(n, y, false, sf) };
                    self.w(low(i, 0), result);
                }
                op => {
                    let d = low(i, 8);
                    let imm = i & 0xFF;
                    match op & 3 {
                        0 => {
                            if sf {
                                self.nz(imm);
                            }
                            self.w(d, imm);
                        }
                        1 => {
                            self.add_flags(self.r(d), !imm, true);
                        }
                        2 => {
                            let result = self.arith(self.r(d), imm, false, sf);
                            self.w(d, result);
                        }
                        _ => {
                            let result = self.arith(self.r(d), !imm, true, sf);
                            self.w(d, result);
                        }
                    }
                }
            },

            // Data processing, register.
            0b010000 => {
                let dn = low(i, 0);
                let x = self.r(dn);
                let y = self.r(low(i, 3));
                let c = self.cpu.c;
                let shifted = |machine: &mut Machine, kind| {
                    let (result, c) = shift_c(kind, x, y & 0xFF, c);
                    if sf {
                        machine.cpu.c = c;
                        machine.nz(result);
                    }
                    machine.w(dn, result);
                };
                let logical = |machine: &mut Machine, result: u32| {
                    if sf {
                        machine.nz(result);
                    }
                    machine.w(dn, result);
                };
                match i >> 6 & 0xF {
                    0x0 => logical(self, x & y),
                    0x1 => logical(self, x ^ y),
                    0x2 => shifted(self, Shift::Lsl),
                    0x3 => shifted(self, Shift::Lsr),
                    0x4 => shifted(self, Shift::Asr),
                    0x5 => {
                        let result = self.arith(x, y, c, sf);
                        self.w(dn, result);
                    }
                    0x6 => {
                        let result = self.arith(x, !y, c, sf);
                        self.w(dn, result);
                    }
                    0x7 => shifted(self, Shift::Ror),
                    0x8 => self.nz(x & y),
                    // RSBS rd, rn, #0: rn is the register in bits 5:3.
                    0x9 => {
                        let result = self.arith(!y, 0, true, sf);
                        self.w(dn, result);
                    }
                    0xA => {
                        self.add_flags(x, !y, true);
                    }
                    0xB => {
                        self.add_flags(x, y, false);
                    }
                    0xC => logical(self, x | y),
                    0xD => logical(self, x.wrapping_mul(y)),
                    0xE => logical(self, x & !y),
                    _ => logical(self, !y),
                }
            }

            // Special data processing, and branch-and-exchange: the only
            // 16-bit instructions that reach r8–r15.
            0b010001 => {
                let m = i >> 3 & 0xF;
                let dn = (i >> 4 & 8) | (i & 7);
                match i >> 8 & 3 {
                    0 => {
                        let result = self.r(dn).wrapping_add(self.r(m));
                        if dn == 15 {
                            self.cpu.pc = result & !1;
                            return Ok(Flow::Next);
                        }
                        self.w(dn, result);
                    }
                    1 => {
                        self.add_flags(self.r(dn), !self.r(m), true);
                    }
                    2 => {
                        let value = self.r(m);
                        if dn == 15 {
                            self.cpu.pc = value & !1;
                            return Ok(Flow::Next);
                        }
                        self.w(dn, value);
                    }
                    _ => {
                        let target = self.r(m);
                        if i & 1 << 7 != 0 {
                            self.cpu.r[14] = next | 1;
                        }
                        self.bx(target)?;
                        return Ok(Flow::Next);
                    }
                }
            }

            // LDR (literal): the base is the pc, word-aligned.
            0b010010 | 0b010011 => {
                let addr = (pc.wrapping_add(4) & !3).wrapping_add((i & 0xFF) << 2);
                let value = self.load(addr, 4)?;
                self.w(low(i, 8), value);
            }

            // Load and store, register offset.
            0b010100..=0b010111 => {
                let addr = self.r(low(i, 3)).wrapping_add(self.r(low(i, 6)));
                let t = low(i, 0);
                match i >> 9 & 7 {
                    0 => self.store_unaligned(addr, 4, self.r(t))?,
                    1 => self.store_unaligned(addr, 2, self.r(t))?,
                    2 => self.store(addr, 1, self.r(t))?,
                    op => {
                        let value = match op {
                            3 => self.load(addr, 1)? as u8 as i8 as u32,
                            4 => self.load_unaligned(addr, 4)?,
                            5 => self.load_unaligned(addr, 2)?,
                            6 => self.load(addr, 1)?,
                            _ => self.load_unaligned(addr, 2)? as u16 as i16 as u32,
                        };
                        self.w(t, value);
                    }
                }
            }

            // Load and store, immediate offset: word, byte, halfword.
            0b011000..=0b100011 => {
                let imm5 = i >> 6 & 31;
                let (size, offset) = match i >> 12 {
                    0b0110 => (4, imm5 << 2),
                    0b0111 => (1, imm5),
                    _ => (2, imm5 << 1),
                };
                let addr = self.r(low(i, 3)).wrapping_add(offset);
                self.transfer(i & 1 << 11 != 0, addr, size, low(i, 0))?;
            }

            // Load and store, sp-relative.
            0b100100..=0b100111 => {
                let addr = self.cpu.sp().wrapping_add((i & 0xFF) << 2);
                self.transfer(i & 1 << 11 != 0, addr, 4, low(i, 8))?;
            }

            // ADR, and ADD rd, sp, #imm.
            0b101000 | 0b101001 => self.w(low(i, 8), (pc.wrapping_add(4) & !3).wrapping_add((i & 0xFF) << 2)),
            0b101010 | 0b101011 => self.w(low(i, 8), self.cpu.sp().wrapping_add((i & 0xFF) << 2)),

            0b101100..=0b101111 => return self.miscellaneous(i),

            // STM and LDM, increment after.
            0b110000..=0b110011 => {
                let n = low(i, 8);
                let list = i & 0xFF;
                let mut addr = self.r(n);
                if list == 0 {
                    return Err(undefined);
                }
                let loading = i & 1 << 11 != 0;
                for reg in 0..8 {
                    if list & 1 << reg != 0 {
                        if loading {
                            let value = self.load(addr, 4)?;
                            self.cpu.r[reg] = value;
                        } else {
                            self.store(addr, 4, self.cpu.r[reg])?;
                        }
                        addr = addr.wrapping_add(4);
                    }
                }
                // LDM writes back only when it did not load the base.
                if !loading || list & 1 << n == 0 {
                    self.w(n, addr);
                }
            }

            // Conditional branch, UDF, SVC.
            0b110100..=0b110111 => match i >> 8 & 0xF {
                0xE => return Err(undefined),
                0xF => {
                    // The return address is the next instruction, so the
                    // instruction is done before the exception is pended —
                    // the run loop takes it before anything else executes.
                    self.cpu.pc = next;
                    let nvic = &self.bus.scs.nvic;
                    if nvic.group(nvic.priority(SVCALL)) < self.priority {
                        self.bus.scs.nvic.pend(SVCALL);
                        return Ok(Flow::Next);
                    }
                    return Err(Fault::SvcEscalation);
                }
                _ if self.it_active => return Err(undefined),
                cond => {
                    if self.condition(cond) {
                        next = pc.wrapping_add(4).wrapping_add(((i & 0xFF) as u8 as i8 as i32 * 2) as u32);
                    }
                }
            },

            // B, unconditional.
            0b111000 | 0b111001 => {
                next = pc.wrapping_add(4).wrapping_add((((i & 0x7FF) << 21) as i32 >> 20) as u32);
            }

            _ => return Err(undefined),
        }

        self.cpu.pc = next;
        Ok(Flow::Next)
    }

    fn transfer(&mut self, loading: bool, addr: u32, size: u32, t: u32) -> Result<(), Fault> {
        if loading {
            let value = self.load_unaligned(addr, size)?;
            self.w(t, value);
        } else {
            self.store_unaligned(addr, size, self.r(t))?;
        }
        Ok(())
    }

    /// The `1011` group: sp adjustment, extends, push and pop, `cps`,
    /// byte reversal, `bkpt`, and the hints.
    fn miscellaneous(&mut self, i: u32) -> Result<Flow, Fault> {
        let pc = self.cpu.pc;
        let undefined = Err(Fault::Undefined { instr: i });
        let m = self.r(low(i, 3));
        let d = low(i, 0);

        match i >> 8 & 0xF {
            0b0000 => {
                let imm = (i & 0x7F) << 2;
                let sp = self.cpu.sp();
                self.cpu.set_sp(if i & 1 << 7 != 0 { sp.wrapping_sub(imm) } else { sp.wrapping_add(imm) });
            }
            0b0010 => {
                let value = match i >> 6 & 3 {
                    0 => m as u16 as i16 as u32,
                    1 => m as u8 as i8 as u32,
                    2 => m & 0xFFFF,
                    _ => m & 0xFF,
                };
                self.w(d, value);
            }
            0b0100 | 0b0101 => {
                let list = i & 0xFF;
                let lr = i & 1 << 8 != 0;
                if list == 0 && !lr {
                    return undefined;
                }
                let base = self.cpu.sp().wrapping_sub(4 * (list.count_ones() + lr as u32));
                let mut addr = base;
                for reg in 0..8 {
                    if list & 1 << reg != 0 {
                        self.store(addr, 4, self.cpu.r[reg])?;
                        addr = addr.wrapping_add(4);
                    }
                }
                if lr {
                    self.store(addr, 4, self.cpu.r[14])?;
                }
                self.cpu.set_sp(base);
            }
            0b0110 => {
                // CPSIE and CPSID: i on both profiles, f on ARMv7-M.
                // Unprivileged, a no-op.
                let v7 = self.core != Core::CortexM0Plus;
                if i & 0xFFEC != 0xB660 || i & 3 == 0 || !v7 && i & 3 != 2 {
                    return undefined;
                }
                if self.cpu.privileged() {
                    let disable = i & 1 << 4 != 0;
                    if i & 2 != 0 {
                        self.cpu.primask = disable;
                    }
                    // FAULTMASK cannot be set from a handler already at
                    // HardFault's priority or above.
                    if i & 1 != 0 && (!disable || self.priority > -1) {
                        self.cpu.faultmask = disable;
                    }
                    self.update_priority();
                }
            }
            0b0001 | 0b0011 | 0b1001 | 0b1011 if self.core != Core::CortexM0Plus => {
                // CBZ and CBNZ, which may not be in an IT block.
                if self.it_active {
                    return undefined;
                }
                let n = self.cpu.r[(i & 7) as usize];
                let offset = (i >> 9 & 1) << 6 | (i >> 3 & 0x1F) << 1;
                if (n == 0) != (i & 1 << 11 != 0) {
                    self.cpu.pc = pc.wrapping_add(4).wrapping_add(offset);
                    return Ok(Flow::Next);
                }
            }
            0b1010 => {
                let value = match i >> 6 & 3 {
                    0 => m.swap_bytes(),
                    1 => (m & 0x00FF_00FF) << 8 | (m >> 8 & 0x00FF_00FF),
                    3 => (m as u16).swap_bytes() as i16 as u32,
                    _ => return undefined,
                };
                self.w(d, value);
            }
            0b1100 | 0b1101 => {
                let list = i & 0xFF;
                let to_pc = i & 1 << 8 != 0;
                if list == 0 && !to_pc {
                    return undefined;
                }
                let mut addr = self.cpu.sp();
                for reg in 0..8 {
                    if list & 1 << reg != 0 {
                        let value = self.load(addr, 4)?;
                        self.cpu.r[reg] = value;
                        addr = addr.wrapping_add(4);
                    }
                }
                if to_pc {
                    let target = self.load(addr, 4)?;
                    // The stack pointer moves before the branch, because the
                    // branch may be an exception return that unstacks from it.
                    self.cpu.set_sp(addr.wrapping_add(4));
                    self.bx(target)?;
                    return Ok(Flow::Next);
                }
                self.cpu.set_sp(addr);
            }
            0b1110 => return Ok(Flow::Halt(Outcome::Breakpoint { pc, imm: i as u8 })),
            0b1111 if i & 0xF != 0 => {
                // IT: firstcond and mask, which together are ITSTATE.
                let firstcond = i >> 4 & 0xF;
                if self.core == Core::CortexM0Plus || self.it_active || firstcond == 0xF || firstcond == 0xE && (i & 0xF).count_ones() != 1 {
                    return undefined;
                }
                self.cpu.itstate = (i & 0xFF) as u8;
            }
            0b1111 => {
                // NOP, YIELD, WFE, WFI, SEV. Only WFI does anything: with no
                // other processor to signal it, WFE is a hint the model is
                // free to treat as a NOP, and does.
                if i >> 4 & 0xF == 3 {
                    self.sleeping = true;
                }
            }
            _ => return undefined,
        }

        self.cpu.pc = pc.wrapping_add(2);
        Ok(Flow::Next)
    }

    fn execute32(&mut self, first: u32, second: u32) -> Result<Flow, Fault> {
        let pc = self.cpu.pc;
        let next = pc.wrapping_add(4);
        let undefined = Err(Fault::Undefined { instr: first << 16 | second });

        if first >> 11 == 0b11110 && second & 0xD000 == 0xD000 {
            // BL: S:I1:I2:imm10:imm11:'0', where I = NOT(J XOR S).
            let s = first >> 10 & 1;
            let i1 = !(second >> 13 ^ s) & 1;
            let i2 = !(second >> 11 ^ s) & 1;
            let imm = s << 24 | i1 << 23 | i2 << 22 | (first & 0x3FF) << 12 | (second & 0x7FF) << 1;
            self.cpu.r[14] = next | 1;
            self.cpu.pc = next.wrapping_add(((imm << 7) as i32 >> 7) as u32);
            return Ok(Flow::Next);
        }

        if first & 0xFFF0 == 0xF380 && second & 0xFF00 == 0x8800 {
            self.msr(first & 0xF, second & 0xFF);
        } else if first == 0xF3EF && second & 0xF000 == 0x8000 {
            let value = self.mrs(second & 0xFF);
            self.w(second >> 8 & 0xF, value);
        } else if first == 0xF3BF && matches!(second & 0xFFF0, 0x8F40 | 0x8F50 | 0x8F60) {
            // DSB, DMB, ISB. Every access in this machine completes before
            // the next instruction, so there is nothing to wait for.
        } else {
            return undefined;
        }

        self.cpu.pc = next;
        Ok(Flow::Next)
    }

    /// MSR (B4.2.3, B5.2.3). Unprivileged code may write only the flags.
    pub(crate) fn msr(&mut self, n: u32, sysm: u32) {
        let value = self.r(n);
        let privileged = self.cpu.privileged();
        let v7 = self.core != Core::CortexM0Plus;
        match sysm {
            0..=3 => self.cpu.set_apsr(value),
            8 if privileged => self.cpu.msp = value & !3,
            9 if privileged => self.cpu.psp = value & !3,
            16 if privileged => {
                self.cpu.primask = value & 1 != 0;
                self.update_priority();
            }
            17 if privileged && v7 => {
                self.cpu.basepri = value as u8 & self.bus.scs.nvic.mask;
                self.update_priority();
            }
            // BASEPRI_MAX: only ever raises the priority.
            18 if privileged && v7 => {
                let new = value as u8 & self.bus.scs.nvic.mask;
                if new != 0 && (self.cpu.basepri == 0 || new < self.cpu.basepri) {
                    self.cpu.basepri = new;
                    self.update_priority();
                }
            }
            // FAULTMASK cannot be set at HardFault's priority or above.
            19 if privileged && v7 => {
                if value & 1 == 0 || self.priority > -1 {
                    self.cpu.faultmask = value & 1 != 0;
                    self.update_priority();
                }
            }
            20 if privileged => {
                self.cpu.npriv = value & 1 != 0;
                // The stack selection is Thread mode's; Handler mode is on
                // the main stack whatever SPSEL says, and ignores the write.
                if self.cpu.mode == Mode::Thread {
                    self.cpu.spsel = value & 2 != 0;
                }
                if v7 {
                    self.cpu.fpca = value & 4 != 0;
                }
            }
            _ => {}
        }
    }

    /// MRS (B4.2.2, B5.2.2). EPSR always reads as zero.
    pub(crate) fn mrs(&self, sysm: u32) -> u32 {
        let privileged = self.cpu.privileged();
        let v7 = self.core != Core::CortexM0Plus;
        match sysm {
            0..=7 => {
                let apsr = if sysm & 4 == 0 { self.cpu.apsr() } else { 0 };
                let ipsr = if sysm & 1 != 0 { self.cpu.ipsr & 0x1FF } else { 0 };
                apsr | ipsr
            }
            8 if privileged => self.cpu.msp,
            9 if privileged => self.cpu.psp,
            16 => self.cpu.primask as u32,
            17 | 18 if v7 => self.cpu.basepri as u32,
            19 if v7 => self.cpu.faultmask as u32,
            20 => (self.cpu.npriv as u32) | (self.cpu.spsel as u32) << 1 | (self.cpu.fpca as u32) << 2,
            _ => 0,
        }
    }
}
