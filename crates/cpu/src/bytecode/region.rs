//! Experimental immutable multi-trace stream. Callers must validate every
//! member against the current code cache before entering a compiled region.

use super::{Op, Trace, field};

pub struct Region {
    /// Directory length, then address/offset/left/right nodes, then bytecode.
    pub code: Vec<u64>,
    pub ip: Vec<u64>,
}

impl Region {
    /// Members must come from the same code-cache view. The first is the root.
    pub fn new(members: &[(u64, &[u64], &[u64])]) -> Self {
        assert!(!members.is_empty());
        let header = 1 + 4 * members.len();
        let mut code = vec![0; header];
        let mut ip = vec![0; header];
        code[0] = header as u64;
        for (index, &(entry, words, addresses)) in members.iter().enumerate() {
            assert_eq!(words.len(), addresses.len());
            assert!(!words.is_empty());
            assert!(!members[..index].iter().any(|m| m.0 == entry));
            let base = code.len();
            code[1 + 4 * index] = entry;
            code[2 + 4 * index] = base as u64;
            code.extend_from_slice(words);
            ip.extend_from_slice(addresses);
            let mut pc = base;
            while pc < code.len() {
                let op = Op::from_byte(code[pc] as u8).expect("valid bytecode");
                match op {
                    Op::Br | Op::BrIf => {
                        let target = (code[pc] >> field::IMM) as usize;
                        assert!(target < words.len());
                        let relocated = u32::try_from(base + target).expect("region too large");
                        code[pc] = (code[pc] & 0xffff_ffff) | ((relocated as u64) << field::IMM);
                    }
                    Op::FusedBranch => {
                        let target = code[pc + 1] as u32 as usize;
                        assert!(target < words.len());
                        let relocated = u32::try_from(base + target).expect("region too large");
                        code[pc + 1] = (code[pc + 1] & !0xffff_ffff) | relocated as u64;
                    }
                    _ => {}
                }
                pc += match op {
                    Op::Li64 | Op::Defer | Op::FusedBranch => 2,
                    _ => 1,
                };
            }
            assert_eq!(pc, code.len());
        }
        let mut directory: Vec<_> = code[1..header]
            .chunks_exact(4)
            .map(|p| (p[0], p[1]))
            .collect();
        directory.sort_unstable_by_key(|p| p.0);
        fn tree(pairs: &[(u64, u64)], nodes: &mut Vec<u64>, missing: u64) -> u64 {
            if pairs.is_empty() {
                return missing;
            }
            let middle = pairs.len() / 2;
            let index = nodes.len();
            nodes.extend([pairs[middle].0, pairs[middle].1, missing, missing]);
            nodes[index + 2] = tree(&pairs[..middle], nodes, missing);
            nodes[index + 3] = tree(&pairs[middle + 1..], nodes, missing);
            index as u64
        }
        let mut nodes = Vec::new();
        tree(&directory, &mut nodes, (header - 1) as u64);
        code[1..header].copy_from_slice(&nodes);
        Self { code, ip }
    }

    pub fn from_traces(traces: &[&Trace]) -> Self {
        Self::new(
            &traces
                .iter()
                .map(|t| (t.entry, t.code.as_slice(), t.ip.as_slice()))
                .collect::<Vec<_>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Leave, Resolver, run};
    use super::*;
    use crate::{
        arena::Arena,
        space::{Protection, Space},
        state::Tcb,
    };
    use iced_x86::code_asm::*;

    fn trace(entry: u64, build: impl FnOnce(&mut CodeAssembler)) -> Trace {
        let mut a = CodeAssembler::new(64).unwrap();
        build(&mut a);
        let bytes = a.assemble(entry).unwrap();
        let instructions: Vec<_> =
            iced_x86::Decoder::with_ip(64, &bytes, entry, iced_x86::DecoderOptions::NONE)
                .into_iter()
                .collect();
        let block = crate::block::Block {
            entry,
            end: entry + bytes.len() as u64,
            simple: false,
            quick: instructions
                .iter()
                .map(crate::quick::Quick::lower)
                .collect(),
            instructions,
            trace: None,
        };
        super::super::transpile(&block).unwrap()
    }

    #[test]
    fn calls_returns_budgets_and_stack_faults_match_separate_traces() {
        let traces = [
            trace(0x1000, |a| {
                a.call(0x3000u64).unwrap();
            }),
            trace(0x1005, |a| {
                a.dec(rcx).unwrap();
                a.jnz(0x1000u64).unwrap();
                a.syscall().unwrap();
            }),
            trace(0x3000, |a| {
                a.add(rax, 7).unwrap();
                a.ret().unwrap();
            }),
        ];
        let region = Region::from_traces(&traces.iter().collect::<Vec<_>>());
        let start = region.code[0] as usize;
        let combined = Trace {
            entry: 0x1000,
            code: region.code.clone(),
            ip: region.ip.clone(),
        };
        let arena = Arena::new(8192);
        for scenario in 0..4 {
            for budget in [0, 1, 2, 3, 4, 5, 11, 1000] {
                let mut states = Vec::new();
                for merged in [false, true] {
                    let mut space = Space::new(arena.limit());
                    space.protect(arena.base(), arena.length(), Protection::READ_WRITE);
                    space.write(arena.base(), &vec![0; 8192]).unwrap();
                    if scenario == 1 {
                        space.protect(arena.base(), arena.length(), Protection::READ);
                    }
                    if scenario == 2 {
                        space.mark_code((arena.limit() - 8) / 4096);
                    }
                    let mut tcb = Tcb::new();
                    tcb.rip = 0x1000;
                    tcb.registers[1] = 7;
                    tcb.set_stack_pointer(if scenario == 3 {
                        arena.limit() + 16
                    } else {
                        arena.limit()
                    });
                    tcb.flags.set_all(0x8d5);
                    let leave = if merged {
                        run(
                            &combined,
                            start,
                            &mut tcb,
                            &mut space,
                            budget,
                            Resolver::Region(&region.code[1..start]),
                        )
                    } else {
                        let mut remaining = budget;
                        loop {
                            let trace = traces.iter().find(|t| t.entry == tcb.rip).unwrap();
                            let before = tcb.retired;
                            let leave =
                                run(trace, 0, &mut tcb, &mut space, remaining, Resolver::Runloop);
                            remaining = remaining.saturating_sub(tcb.retired - before);
                            if leave != Leave::Exit || remaining == 0 || space.has_dirty_code() {
                                break leave;
                            }
                        }
                    };
                    // Defer offsets belong to their respective stream; RIP is
                    // the architectural continuation and must match exactly.
                    let leave = match leave {
                        Leave::Defer { .. } => Leave::Defer { resume: 0 },
                        other => other,
                    };
                    let mut memory = vec![0; 8192];
                    space.read(arena.base(), &mut memory).unwrap();
                    states.push((
                        leave,
                        tcb.registers,
                        tcb.rip,
                        tcb.retired,
                        tcb.flags.status(),
                        memory,
                        space.take_dirty_code(),
                    ));
                }
                assert_eq!(states[0], states[1], "scenario {scenario}, budget {budget}");
            }
        }
    }

    #[test]
    fn targets_outside_the_region_return_to_the_engine() {
        for target in [0x800u64, 0x2000] {
            let source = trace(0x1000, |a| {
                a.jmp(target).unwrap();
            });
            let region = Region::from_traces(&[&source]);
            let start = region.code[0] as usize;
            let combined = Trace {
                entry: source.entry,
                code: region.code.clone(),
                ip: region.ip,
            };
            let mut tcb = Tcb::new();
            let leave = run(
                &combined,
                start,
                &mut tcb,
                &mut Space::new(0),
                100,
                Resolver::Region(&region.code[1..start]),
            );
            assert_eq!(leave, Leave::Exit);
            assert_eq!(tcb.rip, target);
            assert_eq!(tcb.retired, 1);
        }
    }
}
