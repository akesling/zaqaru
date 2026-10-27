//! A Hubris build archive, read for what the board's memory *is*: whose
//! each byte is — the kernel's or a task's — what part of it (its code, its
//! constants, its data, its stack), and which function or static it falls
//! in. What a debugger's `whatis` answers.
//!
//! All of it is read from the archive itself, nothing from the running
//! board: each task's ELF (`elf/task/NAME`) is linked at that task's own
//! addresses, so its allocated sections are where its code and data are,
//! and its symbol table names what is in them; the kernel's (`elf/kernel`)
//! likewise. A stack is no section. The kernel's linker script says
//! where its stack begins and ends (`_stack_base`, `_stack_start`). A
//! task's is the start of its RAM allocation — Hubris's build
//! (`build/xtask/src/dist.rs`) gives the task's linker a `STACK` region of
//! `stacksize` there and its `RAM` region after it, where `.data` and
//! `.bss` begin, `stacksize` being 8-aligned — so it is the `stacksize`
//! bytes below the task's lowest RAM section, the task's own `stacksize`
//! in `app.toml` or the app's. Tasks are indexed in the order `app.toml`
//! lists them, which is the kernel's task table's order.

use crate::zip;

/// What an archive says the board's memory is.
#[derive(Clone, Debug, Default)]
pub struct Image {
    /// The app's name, from `app.toml`.
    pub name: String,
    pub kernel: Owner,
    /// In the kernel's task table's order.
    pub tasks: Vec<Owner>,
}

/// The kernel or a task, and the memory that is its.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    pub name: String,
    /// Its index in the task table; `None` for the kernel.
    pub index: Option<usize>,
    /// Its allocated sections, by address.
    pub sections: Vec<Section>,
    /// Its stack, `[base, base + size)`, when the archive says where.
    pub stack: Option<(u32, u32)>,
    /// Its functions and statics, by address.
    pub symbols: Vec<Symbol>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub writable: bool,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// Demangled, without the hash: `drv_stm32xx_i2c::I2cController::write_read`.
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub function: bool,
}

/// What an address is, as far as the archive says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place<'a> {
    pub owner: &'a Owner,
    /// The section's name, or `stack`.
    pub part: String,
    /// The function or static it falls in, and how far into it.
    pub symbol: Option<(&'a Symbol, u32)>,
}

impl Image {
    /// Reads a Hubris archive.
    pub fn read(archive: &[u8]) -> Result<Image, String> {
        let text = |name: &str| zip::entry(archive, name).map(|b| String::from_utf8_lossy(&b).into_owned());
        let app_toml = text("app.toml").ok_or("no app.toml: not a Hubris archive")?;
        let config = AppToml::parse(&app_toml);
        let kernel_elf = zip::entry(archive, "elf/kernel").ok_or("no elf/kernel")?;
        let kernel = Owner::from_elf("kernel", None, &kernel_elf, config.kernel_stack)?;
        let mut tasks = Vec::new();
        for (index, (name, stacksize)) in config.tasks.iter().enumerate() {
            let Some(elf) = zip::entry(archive, &format!("elf/task/{name}")) else { continue };
            tasks.push(Owner::from_elf(name, Some(index), &elf, Some(stacksize.unwrap_or(config.stacksize)))?);
        }
        Ok(Image { name: config.name, kernel, tasks })
    }

    /// What `address` is: whose, which part, which symbol. `None` when it
    /// is nobody's that the archive knows of.
    pub fn whatis(&self, address: u32) -> Option<Place<'_>> {
        std::iter::once(&self.kernel).chain(&self.tasks).find_map(|owner| owner.place(address))
    }

    /// The owner named `name`: `kernel`, or a task.
    pub fn owner(&self, name: &str) -> Option<&Owner> {
        std::iter::once(&self.kernel).chain(&self.tasks).find(|o| o.name == name)
    }
}

impl Owner {
    fn from_elf(name: &str, index: Option<usize>, elf: &[u8], stacksize: Option<u32>) -> Result<Owner, String> {
        let elf = Elf::parse(elf).ok_or_else(|| format!("elf/{name}: not an ELF32 this reads"))?;
        let mut sections: Vec<Section> = elf.sections.iter().filter(|s| s.alloc && s.addr != 0 && s.size != 0).map(|s| Section {
            name: s.name.clone(),
            base: s.addr,
            size: s.size,
            writable: s.write,
            executable: s.exec,
        }).collect();
        sections.sort_by_key(|s| s.base);
        let mut symbols: Vec<Symbol> = elf
            .symbols
            .iter()
            .filter(|s| (s.kind == STT_FUNC || s.kind == STT_OBJECT) && s.size != 0 && s.value != 0)
            .map(|s| Symbol {
                name: demangle(&s.name),
                // A Thumb function's address has its low bit set.
                base: if s.kind == STT_FUNC { s.value & !1 } else { s.value },
                size: s.size,
                function: s.kind == STT_FUNC,
            })
            .collect();
        symbols.sort_by_key(|s| (s.base, std::cmp::Reverse(s.size)));
        symbols.dedup_by(|a, b| a.base == b.base && a.name == b.name);
        // The kernel's stack, as its linker script says; a task's, the
        // `stacksize` below where its RAM sections begin — an empty
        // `.data` too, which still says where.
        let value = |name: &str| elf.symbols.iter().find(|s| s.name == name).map(|s| s.value);
        let ram = elf
            .sections
            .iter()
            .filter(|s| s.alloc && s.addr != 0 && matches!(s.name.as_str(), ".data" | ".bss" | ".uninit"))
            .map(|s| s.addr)
            .min();
        let stack = match (value("_stack_base"), value("_stack_start")) {
            (Some(base), Some(top)) if base < top => Some((base, top - base)),
            _ => ram.zip(stacksize).filter(|&(_, size)| size > 0).map(|(ram, size)| (ram.saturating_sub(size), size)),
        };
        Ok(Owner { name: name.to_string(), index, sections, stack, symbols })
    }

    fn place(&self, address: u32) -> Option<Place<'_>> {
        let within = |base: u32, size: u32| address >= base && (address - base) < size;
        let part = if self.stack.is_some_and(|(base, size)| within(base, size)) {
            "stack".to_string()
        } else {
            self.sections.iter().find(|s| within(s.base, s.size))?.name.clone()
        };
        // The last symbol starting at or below the address, if the address
        // is inside it.
        let n = self.symbols.partition_point(|s| s.base <= address);
        let symbol = self.symbols[..n]
            .iter()
            .rev()
            .take(8)
            .find(|s| within(s.base, s.size))
            .map(|s| (s, address - s.base));
        Some(Place { owner: self, part, symbol })
    }
}

/// A symbol's name as a person reads it: demangled, without the hash.
fn demangle(name: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(name))
}

/// What `app.toml` says that matters here: the app's name, its tasks in
/// order with their stack sizes, and the default stack size.
struct AppToml {
    name: String,
    stacksize: u32,
    kernel_stack: Option<u32>,
    tasks: Vec<(String, Option<u32>)>,
}

impl AppToml {
    /// A line-wise reading — tables and `key = value` — which is all an
    /// archive's `app.toml`, written by Hubris's build, needs.
    fn parse(text: &str) -> AppToml {
        let mut out = AppToml { name: String::new(), stacksize: 0, kernel_stack: None, tasks: Vec::new() };
        let mut table = String::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                table = name.trim().to_string();
                if let Some(task) = table.strip_prefix("tasks.").filter(|t| !t.contains('.')) {
                    out.tasks.push((task.to_string(), None));
                }
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let (key, value) = (key.trim(), value.trim());
            let number = || value.replace('_', "").parse::<u32>().ok();
            match (table.as_str(), key) {
                ("", "name") => out.name = value.trim_matches('"').to_string(),
                ("", "stacksize") => out.stacksize = number().unwrap_or(0),
                ("kernel", "stacksize") => out.kernel_stack = number(),
                (t, "stacksize") if t.strip_prefix("tasks.").is_some_and(|t| !t.contains('.')) => {
                    if let Some(task) = out.tasks.last_mut() {
                        task.1 = number();
                    }
                }
                _ => {}
            }
        }
        out
    }
}

const STT_OBJECT: u8 = 1;
const STT_FUNC: u8 = 2;

/// The parts of a 32-bit little-endian ELF read here: section headers, and
/// the symbol table.
struct Elf {
    sections: Vec<ElfSection>,
    symbols: Vec<ElfSymbol>,
}

struct ElfSection {
    name: String,
    kind: u32,
    alloc: bool,
    write: bool,
    exec: bool,
    addr: u32,
    offset: u32,
    size: u32,
    link: u32,
}

struct ElfSymbol {
    name: String,
    value: u32,
    size: u32,
    kind: u8,
}

impl Elf {
    fn parse(bytes: &[u8]) -> Option<Elf> {
        if bytes.get(0..6)? != b"\x7fELF\x01\x01" {
            return None;
        }
        let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
        let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let (shoff, shentsize, shnum, shstrndx) =
            (u32_at(0x20)? as usize, u16_at(0x2E)? as usize, u16_at(0x30)? as usize, u16_at(0x32)? as usize);
        let string = |table: &ElfSection, at: u32| -> String {
            let start = (table.offset + at) as usize;
            let Some(rest) = bytes.get(start..(table.offset + table.size) as usize) else { return String::new() };
            let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            String::from_utf8_lossy(&rest[..end]).into_owned()
        };
        let mut sections = Vec::with_capacity(shnum);
        let mut names = Vec::with_capacity(shnum);
        for n in 0..shnum {
            let at = shoff + n * shentsize;
            let flags = u32_at(at + 8)?;
            names.push(u32_at(at)?);
            sections.push(ElfSection {
                name: String::new(),
                kind: u32_at(at + 4)?,
                alloc: flags & 2 != 0,
                write: flags & 1 != 0,
                exec: flags & 4 != 0,
                addr: u32_at(at + 12)?,
                offset: u32_at(at + 16)?,
                size: u32_at(at + 20)?,
                link: u32_at(at + 24)?,
            });
        }
        let named: Vec<String> = names.iter().map(|&at| sections.get(shstrndx).map_or(String::new(), |t| string(t, at))).collect();
        for (section, name) in sections.iter_mut().zip(named) {
            section.name = name;
        }
        let mut symbols = Vec::new();
        // SHT_SYMTAB.
        if let Some(table) = sections.iter().find(|s| s.kind == 2) {
            let strings = sections.get(table.link as usize)?;
            for n in 0..(table.size / 16) as usize {
                let at = table.offset as usize + n * 16;
                symbols.push(ElfSymbol {
                    name: string(strings, u32_at(at)?),
                    value: u32_at(at + 4)?,
                    size: u32_at(at + 8)?,
                    kind: *bytes.get(at + 12)? & 0xF,
                });
            }
        }
        Some(Elf { sections, symbols })
    }
}

#[cfg(test)]
mod tests {
    use super::AppToml;

    #[test]
    fn app_toml_gives_the_tasks_in_order_with_their_stacks() {
        let config = AppToml::parse(
            "name = \"demo\"\nstacksize = 896\n\n[kernel]\nname = \"demo\"\nrequires = {flash = 1}\n\n[tasks.jefe]\nstacksize = 2000\n\n[tasks.jefe.config]\nstacksize = 7\n\n[tasks.idle]\npriority = 7\n",
        );
        assert_eq!(config.name, "demo");
        assert_eq!(config.stacksize, 896);
        assert_eq!(config.tasks, [("jefe".to_string(), Some(2000)), ("idle".to_string(), None)]);
    }
}
