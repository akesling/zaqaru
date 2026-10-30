//! What the board's archive's DWARF says, as the store serves it: the
//! source line an instruction came from, the types of a task's statics,
//! and each task's stack stepped back frame by frame with its variables.
//! See `armv6m::dwarf` for the reading; this is the board's side of it,
//! which knows whose code an address is (the archive's map), what the
//! kernel saved of a task's registers (its task table, read through the
//! kernel's own DWARF), and how to answer in JSON.

use std::collections::HashMap;
use std::rc::Rc;

use armv6m::dwarf::{Debug, Line, Type, TypeId};
use armv6m::hubris::{Image, TaskTable};

use super::Board;
use crate::system::server::quoted;

/// Reads a file of the container's: the archive, a source file.
pub type ReadFile<'a> = &'a dyn Fn(&[u8]) -> Result<Vec<u8>, crate::errno::Errno>;

/// The most frames of a stack stepped back.
const FRAMES: usize = 32;

/// Each owner's DWARF, read the first time it is asked for, under the
/// archive's path: when the archive changes, all of it is read again.
#[derive(Default)]
pub struct Cache {
    pub archive: String,
    owners: HashMap<String, Rc<Result<Debug, String>>>,
    /// `board/index`, once made: see index.rs.
    pub index: Option<Rc<String>>,
}

impl Board {
    /// The DWARF of `owner` (`kernel`, or a task by name) in the board's
    /// archive; `None` when no archive is named.
    pub fn debug_of(&self, owner: &str, read: ReadFile) -> Option<Rc<Result<Debug, String>>> {
        let archive = self.archive.as_ref()?;
        let mut cache = self.debug.borrow_mut();
        if cache.archive != *archive {
            *cache = Cache { archive: archive.clone(), owners: HashMap::new(), index: None };
        }
        if let Some(debug) = cache.owners.get(owner) {
            return Some(Rc::clone(debug));
        }
        let entry = if owner == "kernel" { "elf/kernel".to_string() } else { format!("elf/task/{owner}") };
        let debug = Rc::new(
            read(archive.as_bytes())
                .map_err(|errno| format!("{archive}: {errno:?}"))
                .and_then(|bytes| armv6m::zip::entry(&bytes, &entry).ok_or_else(|| format!("{archive}: no {entry}")))
                .and_then(|elf| Debug::read(&elf).map_err(|why| format!("{entry}: {why}"))),
        );
        cache.owners.insert(owner.to_string(), Rc::clone(&debug));
        Some(debug)
    }

    /// What an address is, as [`Board::describe`] says it, with the
    /// static it is in named down its type from the DWARF of whoever's
    /// memory it is: `display .frame: FRAME_SHOWN[40] (SRAM4 + 0x28)`.
    pub fn describe_typed(&self, address: u32, map: Option<&Result<Image, String>>, read: ReadFile) -> String {
        let plain = self.describe(address, map);
        let Some(place) = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(address)) else { return plain };
        let Some(debug) = self.debug_of(&place.owner.name, read) else { return plain };
        let Ok(debug) = &*debug else { return plain };
        let Some(name) = debug.place_of(address) else { return plain };
        // The owner and part as they were, the symbol and offset made the
        // name; what the chip calls it after, as before.
        let block = plain.find(" (").map_or("", |at| &plain[at..]);
        format!("{} {}: {name}{block}", place.owner.name, place.part)
    }

    /// Where the instruction at `pc` came from, innermost first, from the
    /// DWARF of whoever's code it is.
    pub fn lines_at(&self, pc: u32, map: Option<&Result<Image, String>>, read: ReadFile) -> Vec<Line> {
        let Some(owner) = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(pc)).map(|p| p.owner.name.clone()) else {
            return Vec::new();
        };
        match self.debug_of(&owner, read).as_deref() {
            Some(Ok(debug)) => debug.lines(pc),
            _ => Vec::new(),
        }
    }

    /// `board/source/{pc}`: whose code `pc` is, and where it came from.
    pub fn source_json(&self, pc: u32, map: Option<&Result<Image, String>>, read: ReadFile) -> String {
        let owner = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(pc)).map(|p| p.owner.name.clone());
        format!(
            r#"{{"pc":{pc},"owner":{},"code":{},"lines":[{}]}}"#,
            owner.as_deref().map_or("null".into(), quoted),
            quoted(&self.code_at(pc, map)),
            self.lines_at(pc, map, read).iter().map(line_json).collect::<Vec<_>>().join(",")
        )
    }

    /// `board/typed/{owner}`: the statics in `owner`'s RAM, each with its
    /// type, and every type they are made of.
    pub fn typed_json(&self, owner: &str, map: Option<&Result<Image, String>>, read: ReadFile) -> Option<String> {
        let image = map?.as_ref().ok()?;
        let place = if owner == "kernel" { &image.kernel } else { image.owner(owner)? };
        let debug = self.debug_of(owner, read)?;
        let debug = match &*debug {
            Ok(debug) => debug,
            Err(why) => return Some(format!(r#"{{"owner":{},"error":{}}}"#, quoted(owner), quoted(why))),
        };
        let in_ram = |base: u32| place.sections.iter().any(|s| s.writable && base >= s.base && base - s.base < s.size);
        let statics: Vec<_> = debug.statics.iter().filter(|s| s.size > 0 && in_ram(s.address)).collect();
        let types = debug.closure(statics.iter().map(|s| s.ty));
        Some(format!(
            r#"{{"owner":{},"statics":[{}],"types":{}}}"#,
            quoted(owner),
            statics
                .iter()
                .map(|s| format!(r#"{{"name":{},"address":{},"size":{},"type":{}}}"#, quoted(&s.name), s.address, s.size, s.ty))
                .collect::<Vec<_>>()
                .join(","),
            types_json(debug, &types)
        ))
    }

    /// `board/types/{owner}/{ids}`: the types numbered, comma-separated,
    /// and every type they are made of: for a reader following a pointer
    /// to a type it has not been told about.
    pub fn types_json(&self, owner: &str, ids: &str, read: ReadFile) -> Option<String> {
        let debug = self.debug_of(owner, read)?;
        let Ok(debug) = &*debug else { return None };
        let asked: Vec<TypeId> = ids.split(',').filter_map(|n| n.parse().ok()).collect();
        Some(format!(r#"{{"owner":{},"types":{}}}"#, quoted(owner), types_json(debug, &debug.closure(asked))))
    }

    /// The registers `owner` would go on with: the core's own when it is
    /// the one running (a task in Thread mode, the kernel in an
    /// exception), and otherwise, for a task, what the kernel saved of
    /// them. `None` when there are none to be had: the kernel while a task
    /// runs, a task that never ran.
    fn registers_of(&self, owner: &str, map: &Image, read: ReadFile) -> Option<([Option<u32>; 16], bool)> {
        let cpu = &self.machine.cpu;
        let live = || {
            let mut regs = [None; 16];
            for (n, value) in cpu.r.iter().enumerate().take(13) {
                regs[n] = Some(*value);
            }
            regs[13] = Some(if cpu.ipsr == 0 && cpu.spsel { cpu.psp } else { cpu.msp });
            regs[14] = Some(cpu.r[14]);
            regs[15] = Some(cpu.pc);
            regs
        };
        if owner == "kernel" {
            return (cpu.ipsr != 0).then(|| (live(), true));
        }
        let index = map.owner(owner)?.index?;
        let kernel = self.debug_of("kernel", read)?;
        let table = TaskTable::of(kernel.as_ref().as_ref().ok()?)?;
        let word = |at: u32| self.machine.bus.peek(at, 4).ok();
        if cpu.ipsr == 0 && table.current(&word) == Some(index) {
            return Some((live(), true));
        }
        table.registers(index, &word).map(|regs| (regs, false))
    }

    /// `board/stack/{owner}`: `owner`'s stack stepped back from where it is
    /// stopped, frame by frame. Each frame's instruction, where it came
    /// from (with what was inlined), its CFA, and its variables with their
    /// bytes, unknown bytes as `--`; and the types those are.
    pub fn stack_json(&self, owner: &str, map: Option<&Result<Image, String>>, read: ReadFile) -> Option<String> {
        let image = map?.as_ref().ok()?;
        let debug = self.debug_of(owner, read)?;
        let debug = match &*debug {
            Ok(debug) => debug,
            Err(why) => return Some(format!(r#"{{"owner":{},"error":{}}}"#, quoted(owner), quoted(why))),
        };
        let Some((regs, running)) = self.registers_of(owner, image, read) else {
            return Some(format!(r#"{{"owner":{},"running":false,"frames":[],"types":{{}}}}"#, quoted(owner)));
        };
        let word = |at: u32| self.machine.bus.peek(at, 4).ok();
        let byte = |at: u32| self.machine.bus.peek(at, 1).ok().map(|b| b as u8);
        let frames = debug.unwind(regs, &word, FRAMES);
        let mut roots = Vec::new();
        let mut out = Vec::new();
        for (n, frame) in frames.iter().enumerate() {
            let lines = debug.lines(if n == 0 { frame.pc } else { frame.pc.wrapping_sub(1) });
            let variables: Vec<String> = debug
                .variables(frame, n > 0, &byte)
                .into_iter()
                .map(|v| {
                    roots.push(v.ty);
                    format!(
                        r#"{{"name":{},"type":{},"parameter":{},"function":{},"address":{},"hex":"{}"}}"#,
                        quoted(&v.name),
                        v.ty,
                        v.parameter,
                        quoted(&v.function),
                        v.address.map_or("null".into(), |a| a.to_string()),
                        hex(&v.bytes)
                    )
                })
                .collect();
            out.push(format!(
                r#"{{"pc":{},"sp":{},"cfa":{},"code":{},"lines":[{}],"variables":[{}]}}"#,
                frame.pc,
                frame.regs[13].map_or("null".into(), |v| v.to_string()),
                frame.cfa.map_or("null".into(), |v| v.to_string()),
                quoted(&self.code_at(frame.pc, map)),
                lines.iter().map(line_json).collect::<Vec<_>>().join(","),
                variables.join(",")
            ));
        }
        let types = debug.closure(roots);
        Some(format!(
            r#"{{"owner":{},"running":{running},"frames":[{}],"types":{}}}"#,
            quoted(owner),
            out.join(","),
            types_json(debug, &types)
        ))
    }

    /// `board/file/{path}`: a source file the DWARF names, as the build
    /// kept it beside the archive: under `src/` in the archive's directory
    /// or the one above it (where the bench's other archives are), at the
    /// path the DWARF gives.
    pub fn source_file(&self, path: &str, read: ReadFile) -> Option<Vec<u8>> {
        let archive = self.archive.as_ref()?;
        let dir = archive.rsplit_once('/').map_or("", |(dir, _)| dir);
        let above = dir.rsplit_once('/').map_or("", |(above, _)| above);
        let path = path.trim_start_matches('/');
        if path.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        [dir, above].into_iter().find_map(|root| read(format!("{root}/src/{path}").as_bytes()).ok())
    }

    /// Every source file the build kept beside the archive, as the DWARF
    /// would name each: its `src/.files` (see demo/hubris/sources.sh).
    pub fn kept_files(&self, read: ReadFile) -> Vec<String> {
        let Some(list) = self.source_file(".files", read) else { return Vec::new() };
        String::from_utf8_lossy(&list).lines().filter(|l| !l.is_empty()).map(str::to_string).collect()
    }
}

/// Bytes as hex, `--` for each not known.
fn hex(bytes: &[Option<u8>]) -> String {
    bytes.iter().map(|b| b.map_or("--".to_string(), |b| format!("{b:02x}"))).collect()
}

pub fn line_json(line: &Line) -> String {
    format!(
        r#"{{"function":{},"file":{},"line":{},"column":{}}}"#,
        line.function.as_deref().map_or("null".into(), quoted),
        line.file.as_deref().map_or("null".into(), quoted),
        line.line,
        line.column
    )
}

/// Types by number, as a JSON object: each one's kind and what a reader
/// of memory needs to decode a value of it. A discriminant's value is a
/// string, for the 64-bit ones a JSON number would round.
fn types_json(debug: &Debug, ids: &[TypeId]) -> String {
    let out: Vec<String> = ids
        .iter()
        .filter_map(|&id| Some((id, debug.ty(id)?)))
        .map(|(id, ty)| {
            let body = match &ty {
                Type::Base { name, size, encoding } => {
                    let enc = match encoding {
                        armv6m::dwarf::Encoding::Bool => "bool",
                        armv6m::dwarf::Encoding::Char => "char",
                        armv6m::dwarf::Encoding::Signed => "i",
                        armv6m::dwarf::Encoding::Unsigned => "u",
                        armv6m::dwarf::Encoding::Float => "f",
                    };
                    format!(r#""kind":"base","name":{},"size":{size},"enc":"{enc}""#, quoted(name))
                }
                Type::Pointer { name, target } => {
                    format!(r#""kind":"pointer","name":{},"target":{}"#, quoted(name), target.map_or("null".into(), |t| t.to_string()))
                }
                Type::Struct { name, size, fields, variants, union } => {
                    let fields: Vec<String> = fields.iter().map(|f| format!("[{},{},{}]", quoted(&f.name), f.offset, f.ty)).collect();
                    let variants = variants.as_ref().map_or("null".into(), |v| {
                        format!(
                            r#"{{"discriminant":{},"list":[{}]}}"#,
                            v.discriminant.as_ref().map_or("null".into(), |d| format!("[{},{}]", d.offset, d.ty)),
                            v.variants
                                .iter()
                                .map(|v| format!(
                                    "[{},{},{},{}]",
                                    v.value.map_or("null".into(), |n| format!("\"{n}\"")),
                                    quoted(&v.name),
                                    v.offset,
                                    v.ty
                                ))
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    });
                    format!(
                        r#""kind":"struct","name":{},"size":{size},"union":{union},"fields":[{}],"variants":{variants}"#,
                        quoted(name),
                        fields.join(",")
                    )
                }
                Type::Enum { name, size, signed, values } => format!(
                    r#""kind":"enum","name":{},"size":{size},"signed":{signed},"values":[{}]"#,
                    quoted(name),
                    values.iter().map(|(n, v)| format!("[{},{v}]", quoted(n))).collect::<Vec<_>>().join(",")
                ),
                Type::Array { name, element, count, stride } => {
                    format!(r#""kind":"array","name":{},"element":{element},"count":{count},"stride":{stride}"#, quoted(name))
                }
                Type::Alias { name, target } => format!(r#""kind":"alias","name":{},"target":{target}"#, quoted(name)),
                Type::Opaque { name, size } => format!(r#""kind":"opaque","name":{},"size":{size}"#, quoted(name)),
            };
            format!(r#""{id}":{{{body}}}"#)
        })
        .collect();
    format!("{{{}}}", out.join(","))
}
