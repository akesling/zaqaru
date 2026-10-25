//! `zaqaru-m0`: boot a Hubris image on the emulated STM32G031 and say what
//! it did.
//!
//! ```sh
//! zaqaru-m0 target/demo-stm32g031-nucleo/dist/default/final.bin --ms 3000
//! ```
//!
//! The image is the flat `final.bin` a Hubris `cargo xtask dist` writes,
//! which is flash from `0x0800_0000`. What comes out is the run's events —
//! the user LED going on and off, any HardFault and where — and a summary:
//! how far simulated time got, how fast, and which exceptions it took.
//!
//! `zaqaru-m0 serve` runs the same machine as a board on a bench instead:
//! in real time, behind a GDB server a debugger or Humility can attach to.
//! See [`serve`]. `zaqaru-m0 door` is only that front door, for a board
//! something else runs — zaqaru's kernel, chiefly. See [`door`].

mod door;
mod serve;

use std::process::ExitCode;
use std::time::Instant;

use armv6m::scs::exception;
use armv6m::stm32g0::Edge;
use armv6m::{Event, Machine, Outcome};

/// Hubris's G0 demo runs SysTick off the 16 MHz HSI.
pub(crate) const CYCLES_PER_MS: u64 = 16_000;

pub(crate) fn usage() -> ExitCode {
    eprintln!("usage: zaqaru-m0 <final.bin> [--part stm32g031|stm32g070|…] [--ms N] [--quiet]");
    eprintln!("       zaqaru-m0 serve <final.bin> [--gdb ADDR] [--listen ADDR] [--archive ZIP] [--board FILE]");
    eprintln!("       zaqaru-m0 door [--listen ADDR] [--probe ADDR] [--archive ZIP] [--humility PATH]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args.next_if(|a| a == "serve").is_some() {
        return serve::main(args.collect());
    }
    if args.next_if(|a| a == "door").is_some() {
        return door::main(args.collect());
    }
    let mut image = None;
    let mut ms: u64 = 2000;
    let mut quiet = false;
    let mut part = armv6m::part::STM32G031;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--part" => match args.next().and_then(|v| armv6m::part::Part::named(&v)) {
                Some(named) => part = named,
                None => return usage(),
            },
            "--ms" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => ms = v,
                None => return usage(),
            },
            "--quiet" => quiet = true,
            _ if image.is_none() && !arg.starts_with('-') => image = Some(arg),
            _ => return usage(),
        }
    }
    let Some(image) = image else { return usage() };
    let bytes = match std::fs::read(&image) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("zaqaru-m0: {image}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut machine = match Machine::stm32g0(part, &bytes) {
        Ok(machine) => machine,
        Err(e) => {
            eprintln!("zaqaru-m0: {e}");
            return ExitCode::FAILURE;
        }
    };

    let end = ms * CYCLES_PER_MS;
    let started = Instant::now();
    let mut shown = 0;
    let mut faults = 0;
    let mut resets = 0;
    let outcome = loop {
        // In slices, so events print as they happen rather than at the end.
        let slice = (end - machine.cycles).min(10 * CYCLES_PER_MS);
        let outcome = machine.run(slice);
        for event in &machine.events[shown..] {
            if let Event::Fault { .. } = event {
                faults += 1;
            }
            if !quiet {
                println!("{}", describe(event));
            }
        }
        shown = machine.events.len();
        match outcome {
            Outcome::Budget if machine.cycles < end => continue,
            Outcome::SystemReset => {
                resets += 1;
                println!("{:>10.3} ms  system reset requested", ms_at(machine.cycles));
                if let Err(e) = machine.reset() {
                    eprintln!("zaqaru-m0: {e}");
                    return ExitCode::FAILURE;
                }
            }
            outcome => break outcome,
        }
    };
    let wall = started.elapsed();

    println!();
    match outcome {
        Outcome::Budget => println!("ran {ms} ms of simulated time"),
        Outcome::Lockup { pc, fault } => println!("LOCKUP at pc=0x{pc:08x}: {fault:?}"),
        Outcome::Breakpoint { pc, imm } => println!("bkpt #{imm} at pc=0x{pc:08x}"),
        Outcome::Asleep => println!("asleep with nothing to wake it, at pc=0x{:08x}", machine.cpu.pc),
        Outcome::Stopped { .. } | Outcome::Halted => unreachable!("no debugger is attached"),
        Outcome::SystemReset => unreachable!(),
    }
    let secs = wall.as_secs_f64();
    println!(
        "{} instructions, {:.3} ms simulated, in {:.3} s ({:.1} MIPS, {:.1}x real time)",
        machine.retired,
        ms_at(machine.cycles),
        secs,
        machine.retired as f64 / secs / 1e6,
        ms_at(machine.cycles) / 1000.0 / secs,
    );
    let taken: Vec<String> = machine
        .taken
        .iter()
        .enumerate()
        .filter(|&(_, &count)| count > 0)
        .map(|(n, count)| format!("{} {count}", exception::name(n as u32)))
        .collect();
    println!("exceptions taken: {}", taken.join(", "));
    println!("faults: {faults}, resets: {resets}");
    let mpu = &machine.bus.scs.mpu;
    println!(
        "MPU {}, {} of 8 regions enabled for the current task",
        if mpu.ctrl & 1 != 0 { "on" } else { "off" },
        mpu.regions.iter().filter(|r| r.rasr & 1 != 0).count(),
    );
    if !machine.bus.soc.unmodelled.is_empty() {
        let regs: Vec<String> = machine.bus.soc.unmodelled.iter().map(|a| format!("0x{a:08x}")).collect();
        println!("unmodelled peripheral registers touched: {}", regs.join(" "));
    }

    match outcome {
        Outcome::Budget if faults == 0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

pub(crate) fn ms_at(cycles: u64) -> f64 {
    cycles as f64 / CYCLES_PER_MS as f64
}

pub(crate) fn describe(event: &Event) -> String {
    match *event {
        Event::Pin { cycle, edge } => {
            let Edge { port, before, after } = edge;
            let port = (b'A' + port) as char;
            let mut line = format!("{:>10.3} ms  GPIO{port} ODR 0x{before:04x} -> 0x{after:04x}", ms_at(cycle));
            // The Nucleo-G031's LD4 is PC6, wired active-low.
            if port == 'C' && (before ^ after) & 1 << 6 != 0 {
                line += if after & 1 << 6 == 0 { "  LED on" } else { "  LED off" };
            }
            line
        }
        Event::Serial { cycle, byte } => {
            format!("{:>10.3} ms  USART{} {:?}", ms_at(cycle), byte.port + 1, byte.byte as char)
        }
        Event::Fault { cycle, pc, fault, mode, privileged } => format!(
            "{:>10.3} ms  HardFault at pc=0x{pc:08x} ({mode:?}, {}): {fault:?}",
            ms_at(cycle),
            if privileged { "privileged" } else { "unprivileged" },
        ),
    }
}
