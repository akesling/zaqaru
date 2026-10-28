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
    eprintln!("                 [--nor FILE] [--keys USART:MS:HEX,…] [--frames DIR] [--every N] [--peek ADDR:LEN,…]");
    eprintln!("       zaqaru-m0 serve <final.bin> [--part NAME] [--gdb ADDR] [--listen ADDR] [--archive ZIP] [--board FILE]");
    eprintln!("       zaqaru-m0 door [--listen ADDR] [--probe ADDR] [--archive ZIP] [--humility PATH]");
    eprintln!("       zaqaru-m0 repack <in.zip> <out.zip> [--stored]");
    ExitCode::from(2)
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
    let mut nor = None;
    let mut keys: Vec<(usize, u64, Vec<u8>)> = Vec::new();
    let mut frames_dir = None;
    let mut every: u64 = 35;
    let mut peeks: Vec<(u32, usize)> = Vec::new();
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
            // A NOR flash on the QUADSPI, holding a file, for the image to
            // map: a game's data, say.
            "--nor" => match args.next() {
                Some(path) => nor = Some(path),
                None => return usage(),
            },
            // Bytes to arrive on a USART at a simulated time: `3:5000:1c9c`
            // is USART3 at 5 s, a key's scan codes down and up.
            "--keys" => match args.next().and_then(|v| parse_keys(&v)) {
                Some(more) => keys.extend(more),
                None => return usage(),
            },
            // Every `--every`th frame the LTDC is shown, as a PPM.
            "--frames" => match args.next() {
                Some(dir) => frames_dir = Some(dir),
                None => return usage(),
            },
            // Memory to show at the end, as a debugger reads it.
            "--peek" => {
                let parsed: Option<Vec<(u32, usize)>> = args.next().and_then(|v| {
                    v.split(',')
                        .map(|p| {
                            let (a, n) = p.split_once(':')?;
                            Some((u32::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, n.parse().ok()?))
                        })
                        .collect()
                });
                match parsed {
                    Some(more) => peeks.extend(more),
                    None => return usage(),
                }
            }
            "--every" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) if n > 0 => every = n,
                _ => return usage(),
            },
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
    if let Some(path) = &nor {
        let attached = std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|contents| armv6m::quadspi::Nor::new(&contents, 16 << 20))
            .and_then(|nor| machine.attach_nor(nor));
        if let Err(e) = attached {
            eprintln!("zaqaru-m0: {e}");
            return ExitCode::FAILURE;
        }
    }
    keys.sort_by_key(|&(_, at, _)| at);
    let mut keys = keys.into_iter().peekable();
    let mut shown_frames = 0;

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
        while let Some((port, _, bytes)) = keys.next_if(|&(_, at, _)| machine.time >= at * PS_PER_MS) {
            for byte in bytes {
                machine.receive(port - 1, byte);
            }
        }
        let frames = machine.frames_shown();
        if frames != shown_frames {
            shown_frames = frames;
            if let Some(dir) = frames_dir.as_ref().filter(|_| frames % every == 0) {
                if let Some((screen, _)) = machine.screen() {
                    let path = format!("{dir}/frame-{frames:06}.ppm");
                    if let Err(e) = std::fs::write(&path, ppm(&screen)) {
                        eprintln!("zaqaru-m0: {path}: {e}");
                    }
                }
            }
        }
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
    for &(address, length) in &peeks {
        let bytes = machine.debug_read(address, length).unwrap_or_default();
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        println!("0x{address:08x}: {}", hex.join(" "));
    }
    if shown_frames > 0 {
        println!(
            "LTDC frames shown: {shown_frames}, {:.1} a second of simulated time, {:.1} a second of this host's",
            shown_frames as f64 / (ms_at(machine.time) / 1000.0),
            shown_frames as f64 / secs,
        );
    }
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

/// `USART:MS:HEX`, comma-separated: bytes to arrive on a USART (1 for
/// USART1) at a simulated time.
fn parse_keys(text: &str) -> Option<Vec<(usize, u64, Vec<u8>)>> {
    text.split(',')
        .map(|item| {
            let mut fields = item.splitn(3, ':');
            let port: usize = fields.next()?.parse().ok().filter(|&p| p >= 1)?;
            let at: u64 = fields.next()?.parse().ok()?;
            let hex = fields.next()?;
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
                .collect::<Option<Vec<u8>>>()?;
            Some((port, at, bytes))
        })
        .collect()
}

/// A screen as a binary PPM.
fn ppm(screen: &armv6m::ltdc::Screen) -> Vec<u8> {
    let mut out = format!("P6 {} {} 255\n", screen.width, screen.height).into_bytes();
    match &screen.pixels {
        armv6m::ltdc::Pixels::Indexed { indices, palette } => {
            for &i in indices {
                let c = palette[i as usize];
                out.extend([(c >> 16) as u8, (c >> 8) as u8, c as u8]);
            }
        }
        armv6m::ltdc::Pixels::Rgb(rgb) => out.extend_from_slice(rgb),
    }
    out
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
