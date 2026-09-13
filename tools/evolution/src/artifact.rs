//! Compiler annotations are never runtime imports. Replace them with trapping
//! local functions, preserving table slot identities in the frozen memory.
use anyhow::{Result, ensure};
use wasm_encoder::{
    self as enc,
    reencode::{self, Reencode},
};
use wasmparser::{self as parser, Parser, Payload, TypeRef};

pub fn executable(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    let mut names = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        if let Payload::ImportSection(section) = payload? {
            for import in section.into_imports() {
                let import = import?;
                let TypeRef::Func(ty) = import.ty else {
                    anyhow::bail!("non-function import");
                };
                let index = (kept.len() + removed.len()) as u32;
                if import.module == "weval" {
                    removed.push((index, ty));
                } else {
                    ensure!(
                        import.module == "env" && matches!(import.name, "ll_read" | "ll_write"),
                        "unexpected import {}.{}",
                        import.module,
                        import.name
                    );
                    kept.push(index);
                    names.push(import.name.to_string());
                }
            }
        }
    }
    ensure!(
        kept.len() == 2,
        "executor must have exactly two store imports"
    );
    names.sort();
    ensure!(
        names == ["ll_read", "ll_write"],
        "missing or duplicate store import"
    );
    let mut rewrite = Strip { kept, removed };
    let mut module = enc::Module::new();
    rewrite.parse_core_module(&mut module, Parser::new(0), bytes)?;
    let result = module.finish();
    parser::Validator::new().validate_all(&result)?;
    Ok(result)
}

struct Strip {
    kept: Vec<u32>,
    removed: Vec<(u32, u32)>,
}
impl Reencode for Strip {
    type Error = std::convert::Infallible;
    fn parse_import_section(
        &mut self,
        imports: &mut enc::ImportSection,
        section: parser::ImportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        for import in section.into_imports() {
            self.parse_import(imports, import?)?;
        }
        Ok(())
    }
    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        if let Some(i) = self.kept.iter().position(|&x| x == index) {
            return Ok(i as u32);
        }
        if let Some(i) = self.removed.iter().position(|&(x, _)| x == index) {
            return Ok(self.kept.len() as u32 + i as u32);
        }
        Ok(index)
    }
    fn parse_import(
        &mut self,
        imports: &mut enc::ImportSection,
        import: parser::Import<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        if import.module != "weval" {
            reencode::utils::parse_import(self, imports, import)?;
        }
        Ok(())
    }
    fn parse_function_section(
        &mut self,
        funcs: &mut enc::FunctionSection,
        section: parser::FunctionSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        for &(_, ty) in &self.removed {
            funcs.function(ty);
        }
        reencode::utils::parse_function_section(self, funcs, section)
    }
    fn parse_code_section(
        &mut self,
        code: &mut enc::CodeSection,
        section: parser::CodeSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        for _ in &self.removed {
            let mut f = enc::Function::new([]);
            f.instruction(&enc::Instruction::Unreachable)
                .instruction(&enc::Instruction::End);
            code.function(&f);
        }
        reencode::utils::parse_code_section(self, code, section)
    }
}

/// Materialize a frozen instance as a compiler input. Refuse modules whose
/// state model is broader than this guest's one memory and one mutable global.
pub fn checkpoint(template: &[u8], memory: &[u8], stack_pointer: i32) -> Result<Vec<u8>> {
    ensure!(
        !memory.is_empty() && memory.len() % 65536 == 0,
        "continuation memory must contain whole Wasm pages"
    );
    let mut stack = None;
    let mut memories = 0;
    let mut mutable = Vec::new();
    for payload in Parser::new(0).parse_all(template) {
        match payload? {
            Payload::StartSection { .. } => {
                anyhow::bail!("start function cannot replay a continuation")
            }
            Payload::DataCountSection { .. } => anyhow::bail!("bulk memory state is not supported"),
            Payload::MemorySection(s) => memories += s.count(),
            Payload::GlobalSection(s) => {
                for (i, g) in s.into_iter().enumerate() {
                    if g?.ty.mutable {
                        mutable.push(i as u32);
                    }
                }
            }
            Payload::ExportSection(s) => {
                for e in s {
                    let e = e?;
                    if e.name == "__stack_pointer" {
                        stack = Some(e.index);
                    }
                }
            }
            Payload::DataSection(s) => {
                for d in s {
                    ensure!(
                        matches!(d?.kind, parser::DataKind::Active { .. }),
                        "passive data is not supported"
                    );
                }
            }
            _ => {}
        }
    }
    ensure!(
        memories == 1 && stack.is_some() && mutable == vec![stack.unwrap()],
        "unsupported continuation state model"
    );
    let mut rewrite = Checkpoint {
        memory,
        stack: stack.unwrap(),
        stack_pointer,
        global: 0,
    };
    let mut module = enc::Module::new();
    rewrite.parse_core_module(&mut module, Parser::new(0), template)?;
    let result = module.finish();
    parser::Validator::new().validate_all(&result)?;
    Ok(result)
}
struct Checkpoint<'a> {
    memory: &'a [u8],
    stack: u32,
    stack_pointer: i32,
    global: u32,
}
impl Reencode for Checkpoint<'_> {
    type Error = std::convert::Infallible;
    fn parse_memory_section(
        &mut self,
        out: &mut enc::MemorySection,
        section: parser::MemorySectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        for m in section {
            let mut m = self.memory_type(m?)?;
            m.minimum = self.memory.len() as u64 / 65536;
            out.memory(m);
        }
        Ok(())
    }
    fn parse_global(
        &mut self,
        globals: &mut enc::GlobalSection,
        global: parser::Global<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        if self.global == self.stack {
            let ty = self.global_type(global.ty)?;
            globals.global(ty, &enc::ConstExpr::i32_const(self.stack_pointer));
        } else {
            reencode::utils::parse_global(self, globals, global)?;
        }
        self.global += 1;
        Ok(())
    }
    fn parse_data_section(
        &mut self,
        data: &mut enc::DataSection,
        _: parser::DataSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        // Coalesce adjacent nonzero pages; zero pages need no artifact bytes.
        let mut start = None;
        for (i, page) in self.memory.chunks(4096).enumerate() {
            if page.iter().any(|&b| b != 0) {
                start.get_or_insert(i * 4096);
            } else if let Some(begin) = start.take() {
                data.active(
                    0,
                    &enc::ConstExpr::i32_const(begin as i32),
                    self.memory[begin..i * 4096].iter().copied(),
                );
            }
        }
        if let Some(begin) = start {
            data.active(
                0,
                &enc::ConstExpr::i32_const(begin as i32),
                self.memory[begin..].iter().copied(),
            );
        }
        Ok(())
    }
}
