//! Static emitted-code inventory. Counts are not execution frequencies or costs.
use anyhow::Result;
use std::collections::BTreeMap;
use wasmparser::{KnownCustom, Name, Operator, Parser, Payload, TypeRef};

#[derive(Default, Debug)]
struct Function {
    index: u32,
    bytes: usize,
    locals: u64,
    operators: u64,
    loads: u64,
    stores: u64,
    branches: u64,
    local_accesses: u64,
    indirect_calls: u64,
    calls: BTreeMap<u32, u64>,
    histogram: BTreeMap<String, u64>,
    instructions: Vec<(usize, String)>,
}

fn inspect(bytes: &[u8], detail: Option<u32>) -> Result<(BTreeMap<u32, String>, Vec<Function>)> {
    let mut names = BTreeMap::new();
    let mut functions = Vec::new();
    let mut index = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import?;
                    if matches!(import.ty, TypeRef::Func(_)) {
                        names.insert(index, format!("{}.{}", import.module, import.name));
                        index += 1;
                    }
                }
            }
            Payload::CustomSection(section) => {
                if let KnownCustom::Name(section) = section.as_known() {
                    for subsection in section {
                        if let Name::Function(entries) = subsection? {
                            for entry in entries {
                                let entry = entry?;
                                names.insert(entry.index, entry.name.to_owned());
                            }
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut function = Function {
                    index,
                    bytes: body.range().len(),
                    ..Function::default()
                };
                for local in body.get_locals_reader()? {
                    function.locals += u64::from(local?.0);
                }
                let mut reader = body.get_operators_reader()?;
                while !reader.eof() {
                    let offset = reader.original_position();
                    let op = reader.read()?;
                    let description = format!("{op:?}");
                    let mnemonic = description.split([' ', '{']).next().unwrap();
                    *function.histogram.entry(mnemonic.to_owned()).or_default() += 1;
                    function.operators += 1;
                    // Covers scalar, SIMD and atomic load/store op names.
                    function.loads += u64::from(mnemonic.contains("Load"));
                    function.stores += u64::from(mnemonic.contains("Store"));
                    match op {
                        Operator::Call { function_index }
                        | Operator::ReturnCall { function_index } => {
                            *function.calls.entry(function_index).or_default() += 1;
                        }
                        Operator::CallIndirect { .. }
                        | Operator::ReturnCallIndirect { .. }
                        | Operator::CallRef { .. }
                        | Operator::ReturnCallRef { .. } => function.indirect_calls += 1,
                        Operator::Br { .. }
                        | Operator::BrIf { .. }
                        | Operator::BrTable { .. }
                        | Operator::If { .. } => function.branches += 1,
                        Operator::LocalGet { .. }
                        | Operator::LocalSet { .. }
                        | Operator::LocalTee { .. } => function.local_accesses += 1,
                        _ => {}
                    }
                    if detail == Some(index) {
                        function.instructions.push((offset, description));
                    }
                }
                functions.push(function);
                index += 1;
            }
            _ => {}
        }
    }
    Ok((names, functions))
}

pub fn print(bytes: &[u8], detail: Option<u32>) -> Result<()> {
    let (names, functions) = inspect(bytes, detail)?;
    println!(
        "# Static Wasm counts, including cold paths; not dynamic frequencies or native instructions."
    );
    println!(
        "index\tbytes\tlocals\tops\tloads\tstores\tbranches\tlocal_accesses\tdirect_calls\tindirect_calls\tname"
    );
    for f in &functions {
        if detail.is_some_and(|index| f.index != index) {
            continue;
        }
        let name = names
            .get(&f.index)
            .map(String::as_str)
            .unwrap_or("")
            .replace(['\t', '\n'], " ");
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            f.index,
            f.bytes,
            f.locals,
            f.operators,
            f.loads,
            f.stores,
            f.branches,
            f.local_accesses,
            f.calls.values().sum::<u64>(),
            f.indirect_calls,
            name
        );
        if detail.is_some() {
            for (&target, count) in &f.calls {
                println!(
                    "# callee\t{target}\t{count}\t{}",
                    names.get(&target).map(String::as_str).unwrap_or("")
                );
            }
            for (op, count) in &f.histogram {
                println!("# opcode\t{op}\t{count}");
            }
            for (offset, op) in &f.instructions {
                println!("# instruction\t{offset:#x}\t{op}");
            }
        }
    }
    anyhow::ensure!(
        detail.is_none_or(|index| functions.iter().any(|f| f.index == index)),
        "function index has no body"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_import_indices_memory_operations_and_cold_calls() {
        let bytes = wat::parse_str(
            r#"(module
            (import "env" "helper" (func $helper)) (memory 1)
            (func $fixture (local i32)
                i32.const 0 i64.load drop
                i32.const 0 i32.const 7 i32.store
                i32.const 1 if call $helper end))"#,
        )
        .unwrap();
        let (names, functions) = inspect(&bytes, Some(1)).unwrap();
        let f = &functions[0];
        assert_eq!(f.index, 1);
        assert_eq!(names[&1], "fixture");
        assert_eq!((f.locals, f.loads, f.stores, f.branches), (1, 1, 1, 1));
        assert_eq!(f.calls[&0], 1);
        assert_eq!(f.instructions.len() as u64, f.operators);
    }
}
