//! An ARMv6-M machine: a Cortex-M0+ interpreter, and enough of an
//! STM32G031 around it to boot Hubris.
//!
//! The x86 machine next door runs *programs*, under a kernel of its own.
//! This one runs a *kernel*: the code under test is the thing that owns
//! the exception vectors, programs the MPU and switches stacks, so what
//! this crate has to get right is not an instruction set so much as the
//! M-profile system model — exception entry and return, priorities and
//! preemption, the two stack pointers, privilege, the MPU's verdicts, and
//! a timer that fires when it should. The Hubris FAQ's reason for not
//! running under QEMU is that QEMU's version of exactly those is wrong;
//! that is the bar.
//!
//! The pieces:
//!
//! - [`state::Cpu`] — the registers, as a struct.
//! - [`exec`] — what each of the ARMv6-M instructions means.
//! - [`exception`] — entering and leaving handlers, and deciding which one.
//! - [`scs`] — SysTick, the NVIC, the SCB and the MPU, as memory-mapped
//!   registers.
//! - [`debug`] and [`dap`] — halting debug, and the debug port a probe
//!   reaches it through.
//! - [`bus`] and [`stm32g0`] — the rest of the address space.
//! - [`Machine`] — the run loop.
//!
//! **Time is retired instructions.** Each one is one cycle of the 16 MHz
//! clock Hubris configures SysTick against, and nothing else advances it:
//! a run is a pure function of the image, the same way a zaqaru container
//! is a pure function of its tape. `wfi` does not spin; it jumps the clock
//! to the next event, which is also what makes an idle system cheap to run.

pub mod bus;
pub mod cmsis_dap;
pub mod dap;
pub mod debug;
pub mod exception;
pub mod exec;
pub mod flash;
pub mod gdb;
pub mod scs;
pub mod state;
pub mod stm32g0;

use std::collections::BTreeSet;

use bus::{Bus, Unmapped};
use exec::Flow;
use scs::exception::HARD_FAULT;
use state::{Cpu, Mode};

/// The kind of access a permission check is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

/// Something the guest did that the architecture answers with a
/// HardFault — ARMv6-M has no other fault exception, so these are what a
/// fault handler would find in CFSR on a bigger core, and what an
/// [`Event::Fault`] reports here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The MPU, or the default memory map, refused the access.
    MemManage { addr: u32, access: Access },
    /// Nothing answers at the address.
    Bus { addr: u32 },
    /// ARMv6-M faults every unaligned access.
    Unaligned { addr: u32 },
    /// An encoding ARMv6-M does not define, or a permanently-undefined
    /// `udf`. 32-bit encodings are reported first halfword high.
    Undefined { instr: u32 },
    /// Execution with EPSR.T clear: an interworking branch to an even
    /// address, or an exception return that restored T=0.
    InvState,
    /// An exception return with a reserved EXC_RETURN, or one that
    /// disagrees with how many exceptions are active.
    InvPc,
    /// An `svc` at a priority that cannot take SVCall.
    SvcEscalation,
    /// Pushing an exception frame faulted; the exception that wanted it is
    /// still pending.
    Stacking { exception: u32 },
}

/// Why [`Machine::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cycle budget ran out.
    Budget,
    /// A fault with nowhere to go — inside HardFault itself, or on a bad
    /// vector. A real core stops fetching and waits for a debugger.
    Lockup { pc: u32, fault: Fault },
    /// A `bkpt`. A real core with no debugger attached would HardFault;
    /// this one stops, because a `bkpt` in an image under emulation is
    /// almost always asking for exactly that.
    Breakpoint { pc: u32, imm: u8 },
    /// Software asked for a system reset through AIRCR. Call
    /// [`Machine::reset`] to take it.
    SystemReset,
    /// `wfi` with nothing that could ever wake it: no timer running and
    /// nothing pending.
    Asleep,
    /// The pc reached one of [`Machine::breakpoints`], or a BPU
    /// comparator a debugger set. The instruction there has not executed.
    Stopped { pc: u32 },
    /// The core is halted in Debug state, by a debugger or by a stop above:
    /// it does nothing until a debugger lets it go.
    Halted,
}

/// Something a run did that is worth seeing afterwards, stamped with the
/// cycle it happened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A GPIO port's output changed.
    Pin { cycle: u64, edge: stm32g0::Edge },
    /// A HardFault was taken. `pc` is the instruction it was taken at;
    /// `mode` and `privileged` say what was running.
    Fault { cycle: u64, pc: u32, fault: Fault, mode: Mode, privileged: bool },
}

pub struct Machine {
    pub cpu: Cpu,
    pub bus: Bus,
    /// Cycles elapsed since power-on: retired instructions, plus whatever
    /// `wfi` skipped.
    pub cycles: u64,
    pub retired: u64,
    pub events: Vec<Event>,
    /// How many times each exception has been entered.
    pub taken: [u64; 48],
    /// The execution priority, kept current by everything that changes it
    /// (see [`Machine::update_priority`]) rather than recomputed for every
    /// access that needs it.
    pub(crate) priority: i32,
    pub(crate) sleeping: bool,
    /// Addresses a debugger asked to stop at, checked before an
    /// instruction executes — the way a Cortex-M0+'s BPU compares, with no
    /// `bkpt` written into the image.
    pub breakpoints: BTreeSet<u32>,
    /// Halting debug: whether the core is halted, and the debug registers.
    pub debug: debug::Debug,
    /// A breakpoint address to execute through once rather than stop at:
    /// a debugger resuming from the breakpoint it stopped on.
    skip_breakpoint: Option<u32>,
    /// SysTick is brought up to date in arrears rather than clocked every
    /// cycle: at the cycle it fires, and whenever anything reads or writes
    /// its registers. These are the cycle it was last brought up to, and
    /// the cycle it next reaches zero at.
    ticked_at: u64,
    tick_due: u64,
    /// The 32-byte granules each kind of access was recently allowed in,
    /// direct-mapped, with the context each verdict holds for. 32 bytes is
    /// the MPU's own resolution, so a verdict holds anywhere in its granule
    /// — and a kernel's working set is a few dozen granules, which turns
    /// eight region comparisons per access into one.
    granted: Box<[[Grant; GRANTS]; 3]>,
}

/// Granules remembered per kind of access.
const GRANTS: usize = 64;

#[derive(Clone, Copy)]
struct Grant {
    granule: u32,
    /// The MPU's epoch, privilege, and whether the MPU is in force at the
    /// current priority: everything else a verdict depends on.
    context: u64,
}

impl Grant {
    /// No address is in granule `u32::MAX`: addresses are 32 bits and a
    /// granule is 32 bytes.
    const NONE: Grant = Grant { granule: u32::MAX, context: 0 };
}

impl Machine {
    /// A G031 with `image` in flash, out of reset.
    pub fn stm32g031(image: &[u8]) -> Result<Machine, String> {
        let mut machine = Machine {
            cpu: Cpu::default(),
            bus: Bus::new(image)?,
            cycles: 0,
            retired: 0,
            events: Vec::new(),
            taken: [0; 48],
            priority: scs::BASE_PRIORITY,
            sleeping: false,
            breakpoints: BTreeSet::new(),
            debug: debug::Debug::default(),
            skip_breakpoint: None,
            ticked_at: 0,
            tick_due: u64::MAX,
            granted: Box::new([[Grant::NONE; GRANTS]; 3]),
        };
        machine.reset()?;
        Ok(machine)
    }

    /// A system reset: the processor and peripherals back to their reset
    /// state, memory left as it was, and the boot vector taken (B1.5.5).
    pub fn reset(&mut self) -> Result<(), String> {
        self.cpu = Cpu::default();
        self.bus.scs.reset();
        self.bus.soc.reset();
        self.bus.flash_interface.reset();
        self.sleeping = false;
        *self.granted = [[Grant::NONE; GRANTS]; 3];
        self.ticked_at = self.cycles;
        self.tick_due = u64::MAX;
        let vector = |bus: &mut Bus, n: u32| {
            bus.read(4 * n, 4).map_err(|Unmapped| format!("no vector table at 0x{:08x}", 4 * n))
        };
        self.cpu.msp = vector(&mut self.bus, 0)? & !3;
        let entry = vector(&mut self.bus, 1)?;
        self.cpu.pc = entry & !1;
        self.cpu.t = entry & 1 != 0;
        self.update_priority();
        self.debug_reset();
        Ok(())
    }

    /// Runs for `budget` cycles, or until something stops it. A halted core
    /// runs nothing, though a reset asked for still answers; a core told to
    /// step runs one instruction and halts again.
    pub fn run(&mut self, budget: u64) -> Outcome {
        if self.bus.scs.reset_requested {
            return Outcome::SystemReset;
        }
        if self.debug.halted {
            return Outcome::Halted;
        }
        if self.debug.stepping {
            self.debug.stepping = false;
            let outcome = self.execute_for(budget.min(1));
            if self.debug.halted {
                return self.settle(outcome);
            }
            self.settle(outcome);
            self.debug.halt(debug::DFSR_HALTED);
            return Outcome::Halted;
        }
        let outcome = self.execute_for(budget);
        self.settle(outcome)
    }

    fn execute_for(&mut self, budget: u64) -> Outcome {
        let end = self.cycles.saturating_add(budget);
        while self.cycles < end {
            if self.bus.scs.reset_requested {
                return Outcome::SystemReset;
            }

            // An exception that can preempt is taken before the next
            // instruction — and between any two, which is the whole of
            // "asynchronous" on a core this simple.
            if let Some((n, p)) = self.bus.scs.nvic.highest_pending().filter(|_| !self.debug.maskints) {
                if p < self.priority {
                    if let Err(outcome) = self.enter(n) {
                        return outcome;
                    }
                    continue;
                }
                // `wfi` wakes for anything that would preempt with PRIMASK
                // clear, and then carries on without taking it.
                if self.sleeping && p < self.bus.scs.nvic.execution_priority(false) {
                    self.sleeping = false;
                }
            }

            if self.sleeping {
                if self.tick_due == u64::MAX {
                    return Outcome::Asleep;
                }
                self.cycles = self.tick_due.min(end);
                self.sync_systick();
                continue;
            }

            if (self.debug.breaking && self.debug.breaks_at(self.cpu.pc)
                || !self.breakpoints.is_empty() && self.breakpoints.contains(&self.cpu.pc))
                && self.skip_breakpoint.take() != Some(self.cpu.pc)
            {
                return Outcome::Stopped { pc: self.cpu.pc };
            }
            self.skip_breakpoint = None;

            let flow = if self.cpu.t { self.execute() } else { Err(Fault::InvState) };
            match flow {
                Ok(Flow::Next) => {}
                Ok(Flow::Halt(outcome)) => return outcome,
                Err(fault) => {
                    if let Err(outcome) = self.fault(fault) {
                        return outcome;
                    }
                }
            }
            self.cycles += 1;
            self.retired += 1;
            if self.cycles >= self.tick_due {
                self.sync_systick();
            }
        }
        Outcome::Budget
    }

    /// Resumes after a debugger stop: the core leaves Debug state, and the
    /// next [`run`](Machine::run) executes the instruction at the pc even
    /// if a breakpoint is set on it.
    pub fn resume(&mut self) {
        self.debug.halted = false;
        self.debug.stepping = false;
        self.skip_breakpoint = Some(self.cpu.pc);
    }

    /// Executes one instruction, or takes one exception — whichever the
    /// machine would do next — and halts.
    pub fn step(&mut self) -> Outcome {
        self.resume();
        self.debug.stepping = true;
        self.run(1)
    }

    /// Brings SysTick up to the present, and works out when it next fires.
    pub(crate) fn sync_systick(&mut self) {
        let elapsed = self.cycles - self.ticked_at;
        if elapsed > 0 {
            self.bus.scs.tick(elapsed);
        }
        self.ticked_at = self.cycles;
        self.tick_due = self.bus.scs.systick.cycles_to_fire().map_or(u64::MAX, |n| self.cycles + n);
    }

    /// Whether an access at `addr` reaches SysTick, and so must find it
    /// up to date.
    pub(crate) fn touches_systick(addr: u32, len: u32) -> bool {
        addr < 0xE000_E020 && addr.saturating_add(len) > 0xE000_E010
    }

    /// Reads memory as a debugger does: no permission check, no side
    /// effects. `None` if any of it is unmapped.
    pub fn debug_read(&mut self, addr: u32, len: usize) -> Option<Vec<u8>> {
        if Self::touches_systick(addr, len as u32) {
            self.sync_systick();
        }
        (0..len as u32)
            .map(|i| {
                let a = addr.wrapping_add(i);
                match self.debug_register(a, true) {
                    Some(word) => Some((word >> (8 * (a & 3))) as u8),
                    None => self.bus.peek(a, 1).ok().map(|b| b as u8),
                }
            })
            .collect()
    }

    /// Writes memory as a debugger does: no permission check, word stores
    /// where the address and length allow — a peripheral register wants
    /// the whole word at once — and bytes elsewhere. False if any of it
    /// was refused.
    pub fn debug_write(&mut self, addr: u32, bytes: &[u8]) -> bool {
        let mut at = 0;
        while at < bytes.len() {
            let a = addr.wrapping_add(at as u32);
            let ok = if a & 3 == 0 && bytes.len() - at >= 4 {
                let word = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                at += 4;
                if debug::answers(a) { self.set_debug_register(a, word) } else { self.bus.write(a, 4, word).is_ok() }
            } else {
                at += 1;
                self.bus.write(a, 1, bytes[at - 1] as u32).is_ok()
            };
            if !ok {
                return false;
            }
        }
        self.drain_edges();
        if Self::touches_systick(addr, bytes.len() as u32) {
            self.sync_systick();
        }
        true
    }

    /// Whether an access by code of the given privilege is allowed, at the
    /// current execution priority.
    pub(crate) fn permits(&mut self, addr: u32, access: Access, privileged: bool) -> bool {
        let negative = self.priority < 0;
        let context = (self.bus.scs.mpu.epoch as u64) << 2 | (negative as u64) << 1 | privileged as u64;
        let granule = addr >> 5;
        let grant = &mut self.granted[access as usize][granule as usize % GRANTS];
        if grant.granule == granule && grant.context == context {
            return true;
        }
        let allowed = self.bus.scs.mpu.permits(addr, access, privileged, negative);
        if allowed {
            *grant = Grant { granule, context };
        }
        allowed
    }

    pub(crate) fn update_priority(&mut self) {
        self.priority = self.bus.scs.nvic.execution_priority(self.cpu.primask);
    }

    /// Takes a HardFault for `fault`, at the current pc. A fault the
    /// processor cannot take — because it is already at HardFault's
    /// priority or above — is a lockup.
    pub(crate) fn fault(&mut self, fault: Fault) -> Result<(), Outcome> {
        self.events.push(Event::Fault {
            cycle: self.cycles,
            pc: self.cpu.pc,
            fault,
            mode: self.cpu.mode,
            privileged: self.cpu.privileged(),
        });
        if self.priority < 0 {
            return Err(Outcome::Lockup { pc: self.cpu.pc, fault });
        }
        self.enter(HARD_FAULT)?;
        if self.catches_hard_fault() {
            self.debug.halt(debug::DFSR_VCATCH);
            return Err(Outcome::Halted);
        }
        Ok(())
    }

    /// Moves the GPIO edges a store produced into the event log.
    pub(crate) fn drain_edges(&mut self) {
        let cycle = self.cycles;
        self.events.extend(self.bus.soc.edges.drain(..).map(|edge| Event::Pin { cycle, edge }));
    }
}
