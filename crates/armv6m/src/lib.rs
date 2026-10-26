//! An M-profile machine: a Cortex-M0+ (ARMv6-M) or Cortex-M7 (ARMv7E-M,
//! with its FPU) interpreter, and enough of an STM32G0 or STM32H753 around
//! it to boot Hubris.
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
//! - [`exec`] — what each of the ARMv6-M instructions means, and the
//!   16-bit ones of ARMv7-M; [`thumb2`] the 32-bit ones; [`fpu`] the
//!   floating-point unit's.
//! - [`exception`] — entering and leaving handlers, and deciding which one.
//! - [`scs`] — SysTick, the NVIC, the SCB and the MPU, as memory-mapped
//!   registers.
//! - [`debug`] and [`dap`] — halting debug, and the debug port a probe
//!   reaches it through.
//! - [`bus`], [`part`], [`stm32g0`] and [`stm32h7`] — the rest of the
//!   address space, and the peripherals in it.
//! - [`net`] — a host at the other end of an H7's Ethernet cable.
//! - [`Machine`] — the run loop.
//!
//! **Time is retired instructions.** Each one is one cycle of the clock
//! Hubris configures SysTick against — 16 MHz on a G0, 400 MHz on an H7 —
//! and nothing else advances it:
//! a run is a pure function of the image, the same way a zaqaru container
//! is a pure function of its tape. `wfi` does not spin; it jumps the clock
//! to the next event, which is also what makes an idle system cheap to run.

pub mod bus;
pub mod clock;
pub mod cmsis_dap;
pub mod dap;
pub mod debug;
pub mod devices;
pub mod exception;
pub mod eth_h7;
pub mod exec;
pub mod flash;
pub mod flash_h7;
pub mod fpu;
pub mod gdb;
pub mod gpio;
pub mod i2c;
pub mod net;
pub mod part;
pub mod scs;
pub mod spi_h7;
pub mod state;
pub mod stm32g0;
pub mod stm32h7;
pub mod thumb2;
pub mod timer;
pub mod usart;

use std::collections::BTreeSet;

use bus::{Bus, Unmapped};
use exec::Flow;
use scs::exception::HARD_FAULT;
use state::{Core, Cpu, Mode};

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
    /// An instruction fetch nothing answered.
    InstructionBus { addr: u32 },
    /// Pushing an exception frame faulted; the exception that wanted it is
    /// still pending.
    Stacking { exception: u32 },
    /// SDIV or UDIV by zero with CCR.DIV_0_TRP set. ARMv7-M only.
    DivideByZero,
    /// A coprocessor instruction with the coprocessor absent or disabled.
    NoCoprocessor { instr: u32 },
    /// Popping an exception frame faulted, returning from `exception`.
    /// ARMv7-M only: ARMv6-M reports it as the MemManage it is.
    Unstacking { exception: u32 },
}

/// Why [`Machine::run`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The time or the instructions it was given ran out.
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
/// simulated time it happened at, in picoseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A GPIO port's output changed.
    Pin { time: u64, edge: gpio::Edge },
    /// A USART transmitted a byte.
    Serial { time: u64, byte: usart::Byte },
    /// The Ethernet controller put a frame on the wire: destination MAC
    /// first, no FCS.
    Frame { time: u64, frame: Vec<u8> },
    /// A HardFault was taken. `pc` is the instruction it was taken at;
    /// `mode` and `privileged` say what was running.
    Fault { time: u64, pc: u32, fault: Fault, mode: Mode, privileged: bool },
}

pub struct Machine {
    /// Which STM32 this is.
    pub part: part::Part,
    /// Which processor: the part's own Cortex-M0+, or a Cortex-M7.
    pub core: Core,
    /// Whether the instruction executing is inside an IT block.
    pub(crate) it_active: bool,
    /// The local exclusive monitor: the address an LDREX marked, until a
    /// STREX, a CLREX or an exception clears it.
    pub(crate) exclusive: Option<u32>,
    /// A 32-bit instruction's write to the pc, taken as a branch when it
    /// completes.
    pub(crate) branch_to: Option<u32>,
    pub cpu: Cpu,
    pub bus: Bus,
    /// Cycles of the core's clock since power-on: what each instruction
    /// cost, plus whatever `wfi` slept through.
    pub cycles: u64,
    /// Simulated time since power-on, in picoseconds: each cycle at the
    /// core's clock as RCC had it then (see [`clock`]).
    pub time: u64,
    /// The core clock's period, in picoseconds.
    pub period: u64,
    /// Cycles the instruction executing costs beyond its first: its bus
    /// accesses' wait states, a taken branch's refill, a divide's.
    pub(crate) stall: u64,
    pub retired: u64,
    pub events: Vec<Event>,
    /// How many times each exception has been entered.
    pub taken: Box<[u64; scs::exception::COUNT]>,
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
    /// the time the next timed event is due at — its reaching zero, or a
    /// peripheral's.
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
        Machine::stm32g0(part::STM32G031, image)
    }

    /// A G0 of the given part with `image` in flash, out of reset.
    pub fn stm32g0(part: part::Part, image: &[u8]) -> Result<Machine, String> {
        Machine::new(part, image)
    }

    /// The given part, with its own processor and `image` in flash, out of
    /// a power-on reset.
    pub fn new(part: part::Part, image: &[u8]) -> Result<Machine, String> {
        let mut machine = Machine {
            part,
            core: part.core(),
            it_active: false,
            exclusive: None,
            branch_to: None,
            cpu: Cpu::default(),
            bus: Bus::new(part, image)?,
            cycles: 0,
            time: 0,
            period: 1,
            stall: 0,
            retired: 0,
            events: Vec::new(),
            taken: Box::new([0; scs::exception::COUNT]),
            priority: scs::BASE_PRIORITY,
            sleeping: false,
            breakpoints: BTreeSet::new(),
            debug: debug::Debug::default(),
            skip_breakpoint: None,
            ticked_at: 0,
            tick_due: u64::MAX,
            granted: Box::new([[Grant::NONE; GRANTS]; 3]),
        };
        machine.bus.scs.boot = part.boot();
        machine.bus.scs.irq_lines = match part.family {
            part::Family::G0 => 32,
            // 150 lines, as ICTR counts them.
            part::Family::H7 => 160,
        };
        machine.period = clock::Clocks::period(machine.bus.soc.clocks().core);
        machine.reset_core()?;
        Ok(machine)
    }

    /// The same machine with a different processor in it, out of reset.
    pub fn set_core(&mut self, core: Core) -> Result<(), String> {
        self.core = core;
        self.reset()
    }

    /// A system reset: the processor and peripherals back to their reset
    /// state, memory left as it was, and the boot vector taken (B1.5.5).
    pub fn reset(&mut self) -> Result<(), String> {
        self.bus.now = self.time;
        self.bus.reset_soc();
        self.period = clock::Clocks::period(self.bus.soc.clocks().core);
        self.reset_core()
    }

    /// The processor's half of a reset.
    fn reset_core(&mut self) -> Result<(), String> {
        self.cpu = Cpu::default();
        self.exclusive = None;
        self.bus.scs.core = self.core;
        self.bus.scs.reset();
        self.sleeping = false;
        *self.granted = [[Grant::NONE; GRANTS]; 3];
        self.ticked_at = self.cycles;
        self.tick_due = u64::MAX;
        let boot = self.part.boot();
        let vector = |bus: &mut Bus, n: u32| {
            let at = boot + 4 * n;
            bus.read(at, 4).map_err(|Unmapped| format!("no vector table at 0x{at:08x}"))
        };
        self.cpu.msp = vector(&mut self.bus, 0)? & !3;
        let entry = vector(&mut self.bus, 1)?;
        self.cpu.pc = entry & !1;
        self.cpu.t = entry & 1 != 0;
        self.update_priority();
        self.debug_reset();
        Ok(())
    }

    /// Runs for `budget` cycles of the core's clock as it is now, or until
    /// something stops it: see [`Machine::run_until`].
    pub fn run(&mut self, budget: u64) -> Outcome {
        let until = self.time.saturating_add(budget.saturating_mul(self.period));
        self.run_until(until, u64::MAX)
    }

    /// Runs until simulated time `until`, in picoseconds, or for
    /// `instructions`, whichever comes first, or until something stops it.
    /// A halted core runs nothing, though a reset asked for still answers;
    /// a core told to step runs one instruction, even with no time to run
    /// it in, and halts again.
    pub fn run_until(&mut self, until: u64, instructions: u64) -> Outcome {
        if self.bus.scs.reset_requested {
            return Outcome::SystemReset;
        }
        if self.debug.halted {
            return Outcome::Halted;
        }
        // A step is one instruction whatever the budget: a debugger that
        // asked for one is owed it, even by a caller with no time to give.
        if self.debug.stepping {
            self.debug.stepping = false;
            let outcome = self.execute_for(u64::MAX, 1);
            if self.debug.halted {
                return self.settle(outcome);
            }
            self.settle(outcome);
            self.debug.halt(debug::DFSR_HALTED);
            return Outcome::Halted;
        }
        let outcome = self.execute_for(until, instructions);
        self.settle(outcome)
    }

    /// Charges `cycles` of the core's clock: the time they take, and
    /// whatever falls due in it.
    fn charge(&mut self, cycles: u64) {
        self.cycles += cycles;
        self.time += cycles * self.period;
        if self.time >= self.tick_due {
            self.sync_systick();
        }
    }

    fn execute_for(&mut self, until: u64, instructions: u64) -> Outcome {
        let last = self.retired.saturating_add(instructions);
        while self.time < until && self.retired < last {
            if self.bus.scs.reset_requested {
                return Outcome::SystemReset;
            }

            // An exception that can preempt is taken before the next
            // instruction — and between any two, which is the whole of
            // "asynchronous" on a core this simple.
            if let Some((n, p)) = self.bus.scs.nvic.highest_pending().filter(|_| !self.debug.maskints) {
                if p < self.priority {
                    let entered = self.enter(n);
                    let stall = std::mem::take(&mut self.stall);
                    self.charge(stall);
                    if let Err(outcome) = entered {
                        return outcome;
                    }
                    continue;
                }
                // `wfi` wakes for anything that would preempt with PRIMASK
                // clear, and then carries on without taking it.
                if self.sleeping && p < self.bus.scs.nvic.execution_priority(false, self.cpu.basepri, self.cpu.faultmask) {
                    self.sleeping = false;
                }
            }

            if self.sleeping {
                if self.tick_due == u64::MAX {
                    return Outcome::Asleep;
                }
                // Asleep, the clock runs on to whatever is due next: whole
                // cycles of it, the first that reaches that time.
                let to = self.tick_due.min(until);
                let cycles = (to.saturating_sub(self.time)).div_ceil(self.period).max(1);
                self.charge(cycles);
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
            self.retired += 1;
            let stall = std::mem::take(&mut self.stall);
            self.charge(1 + stall);
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

    /// Brings SysTick and the peripherals' timers up to the present, and
    /// works out when the next of them fires.
    pub(crate) fn sync_systick(&mut self) {
        let elapsed = self.cycles - self.ticked_at;
        if elapsed > 0 {
            self.bus.scs.tick(elapsed);
        }
        self.ticked_at = self.cycles;
        // A store to RCC may have changed the core's clock: from here on a
        // cycle is the new clock's.
        self.period = clock::Clocks::period(self.bus.soc.clocks().core);
        self.tick_due = self
            .bus
            .scs
            .systick
            .cycles_to_fire()
            .map_or(u64::MAX, |n| self.time.saturating_add(n.saturating_mul(self.period)));
        if let Some(due) = self.bus.soc.due() {
            if due <= self.time {
                self.bus.now = self.time;
                self.bus.soc.advance(self.time);
                // A frame all in goes to the receive DMA.
                self.bus.run_dma();
                self.drain_edges();
                self.sample_interrupts();
            }
            self.tick_due = self.tick_due.min(self.bus.soc.due().unwrap_or(u64::MAX));
        }
    }

    /// When the next timed event is due, in picoseconds: SysTick reaching
    /// zero, or a peripheral's timer or transfer.
    pub fn next_due(&mut self) -> Option<u64> {
        self.bus.now = self.time;
        self.sync_systick();
        (self.tick_due != u64::MAX).then_some(self.tick_due)
    }

    /// Whether an access at `addr` reaches SysTick, and so must find it
    /// up to date.
    pub(crate) fn touches_systick(addr: u32, len: u32) -> bool {
        addr < 0xE000_E020 && addr.saturating_add(len) > 0xE000_E010
    }

    /// Reads memory as a debugger does: no permission check, no side
    /// effects. `None` if any of it is unmapped.
    pub fn debug_read(&mut self, addr: u32, len: usize) -> Option<Vec<u8>> {
        self.bus.now = self.time;
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
        self.bus.now = self.time;
        let mut at = 0;
        while at < bytes.len() {
            let a = addr.wrapping_add(at as u32);
            let ok = if a & 3 == 0 && bytes.len() - at >= 4 {
                let word = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                at += 4;
                if debug::answers(self.core, a) { self.set_debug_register(a, word) } else { self.bus.write(a, 4, word).is_ok() }
            } else {
                at += 1;
                self.bus.write(a, 1, bytes[at - 1] as u32).is_ok()
            };
            if !ok {
                return false;
            }
        }
        self.drain_edges();
        if Self::touches_systick(addr, bytes.len() as u32) || (0x4000_0000..0xE000_0000).contains(&addr) {
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
        self.priority = self.bus.scs.nvic.execution_priority(self.cpu.primask, self.cpu.basepri, self.cpu.faultmask);
    }

    /// Takes the exception for `fault`, at the current pc: HardFault on
    /// ARMv6-M; on ARMv7-M the configurable fault it is, with its syndrome
    /// in CFSR, unless it has to escalate (B1.5.15). A fault the processor
    /// cannot take — because it is already at HardFault's priority or
    /// above — is a lockup.
    pub(crate) fn fault(&mut self, fault: Fault) -> Result<(), Outcome> {
        self.events.push(Event::Fault {
            time: self.time,
            pc: self.cpu.pc,
            fault,
            mode: self.cpu.mode,
            privileged: self.cpu.privileged(),
        });
        let n = match self.core {
            Core::CortexM0Plus => (self.priority >= 0).then_some(HARD_FAULT),
            Core::CortexM7 => {
                let n = self.syndrome(fault);
                self.escalate(n)
            }
        };
        let Some(n) = n else {
            return Err(Outcome::Lockup { pc: self.cpu.pc, fault });
        };
        self.enter(n)?;
        if self.catches_hard_fault() {
            self.debug.halt(debug::DFSR_VCATCH);
            return Err(Outcome::Halted);
        }
        Ok(())
    }

    /// Which configurable fault an ARMv7-M fault is, recorded in CFSR.
    fn syndrome(&mut self, fault: Fault) -> u32 {
        use scs::cfsr;
        use scs::exception::{BUS_FAULT, MEM_MANAGE, USAGE_FAULT};
        match fault {
            Fault::MemManage { access: Access::Execute, .. } => self.record_fault(MEM_MANAGE, cfsr::IACCVIOL, None),
            Fault::MemManage { addr, .. } => self.record_fault(MEM_MANAGE, cfsr::DACCVIOL, Some(addr)),
            Fault::Bus { addr } => self.record_fault(BUS_FAULT, cfsr::PRECISERR, Some(addr)),
            Fault::InstructionBus { .. } => self.record_fault(BUS_FAULT, cfsr::IBUSERR, None),
            Fault::Unaligned { .. } => self.record_fault(USAGE_FAULT, cfsr::UNALIGNED, None),
            Fault::Undefined { .. } => self.record_fault(USAGE_FAULT, cfsr::UNDEFINSTR, None),
            Fault::InvState => self.record_fault(USAGE_FAULT, cfsr::INVSTATE, None),
            Fault::InvPc => self.record_fault(USAGE_FAULT, cfsr::INVPC, None),
            Fault::NoCoprocessor { .. } => self.record_fault(USAGE_FAULT, cfsr::NOCP, None),
            Fault::DivideByZero => self.record_fault(USAGE_FAULT, cfsr::DIVBYZERO, None),
            // An SVC that could not be taken, and a fault deriving from a
            // frame, are HardFaults by the time they get here.
            Fault::SvcEscalation | Fault::Stacking { .. } | Fault::Unstacking { .. } => {
                self.bus.scs.faults.hfsr |= scs::HFSR_FORCED;
                HARD_FAULT
            }
        }
    }

    /// Moves the GPIO edges and the serial bytes a store produced into the
    /// event log.
    pub(crate) fn drain_edges(&mut self) {
        let time = self.time;
        let (edges, serial) = self.bus.soc.outputs();
        self.events.extend(edges.drain(..).map(|edge| Event::Pin { time, edge }));
        self.events.extend(serial.drain(..).map(|byte| Event::Serial { time, byte }));
        if let Some(frames) = self.bus.soc.frames() {
            self.events.extend(frames.drain(..).map(|frame| Event::Frame { time, frame }));
        }
    }

    /// Pends every external interrupt whose line a peripheral asserts. A
    /// level, sampled wherever it can have changed — a store to a peripheral
    /// or the NVIC, an exception return, a byte arriving — so an interrupt
    /// still asserted when its handler returns, or when it is unmasked, is
    /// pending again, as the NVIC makes a level-triggered line.
    pub(crate) fn sample_interrupts(&mut self) {
        for (word, lines) in self.bus.soc.interrupts().into_iter().enumerate() {
            if lines != 0 {
                self.bus.scs.nvic.pending.set_irq_word(32 * word as u32, lines, true);
            }
        }
    }

    /// A frame arriving on the Ethernet cable, destination MAC first and no
    /// FCS: into the receive ring, if the DMA is running and has a
    /// descriptor for it. False if the part has no Ethernet.
    pub fn deliver(&mut self, frame: Vec<u8>) -> bool {
        let crate::bus::Soc::H7(soc) = &mut self.bus.soc else { return false };
        soc.eth.arrive(frame, self.time);
        // The frame is due when it is all in.
        self.sync_systick();
        true
    }

    /// Something outside the chip drives a pin to a level — a button
    /// pressed, say — or lets go of it (`None`). Port 0 is A.
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
        self.bus.soc.drive(port, pin, level);
        self.sample_interrupts();
    }

    /// Wires a part to the board's buses: see [`devices::Wiring`].
    pub fn attach(&mut self, wiring: devices::Wiring) -> Result<(), String> {
        let crate::bus::Soc::H7(soc) = &mut self.bus.soc else {
            return Err(format!("parts are wired only to the H7s, not the {}", self.part.name));
        };
        soc.attach(wiring)?;
        self.sample_interrupts();
        Ok(())
    }

    /// The parts on the I2C buses, to set what they read.
    pub fn i2c_parts_mut(&mut self) -> Vec<&mut devices::I2cPart> {
        match &mut self.bus.soc {
            crate::bus::Soc::H7(soc) => soc.i2cs.iter_mut().flat_map(|i2c| i2c.targets.iter_mut()).collect(),
            _ => Vec::new(),
        }
    }

    /// What is wired to the board's buses, a line each.
    pub fn wired(&self) -> Vec<String> {
        match &self.bus.soc {
            crate::bus::Soc::H7(soc) => soc.wired(),
            _ => Vec::new(),
        }
    }

    /// A byte arriving on USART `port`'s line (0 for USART1): taken if the
    /// receiver is on, and its interrupt raised if enabled.
    pub fn receive(&mut self, port: usize, byte: u8) -> bool {
        let taken = self.bus.soc.receive(port, byte, self.time);
        // The byte is due when its frame has passed.
        self.sync_systick();
        taken
    }
}
