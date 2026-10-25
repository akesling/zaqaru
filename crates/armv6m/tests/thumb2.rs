//! The Cortex-M7's instructions against QEMU's.
//!
//! `tests/thumb2.corpus` is random Thumb and Thumb-2 instructions, each with
//! a starting state and what Unicorn — QEMU's Thumb-2, with its Cortex-M7
//! model — did with it; `tools/thumb2-oracle.py` writes it, and says why
//! only instruction semantics are compared. Each case runs here from the
//! same state, and every register, flag and byte of SRAM has to agree.
//!
//! `THUMB2_CORPUS=path` runs a different corpus — a bigger one, say, made
//! locally with more cases or another seed.

use armv6m::Machine;
use armv6m::bus::SRAM_BASE;
use armv6m::part::STM32G070;
use armv6m::state::Core;

const SRAM_SIZE: usize = 0x9000;
const CODE: u32 = SRAM_BASE + 0x100;

fn pattern() -> Vec<u8> {
    (0..SRAM_SIZE as u32).map(|i| ((SRAM_BASE + i).wrapping_mul(0x9E37_79B1) >> 24) as u8).collect()
}

struct Case {
    line: usize,
    instr: String,
    before: Vec<u32>,
    expected: Option<(Vec<u32>, Vec<(u32, u8)>)>,
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s, 16).unwrap()
}

fn parse(text: &str) -> Vec<Case> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|(n, l)| {
            let (input, output) = l.split_once(" => ").unwrap();
            let mut fields = input.split(' ');
            let instr = fields.next().unwrap().to_string();
            let before = fields.map(hex).collect();
            let expected = (output.trim() != "fault").then(|| {
                let mut fields = output.split(' ');
                let mut regs: Vec<u32> = fields.by_ref().take(17).map(hex).collect();
                // The stack pointer's low bits: QEMU keeps what was
                // written, the architecture says nothing may be.
                regs[13] &= !3;
                let writes = fields
                    .map(|w| {
                        let (a, b) = w.split_once(':').unwrap();
                        (hex(a), hex(b) as u8)
                    })
                    .collect();
                (regs, writes)
            });
            Case { line: n + 1, instr, before, expected }
        })
        .collect()
}

/// A G070's memory with an M7 in it, and a vector table that points
/// nowhere in particular: a case that faults is seen as the fault event,
/// not by where the handler would have gone.
fn machine() -> Machine {
    let mut image = vec![0u8; 0x200];
    image[0..4].copy_from_slice(&(SRAM_BASE + 0x8000).to_le_bytes());
    image[4..8].copy_from_slice(&(0x0800_0101u32).to_le_bytes());
    let mut m = Machine::stm32g0(STM32G070, &image).unwrap();
    m.set_core(Core::CortexM7).unwrap();
    m
}

/// Runs one case: `None` if it faulted, otherwise the registers in the
/// corpus's order and the SRAM that changed.
fn run(m: &mut Machine, base: &[u8], case: &Case) -> Option<(Vec<u32>, Vec<(u32, u8)>)> {
    // An IT block is several instructions, joined by `+`.
    let words: Vec<&str> = case.instr.split('+').collect();
    let code: Vec<u8> = words
        .iter()
        .flat_map(|w| {
            if w.len() == 8 {
                let (hw1, hw2) = (hex(&w[..4]) as u16, hex(&w[4..]) as u16);
                [hw1.to_le_bytes(), hw2.to_le_bytes()].concat()
            } else {
                (hex(w) as u16).to_le_bytes().to_vec()
            }
        })
        .collect();
    m.reset().unwrap();
    m.bus.rams[0].bytes.copy_from_slice(base);
    let at = (CODE - SRAM_BASE) as usize;
    m.bus.rams[0].bytes[at..at + code.len()].copy_from_slice(&code);
    let b = &case.before;
    m.cpu.r[..13].copy_from_slice(&b[..13]);
    m.cpu.msp = b[13];
    m.cpu.r[14] = b[14];
    m.cpu.set_apsr(b[15]);
    m.cpu.ge = (b[15] >> 16 & 0xF) as u8;
    m.cpu.pc = CODE;
    m.cpu.t = true;
    m.events.clear();
    if words.len() > 1 {
        // An IT block runs to its end.
        let end = CODE + code.len() as u32;
        m.breakpoints.insert(end);
        m.run(32);
        m.breakpoints.remove(&end);
    } else {
        m.run(1);
    }
    if m.events.iter().any(|e| matches!(e, armv6m::Event::Fault { .. })) {
        return None;
    }
    let mut regs = m.cpu.r[..13].to_vec();
    regs.extend([m.cpu.msp, m.cpu.r[14], m.cpu.pc, m.cpu.apsr()]);
    regs[13] &= !3;
    let writes = (0..SRAM_SIZE)
        .filter(|&i| m.bus.rams[0].bytes[i] != base[i] && !(at..at + code.len()).contains(&i))
        .map(|i| (SRAM_BASE + i as u32, m.bus.rams[0].bytes[i]))
        .collect();
    Some((regs, writes))
}

#[test]
fn every_instruction_agrees_with_qemu() {
    let path = std::env::var("THUMB2_CORPUS").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/thumb2.corpus").into());
    let cases = parse(&std::fs::read_to_string(&path).unwrap());
    assert!(!cases.is_empty());
    let base = pattern();
    let mut m = machine();
    let names = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr", "pc", "apsr"];
    let mut failures = Vec::new();
    let mut unaligned = 0;
    for case in &cases {
        let got = run(&mut m, &base, case);
        if got == case.expected {
            continue;
        }
        // Unicorn does not fault an unaligned LDM, LDRD or exclusive, which
        // the architecture does whatever CCR says (A3.2.1).
        if got.is_none() && m.events.iter().any(|e| matches!(e, armv6m::Event::Fault { fault: armv6m::Fault::Unaligned { .. }, .. })) {
            unaligned += 1;
            continue;
        }
        let why = match (&got, &case.expected) {
            (None, Some(_)) => "faulted here, ran there".to_string(),
            (Some(_), None) => "ran here, faulted there".to_string(),
            (Some((regs, writes)), Some((want, want_writes))) => {
                let mut diffs: Vec<String> = (0..17)
                    .filter(|&i| regs[i] != want[i])
                    .map(|i| format!("{} {:x} want {:x}", names[i], regs[i], want[i]))
                    .collect();
                if writes != want_writes {
                    diffs.push(format!("writes {writes:x?} want {want_writes:x?}"));
                }
                diffs.join(", ")
            }
            (None, None) => unreachable!(),
        };
        failures.push(format!("line {}: {}: {why}", case.line, case.instr));
    }
    eprintln!("{} cases, {} faulted unaligned where QEMU does not check", cases.len(), unaligned);
    assert!(
        failures.is_empty(),
        "{} of {} cases disagree:\n{}",
        failures.len(),
        cases.len(),
        failures.iter().take(400).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// One FP case: the core state as above, then FPSCR and s0–s31.
struct FpCase {
    line: usize,
    instr: String,
    before: Vec<u32>,
    expected: Option<(Vec<u32>, Vec<(u32, u8)>)>,
}

fn parse_fp(text: &str) -> Vec<FpCase> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|(n, l)| {
            let (input, output) = l.split_once(" => ").unwrap();
            let mut fields = input.split(' ');
            let instr = fields.next().unwrap().to_string();
            let before = fields.map(hex).collect();
            let expected = (output.trim() != "fault").then(|| {
                let mut fields = output.split(' ');
                let mut regs: Vec<u32> = fields.by_ref().take(50).map(hex).collect();
                regs[13] &= !3;
                let writes = fields
                    .map(|w| {
                        let (a, b) = w.split_once(':').unwrap();
                        (hex(a), hex(b) as u8)
                    })
                    .collect();
                (regs, writes)
            });
            FpCase { line: n + 1, instr, before, expected }
        })
        .collect()
}

fn run_fp(m: &mut Machine, base: &[u8], case: &FpCase) -> Option<(Vec<u32>, Vec<(u32, u8)>)> {
    let (hw1, hw2) = (hex(&case.instr[..4]) as u16, hex(&case.instr[4..]) as u16);
    let code = [hw1.to_le_bytes(), hw2.to_le_bytes()].concat();
    m.reset().unwrap();
    m.bus.rams[0].bytes.copy_from_slice(base);
    let at = (CODE - SRAM_BASE) as usize;
    m.bus.rams[0].bytes[at..at + 4].copy_from_slice(&code);
    let b = &case.before;
    m.cpu.r[..13].copy_from_slice(&b[..13]);
    m.cpu.msp = b[13];
    m.cpu.r[14] = b[14];
    m.cpu.set_apsr(b[15]);
    m.cpu.fpscr = b[16];
    m.cpu.s.copy_from_slice(&b[17..49]);
    // The FPU on, and the context already using it, so that FPDSCR's
    // defaults do not replace the case's FPSCR.
    m.bus.scs.cpacr = 0xF << 20;
    m.cpu.fpca = true;
    m.cpu.pc = CODE;
    m.cpu.t = true;
    m.events.clear();
    m.run(1);
    if m.events.iter().any(|e| matches!(e, armv6m::Event::Fault { .. })) {
        return None;
    }
    let mut regs = m.cpu.r[..13].to_vec();
    // AHP is the oracle's to keep or drop; the corpus leaves it out.
    regs.extend([m.cpu.msp & !3, m.cpu.r[14], m.cpu.pc, m.cpu.apsr() & 0xF000_0000, m.cpu.fpscr & 0xF3C0_009F]);
    regs.extend(m.cpu.s);
    // Against SRAM as it started, the instruction included: a store may
    // land on it.
    let mut initial = base.to_vec();
    initial[at..at + 4].copy_from_slice(&code);
    let writes = (0..SRAM_SIZE)
        .filter(|&i| m.bus.rams[0].bytes[i] != initial[i])
        .map(|i| (SRAM_BASE + i as u32, m.bus.rams[0].bytes[i]))
        .collect();
    Some((regs, writes))
}

#[test]
fn every_fp_instruction_agrees_with_qemu() {
    let path = std::env::var("FPU_CORPUS").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fpu.corpus").into());
    let cases = parse_fp(&std::fs::read_to_string(&path).unwrap());
    assert!(!cases.is_empty());
    let base = pattern();
    let mut m = machine();
    let mut names: Vec<String> =
        ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr", "pc", "apsr", "fpscr"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    names.extend((0..32).map(|i| format!("s{i}")));
    let mut failures = Vec::new();
    let mut unaligned = 0;
    for case in &cases {
        let got = run_fp(&mut m, &base, case);
        if got == case.expected {
            continue;
        }
        if got.is_none() && m.events.iter().any(|e| matches!(e, armv6m::Event::Fault { fault: armv6m::Fault::Unaligned { .. }, .. })) {
            unaligned += 1;
            continue;
        }
        let why = match (&got, &case.expected) {
            (None, Some(_)) => format!("faulted here, ran there: {:?}", m.events.last()),
            (Some(_), None) => "ran here, faulted there".to_string(),
            (Some((regs, writes)), Some((want, want_writes))) => {
                let mut diffs: Vec<String> = (0..50)
                    .filter(|&i| regs[i] != want[i])
                    .map(|i| format!("{} {:x} want {:x}", names[i], regs[i], want[i]))
                    .collect();
                if writes != want_writes {
                    diffs.push(format!("writes {writes:x?} want {want_writes:x?}"));
                }
                diffs.join(", ")
            }
            (None, None) => unreachable!(),
        };
        failures.push(format!("line {}: {}: {why}", case.line, case.instr));
    }
    eprintln!("{} cases, {} faulted unaligned where QEMU does not check", cases.len(), unaligned);
    assert!(
        failures.is_empty(),
        "{} of {} cases disagree:\n{}",
        failures.len(),
        cases.len(),
        failures.iter().take(400).cloned().collect::<Vec<_>>().join("\n")
    );
}
