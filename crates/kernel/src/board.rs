//! A microcontroller board on the container's bench: an emulated Cortex-M0+
//! (see the `armv6m` crate) that the kernel runs beside the processes, with
//! a debug probe the processes can reach.
//!
//! An image asks for one by containing `/etc/zaqaru/board`:
//!
//! ```text
//! chip   stm32g031        # or stm32g030, stm32g070, or an ordering code
//! led    PC6 low          # optional: the part's Nucleo's, by default
//! image  /hubris/final.bin
//! probe  127.0.0.1:3333
//! ```
//!
//! The board is kernel state, like the socket arena: one per container,
//! shared by every process, kept across `fork` and `execve`. That is the
//! point of having it here rather than in a process. A process that
//! emulated it would be interpreted twice over — every ARM instruction as
//! the hundreds of x86 instructions an emulator spends on it — and would
//! pace itself with timed waits; here it is plain Rust in the module, a
//! few instructions of the host per instruction of the board, and it keeps
//! time with the container's own clock.
//!
//! # Time
//!
//! The board runs at 16 MHz of *simulated* time per second of the
//! container's monotonic clock, advanced at the points the scheduler
//! already crosses to the host — a process finishing its slice, and the
//! container going idle — and so at points that are a pure function of
//! execution, against clock readings that are store answers and so on the
//! tape. A recorded run replays the board too. A halted board's time
//! stands still, as SysTick's does under a debugger, and a board that
//! falls more than [`MAX_BEHIND_NS`] behind gives the time up rather than
//! racing to make it up.
//!
//! # The probe
//!
//! A listening socket the kernel holds, on the configured address. A
//! `connect` to it is an ordinary loopback connection, queued on the
//! listener like any other; the board takes it off the queue as `accept`
//! would and serves the GDB remote protocol (`armv6m::gdb`) on its two
//! rings. One debugger at a time: a second connection replaces the first,
//! which is what a probe with one debug port does.
//!
//! And a second probe, on USB: a CMSIS-DAP v2 probe (`armv6m::cmsis_dap`)
//! on the board's debug port, which the board serves as a USB provider
//! (see [`crate::usb`]) under the id `board`. That is the one a debugger
//! built on probe-rs finds by itself, as it would find a probe plugged in.
//! Both reach the same core, and see the same halts.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use armv6m::cmsis_dap::{self, Probe};

use armv6m::gdb::{self, Debugger, Response};
use armv6m::stm32g0::Edge;
use armv6m::{Event, Machine, Outcome};

use crate::ring::End;
use crate::socket::{Address, Endpoint, State};

/// Where an image asks for a board.
pub const CONFIGURATION: &[u8] = b"/etc/zaqaru/board";

/// The board's clock: the G0 demo's 16 MHz HSI, in cycles per microsecond.
const CYCLES_PER_MICROSECOND: u64 = 16;

/// How far behind the container's clock the board may fall before it gives
/// the time up.
pub const MAX_BEHIND_NS: u64 = 250_000_000;

/// How often an idle container wakes to move a running board on. Nothing a
/// process can see depends on it; it bounds how stale the board's state is
/// when nobody is looking, and how bunched its events are.
pub const TICK_NS: u64 = 20_000_000;

/// The board, as the kernel holds it: absent until the first look, then
/// either nothing (no configuration) or a board.
#[derive(Default)]
pub enum Slot {
    #[default]
    Unlooked,
    Absent,
    Present(Box<Board>),
}

pub type Shared = Rc<RefCell<Slot>>;

pub struct Board {
    pub machine: Machine,
    pub debugger: Debugger,
    pub chip: String,
    pub image: String,
    /// The listener the probe answers on.
    listener: u32,
    pub probe: Address,
    client: Option<Client>,
    /// The monotonic instant simulated time zero corresponds to; `None`
    /// until the first reading.
    origin: Option<u64>,
    /// The last monotonic reading the board was moved on at.
    pub now: u64,
    pub led: Option<bool>,
    /// Which pin the LED is on, if the board has one.
    pub led_pin: Option<Led>,
    /// What the USARTs have sent, the latest [`SERIAL_KEPT`] bytes or so.
    pub serial: Vec<u8>,
    pub faults: u64,
    /// Lines for the console, not yet written.
    pub console: Vec<String>,
    /// The USB probe, and the responses it has for the host to read.
    usb_probe: Probe,
    usb_responses: VecDeque<Vec<u8>>,
}

/// The debugger connected to the probe.
struct Client {
    socket: u32,
    /// What the debugger writes, which the board reads.
    receive: u32,
    /// What the board writes, which the debugger reads.
    transmit: u32,
    /// Bytes read that do not make a whole packet yet.
    partial: Vec<u8>,
    /// Replies the transmit ring had no room for yet.
    outbox: Vec<u8>,
}

/// What a configuration file says, before anything is loaded.
#[derive(Debug, PartialEq, Eq)]
pub struct Configuration {
    pub chip: String,
    pub image: String,
    pub probe: (u32, u16),
    pub led: Option<Led>,
}

/// The board's user LED: which pin, and which level lights it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Led {
    /// 0 for port A.
    pub port: u8,
    pub pin: u8,
    pub active_low: bool,
}

impl Led {
    /// The LED of the board a part comes on, as Hubris's own LED driver has
    /// it: the G031 Nucleo's PC6, the G070 Nucleo's PA5 (lit high, as the
    /// Nucleo-64 wires it), the G030 oxcon2023g0 badge's PB7.
    pub fn of(part: &str) -> Option<Led> {
        Some(match part {
            "stm32g031" => Led { port: 2, pin: 6, active_low: true },
            "stm32g070" => Led { port: 0, pin: 5, active_low: false },
            "stm32g030" => Led { port: 1, pin: 7, active_low: true },
            _ => return None,
        })
    }

    /// `PC6 low`, `PA5 high`.
    fn parse(text: &str) -> Option<Led> {
        let (pin, level) = text.split_once(char::is_whitespace).unwrap_or((text, "high"));
        let bytes = pin.as_bytes();
        let port = match bytes {
            [b'P', port @ b'A'..=b'F', ..] => port - b'A',
            _ => return None,
        };
        let pin = pin[2..].parse().ok().filter(|&pin: &u8| pin < 16)?;
        let active_low = match level.trim() {
            "low" => true,
            "high" => false,
            _ => return None,
        };
        Some(Led { port, pin, active_low })
    }

    fn name(&self) -> String {
        format!("P{}{}", (b'A' + self.port) as char, self.pin)
    }
}

/// How much of what the USARTs sent the board keeps: the tail, for the
/// page's serial panel.
const SERIAL_KEPT: usize = 16 * 1024;

/// Parses `/etc/zaqaru/board`: one `key value` per line, `#` comments.
pub fn parse(text: &str) -> Result<Configuration, String> {
    let (mut chip, mut image, mut probe, mut led) = (None, None, None, None);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(char::is_whitespace)
            .map(|(k, v)| (k, v.trim()))
            .ok_or_else(|| format!("`{line}`: expected `key value`"))?;
        match key {
            "chip" => chip = Some(value.to_string()),
            "image" => image = Some(value.to_string()),
            "probe" => probe = Some(parse_address(value).ok_or_else(|| format!("probe `{value}`: expected a.b.c.d:port"))?),
            "led" => led = Some(Led::parse(value).ok_or_else(|| format!("led `{value}`: expected a pin and a level, `PC6 low`"))?),
            _ => return Err(format!("unknown key `{key}`")),
        }
    }
    let chip = chip.unwrap_or_else(|| "stm32g031".to_string());
    let part = armv6m::part::Part::named(&chip)
        .ok_or_else(|| format!("chip `{chip}`: the modelled parts are stm32g030, stm32g031 and stm32g070"))?;
    Ok(Configuration {
        chip: part.name.to_string(),
        image: image.ok_or("no `image`")?,
        probe: probe.unwrap_or((crate::socket::INADDR_LOOPBACK, 3333)),
        led: led.or_else(|| Led::of(part.name)),
    })
}

fn parse_address(text: &str) -> Option<(u32, u16)> {
    let (address, port) = text.rsplit_once(':')?;
    let mut packed = 0u32;
    let mut parts = 0;
    for part in address.split('.') {
        packed = (packed << 8) | u32::from(part.parse::<u8>().ok()?);
        parts += 1;
    }
    (parts == 4).then_some(())?;
    Some((packed, port.parse().ok()?))
}

impl Board {
    /// A command written to `/dev/board`, a line at a time:
    ///
    /// - `part NAME` puts a different G0 on the bench — `stm32g070`, or an
    ///   ordering code such as a Hubris archive names (`STM32G070RBTx`) —
    ///   erased, and held halted as a probe holds a blank part, for
    ///   `humility flash` to program;
    /// - `image TEXT` says what is in its flash now, for the panel.
    pub fn command(&mut self, line: &str) -> Result<(), String> {
        let (verb, rest) = line.trim().split_once(char::is_whitespace).unwrap_or((line.trim(), ""));
        match verb {
            "part" => {
                let part = armv6m::part::Part::named(rest.trim())
                    .ok_or_else(|| format!("part `{}`: the modelled parts are stm32g030, stm32g031 and stm32g070", rest.trim()))?;
                let mut machine = Machine::stm32g0(part, &[])?;
                machine.halt();
                self.machine = machine;
                self.chip = part.name.to_string();
                self.image = "(erased)".to_string();
                self.led = None;
                self.led_pin = Led::of(part.name);
                self.serial.clear();
                self.faults = 0;
                self.usb_probe = Probe::default();
                self.usb_responses.clear();
                self.rebase(self.now);
                self.console.push(format!(
                    "board: now an {} with {} KiB of flash, erased and halted",
                    part.name.to_ascii_uppercase(),
                    part.flash / 1024
                ));
                Ok(())
            }
            "image" if !rest.trim().is_empty() => {
                self.image = rest.trim().to_string();
                Ok(())
            }
            "" => Ok(()),
            other => Err(format!("`{other}`: the board takes `part NAME` and `image TEXT`")),
        }
    }

    /// The instant, in monotonic nanoseconds, by which an idle container
    /// should wake to move the board on — `None` when it is halted, since
    /// then only a debugger moves it.
    pub fn wake(&self) -> Option<u64> {
        (!self.machine.halted()).then_some(self.now + TICK_NS)
    }

    /// Brings simulated time up to `now`.
    fn advance(&mut self, now: u64) {
        let origin = *self.origin.get_or_insert(now);
        self.now = now;
        // A halted board's time stands still; a reset asked for while
        // halted is still taken.
        if self.machine.halted() && !self.machine.bus.scs.reset_requested {
            self.rebase(now);
            return;
        }
        let due = now.saturating_sub(origin) / 1000 * CYCLES_PER_MICROSECOND;
        let behind = due.saturating_sub(self.machine.cycles);
        let budget = behind.min(MAX_BEHIND_NS / 1000 * CYCLES_PER_MICROSECOND);
        let outcome = self.machine.run(budget);
        if behind > budget {
            self.rebase(now);
        }
        self.collect();
        match outcome {
            Outcome::Budget | Outcome::Asleep | Outcome::Halted => {}
            Outcome::SystemReset => self.take_reset(),
            outcome => {
                // A halt a debugger with halting debug on asked for — its
                // breakpoint, or a flash algorithm returning to one — is the
                // debugger's business, and a flash writes one per page; a
                // lockup is everyone's.
                if !self.machine.debug.enabled || matches!(outcome, Outcome::Lockup { .. }) {
                    self.console.push(format!("{} halted: {outcome:?}", self.stamp()));
                }
                if let Some(stop) = self.debugger.stopped(outcome) {
                    self.reply(&gdb::frame(&stop));
                }
            }
        }
    }

    /// A system reset, as SYSRESETREQ asks for one.
    fn take_reset(&mut self) {
        self.console.push(format!("{} system reset", self.stamp()));
        if let Err(e) = self.machine.reset() {
            self.console.push(format!("board: reset failed: {e}"));
        }
    }

    /// Restarts the clock from where the board is: simulated time now
    /// corresponds to `now`.
    fn rebase(&mut self, now: u64) {
        let ran = self.machine.cycles / CYCLES_PER_MICROSECOND * 1000;
        self.origin = Some(now.saturating_sub(ran));
    }

    fn stamp(&self) -> String {
        format!("[board {:>10.3} ms]", self.machine.cycles as f64 / (CYCLES_PER_MICROSECOND * 1000) as f64)
    }

    /// Turns the machine's events into console lines and board state.
    fn collect(&mut self) {
        let events: Vec<Event> = self.machine.events.drain(..).collect();
        for event in events {
            match event {
                Event::Pin { cycle, edge: Edge { port, before, after } } => {
                    let at = cycle as f64 / (CYCLES_PER_MICROSECOND * 1000) as f64;
                    let mut line = format!("[board {at:>10.3} ms] GPIO{} 0x{before:04x} -> 0x{after:04x}", (b'A' + port) as char);
                    if let Some(led) = self.led_pin
                        && port == led.port
                        && (before ^ after) & 1 << led.pin != 0
                    {
                        let on = (after >> led.pin & 1 != 0) != led.active_low;
                        self.led = Some(on);
                        line += if on { "  LED on" } else { "  LED off" };
                    }
                    self.console.push(line);
                }
                // Kept for the serial panel rather than put on the console:
                // a demo that prints a line a millisecond would drown it.
                Event::Serial { byte, .. } => {
                    self.serial.push(byte.byte);
                    if self.serial.len() > 2 * SERIAL_KEPT {
                        self.serial.drain(..self.serial.len() - SERIAL_KEPT);
                    }
                }
                Event::Fault { cycle, pc, fault, mode, privileged } => {
                    self.faults += 1;
                    let at = cycle as f64 / (CYCLES_PER_MICROSECOND * 1000) as f64;
                    self.console.push(format!(
                        "[board {at:>10.3} ms] HardFault at pc=0x{pc:08x} ({mode:?}, {}): {fault:?}",
                        if privileged { "privileged" } else { "unprivileged" }
                    ));
                }
            }
        }
    }

    /// Queues bytes for the debugger.
    fn reply(&mut self, bytes: &str) {
        if let Some(client) = &mut self.client {
            client.outbox.extend_from_slice(bytes.as_bytes());
        }
    }

    /// Answers whatever whole packets the debugger has sent.
    fn converse(&mut self, input: &[u8]) {
        let Some(client) = &mut self.client else { return };
        client.partial.extend_from_slice(input);
        let mut buffer = std::mem::take(&mut client.partial);
        let mut at = 0;
        while at < buffer.len() {
            match buffer[at] {
                0x03 => {
                    at += 1;
                    if !self.machine.halted() {
                        let stop = self.debugger.interrupt(&mut self.machine);
                        self.reply(&gdb::frame(&stop));
                    }
                }
                b'$' => {
                    // A packet is `$payload#xx`; wait for all of it.
                    let Some(hash) = buffer[at..].iter().position(|&b| b == b'#') else { break };
                    if at + hash + 3 > buffer.len() {
                        break;
                    }
                    let payload = String::from_utf8_lossy(&buffer[at + 1..at + hash]).into_owned();
                    at += hash + 3;
                    let was_halted = self.machine.halted();
                    match self.debugger.packet(&mut self.machine, &payload) {
                        Response::Reply(reply) => self.reply(&format!("+{}", gdb::frame(&reply))),
                        Response::Resumed => self.reply("+"),
                        Response::Detach(reply) => self.reply(&format!("+{}", gdb::frame(&reply))),
                    }
                    if was_halted && !self.machine.halted() {
                        self.rebase(self.now);
                    }
                    // A debugger's write can change a pin.
                    self.collect();
                }
                // Acks, and anything between packets.
                _ => at += 1,
            }
        }
        buffer.drain(..at);
        if let Some(client) = &mut self.client {
            client.partial = buffer;
        }
    }

    /// Lets go of a halted board when its debugger goes, as unplugging a
    /// probe does.
    fn release(&mut self) {
        self.machine.breakpoints.clear();
        let halted = self.machine.halted();
        self.machine.debug.release();
        if halted {
            self.machine.resume();
            self.rebase(self.now);
        }
    }

    /// Brings the board up to `now`, as the next service would.
    pub fn catch_up(&mut self, now: u64) {
        self.advance(now);
    }

    /// A read of the USB probe's provider paths: see [`crate::usb`].
    pub fn usb_read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> crate::abi::StoreOutcome {
        use crate::abi::StoreOutcome::{Absent, Failed, Present};
        match path {
            [b"descriptors"] => into.extend(cmsis_dap::descriptors()),
            [b"strings", index] => {
                let text = match std::str::from_utf8(index).ok().and_then(|i| i.parse::<usize>().ok()) {
                    Some(4) => cmsis_dap::INTERFACE_STRING,
                    Some(i) if (1..cmsis_dap::STRINGS.len()).contains(&i) => cmsis_dap::STRINGS[i],
                    _ => return Absent,
                };
                into.extend(text.as_bytes());
            }
            [b"speed"] => into.extend(b"480"),
            [b"endpoints", b"81", b"pending", _] => match self.usb_responses.pop_front() {
                Some(response) => into.extend(response),
                None => return Absent,
            },
            // No class or vendor requests: a stall.
            _ => return Failed,
        }
        Present
    }

    /// A write to the USB probe: a command, answered at once, as the
    /// probe's firmware would before it read the next.
    pub fn usb_write(&mut self, path: &[&[u8]], data: &[u8]) -> crate::abi::StoreOutcome {
        if path != [&b"endpoints"[..], b"01", b"tx"] {
            return crate::abi::StoreOutcome::Failed;
        }
        let was_halted = self.machine.halted();
        let response = self.usb_probe.command(&mut self.machine, data);
        if !response.is_empty() {
            self.usb_responses.push_back(response);
        }
        // A reset asked for through AIRCR happens now, as it does on the
        // part, rather than when the board is next moved on: a debugger
        // that resets and then asks whether the core is halted at the
        // vector expects the answer to be yes.
        if self.machine.bus.scs.reset_requested {
            self.take_reset();
        }
        // And a step, which a core takes in a cycle: a debugger polls for
        // the halt after it for a hundred milliseconds, not for however long
        // until the board is next moved on.
        if self.machine.debug.stepping {
            self.machine.run(1);
        }
        if was_halted && !self.machine.halted() {
            self.rebase(self.now);
        }
        // A debugger's write can change a pin.
        self.collect();
        crate::abi::StoreOutcome::Present
    }

    /// The board's state as the container's store serves it: what the
    /// page's board panel shows.
    pub fn json(&self) -> String {
        use crate::system::server::quoted;
        let m = &self.machine;
        let cpu = &m.cpu;
        let hex = |value: u32| quoted(&format!("0x{value:08x}"));
        let mut registers: Vec<String> = (0..13).map(|n| format!(r#""r{n}":{}"#, hex(cpu.r[n]))).collect();
        registers.push(format!(r#""sp":{}"#, hex(cpu.sp())));
        registers.push(format!(r#""lr":{}"#, hex(cpu.r[14])));
        registers.push(format!(r#""pc":{}"#, hex(cpu.pc)));
        registers.push(format!(r#""xpsr":{}"#, hex(cpu.xpsr())));
        registers.push(format!(r#""msp":{}"#, hex(cpu.msp)));
        registers.push(format!(r#""psp":{}"#, hex(cpu.psp)));
        let exceptions: Vec<String> = m
            .taken
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(n, count)| format!("{}:{count}", quoted(&armv6m::scs::exception::name(n as u32))))
            .collect();
        let probe = match &self.probe {
            Address::Inet { address, port } => {
                format!("{}.{}.{}.{}:{port}", address >> 24, address >> 16 & 0xff, address >> 8 & 0xff, address & 0xff)
            }
            _ => String::new(),
        };
        format!(
            r#"{{"chip":{},"image":{},"state":{},"mode":{},"privileged":{},"simulated_ms":{:.3},"instructions":{},"led":{},"led_pin":{},"faults":{},"probe":{},"debugger":{},"serial":{},"exceptions":{{{}}},"registers":{{{}}}}}"#,
            quoted(&self.chip),
            quoted(&self.image),
            quoted(if self.machine.halted() { "halted" } else { "running" }),
            quoted(&format!("{:?}", cpu.mode)),
            cpu.privileged(),
            m.cycles as f64 / (CYCLES_PER_MICROSECOND * 1000) as f64,
            m.retired,
            match self.led {
                Some(true) => quoted("on"),
                Some(false) => quoted("off"),
                None => "null".into(),
            },
            self.led_pin.map_or("null".into(), |led| quoted(&led.name())),
            self.faults,
            quoted(&probe),
            self.client.is_some(),
            // The tail, as text: enough for the panel to show, and no more
            // than a read of the lens should carry.
            quoted(&String::from_utf8_lossy(&self.serial[self.serial.len().saturating_sub(4096)..])),
            exceptions.join(","),
            registers.join(","),
        )
    }

    /// The board's state, as the console's `board` command and the store
    /// show it.
    pub fn status(&self) -> String {
        let m = &self.machine;
        let state = match self.machine.halted() {
            true => format!("halted at pc=0x{:08x}", m.cpu.pc),
            false => "running".to_string(),
        };
        let led = match self.led {
            Some(true) => "on",
            Some(false) => "off",
            None => "never driven",
        };
        let taken: Vec<String> = m
            .taken
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(n, count)| format!("{} {count}", armv6m::scs::exception::name(n as u32)))
            .collect();
        format!(
            "board       {} (emulated Cortex-M0+), in the kernel\nimage       {}\nstate       {state}\ntime        {:.3} s simulated, {} instructions\nLED ({})   {led}\nfaults      {}\nexceptions  {}\n",
            self.chip,
            self.image,
            m.cycles as f64 / (CYCLES_PER_MICROSECOND * 1_000_000) as f64,
            m.retired,
            self.led_pin.map_or("none".into(), |led| led.name()),
            self.faults,
            taken.join(", "),
        )
    }
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Moves the board on: takes any new debugger off the probe's queue,
    /// answers what the debugger sent, runs the board up to the clock, and
    /// sends what it has to say. Answers whether bytes moved toward a
    /// process — which is whether something parked may now run.
    ///
    /// Kernel state only, like the edge pump it runs beside: the board,
    /// rings, the socket arena, and the store's clock and console.
    pub fn service_board(&mut self) -> bool {
        self.look_for_board_now();
        let board = Rc::clone(&self.board);
        let mut slot = board.borrow_mut();
        let Slot::Present(board) = &mut *slot else {
            drop(slot);
            return self.service_usb();
        };

        self.accept_debugger(board);
        let input = match &board.client {
            Some(client) => {
                let queued = self.rings.borrow().queued(client.receive);
                let mut bytes = vec![0u8; queued];
                self.rings.borrow_mut().take(client.receive, &mut bytes);
                bytes
            }
            None => Vec::new(),
        };
        if !input.is_empty() {
            board.converse(&input);
        }
        if let Some(now) = self.monotonic() {
            board.advance(now);
        }
        let moved = self.flush_debugger(board);
        self.hang_up_if_gone(board);

        for line in board.console.drain(..) {
            let mut line = line.into_bytes();
            line.push(b'\n');
            self.store.write(crate::paths::CONSOLE_STDERR, &line);
        }
        drop(slot);
        // The USB probe's answers, and any other provider's, are what a
        // parked process may be waiting for too.
        self.service_usb() || moved
    }

    /// Looks for the board now rather than at the next service, for
    /// whoever needs to know whether there is one.
    pub(crate) fn look_for_board_now(&mut self) {
        if matches!(*self.board.borrow(), Slot::Unlooked) {
            let slot = self.look_for_board();
            *self.board.borrow_mut() = slot;
        }
    }

    /// When an idle container must wake to move the board on.
    pub fn board_wake(&self) -> Option<u64> {
        match &*self.board.borrow() {
            Slot::Present(board) => board.wake(),
            _ => None,
        }
    }

    /// Reads `/etc/zaqaru/board` and, if it asks for one, builds the board
    /// and puts its probe's listener in the socket arena.
    fn look_for_board(&mut self) -> Slot {
        let Ok(text) = self.read_file(CONFIGURATION) else {
            return Slot::Absent;
        };
        let configuration = match parse(&String::from_utf8_lossy(&text)) {
            Ok(configuration) => configuration,
            Err(why) => {
                self.board_says(&format!("board: {}: {why}", String::from_utf8_lossy(CONFIGURATION)));
                return Slot::Absent;
            }
        };
        let firmware = match self.read_file(configuration.image.as_bytes()) {
            Ok(firmware) => firmware,
            Err(errno) => {
                self.board_says(&format!("board: {}: {errno:?}", configuration.image));
                return Slot::Absent;
            }
        };
        let part = armv6m::part::Part::named(&configuration.chip).expect("parse checked the part");
        let machine = match Machine::stm32g0(part, &firmware) {
            Ok(machine) => machine,
            Err(why) => {
                self.board_says(&format!("board: {}: {why}", configuration.image));
                return Slot::Absent;
            }
        };
        let (address, port) = configuration.probe;
        let probe = Address::Inet { address, port };
        let listener = {
            let mut sockets = self.sockets.borrow_mut();
            let listener = sockets.create(crate::socket::family::INET, crate::socket::kind::STREAM);
            if let Some(socket) = sockets.get_mut(listener) {
                socket.state = State::Listening {
                    address: probe.clone(),
                    backlog: 4,
                    queue: Default::default(),
                };
            }
            listener
        };
        self.board_says(&format!(
            "board: {} running {}, probe on {}.{}.{}.{}:{port}",
            configuration.chip,
            configuration.image,
            address >> 24,
            address >> 16 & 0xff,
            address >> 8 & 0xff,
            address & 0xff,
        ));
        Slot::Present(Box::new(Board {
            machine,
            debugger: Debugger::default(),
            chip: configuration.chip,
            image: configuration.image,
            listener,
            probe,
            client: None,
            origin: None,
            now: 0,
            led: None,
            led_pin: configuration.led,
            serial: Vec::new(),
            faults: 0,
            console: Vec::new(),
            usb_probe: Probe::default(),
            usb_responses: VecDeque::new(),
        }))
    }

    fn board_says(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.store.write(crate::paths::CONSOLE_STDERR, &bytes);
    }

    /// The whole of a regular file in the container's filesystem.
    fn read_file(&self, path: &[u8]) -> Result<Vec<u8>, crate::errno::Errno> {
        let root = self.vfs.root();
        let vnode = self.vfs.resolve(root, path, crate::vfs::Lookup::FOLLOW)?;
        let inode = self.vfs.inode(vnode)?;
        if !inode.is_regular() {
            return Err(crate::errno::Errno::Invalid);
        }
        let filesystem = self.vfs.filesystem_of(vnode)?;
        Ok(filesystem.contents(&inode, vnode.inode)?.to_vec())
    }

    /// Takes the newest connection off the probe's queue; an older one is
    /// hung up on, as a probe with one debug port would.
    fn accept_debugger(&mut self, board: &mut Board) {
        loop {
            let Some(socket) = self.sockets.borrow_mut().dequeue(board.listener) else { return };
            let Some(Endpoint { receive, transmit, .. }) = self.sockets.borrow().endpoint(socket).cloned() else {
                continue;
            };
            if let Some(old) = board.client.take() {
                self.close_debugger(old);
            }
            board.client = Some(Client { socket, receive, transmit, partial: Vec::new(), outbox: Vec::new() });
            // Whatever the last debugger left, the next finds it running.
            board.release();
        }
    }

    /// Moves queued replies into the debugger's ring, as far as it has room.
    fn flush_debugger(&mut self, board: &mut Board) -> bool {
        let Some(client) = &mut board.client else { return false };
        if client.outbox.is_empty() {
            return false;
        }
        let given = self.rings.borrow_mut().give(client.transmit, &client.outbox);
        client.outbox.drain(..given);
        given > 0
    }

    /// Lets go of a debugger that has closed its end: no writer left on what
    /// it sends, and nothing of it unread.
    fn hang_up_if_gone(&mut self, board: &mut Board) {
        let gone = match &board.client {
            Some(client) => {
                let rings = self.rings.borrow();
                rings.writers(client.receive) == 0 && rings.queued(client.receive) == 0
            }
            None => false,
        };
        if gone {
            if let Some(client) = board.client.take() {
                self.close_debugger(client);
            }
            board.release();
        }
    }

    /// The board's end of a connection, closed as a process's `close` would.
    fn close_debugger(&mut self, client: Client) {
        {
            let mut rings = self.rings.borrow_mut();
            rings.release(client.receive, End::Read);
            rings.release(client.transmit, End::Write);
        }
        let retired = self.sockets.borrow_mut().release(client.socket);
        crate::socket::detach_endpoint(&mut self.rings.borrow_mut(), retired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_reads_as_written() {
        let parsed = parse("# the G031 demo\nchip stm32g031\nimage /hubris/final.bin\nprobe 127.0.0.1:3333\n").unwrap();
        assert_eq!(
            parsed,
            Configuration {
                chip: "stm32g031".into(),
                image: "/hubris/final.bin".into(),
                probe: (0x7F00_0001, 3333),
                led: Some(Led { port: 2, pin: 6, active_low: true }),
            }
        );
    }

    #[test]
    fn a_part_brings_its_board_s_led_unless_told_otherwise() {
        let g070 = parse("chip STM32G070RBTx\nimage /f.bin").unwrap();
        assert_eq!((g070.chip.as_str(), g070.led), ("stm32g070", Some(Led { port: 0, pin: 5, active_low: false })));
        let told = parse("chip stm32g031\nimage /f.bin\nled PA12 low").unwrap();
        assert_eq!(told.led, Some(Led { port: 0, pin: 12, active_low: true }));
        assert!(parse("image /f.bin\nled PZ9 low").is_err());
    }

    #[test]
    fn the_probe_and_chip_have_defaults_and_the_image_does_not() {
        assert_eq!(parse("image /f.bin").unwrap().probe, (crate::socket::INADDR_LOOPBACK, 3333));
        assert!(parse("chip stm32g031").is_err());
        assert!(parse("image /f.bin\nchip stm32h753").is_err());
        assert!(parse("image /f.bin\nflavour vanilla").is_err());
    }
}
