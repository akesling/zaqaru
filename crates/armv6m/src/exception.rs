//! Exception entry and return: the part of the machine a kernel is made
//! of.
//!
//! Hubris enters itself four ways — SVCall for a syscall, PendSV for a
//! deferred context switch, SysTick for time, HardFault when a task
//! misbehaves — and leaves every one the same way, by `bx lr` with an
//! EXC_RETURN the hardware unstacks from. A context switch is the kernel
//! changing PSP and the saved EXC_RETURN in between. So the two functions
//! here are, between them, how every task runs at all.

use crate::scs::exception::HARD_FAULT;
use crate::state::Mode;
use crate::{Access, Fault, Machine, Outcome};

/// Return to Handler mode, main stack.
const EXC_RETURN_HANDLER: u32 = 0xFFFF_FFF1;
/// Return to Thread mode, main stack.
const EXC_RETURN_THREAD_MSP: u32 = 0xFFFF_FFF9;
/// Return to Thread mode, process stack.
const EXC_RETURN_THREAD_PSP: u32 = 0xFFFF_FFFD;

/// xPSR bit 9 in a stacked frame: the stack pointer was realigned by four
/// bytes to stack it, and gets them back on return.
const FRAME_REALIGNED: u32 = 1 << 9;

impl Machine {
    /// Takes exception `n`, with the current pc as the return address
    /// (B1.5.6).
    ///
    /// The frame — r0–r3, r12, lr, the return address, xPSR — is pushed to
    /// the stack the interrupted code was using, with the privilege it was
    /// running at. If that faults (a task whose stack pointer the MPU
    /// refuses), the frame is abandoned half-written, `n` stays pending,
    /// and HardFault is taken in its place on the same stack pointer. That
    /// is the case Hubris's ARMv6-M fault handler goes out of its way to
    /// recognise — it finds SVCall still pending in ICSR — so it has to
    /// happen here the same way.
    pub(crate) fn enter(&mut self, n: u32) -> Result<(), Outcome> {
        let from_handler = self.cpu.mode == Mode::Handler;
        let on_psp = self.cpu.on_psp();
        let sp = self.cpu.sp();
        let realign = sp & 4;
        let frame = sp.wrapping_sub(0x20) & !4;
        let xpsr = self.cpu.xpsr() | if realign != 0 { FRAME_REALIGNED } else { 0 };
        let words = [
            self.cpu.r[0],
            self.cpu.r[1],
            self.cpu.r[2],
            self.cpu.r[3],
            self.cpu.r[12],
            self.cpu.r[14],
            self.cpu.pc,
            xpsr,
        ];

        let privileged = self.cpu.privileged();
        let mut stacked = true;
        for (i, &word) in words.iter().enumerate() {
            let addr = frame.wrapping_add(4 * i as u32);
            if !(self.permits(addr, Access::Write, privileged) && self.bus.write(addr, 4, word).is_ok()) {
                stacked = false;
                break;
            }
        }
        self.cpu.set_sp(frame);

        let n = if stacked {
            self.bus.scs.nvic.unpend(n);
            n
        } else {
            let fault = Fault::Stacking { exception: n };
            self.events.push(crate::Event::Fault {
                cycle: self.cycles,
                pc: self.cpu.pc,
                fault,
                mode: self.cpu.mode,
                privileged,
            });
            // A task that overflows its stack faults on the push, and its
            // HardFault's own push faults the same way; that is still a
            // HardFault, not a lockup. Only a fault at HardFault's priority
            // or above has nowhere to go.
            if self.priority < 0 {
                return Err(Outcome::Lockup { pc: self.cpu.pc, fault });
            }
            HARD_FAULT
        };

        self.cpu.r[14] = if from_handler {
            EXC_RETURN_HANDLER
        } else if on_psp {
            EXC_RETURN_THREAD_PSP
        } else {
            EXC_RETURN_THREAD_MSP
        };
        self.cpu.mode = Mode::Handler;
        self.cpu.ipsr = n;
        self.cpu.spsel = false;
        self.bus.scs.nvic.active |= 1 << n;
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

    /// Returns from the active exception through `exc_return` (B1.5.8).
    ///
    /// Only bits 3:0 select anything. Hubris returns through `0xFFFF_FFED`
    /// on every profile, which has bit 4 clear — on ARMv8-M that means
    /// "no FP context", on ARMv6-M it is a bit the architecture says
    /// should be one — and the Cortex-M0+ returns through it regardless,
    /// so the model does too.
    pub(crate) fn exception_return(&mut self, exc_return: u32) -> Result<(), Fault> {
        let (mode, on_psp) = match exc_return & 0xF {
            0b0001 => (Mode::Handler, false),
            0b1001 => (Mode::Thread, false),
            0b1101 => (Mode::Thread, true),
            _ => return Err(Fault::InvPc),
        };
        let returning = self.cpu.ipsr;
        let others = self.bus.scs.nvic.active & !(1 << returning);
        // Thread mode with exceptions still active, or Handler mode with
        // none, is a return the active set cannot explain.
        if (mode == Mode::Thread) != (others == 0) {
            return Err(Fault::InvPc);
        }

        // Unstack with the privilege, and at the priority, being returned
        // to: deactivate first, put it back if the pops fault.
        self.bus.scs.nvic.active = others;
        self.update_priority();
        let privileged = mode == Mode::Handler || !self.cpu.npriv;
        let sp = if on_psp { self.cpu.psp } else { self.cpu.msp };
        let mut frame = [0u32; 8];
        for (i, word) in frame.iter_mut().enumerate() {
            let addr = sp.wrapping_add(4 * i as u32);
            let loaded = if self.permits(addr, Access::Read, privileged) { self.bus.read(addr, 4).ok() } else { None };
            match loaded {
                Some(value) => *word = value,
                None => {
                    self.bus.scs.nvic.active |= 1 << returning;
                    self.update_priority();
                    return Err(Fault::MemManage { addr, access: Access::Read });
                }
            }
        }

        let [r0, r1, r2, r3, r12, lr, pc, xpsr] = frame;
        self.cpu.r[0] = r0;
        self.cpu.r[1] = r1;
        self.cpu.r[2] = r2;
        self.cpu.r[3] = r3;
        self.cpu.r[12] = r12;
        self.cpu.r[14] = lr;
        self.cpu.pc = pc & !1;
        self.cpu.set_apsr(xpsr);
        self.cpu.t = xpsr & 1 << 24 != 0;
        self.cpu.mode = mode;
        self.cpu.spsel = on_psp;
        self.cpu.ipsr = if mode == Mode::Handler { xpsr & 0x3F } else { 0 };
        let popped = sp.wrapping_add(0x20) | if xpsr & FRAME_REALIGNED != 0 { 4 } else { 0 };
        if on_psp {
            self.cpu.psp = popped;
        } else {
            self.cpu.msp = popped;
        }
        self.bus.scs.vectactive = self.cpu.ipsr;
        // A line its handler left asserted is pending again.
        self.sample_interrupts();
        Ok(())
    }
}
