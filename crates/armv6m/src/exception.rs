//! Exception entry and return: the part of the machine a kernel is made
//! of.
//!
//! Hubris enters itself four ways — SVCall for a syscall, PendSV for a
//! deferred context switch, SysTick for time, and a fault when a task
//! misbehaves — and leaves every one the same way, by `bx lr` with an
//! EXC_RETURN the hardware unstacks from. A context switch is the kernel
//! changing PSP and the saved EXC_RETURN in between. So the functions here
//! are, between them, how every task runs at all.
//!
//! ARMv7-M adds three things a kernel sees:
//!
//! - **Faults have exceptions of their own** — MemManage, BusFault and
//!   UsageFault — each enabled in SHCSR and escalated to HardFault when it
//!   is not, or when it cannot preempt what is running (B1.5.15). What
//!   happened is recorded in CFSR, and where in MMFAR or BFAR.
//! - **A derived fault** — one raised while stacking or unstacking a frame
//!   — is its own kind: MSTKERR or STKERR on the way in, with the original
//!   exception left pending; MUNSTKERR or UNSTKERR on the way out, taken
//!   in place of the return as a tail-chain, the frame left where it was.
//! - **The extended frame.** A context that has used the FPU
//!   (CONTROL.FPCA) is stacked with s0–s15 and FPSCR as well, and the
//!   EXC_RETURN says so in bit 4. The model stacks it eagerly: lazy
//!   preservation defers the same stores to the handler's first FP
//!   instruction, and the difference shows only as which fault a bad stack
//!   is blamed on (MLSPERR rather than MSTKERR).

use crate::scs::exception::*;
use crate::scs::{CCR_NONBASETHRDENA, HFSR_FORCED, cfsr};
use crate::state::{Core, Mode};
use crate::{Access, Fault, Machine, Outcome};

/// xPSR bit 9 in a stacked frame: the stack pointer was realigned by four
/// bytes to stack it, and gets them back on return.
const FRAME_REALIGNED: u32 = 1 << 9;
/// The words of a basic frame, and of one with the FP context too: s0–s15,
/// FPSCR, and a reserved word that keeps it a multiple of eight bytes.
const BASIC_FRAME: u32 = 8;
const EXTENDED_FRAME: u32 = 26;
/// EXC_RETURN bit 4: zero when the frame is extended.
const EXC_RETURN_BASIC: u32 = 1 << 4;

/// Why a frame could not be pushed or popped: the MPU refused it, or
/// nothing answered.
#[derive(Clone, Copy)]
enum StackFault {
    MemManage,
    Bus,
}

impl Machine {
    /// Takes exception `n`, with the current pc as the return address
    /// (B1.5.6).
    ///
    /// The frame is pushed to the stack the interrupted code was using,
    /// with the privilege it was running at. If that faults (a task whose
    /// stack pointer the MPU refuses), the frame is abandoned half-written,
    /// `n` stays pending, and the derived fault is taken in its place on
    /// the same stack pointer: HardFault on ARMv6-M, MemManage or BusFault
    /// on ARMv7-M if they can be. That is the case Hubris's fault handlers
    /// go out of their way to recognise — on ARMv6-M it finds SVCall still
    /// pending in ICSR, on ARMv7-M it clears SHCSR's pended bits — so it
    /// has to happen here the same way.
    pub(crate) fn enter(&mut self, n: u32) -> Result<(), Outcome> {
        self.stall += self.timing.entry;
        self.entries += 1;
        // What a supervisor call's caller left in the registers a kernel
        // reads its arguments from, for the trace: they are not stacked.
        let asked = (self.cpu.r[11], self.cpu.r[4], self.cpu.r[5]);
        let from_handler = self.cpu.mode == Mode::Handler;
        let on_psp = self.cpu.on_psp();
        let extended = self.cpu.fpca;
        let words = if extended { EXTENDED_FRAME } else { BASIC_FRAME };
        let sp = self.cpu.sp();
        let realign = sp & 4;
        let frame = sp.wrapping_sub(4 * words) & !4;
        let xpsr = self.cpu.xpsr() | if realign != 0 { FRAME_REALIGNED } else { 0 };
        let mut values = vec![
            self.cpu.r[0],
            self.cpu.r[1],
            self.cpu.r[2],
            self.cpu.r[3],
            self.cpu.r[12],
            self.cpu.r[14],
            self.cpu.pc,
            xpsr,
        ];
        if extended {
            values.extend_from_slice(&self.cpu.s[..16]);
            values.extend([self.cpu.fpscr, 0]);
        }

        let privileged = self.cpu.privileged();
        let mut stacked = None;
        for (i, &word) in values.iter().enumerate() {
            let addr = frame.wrapping_add(4 * i as u32);
            if !self.permits(addr, Access::Write, privileged) {
                stacked = Some(StackFault::MemManage);
                break;
            }
            if self.bus.write(addr, 4, word).is_err() {
                stacked = Some(StackFault::Bus);
                break;
            }
        }
        self.cpu.set_sp(frame);

        let n = match stacked {
            None => {
                self.bus.scs.nvic.unpend(n);
                n
            }
            Some(why) => {
                let fault = Fault::Stacking { exception: n };
                self.events.push(crate::Event::Fault {
                    time: self.time,
                    pc: self.cpu.pc,
                    fault,
                    mode: self.cpu.mode,
                    privileged,
                });
                // The exception that wanted the frame is still owed.
                if n != HARD_FAULT {
                    self.bus.scs.nvic.pend(n);
                }
                let derived = match (self.core, why) {
                    (Core::CortexM0Plus, _) => HARD_FAULT,
                    (_, StackFault::MemManage) => self.record_fault(MEM_MANAGE, cfsr::MSTKERR, None),
                    (_, StackFault::Bus) => self.record_fault(BUS_FAULT, cfsr::STKERR, None),
                };
                // A task that overflows its stack faults on the push, and
                // its fault's own push faults the same way; that is still a
                // fault, not a lockup. Only a fault at HardFault's priority
                // or above has nowhere to go.
                match self.escalate(derived) {
                    Some(n) => n,
                    None => return Err(Outcome::Lockup { pc: self.cpu.pc, fault }),
                }
            }
        };

        let mut exc_return = match (from_handler, on_psp) {
            (true, _) => 0xFFFF_FFF1,
            (false, true) => 0xFFFF_FFFD,
            (false, false) => 0xFFFF_FFF9,
        };
        if extended {
            exc_return &= !EXC_RETURN_BASIC;
        }
        self.cpu.r[14] = exc_return;
        let time = self.time;
        let signal = match n {
            SVCALL => crate::trace::Signal::Syscall { r11: asked.0, r4: asked.1, r5: asked.2 },
            n => crate::trace::Signal::Exception { number: n as u16 },
        };
        self.trace().record(time, signal);
        self.activate(n)
    }

    /// Makes `n` the running exception and branches to its handler: what
    /// entry does once the frame is pushed, and what a tail-chain does
    /// with no frame to push.
    fn activate(&mut self, n: u32) -> Result<(), Outcome> {
        self.cpu.mode = Mode::Handler;
        self.cpu.itstate = 0;
        self.cpu.fpca = false;
        self.exclusive = None;
        self.cpu.ipsr = n;
        self.cpu.spsel = false;
        self.bus.scs.nvic.active.set(n);
        self.bus.scs.vectactive = n;
        self.update_priority();
        self.sleeping = false;
        self.taken[n as usize] += 1;

        let slot = self.bus.scs.vtor.wrapping_add(4 * n);
        let vector = self.bus.read(slot, 4).map_err(|_| Outcome::Lockup {
            pc: self.cpu.pc,
            fault: Fault::Bus { addr: slot },
        })?;
        self.cpu.pc = vector & !1;
        self.cpu.t = vector & 1 != 0;
        Ok(())
    }

    /// Records a configurable fault's syndrome in CFSR — and where it was,
    /// in MMFAR or BFAR — and answers the exception it is for.
    pub(crate) fn record_fault(&mut self, n: u32, bits: u32, addr: Option<u32>) -> u32 {
        let faults = &mut self.bus.scs.faults;
        faults.cfsr |= bits;
        match (n, addr) {
            (MEM_MANAGE, Some(addr)) => {
                faults.cfsr |= cfsr::MMARVALID;
                faults.mmfar = addr;
            }
            (BUS_FAULT, Some(addr)) => {
                faults.cfsr |= cfsr::BFARVALID;
                faults.bfar = addr;
            }
            _ => {}
        }
        n
    }

    /// Which exception a synchronous fault for `n` is taken as: `n` if it
    /// is enabled and can preempt what is running, HardFault (marked
    /// FORCED) if not, and `None` — a lockup — if not even HardFault can.
    pub(crate) fn escalate(&mut self, n: u32) -> Option<u32> {
        if n != HARD_FAULT {
            let nvic = &self.bus.scs.nvic;
            if self.bus.scs.fault_enabled(n) && nvic.group(nvic.priority(n)) < self.priority {
                return Some(n);
            }
            self.bus.scs.faults.hfsr |= HFSR_FORCED;
        }
        (self.priority > -1).then_some(HARD_FAULT)
    }

    /// Returns from the active exception through `exc_return` (B1.5.8).
    ///
    /// Only bits 4:0 select anything. Hubris returns through `0xFFFF_FFED`
    /// on every profile: bit 4 clear, which on ARMv7-M with an FPU means
    /// "the frame is extended", and on ARMv6-M is a bit the architecture
    /// says should be one — the Cortex-M0+ returns through it regardless,
    /// so the model does too.
    pub(crate) fn exception_return(&mut self, exc_return: u32) -> Result<(), Fault> {
        self.stall += self.timing.exit;
        let (mode, on_psp) = match exc_return & 0xF {
            0b0001 => (Mode::Handler, false),
            0b1001 => (Mode::Thread, false),
            0b1101 => (Mode::Thread, true),
            _ => return Err(Fault::InvPc),
        };
        let v7 = self.core != Core::CortexM0Plus;
        let extended = v7 && exc_return & EXC_RETURN_BASIC == 0;
        let returning = self.cpu.ipsr;
        let mut others = self.bus.scs.nvic.active;
        others.clear(returning);
        // Thread mode with exceptions still active, or Handler mode with
        // none, is a return the active set cannot explain — unless, on
        // ARMv7-M, software asked to be allowed the first.
        let nonbase = v7 && self.bus.scs.ccr & CCR_NONBASETHRDENA != 0;
        if !self.bus.scs.nvic.active.contains(returning)
            || mode == Mode::Thread && !others.is_empty() && !nonbase
            || mode == Mode::Handler && others.is_empty()
        {
            return Err(Fault::InvPc);
        }

        // Unstack with the privilege, and at the priority, being returned
        // to: deactivate first, put it back if the pops fault.
        self.bus.scs.nvic.active = others;
        let faultmask = self.cpu.faultmask;
        if returning != NMI {
            self.cpu.faultmask = false;
        }
        self.update_priority();
        let privileged = mode == Mode::Handler || !self.cpu.npriv;
        let sp = if on_psp { self.cpu.psp } else { self.cpu.msp };
        let words = if extended { EXTENDED_FRAME } else { BASIC_FRAME };
        let mut frame = Vec::with_capacity(words as usize);
        for i in 0..words {
            let addr = sp.wrapping_add(4 * i);
            let why = if !self.permits(addr, Access::Read, privileged) {
                StackFault::MemManage
            } else {
                match self.bus.read(addr, 4) {
                    Ok(value) => {
                        frame.push(value);
                        continue;
                    }
                    Err(_) => StackFault::Bus,
                }
            };
            if !v7 {
                self.bus.scs.nvic.active.set(returning);
                self.cpu.faultmask = faultmask;
                self.update_priority();
                return Err(Fault::MemManage { addr, access: Access::Read });
            }
            return self.unstacking_fault(why, exc_return, mode, on_psp);
        }

        let xpsr = frame[7];
        // The stacked IPSR has to agree with where the return is going: a
        // return to Thread mode with an exception number, or to Handler
        // mode without one, is an INVPC fault (B1.5.8).
        if v7 && (mode == Mode::Thread) != (xpsr & 0x1FF == 0) {
            self.bus.scs.nvic.active.set(returning);
            self.cpu.faultmask = faultmask;
            self.update_priority();
            return Err(Fault::InvPc);
        }
        self.cpu.r[0] = frame[0];
        self.cpu.r[1] = frame[1];
        self.cpu.r[2] = frame[2];
        self.cpu.r[3] = frame[3];
        self.cpu.r[12] = frame[4];
        self.cpu.r[14] = frame[5];
        self.cpu.pc = frame[6] & !1;
        self.cpu.set_xpsr(xpsr);
        if !v7 {
            // Reserved on ARMv6-M, whatever the frame held.
            self.cpu.q = false;
            self.cpu.ge = 0;
            self.cpu.itstate = 0;
        }
        if extended {
            self.cpu.s[..16].copy_from_slice(&frame[8..24]);
            self.cpu.fpscr = frame[24];
        }
        self.cpu.fpca = extended;
        self.exclusive = None;
        self.cpu.mode = mode;
        self.cpu.spsel = on_psp;
        self.cpu.ipsr = if mode == Mode::Handler { xpsr & 0x1FF } else { 0 };
        let popped = sp.wrapping_add(4 * words) | if xpsr & FRAME_REALIGNED != 0 { 4 } else { 0 };
        if on_psp {
            self.cpu.psp = popped;
        } else {
            self.cpu.msp = popped;
        }
        self.bus.scs.vectactive = self.cpu.ipsr;
        if mode == Mode::Thread && on_psp {
            let time = self.time;
            self.trace().returned(time, popped);
        }
        // A line its handler left asserted is pending again.
        self.sample_interrupts();
        Ok(())
    }

    /// An unstacking fault on ARMv7-M: the return is abandoned with the
    /// returning exception already inactive and the frame still on the
    /// stack, and the derived fault is tail-chained in its place — entered
    /// with no new frame and the same EXC_RETURN, so that its handler
    /// returns to wherever the abandoned return was going.
    fn unstacking_fault(&mut self, why: StackFault, exc_return: u32, mode: Mode, on_psp: bool) -> Result<(), Fault> {
        let (n, bits) = match why {
            StackFault::MemManage => (MEM_MANAGE, cfsr::MUNSTKERR),
            StackFault::Bus => (BUS_FAULT, cfsr::UNSTKERR),
        };
        let exception = self.cpu.ipsr;
        self.events.push(crate::Event::Fault {
            time: self.time,
            pc: self.cpu.pc,
            fault: Fault::Unstacking { exception },
            mode,
            privileged: mode == Mode::Handler || !self.cpu.npriv,
        });
        self.record_fault(n, bits, None);
        // The context the return was going to is the one the fault
        // preempts, and the one it is judged against.
        self.cpu.mode = mode;
        self.cpu.spsel = on_psp;
        self.cpu.ipsr = 0;
        self.update_priority();
        let Some(n) = self.escalate(n) else {
            return Err(Fault::Unstacking { exception });
        };
        self.cpu.r[14] = exc_return;
        // A vector the bus cannot read is a lockup either way.
        let _ = self.activate(n);
        Ok(())
    }
}
