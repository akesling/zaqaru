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
use armv6m::zip;
use armv6m::stm32g0::Edge;
use armv6m::{Event, Machine, Outcome};

/// Picoseconds in a millisecond, the unit simulated time is shown in.
pub(crate) const PS_PER_MS: u64 = 1_000_000_000;

pub(crate) fn usage() -> ExitCode {
    eprintln!("usage: zaqaru-m0 <final.bin> [--part stm32g031|stm32g070|stm32h753|…] [--core m0+|m7] [--ms N] [--quiet] [--udp PORT:TEXT] [--ping]");
    eprintln!("       zaqaru-m0 serve <final.bin> [--part NAME] [--gdb ADDR] [--listen ADDR] [--archive ZIP] [--board FILE]");
    eprintln!("       zaqaru-m0 door [--listen ADDR] [--probe ADDR] [--archive ZIP] [--humility PATH]");
    eprintln!("       zaqaru-m0 repack <in.zip> <out.zip> [--stored]");
    eprintln!("       zaqaru-m0 sources <archive.zip>");
    ExitCode::from(2)
}

/// `zaqaru-m0 sources ARCHIVE`: every source file the DWARF of the
/// archive's kernel and tasks names, one a line, as the build remapped it
/// (`/hubris/…`, `/crates.io/…`, `/git/…`, `/rustc/HASH/…`): what a build
/// keeps beside the archive for a debugger to show the code.
fn sources(args: Vec<String>) -> ExitCode {
    let [input] = args.as_slice() else { return usage() };
    let archive = match std::fs::read(input) {
        Ok(archive) => archive,
        Err(e) => {
            eprintln!("zaqaru-m0: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut files = std::collections::BTreeSet::new();
    for name in zip::names(&archive).into_iter().filter(|n| n == "elf/kernel" || n.starts_with("elf/task/")) {
        let Some(elf) = zip::entry(&archive, &name) else { continue };
        match armv6m::dwarf::Debug::read(&elf) {
            Ok(debug) => files.extend(debug.files()),
            Err(why) => eprintln!("zaqaru-m0: {name}: {why}"),
        }
    }
    for file in files {
        println!("{file}");
    }
    ExitCode::SUCCESS
}

/// `zaqaru-m0 repack IN OUT`: a Hubris archive with its entries deflated
/// (or stored) rather than bzip2'd, which Humility reads in less than half
/// the time under zaqaru. See `zip::repack`.
fn repack(args: Vec<String>) -> ExitCode {
    let (input, output, packing) = match args.as_slice() {
        [input, output] => (input, output, zip::Packing::Deflated(9)),
        [input, output, flag] if flag == "--stored" => (input, output, zip::Packing::Stored),
        _ => return usage(),
    };
    let archive = match std::fs::read(input) {
        Ok(archive) => archive,
        Err(e) => {
            eprintln!("zaqaru-m0: {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(repacked) = zip::repack(&archive, packing) else {
        eprintln!("zaqaru-m0: {input}: not an archive this reads");
        return ExitCode::FAILURE;
    };
    if let Err(e) = std::fs::write(output, &repacked) {
        eprintln!("zaqaru-m0: {output}: {e}");
        return ExitCode::FAILURE;
    }
    println!("{input}: {} bytes, {} repacked", archive.len(), repacked.len());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args.next_if(|a| a == "serve").is_some() {
        return serve::main(args.collect());
    }
    if args.next_if(|a| a == "door").is_some() {
        return door::main(args.collect());
    }
    if args.next_if(|a| a == "sources").is_some() {
        return sources(args.collect());
    }
    if args.next_if(|a| a == "repack").is_some() {
        return repack(args.collect());
    }
    let mut image = None;
    let mut ms: u64 = 2000;
    let mut quiet = false;
    let mut part = armv6m::part::STM32G031;
    let mut core = None;
    let mut datagrams: Vec<(u16, String)> = Vec::new();
    let mut ping = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--part" => match args.next().and_then(|v| armv6m::part::Part::named(&v)) {
                Some(named) => part = named,
                None => return usage(),
            },
            // The part's own Cortex-M0+, or a Cortex-M7 in its place: for
            // running an image built for ARMv7E-M on the G0's peripherals.
            "--core" => match args.next().as_deref() {
                Some("m0+" | "m0plus") => core = Some(armv6m::state::Core::CortexM0Plus),
                Some("m7") => core = Some(armv6m::state::Core::CortexM7),
                _ => return usage(),
            },
            "--ms" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => ms = v,
                None => return usage(),
            },
            "--quiet" => quiet = true,
            // A datagram to the board's port, from the host at the other end
            // of its Ethernet cable, sent at 1 s and again at 2 s.
            "--udp" => match args.next().and_then(|v| v.split_once(':').and_then(|(p, t)| Some((p.parse().ok()?, t.to_string())))) {
                Some(datagram) => datagrams.push(datagram),
                None => return usage(),
            },
            "--ping" => ping = true,
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
    let machine = Machine::new(part, &bytes).and_then(|mut m| match core {
        Some(core) if core != m.core => m.set_core(core).map(|()| m),
        _ => Ok(m),
    });
    let mut machine = match machine {
        Ok(machine) => machine,
        Err(e) => {
            eprintln!("zaqaru-m0: {e}");
            return ExitCode::FAILURE;
        }
    };

    let end = ms * PS_PER_MS;
    let started = Instant::now();
    let mut peer = armv6m::net::Peer::default();
    let mut sends = [1000, 2000].map(|ms| ms * PS_PER_MS).into_iter().peekable();
    let mut shown = 0;
    let mut faults = 0;
    let mut resets = 0;
    let outcome = loop {
        // In slices, so events print as they happen rather than at the end.
        let outcome = machine.run_until(end.min(machine.time + 10 * PS_PER_MS), u64::MAX);
        let mut replies = Vec::new();
        for event in &machine.events[shown..] {
            if let Event::Fault { .. } = event {
                faults += 1;
            }
            if !quiet {
                println!("{}", describe(event));
            }
            if let Event::Frame { frame, .. } = event {
                let (answers, heard) = peer.receive(frame);
                replies.extend(answers);
                for heard in heard {
                    println!("{:>10.3} ms  the host heard {}", ms_at(machine.time), heard_line(&heard));
                }
            }
        }
        shown = machine.events.len();
        for reply in replies {
            machine.deliver(reply);
        }
        if sends.next_if(|&at| machine.time >= at).is_some() {
            for (port, text) in &datagrams {
                println!("{:>10.3} ms  the host sends {text:?} to [{}]:{port}", ms_at(machine.time), peer_target(&peer));
                for frame in peer.udp(*port, text.as_bytes()) {
                    machine.deliver(frame);
                }
            }
            if ping {
                println!("{:>10.3} ms  the host pings [{}]", ms_at(machine.time), peer_target(&peer));
                for frame in peer.ping(1) {
                    machine.deliver(frame);
                }
            }
        }
        match outcome {
            Outcome::Budget if machine.time < end => continue,
            Outcome::SystemReset => {
                resets += 1;
                println!("{:>10.3} ms  system reset requested", ms_at(machine.time));
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
        ms_at(machine.time),
        secs,
        machine.retired as f64 / secs / 1e6,
        ms_at(machine.time) / 1000.0 / secs,
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
    if !machine.bus.soc.unmodelled().is_empty() {
        let regs: Vec<String> = machine.bus.soc.unmodelled().iter().map(|a| format!("0x{a:08x}")).collect();
        println!("unmodelled peripheral registers touched: {}", regs.join(" "));
    }

    match outcome {
        Outcome::Budget if faults == 0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// Simulated milliseconds at `time`, in picoseconds.
pub(crate) fn ms_at(time: u64) -> f64 {
    time as f64 / PS_PER_MS as f64
}

fn peer_target(peer: &armv6m::net::Peer) -> String {
    peer.board.map_or("ff02::1".into(), |(_, ip)| armv6m::net::show(&ip))
}

pub(crate) fn heard_line(heard: &armv6m::net::Heard) -> String {
    match heard {
        armv6m::net::Heard::Udp { from, source, port, payload, multicast } => format!(
            "{:?} from [{}]:{source} on port {port}{}",
            String::from_utf8_lossy(payload),
            armv6m::net::show(from),
            if *multicast { ", to all nodes" } else { "" }
        ),
        armv6m::net::Heard::Pong { from, sequence } => format!("an echo reply from [{}], sequence {sequence}", armv6m::net::show(from)),
    }
}

pub(crate) fn describe(event: &Event) -> String {
    match *event {
        Event::Frame { time, ref frame } => {
            let kind = match frame.get(12..14) {
                Some([0x86, 0xDD]) => match frame.get(20) {
                    Some(58) => format!("IPv6 ICMPv6 type {}", frame.get(54).copied().unwrap_or(0)),
                    Some(17) => "IPv6 UDP".to_string(),
                    Some(0) => "IPv6 hop-by-hop".to_string(),
                    other => format!("IPv6 next header {other:?}"),
                },
                Some(&[a, b]) => format!("ethertype 0x{a:02x}{b:02x}"),
                _ => "runt".to_string(),
            };
            let to: Vec<String> = frame.iter().take(6).map(|b| format!("{b:02x}")).collect();
            format!("{:>10.3} ms  ETH frame of {} bytes to {}: {kind}", ms_at(time), frame.len(), to.join(":"))
        }
        Event::Pin { time, edge } => {
            let Edge { port, before, after } = edge;
            let port = (b'A' + port) as char;
            let mut line = format!("{:>10.3} ms  GPIO{port} ODR 0x{before:04x} -> 0x{after:04x}", ms_at(time));
            // The Nucleo-G031's LD4 is PC6, wired active-low.
            if port == 'C' && (before ^ after) & 1 << 6 != 0 {
                line += if after & 1 << 6 == 0 { "  LED on" } else { "  LED off" };
            }
            line
        }
        Event::Serial { time, byte } => {
            format!("{:>10.3} ms  USART{} {:?}", ms_at(time), byte.port + 1, byte.byte as char)
        }
        Event::Fault { time, pc, fault, mode, privileged } => format!(
            "{:>10.3} ms  HardFault at pc=0x{pc:08x} ({mode:?}, {}): {fault:?}",
            ms_at(time),
            if privileged { "privileged" } else { "unprivileged" },
        ),
    }
}
