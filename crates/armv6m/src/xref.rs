//! Where a function is called and a static's address taken, as the code
//! the compiler made says — not where a name is written, which a function
//! of the same name in another module, or a local, would be too. Read from
//! an ELF: its instructions, swept for calls (`BL`, `B.W` to a function's
//! first instruction) and for addresses (a word `LDR` loads from a literal
//! pool, a `MOVW`/`MOVT` pair) that fall in a function or a static; and
//! its DWARF, for every place a function was inlined, which no call is
//! left of, and for the source line of each instruction found.
//!
//! What the code does not say — a call through a pointer, a static reached
//! through another's address — is not found; a function inlined into all
//! its callers is found only as inlined.

use crate::dwarf::Debug;

/// How a place uses what it refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum How {
    /// Calls it: `BL`.
    Call,
    /// Calls it last, as a jump: `B.W`.
    TailCall,
    /// Has a copy of it in its own code.
    Inlined,
    /// Takes its address: a function's to call later, a static's to read or
    /// write it.
    Address,
}

/// A place in the code that refers to a function or a static.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Site {
    pub how: How,
    pub file: Option<String>,
    pub line: u32,
    /// The function the place is in, as its source names it.
    pub function: Option<String>,
    /// An instruction of it, for an instruction's; `None` for an inlined
    /// copy, which is many.
    pub pc: Option<u32>,
}

/// What is referred to: a function's code, or a static's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Its first instruction.
    Function(u32),
    /// Its bytes, `[start, end)`.
    Static(u32, u32),
}

/// Every instruction of `code` — `(address, bytes)` sections — that calls
/// one of `targets` or takes an address in one: `(target, how, pc)`.
/// Swept a function at a time (`functions`: address and size), a literal
/// pool in one read as instructions it is not, which seldom matters.
pub fn sweep(code: &[(u32, Vec<u8>)], functions: &[(u32, u32)], targets: &[Target]) -> Vec<(usize, How, u32)> {
    let hits = |value: u32| -> Vec<usize> {
        targets
            .iter()
            .enumerate()
            .filter(|(_, t)| match **t {
                // An address of code is odd: the Thumb bit.
                Target::Function(at) => value == at | 1,
                Target::Static(start, end) => value >= start && value < end,
            })
            .map(|(n, _)| n)
            .collect()
    };
    let halfword = |at: u32| -> Option<u16> {
        code.iter().find_map(|(base, bytes)| {
            let off = at.checked_sub(*base)? as usize;
            Some(u16::from_le_bytes([*bytes.get(off)?, *bytes.get(off + 1)?]))
        })
    };
    let word = |at: u32| -> Option<u32> { Some(u32::from(halfword(at)?) | u32::from(halfword(at + 2)?) << 16) };
    let mut out = Vec::new();
    for &(start, size) in functions {
        // What each register was last given by `MOVW`, and where.
        let mut low: [Option<(u32, u32)>; 16] = [None; 16];
        let mut pc = start;
        while pc < start + size {
            let Some(hw1) = halfword(pc) else { break };
            let wide = hw1 >> 11 >= 0b11101;
            if !wide {
                // `LDR Rt, [PC, #imm8 * 4]`.
                if hw1 >> 11 == 0b01001 {
                    let from = ((pc + 4) & !3) + u32::from(hw1 & 0xFF) * 4;
                    if let Some(value) = word(from) {
                        out.extend(hits(value).into_iter().map(|n| (n, How::Address, pc)));
                    }
                }
                pc += 2;
                continue;
            }
            let Some(hw2) = halfword(pc + 2) else { break };
            // `BL` and `B.W`: a 25-bit offset, its sign and two bits that
            // say which way they go from it.
            if hw1 >> 11 == 0b11110 && hw2 >> 14 == 0b11 && (hw2 >> 12) & 1 == 1 || hw1 >> 11 == 0b11110 && hw2 >> 14 == 0b10 && (hw2 >> 12) & 1 == 1 {
                let s = u32::from(hw1 >> 10) & 1;
                let j1 = u32::from(hw2 >> 13) & 1;
                let j2 = u32::from(hw2 >> 11) & 1;
                let (i1, i2) = (!(j1 ^ s) & 1, !(j2 ^ s) & 1);
                let offset = (s << 24) | (i1 << 23) | (i2 << 22) | (u32::from(hw1 & 0x3FF) << 12) | (u32::from(hw2 & 0x7FF) << 1);
                let offset = ((offset << 7) as i32 >> 7) as u32;
                let target = (pc + 4).wrapping_add(offset);
                let how = if hw2 >> 14 == 0b11 { How::Call } else { How::TailCall };
                out.extend(hits(target | 1).into_iter().filter(|&n| matches!(targets[n], Target::Function(_))).map(|n| (n, how, pc)));
            }
            // `LDR.W Rt, [PC, #±imm12]`.
            if hw1 & 0xFF7F == 0xF85F {
                let imm = u32::from(hw2 & 0xFFF);
                let base = (pc + 4) & !3;
                let from = if hw1 & 0x80 != 0 { base + imm } else { base.wrapping_sub(imm) };
                if let Some(value) = word(from) {
                    out.extend(hits(value).into_iter().map(|n| (n, How::Address, pc)));
                }
            }
            // `MOVW` then `MOVT` to the same register: a 32-bit value.
            let imm16 = (u32::from(hw1 & 0xF) << 12) | (u32::from(hw1 >> 10 & 1) << 11) | (u32::from(hw2 >> 12 & 7) << 8) | u32::from(hw2 & 0xFF);
            let rd = usize::from(hw2 >> 8 & 0xF);
            if hw1 & 0xFBF0 == 0xF240 {
                low[rd] = Some((imm16, pc));
            } else if hw1 & 0xFBF0 == 0xF2C0
                && let Some((value, at)) = low[rd].take()
            {
                out.extend(hits(imm16 << 16 | value).into_iter().map(|n| (n, How::Address, at)));
            }
            pc += 4;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

impl Debug {
    /// Where the function or static `name`, declared at `file` and `line`,
    /// is called, inlined or has its address taken in this ELF's code,
    /// each place once by its line: `None` when it is nothing of the
    /// ELF's. A function or static the DWARF gives no declaration — C
    /// built with line tables alone — is known by its name.
    pub fn sites(&self, name: &str, file: &str, line: u32) -> Option<Vec<Site>> {
        let matches = |n: &str, decl: &Option<(String, u32)>| n == name && decl.as_ref().is_none_or(|(f, l)| f == file && *l == line);
        let instances = self.instances();
        let mut targets: Vec<crate::xref::Target> = Vec::new();
        for f in instances.functions.iter().filter(|f| matches(&f.name, &f.decl)) {
            targets.push(Target::Function(f.low));
        }
        for s in instances.statics.iter().filter(|s| matches(&s.name, &s.decl)) {
            targets.push(Target::Static(s.address, s.address + s.size.max(1)));
        }
        // Known to the symbol table alone.
        if targets.is_empty() {
            targets.extend(self.named_functions.iter().filter(|(n, ..)| n == name).map(|&(_, at, _)| Target::Function(at)));
        }
        let inlined: Vec<&Inlined> = instances.inlined.iter().filter(|i| matches(&i.name, &i.decl)).collect();
        if targets.is_empty() && inlined.is_empty() {
            return None;
        }
        let mut out: Vec<Site> = sweep(&self.code, &self.functions, &targets)
            .into_iter()
            .map(|(_, how, pc)| {
                // The innermost frame of the program's own, not the
                // standard library's inlined into it: the line that asked.
                let frames = self.lines(pc);
                let own = frames.iter().position(|f| !f.file.as_deref().is_some_and(|f| f.starts_with("/rustc/"))).unwrap_or(0);
                let frame = frames.into_iter().nth(own);
                // C of line tables alone names no function: the symbol
                // table's, which the instruction is in.
                let named = || self.named_functions.iter().find(|(_, at, size)| pc >= *at && pc < at + size).map(|(n, ..)| n.clone());
                Site {
                    how,
                    file: frame.as_ref().and_then(|f| f.file.clone()),
                    line: frame.as_ref().map_or(0, |f| f.line),
                    function: frame.and_then(|f| f.function).or_else(named),
                    pc: Some(pc),
                }
            })
            .collect();
        out.extend(inlined.iter().map(|i| Site { how: How::Inlined, file: i.call.0.clone(), line: i.call.1, function: i.within.clone(), pc: None }));
        // Each line once, for each way it uses it.
        out.sort_by(|a, b| (&a.file, a.line, a.how).cmp(&(&b.file, b.line, b.how)));
        out.dedup_by(|a, b| a.file == b.file && a.line == b.line && a.how == b.how);
        Some(out)
    }
}

use crate::dwarf::Inlined;
