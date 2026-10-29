//! What the compiler said about the code it made, read from the DWARF in an
//! ELF's debugging sections: which source line an instruction came from
//! and which calls were inlined into it, the type of every static, how to
//! step a stack back from a frame to its caller, and where each of a
//! frame's variables is. What a debugger knows that a symbol table does
//! not: `drv/stm32xx-i2c-server/src/main.rs:212` rather than `main +
//! 0x80e`, and a struct's fields rather than its bytes.
//!
//! Types are read as they are asked for and numbered in the order they
//! were, so a number once given keeps meaning the same type: whoever has
//! been told what type 12 is need not be told again. A type is described
//! as plainly as a reader of memory needs (sizes, offsets, names, and
//! which bytes say which variant of an enum holds) and decoding bytes with
//! it is left to the reader.
//!
//! Variables are found where DWARF says they are at the instruction a
//! frame is stopped at: in memory, in a register, in pieces of both, or
//! nowhere (optimized out). A frame's registers are known only as far as
//! unwinding could recover them, so a variable in a register its callee
//! was free to overwrite reads as unknown rather than wrong.

// gimli's constants keep DWARF's own spelling.
#![allow(non_upper_case_globals)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gimli::{AttributeValue, Reader as _, UnitOffset, UnwindSection as _, constants::*};

type R = gimli::EndianRcSlice<gimli::LittleEndian>;

/// A type's number: see the module's documentation.
pub type TypeId = u32;

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// A number, a `bool` or a `char`.
    Base { name: String, size: u32, encoding: Encoding },
    /// A pointer or a reference, and what it points at when that is said.
    Pointer { name: String, target: Option<TypeId> },
    /// A struct, a tuple or a union (whose fields are all at offset 0); or
    /// a Rust enum, which is a struct whose `variants` say which of them
    /// its bytes hold.
    Struct { name: String, size: u32, fields: Vec<Field>, variants: Option<Variants>, union: bool },
    /// A C-like enum: a number, some of whose values have names.
    Enum { name: String, size: u32, signed: bool, values: Vec<(String, i64)> },
    Array { name: String, element: TypeId, count: u32, stride: u32 },
    /// Another name for a type, or the type made `const` or `volatile`.
    Alias { name: String, target: TypeId },
    /// What there is nothing to decode in: a function, `()`, what DWARF
    /// only declares.
    Opaque { name: String, size: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Bool,
    Char,
    Signed,
    Unsigned,
    Float,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub offset: u32,
    pub ty: TypeId,
}

/// Which variant a Rust enum's bytes hold.
#[derive(Clone, Debug, PartialEq)]
pub struct Variants {
    /// The bytes that say: where, and of what type. `None` for an enum of
    /// one variant, whose bytes have nothing to say.
    pub discriminant: Option<Field>,
    pub variants: Vec<Variant>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    /// The discriminant's value when it is this variant; `None` for the
    /// one it is when no other's matches (the variant whose own bytes hold
    /// the others' values, in a niche).
    pub value: Option<u64>,
    pub name: String,
    pub offset: u32,
    /// A struct of the variant's fields.
    pub ty: TypeId,
}

impl Type {
    pub fn name(&self) -> &str {
        match self {
            Type::Base { name, .. }
            | Type::Pointer { name, .. }
            | Type::Struct { name, .. }
            | Type::Enum { name, .. }
            | Type::Array { name, .. }
            | Type::Alias { name, .. }
            | Type::Opaque { name, .. } => name,
        }
    }
}

/// A static: a variable at an address of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct Static {
    /// With the path of modules it is in: `task_jefe::TASK_STATE`.
    pub name: String,
    pub address: u32,
    pub size: u32,
    pub ty: TypeId,
}

/// Where an instruction came from: a function, and the file and line in
/// it. Several, innermost first, when calls were inlined into it; each
/// after the first is where the one before it was called from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
}

/// A frame of a stack: the instruction it is at, its canonical frame
/// address (the stack pointer its caller had before the call), and the
/// registers as far as they are known. `regs[13]` is its stack pointer,
/// `regs[15]` its `pc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pc: u32,
    pub cfa: Option<u32>,
    pub regs: [Option<u32>; 16],
}

/// A variable of a frame: a local, or a parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub ty: TypeId,
    pub parameter: bool,
    /// The function it is a variable of: the frame's own, or one inlined
    /// into it.
    pub function: String,
    /// Where it is, when it is in memory in one piece.
    pub address: Option<u32>,
    /// Its bytes, as many as its type has (up to `VARIABLE_BYTES`);
    /// `None` for each byte not known: optimized out, or in a register
    /// unwinding could not recover.
    pub bytes: Vec<Option<u8>>,
}

/// The most of a variable's bytes read.
pub const VARIABLE_BYTES: u32 = 1024;

#[derive(Default)]
struct Types {
    all: Vec<Type>,
    /// By unit and offset in it.
    by_die: HashMap<(usize, usize), TypeId>,
}

/// An ELF's DWARF, read.
pub struct Debug {
    dwarf: gimli::Dwarf<R>,
    units: Vec<gimli::Unit<R>>,
    /// Each unit's ranges of code, by address: for the unit an
    /// instruction's description is in.
    ranges: Vec<(u64, u64, usize)>,
    lines: addr2line::Context<R>,
    frames: gimli::DebugFrame<R>,
    /// Every function the symbol table names, by address, with its size:
    /// for where one without call frame information begins.
    functions: Vec<(u32, u32)>,
    types: RefCell<Types>,
    /// By address.
    pub statics: Vec<Static>,
}

/// Types that only hold another, whose own fields a name leaves out.
const WRAPPERS: [&str; 8] = ["MaybeUninit<", "ManuallyDrop<", "UnsafeCell<", "SyncUnsafeCell<", "Cell<", "RefCell<", "StaticCell<", "UninitCell<"];

fn fail(why: gimli::Error) -> String {
    format!("DWARF: {why}")
}

impl Debug {
    /// Reads an ELF's DWARF; an error when it has none.
    pub fn read(elf: &[u8]) -> Result<Debug, String> {
        let sections = crate::hubris::sections(elf).ok_or("not an ELF32 this reads")?;
        let mut held: HashMap<&str, R> = HashMap::new();
        for (name, bytes) in &sections {
            if name.starts_with(".debug_") {
                held.insert(name.as_str(), R::new(Rc::from(*bytes), gimli::LittleEndian));
            }
        }
        if !held.contains_key(".debug_info") {
            return Err("no DWARF: the ELF has no .debug_info".into());
        }
        let section = |id: gimli::SectionId| -> Result<R, gimli::Error> {
            Ok(held.get(id.name()).cloned().unwrap_or_else(|| R::new(Rc::from(&[][..]), gimli::LittleEndian)))
        };
        let dwarf = gimli::Dwarf::load(section).map_err(fail)?;
        let lines = addr2line::Context::from_dwarf(gimli::Dwarf::load(section).map_err(fail)?).map_err(fail)?;
        let mut frames = gimli::DebugFrame::from(section(gimli::SectionId::DebugFrame).map_err(fail)?);
        frames.set_address_size(4);
        let mut units = Vec::new();
        let mut headers = dwarf.units();
        while let Some(header) = headers.next().map_err(fail)? {
            units.push(dwarf.unit(header).map_err(fail)?);
        }
        let mut ranges = Vec::new();
        for (n, unit) in units.iter().enumerate() {
            let mut iter = dwarf.unit_ranges(unit).map_err(fail)?;
            while let Some(range) = iter.next().map_err(fail)? {
                if range.begin < range.end {
                    ranges.push((range.begin, range.end, n));
                }
            }
        }
        ranges.sort_unstable();
        let functions = crate::hubris::functions(elf);
        let mut debug = Debug { dwarf, units, ranges, lines, frames, functions, types: RefCell::default(), statics: Vec::new() };
        debug.statics = debug.read_statics();
        Ok(debug)
    }

    /// Where the instruction at `pc` came from: innermost first.
    pub fn lines(&self, pc: u32) -> Vec<Line> {
        let mut out = Vec::new();
        let Ok(mut frames) = self.lines.find_frames(u64::from(pc)).skip_all_loads() else { return out };
        while let Ok(Some(frame)) = frames.next() {
            let location = frame.location.as_ref();
            out.push(Line {
                function: frame.function.as_ref().and_then(|f| f.demangle().ok()).map(|f| f.into_owned()),
                file: location.and_then(|l| l.file).map(str::to_string),
                line: location.and_then(|l| l.line).unwrap_or(0),
                column: location.and_then(|l| l.column).unwrap_or(0),
            });
        }
        out
    }

    /// Every source file the line tables name: what a build has to keep
    /// for its code to be shown.
    pub fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        for unit in &self.units {
            let Some(program) = unit.line_program.as_ref() else { continue };
            let header = program.header();
            let comp_dir = unit.comp_dir.as_ref().and_then(|d| Some(d.to_string_lossy().ok()?.into_owned())).unwrap_or_default();
            for file in header.file_names() {
                let string = |value: AttributeValue<R>| -> Option<String> { Some(self.dwarf.attr_string(unit, value).ok()?.to_string_lossy().ok()?.into_owned()) };
                let Some(name) = string(file.path_name()) else { continue };
                let dir = file.directory(header).and_then(string).unwrap_or_default();
                out.push(join(&join(&comp_dir, &dir), &name));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// The type numbered `id`.
    pub fn ty(&self, id: TypeId) -> Option<Type> {
        self.types.borrow().all.get(id as usize).cloned()
    }

    /// How many types have been numbered so far.
    pub fn types_known(&self) -> usize {
        self.types.borrow().all.len()
    }

    /// Every type `roots` refer to, themselves included, each once.
    pub fn closure(&self, roots: impl IntoIterator<Item = TypeId>) -> Vec<TypeId> {
        let mut seen = std::collections::BTreeSet::new();
        let mut todo: Vec<TypeId> = roots.into_iter().collect();
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            match self.ty(id) {
                Some(Type::Pointer { target: Some(t), .. } | Type::Alias { target: t, .. }) => todo.push(t),
                Some(Type::Array { element, .. }) => todo.push(element),
                Some(Type::Struct { fields, variants, .. }) => {
                    todo.extend(fields.iter().map(|f| f.ty));
                    if let Some(v) = variants {
                        todo.extend(v.discriminant.iter().map(|f| f.ty));
                        todo.extend(v.variants.iter().map(|v| v.ty));
                    }
                }
                _ => {}
            }
        }
        // A pointer's target is named, not followed: its own targets are
        // only needed when someone follows it.
        seen.into_iter().collect()
    }

    /// How many bytes a value of type `id` takes.
    pub fn size(&self, id: TypeId) -> u32 {
        let mut id = id;
        for _ in 0..32 {
            match self.ty(id) {
                Some(Type::Base { size, .. } | Type::Struct { size, .. } | Type::Enum { size, .. } | Type::Opaque { size, .. }) => return size,
                Some(Type::Pointer { .. }) => return 4,
                Some(Type::Array { count, stride, .. }) => return count.saturating_mul(stride),
                Some(Type::Alias { target, .. }) => id = target,
                None => return 0,
            }
        }
        0
    }

    /// The type `id` is another name for, or itself.
    pub fn resolve(&self, id: TypeId) -> TypeId {
        let mut id = id;
        for _ in 0..32 {
            match self.ty(id) {
                Some(Type::Alias { target, .. }) => id = target,
                _ => break,
            }
        }
        id
    }

    /// The field of a struct called `name`: its offset and type.
    pub fn field(&self, id: TypeId, name: &str) -> Option<(u32, TypeId)> {
        match self.ty(self.resolve(id))? {
            Type::Struct { fields, .. } => fields.iter().find(|f| f.name == name).map(|f| (f.offset, f.ty)),
            _ => None,
        }
    }

    /// An address in one of the statics, named down its type: the static's
    /// own name without its modules, then each element and field the
    /// address is in — `HUBRIS_TASK_TABLE_SPACE[3].save.r4`,
    /// `FRAME_SHOWN[40]` — with whatever is left over as an offset into the
    /// last. A wrapper's own field (`MaybeUninit`'s `value`, a newtype's
    /// `__0`) says nothing and is left out. `None` outside every static.
    pub fn place_of(&self, address: u32) -> Option<String> {
        let at = self.statics.partition_point(|s| s.address <= address).checked_sub(1)?;
        let st = self.statics[..=at].iter().rev().find(|s| address >= s.address && address - s.address < s.size.max(1))?;
        let mut name = st.name.rsplit("::").next().unwrap_or(&st.name).to_string();
        let (mut offset, mut id) = (address - st.address, st.ty);
        for _ in 0..16 {
            id = self.resolve(id);
            match self.ty(id) {
                Some(Type::Array { element, stride, count, .. }) if stride > 0 && offset < stride.saturating_mul(count) => {
                    name += &format!("[{}]", offset / stride);
                    offset %= stride;
                    id = element;
                }
                Some(Type::Struct { name: ty, fields, variants: None, .. }) => {
                    let Some(field) = fields
                        .iter()
                        .filter(|f| f.offset <= offset && offset - f.offset < self.size(f.ty).max(1) && self.size(f.ty) > 0)
                        .last()
                    else {
                        break;
                    };
                    let wrapper = WRAPPERS.iter().any(|w| ty.starts_with(w)) || (fields.len() == 1 && field.name.starts_with("__"));
                    if !wrapper {
                        name += &format!(".{}", field.name.trim_start_matches("__"));
                    }
                    offset -= field.offset;
                    id = field.ty;
                }
                _ => break,
            }
        }
        if offset > 0 {
            name += &format!(" + {offset:#x}");
        }
        Some(name)
    }

    /// The static whose name is `name` or ends `::name`.
    pub fn static_named(&self, name: &str) -> Option<&Static> {
        self.statics.iter().find(|s| s.name == name || s.name.strip_suffix(name).is_some_and(|p| p.ends_with("::")))
    }

    /// Steps a stack back from the registers of its innermost frame, at
    /// most `limit` frames, reading the stack's words through `read`.
    /// Where there is no call frame information, as for the hand-written
    /// stubs a task makes its system calls through, the function's first
    /// instructions say what it pushed (see `prologue`). Stops at a return
    /// address that is none (0, or an exception's `EXC_RETURN`), or where
    /// a frame would not move.
    pub fn unwind(&self, regs: [Option<u32>; 16], read: &dyn Fn(u32) -> Option<u32>, limit: usize) -> Vec<Frame> {
        use gimli::{CfaRule, Register, RegisterRule};
        let mut out = Vec::new();
        let mut regs = regs;
        let mut context = gimli::UnwindContext::new();
        let bases = gimli::BaseAddresses::default();
        for depth in 0..limit {
            let Some(pc) = regs[15] else { break };
            // A caller's pc is where it resumes, after the call: the call
            // is the instruction before.
            let probe = if depth == 0 { pc } else { pc.wrapping_sub(1) };
            // The frame's CFA, and its caller's registers.
            let mut next = [None; 16];
            let cfa;
            match self.frames.unwind_info_for_address(&bases, &mut context, u64::from(probe), gimli::DebugFrame::cie_from_offset) {
                Ok(row) => {
                    cfa = match row.cfa() {
                        CfaRule::RegisterAndOffset { register, offset } => {
                            regs.get(register.0 as usize).copied().flatten().map(|v| (i64::from(v) + offset) as u32)
                        }
                        CfaRule::Expression(_) => None,
                    };
                    let Some(at) = cfa else {
                        out.push(Frame { pc, cfa, regs });
                        break;
                    };
                    for (n, slot) in next.iter_mut().enumerate() {
                        *slot = match row.register(Register(n as u16)) {
                            // What a call leaves alone, r4 to r11, is still
                            // the caller's; the rest were the callee's.
                            None => regs[n].filter(|_| (4..=11).contains(&n)),
                            Some(RegisterRule::SameValue) => regs[n],
                            Some(RegisterRule::Offset(o)) => read((i64::from(at) + o) as u32),
                            Some(RegisterRule::ValOffset(o)) => Some((i64::from(at) + o) as u32),
                            Some(RegisterRule::Register(other)) => regs.get(other.0 as usize).copied().flatten(),
                            Some(_) => None,
                        };
                    }
                    // Where the caller resumes: lr as the frame saved it,
                    // or as it still is in a function that never did.
                    if row.register(Register(14)).is_none() {
                        next[14] = regs[14];
                    }
                }
                Err(_) => {
                    let Some((down, saved)) = self.prologue(pc, read) else {
                        out.push(Frame { pc, cfa: None, regs });
                        break;
                    };
                    let Some(sp) = regs[13] else {
                        out.push(Frame { pc, cfa: None, regs });
                        break;
                    };
                    cfa = Some(sp.wrapping_add(down));
                    for n in 4..=11 {
                        next[n] = regs[n];
                    }
                    next[14] = regs[14];
                    for (reg, below) in saved {
                        next[reg] = read(sp.wrapping_add(down).wrapping_sub(below));
                    }
                }
            }
            out.push(Frame { pc, cfa, regs });
            let Some(at) = cfa else { break };
            next[13] = Some(at);
            let Some(resume) = next[14] else { break };
            if resume == 0 || resume >= 0xF000_0000 {
                break;
            }
            next[15] = Some(resume & !1);
            if next[15] == regs[15] && next[13] == regs[13] {
                break;
            }
            regs = next;
        }
        out
    }

    /// What a function with no call frame information did to the stack on
    /// its way to `pc`, read from its first instructions: how far it moved
    /// the stack pointer down, and for each register it pushed, how far
    /// below where the stack pointer began it put it. Only pushes and
    /// immediate subtractions from `sp` are understood: enough for the
    /// stubs, which push the registers a system call takes and do nothing
    /// else to the stack before the `svc`.
    fn prologue(&self, pc: u32, read: &dyn Fn(u32) -> Option<u32>) -> Option<(u32, Vec<(usize, u32)>)> {
        let n = self.functions.partition_point(|&(base, _)| base <= pc);
        let &(start, size) = self.functions.get(n.checked_sub(1)?)?;
        if pc >= start + size {
            return None;
        }
        let halfword = |at: u32| read(at & !3).map(|w| if at & 2 == 0 { w & 0xFFFF } else { w >> 16 });
        let (mut at, mut down, mut saved) = (start, 0u32, Vec::new());
        let mut push = |regs: Vec<usize>, down: &mut u32| {
            *down += 4 * regs.len() as u32;
            // The lowest-numbered register goes lowest.
            for (k, reg) in regs.into_iter().enumerate() {
                saved.push((reg, *down - 4 * k as u32));
            }
        };
        while at < pc && at < start + 64 {
            let hw = halfword(at)?;
            if hw & 0xFE00 == 0xB400 {
                // PUSH {r0-r7, lr}
                let mut regs: Vec<usize> = (0..8).filter(|r| hw & (1 << r) != 0).collect();
                if hw & 0x100 != 0 {
                    regs.push(14);
                }
                push(regs, &mut down);
                at += 2;
            } else if hw == 0xE92D {
                // STMDB sp!, {…}
                let list = halfword(at + 2)?;
                push((0..15).filter(|r| list & (1 << r) != 0).collect(), &mut down);
                at += 4;
            } else if hw & 0xFF80 == 0xB080 {
                // SUB sp, #imm
                down += (hw & 0x7F) * 4;
                at += 2;
            } else {
                // Anything else is skipped: 32-bit encodings begin 0b11101,
                // 0b11110 or 0b11111.
                at += if hw >> 11 >= 0b11101 { 4 } else { 2 };
            }
        }
        Some((down, saved))
    }

    /// The variables of the function `frame` is in, and of every function
    /// inlined into it there, that are in scope at its instruction. `outer`
    /// for a frame that is a caller, stopped after its call.
    pub fn variables(&self, frame: &Frame, outer: bool, read: &dyn Fn(u32) -> Option<u8>) -> Vec<Variable> {
        let probe = u64::from(if outer { frame.pc.wrapping_sub(1) } else { frame.pc });
        let Some(&(_, _, u)) = self.ranges.iter().find(|&&(begin, end, _)| begin <= probe && probe < end) else { return Vec::new() };
        let unit = &self.units[u];
        let mut out = Vec::new();
        // The scopes the instruction is in, innermost last: each one's
        // depth, the function it is of, and a subprogram's frame base.
        let mut scopes: Vec<(isize, String, Option<gimli::Expression<R>>)> = Vec::new();
        let mut skip_below: Option<isize> = None;
        let mut cursor = unit.entries();
        while let Ok(Some(entry)) = cursor.next_dfs() {
            let depth = entry.depth();
            if let Some(d) = skip_below {
                if depth > d {
                    continue;
                }
                skip_below = None;
            }
            while scopes.last().is_some_and(|(d, _, _)| *d >= depth) {
                scopes.pop();
            }
            match entry.tag() {
                DW_TAG_subprogram | DW_TAG_inlined_subroutine | DW_TAG_lexical_block => {
                    if !self.contains(unit, entry, probe) {
                        skip_below = Some(depth);
                        continue;
                    }
                    let function = match entry.tag() {
                        DW_TAG_lexical_block => scopes.last().map(|s| s.1.clone()).unwrap_or_default(),
                        _ => self.name_of(u, entry).unwrap_or_default(),
                    };
                    let base = match entry.attr_value(DW_AT_frame_base) {
                        Some(AttributeValue::Exprloc(e)) => Some(e),
                        _ => scopes.last().and_then(|s| s.2.clone()),
                    };
                    scopes.push((depth, function, base));
                }
                DW_TAG_variable | DW_TAG_formal_parameter if !scopes.is_empty() => {
                    let (_, function, base) = scopes.last().cloned().unwrap();
                    let Some(name) = self.name_of(u, entry) else { continue };
                    let Some(ty) = self.type_of(u, entry) else { continue };
                    let size = self.size(ty).min(VARIABLE_BYTES);
                    let (address, bytes) = match self.location(u, entry, probe) {
                        Some(expr) => self.place(u, expr, frame, base, size, read),
                        None => (None, vec![None; size as usize]),
                    };
                    out.push(Variable { name, ty, parameter: entry.tag() == DW_TAG_formal_parameter, function, address, bytes });
                }
                _ => {}
            }
        }
        out
    }

    fn contains(&self, unit: &gimli::Unit<R>, entry: &gimli::DebuggingInformationEntry<R>, probe: u64) -> bool {
        let Ok(mut ranges) = self.dwarf.die_ranges(unit, entry) else { return false };
        while let Ok(Some(range)) = ranges.next() {
            if range.begin <= probe && probe < range.end {
                return true;
            }
        }
        false
    }

    /// A variable's location expression at `probe`: its only one, or the
    /// one of its location list whose range has the instruction.
    fn location(&self, u: usize, entry: &gimli::DebuggingInformationEntry<R>, probe: u64) -> Option<gimli::Expression<R>> {
        let value = entry.attr_value(DW_AT_location)?;
        if let AttributeValue::Exprloc(e) = value {
            return Some(e);
        }
        let mut list = self.dwarf.attr_locations(&self.units[u], value).ok()??;
        while let Ok(Some(item)) = list.next() {
            if item.range.begin <= probe && probe < item.range.end {
                return Some(item.data);
            }
        }
        None
    }

    /// Runs a location expression for `frame`: its pieces, or `None` when
    /// it needs what is not known, a register unwinding did not recover.
    fn evaluate(
        &self,
        u: usize,
        expr: gimli::Expression<R>,
        frame: &Frame,
        base: Option<u64>,
        read: &dyn Fn(u32) -> Option<u8>,
    ) -> Option<Vec<gimli::Piece<R>>> {
        use gimli::{EvaluationResult as E, Value};
        let mut eval = expr.evaluation(self.units[u].encoding());
        let mut step = eval.evaluate().ok()?;
        for _ in 0..256 {
            step = match step {
                E::Complete => return Some(eval.result()),
                E::RequiresMemory { address, size, .. } => {
                    let mut value = 0u64;
                    for i in 0..u32::from(size.min(8)) {
                        value |= u64::from(read((address as u32).wrapping_add(i))?) << (8 * i);
                    }
                    eval.resume_with_memory(Value::Generic(value)).ok()?
                }
                E::RequiresRegister { register, .. } => {
                    let value = frame.regs.get(register.0 as usize).copied().flatten()?;
                    eval.resume_with_register(Value::Generic(u64::from(value))).ok()?
                }
                E::RequiresFrameBase => eval.resume_with_frame_base(base?).ok()?,
                E::RequiresCallFrameCfa => eval.resume_with_call_frame_cfa(u64::from(frame.cfa?)).ok()?,
                E::RequiresRelocatedAddress(address) => eval.resume_with_relocated_address(address).ok()?,
                _ => return None,
            };
        }
        None
    }

    /// Where a variable is, and its bytes.
    fn place(
        &self,
        u: usize,
        expr: gimli::Expression<R>,
        frame: &Frame,
        base: Option<gimli::Expression<R>>,
        size: u32,
        read: &dyn Fn(u32) -> Option<u8>,
    ) -> (Option<u32>, Vec<Option<u8>>) {
        let unknown = || (None, vec![None; size as usize]);
        // The frame base: a register's value, almost always.
        let base = base.and_then(|b| self.evaluate(u, b, frame, None, read)).and_then(|pieces| match pieces.first()?.location {
            gimli::Location::Register { register } => frame.regs.get(register.0 as usize).copied().flatten().map(u64::from),
            gimli::Location::Address { address } => Some(address),
            _ => None,
        });
        let Some(pieces) = self.evaluate(u, expr, frame, base, read) else { return unknown() };
        let memory = |at: u32, n: u32| (0..n).map(|i| read(at.wrapping_add(i))).collect::<Vec<_>>();
        if let [gimli::Piece { size_in_bits: None, location: gimli::Location::Address { address }, .. }] = pieces.as_slice() {
            return (Some(*address as u32), memory(*address as u32, size));
        }
        let mut bytes = Vec::new();
        for piece in &pieces {
            let n = piece.size_in_bits.map_or(size.saturating_sub(bytes.len() as u32), |bits| (bits / 8) as u32);
            match &piece.location {
                gimli::Location::Address { address } => bytes.extend(memory(*address as u32, n)),
                gimli::Location::Register { register } => {
                    let value = frame.regs.get(register.0 as usize).copied().flatten();
                    bytes.extend((0..n).map(|i| value.filter(|_| i < 4).map(|v| (v >> (8 * i)) as u8)));
                }
                gimli::Location::Value { value } => {
                    let v = value.to_u64(u64::MAX).unwrap_or(0);
                    bytes.extend((0..n).map(|i| (i < 8).then(|| (v >> (8 * i)) as u8)));
                }
                gimli::Location::Bytes { value } => {
                    let b = value.to_slice().map(|s| s.into_owned()).unwrap_or_default();
                    bytes.extend((0..n as usize).map(|i| b.get(i).copied()));
                }
                _ => bytes.extend((0..n).map(|_| None)),
            }
        }
        bytes.resize(size as usize, None);
        (None, bytes)
    }

    /// Every static: a variable whose location is an address.
    fn read_statics(&self) -> Vec<Static> {
        let mut out: Vec<Static> = Vec::new();
        for (u, unit) in self.units.iter().enumerate() {
            let mut path: Vec<(isize, String)> = Vec::new();
            let mut cursor = unit.entries();
            while let Ok(Some(entry)) = cursor.next_dfs() {
                let depth = entry.depth();
                while path.last().is_some_and(|(d, _)| *d >= depth) {
                    path.pop();
                }
                match entry.tag() {
                    DW_TAG_namespace | DW_TAG_subprogram => {
                        if let Some(name) = self.name_of(u, entry) {
                            path.push((depth, name));
                        }
                    }
                    DW_TAG_variable => {
                        let Some(AttributeValue::Exprloc(expr)) = entry.attr_value(DW_AT_location) else { continue };
                        let Ok(code) = expr.0.to_slice() else { continue };
                        let Some(address) = static_address(&code) else { continue };
                        // What the linker threw away is left at 0.
                        if address == 0 {
                            continue;
                        }
                        let (Some(name), Some(ty)) = (self.name_of(u, entry), self.type_of(u, entry)) else { continue };
                        let full = path.iter().map(|(_, p)| p.as_str()).chain([name.as_str()]).collect::<Vec<_>>().join("::");
                        out.push(Static { name: full, address, size: self.size(ty), ty });
                    }
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| (a.address, std::cmp::Reverse(a.size), &a.name).cmp(&(b.address, std::cmp::Reverse(b.size), &b.name)));
        out.dedup_by(|a, b| a.address == b.address && a.name == b.name);
        out
    }

    /// The unit and offset an attribute refers to.
    fn reference(&self, u: usize, value: AttributeValue<R>) -> Option<(usize, UnitOffset)> {
        match value {
            AttributeValue::UnitRef(offset) => Some((u, offset)),
            AttributeValue::DebugInfoRef(offset) => {
                let n = self.units.partition_point(|unit| unit.header.offset().0 <= offset.0);
                let n = n.checked_sub(1)?;
                Some((n, offset.to_unit_offset(&self.units[n].header)?))
            }
            _ => None,
        }
    }

    fn string(&self, u: usize, value: AttributeValue<R>) -> Option<String> {
        Some(self.dwarf.attr_string(&self.units[u], value).ok()?.to_string_lossy().ok()?.into_owned())
    }

    /// An entry's name, or its abstract origin's or specification's: an
    /// inlined function's or variable's is its origin's.
    fn name_of(&self, u: usize, entry: &gimli::DebuggingInformationEntry<R>) -> Option<String> {
        if let Some(name) = entry.attr_value(DW_AT_name).and_then(|v| self.string(u, v)) {
            return Some(name);
        }
        let origin = entry.attr_value(DW_AT_abstract_origin).or_else(|| entry.attr_value(DW_AT_specification))?;
        let (u, offset) = self.reference(u, origin)?;
        let entry = self.units[u].entry(offset).ok()?;
        self.name_of(u, &entry)
    }

    /// An entry's type, numbered, or its abstract origin's.
    fn type_of(&self, u: usize, entry: &gimli::DebuggingInformationEntry<R>) -> Option<TypeId> {
        if let Some((tu, offset)) = entry.attr_value(DW_AT_type).and_then(|v| self.reference(u, v)) {
            return Some(self.type_at(tu, offset));
        }
        let origin = entry.attr_value(DW_AT_abstract_origin).or_else(|| entry.attr_value(DW_AT_specification))?;
        let (u, offset) = self.reference(u, origin)?;
        let entry = self.units[u].entry(offset).ok()?;
        self.type_of(u, &entry)
    }

    /// The number of the type at `offset` of unit `u`, read now if it has
    /// not been. A type that refers to itself, through a pointer, finds
    /// its own number already given.
    fn type_at(&self, u: usize, offset: UnitOffset) -> TypeId {
        if let Some(&id) = self.types.borrow().by_die.get(&(u, offset.0)) {
            return id;
        }
        let id = self.add(Type::Opaque { name: String::new(), size: 0 });
        self.types.borrow_mut().by_die.insert((u, offset.0), id);
        let ty = self.read_type(u, offset).unwrap_or(Type::Opaque { name: "?".into(), size: 0 });
        self.types.borrow_mut().all[id as usize] = ty;
        id
    }

    fn add(&self, ty: Type) -> TypeId {
        let mut types = self.types.borrow_mut();
        types.all.push(ty);
        (types.all.len() - 1) as TypeId
    }

    fn read_type(&self, u: usize, offset: UnitOffset) -> Option<Type> {
        let unit = &self.units[u];
        let mut tree = unit.entries_tree(Some(offset)).ok()?;
        let root = tree.root().ok()?;
        let entry = root.entry().clone();
        let name = self.name_of(u, &entry).unwrap_or_default();
        let size = entry.attr_value(DW_AT_byte_size).and_then(|v| v.udata_value()).unwrap_or(0) as u32;
        let target = entry.attr_value(DW_AT_type).and_then(|v| self.reference(u, v)).map(|(tu, o)| self.type_at(tu, o));
        let declared = matches!(entry.attr_value(DW_AT_declaration), Some(AttributeValue::Flag(true)));
        let mut children = Vec::new();
        let mut iter = root.children();
        while let Ok(Some(child)) = iter.next() {
            children.push(child.entry().clone());
        }
        Some(match entry.tag() {
            DW_TAG_base_type => {
                let encoding = match entry.attr_value(DW_AT_encoding) {
                    Some(AttributeValue::Encoding(DW_ATE_boolean)) => Encoding::Bool,
                    Some(AttributeValue::Encoding(DW_ATE_signed | DW_ATE_signed_char)) => Encoding::Signed,
                    Some(AttributeValue::Encoding(DW_ATE_float)) => Encoding::Float,
                    Some(AttributeValue::Encoding(DW_ATE_UTF)) => Encoding::Char,
                    _ => Encoding::Unsigned,
                };
                Type::Base { name, size, encoding }
            }
            DW_TAG_pointer_type | DW_TAG_reference_type | DW_TAG_rvalue_reference_type => Type::Pointer { name, target },
            DW_TAG_typedef | DW_TAG_const_type | DW_TAG_volatile_type | DW_TAG_atomic_type | DW_TAG_restrict_type => match target {
                Some(target) => Type::Alias { name, target },
                None => Type::Opaque { name: if name.is_empty() { "()".into() } else { name }, size: 0 },
            },
            DW_TAG_enumeration_type => {
                let signed = target.and_then(|t| self.ty(self.resolve(t))).is_some_and(|t| matches!(t, Type::Base { encoding: Encoding::Signed, .. }));
                let values = children
                    .iter()
                    .filter(|c| c.tag() == DW_TAG_enumerator)
                    .filter_map(|c| {
                        let value = match c.attr_value(DW_AT_const_value)? {
                            AttributeValue::Sdata(v) => v,
                            other => other.udata_value()? as i64,
                        };
                        Some((self.name_of(u, c)?, value))
                    })
                    .collect();
                Type::Enum { name, size, signed, values }
            }
            DW_TAG_array_type => {
                let element = target?;
                let stride = entry.attr_value(DW_AT_byte_stride).and_then(|v| v.udata_value()).map(|s| s as u32);
                let counts: Vec<u32> = children
                    .iter()
                    .filter(|c| c.tag() == DW_TAG_subrange_type)
                    .map(|c| {
                        if let Some(count) = c.attr_value(DW_AT_count).and_then(|v| v.udata_value()) {
                            return count as u32;
                        }
                        let lower = c.attr_value(DW_AT_lower_bound).and_then(|v| v.udata_value()).unwrap_or(0);
                        c.attr_value(DW_AT_upper_bound).and_then(|v| v.udata_value()).map_or(0, |upper| (upper + 1).saturating_sub(lower) as u32)
                    })
                    .collect();
                // `[[T; 3]; 2]` is one array of two subranges: the inner
                // dimensions become arrays of their own.
                let counts = if counts.is_empty() { vec![0] } else { counts };
                let (mut inner, mut stride) = (element, stride.unwrap_or_else(|| self.size(element)));
                for &count in counts[1..].iter().rev() {
                    inner = self.add(Type::Array { name: self.array_name(inner, count), element: inner, count, stride });
                    stride = count.saturating_mul(stride);
                }
                // Rust's arrays are nameless in DWARF: named here as written.
                let name = if name.is_empty() { self.array_name(inner, counts[0]) } else { name };
                Type::Array { name, element: inner, count: counts[0], stride }
            }
            DW_TAG_structure_type | DW_TAG_class_type | DW_TAG_union_type if !declared => {
                let union = entry.tag() == DW_TAG_union_type;
                let mut fields = Vec::new();
                let mut variants = None;
                for child in &children {
                    match child.tag() {
                        DW_TAG_member => {
                            if let Some(field) = self.field_of(u, child, union) {
                                fields.push(field);
                            }
                        }
                        DW_TAG_variant_part => variants = self.variants_of(u, child.offset()),
                        _ => {}
                    }
                }
                Type::Struct { name, size, fields, variants, union }
            }
            DW_TAG_subroutine_type => Type::Opaque { name: if name.is_empty() { "fn".into() } else { name }, size: 0 },
            _ => Type::Opaque { name, size },
        })
    }

    fn array_name(&self, element: TypeId, count: u32) -> String {
        let inner = self.ty(element).map(|t| t.name().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "?".into());
        format!("[{inner}; {count}]")
    }

    /// A member of a struct: its name, offset and type; `None` for one
    /// with no place in the struct's bytes.
    fn field_of(&self, u: usize, member: &gimli::DebuggingInformationEntry<R>, union: bool) -> Option<Field> {
        let offset = match member.attr_value(DW_AT_data_member_location) {
            Some(value) => value.udata_value()? as u32,
            None if union => 0,
            None => return None,
        };
        Some(Field { name: self.name_of(u, member).unwrap_or_default(), offset, ty: self.type_of(u, member)? })
    }

    /// A Rust enum's variant part: the member that is its discriminant,
    /// and each variant with the value that says it is that one.
    fn variants_of(&self, u: usize, offset: UnitOffset) -> Option<Variants> {
        let unit = &self.units[u];
        let mut tree = unit.entries_tree(Some(offset)).ok()?;
        let root = tree.root().ok()?;
        let discr = match root.entry().attr_value(DW_AT_discr) {
            Some(AttributeValue::UnitRef(o)) => Some(o),
            _ => None,
        };
        let mut out = Variants { discriminant: None, variants: Vec::new() };
        let mut children = root.children();
        while let Ok(Some(child)) = children.next() {
            let entry = child.entry().clone();
            match entry.tag() {
                DW_TAG_member if Some(entry.offset()) == discr => out.discriminant = self.field_of(u, &entry, false),
                DW_TAG_variant => {
                    let value = match entry.attr_value(DW_AT_discr_value) {
                        Some(AttributeValue::Sdata(v)) => Some(v as u64),
                        Some(other) => other.udata_value(),
                        None => None,
                    };
                    let mut members = child.children();
                    while let Ok(Some(member)) = members.next() {
                        let member = member.entry();
                        if member.tag() == DW_TAG_member
                            && let Some(field) = self.field_of(u, member, false)
                        {
                            out.variants.push(Variant { value, name: field.name, offset: field.offset, ty: field.ty });
                        }
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }
}

/// A static's address from its location expression: `DW_OP_addr`, and a
/// `DW_OP_plus_uconst` after it for one the compiler placed inside another
/// allocation; `None` for anything else, which is no static's.
fn static_address(code: &[u8]) -> Option<u32> {
    let [0x03, a, b, c, d, rest @ ..] = code else { return None };
    let address = u32::from_le_bytes([*a, *b, *c, *d]);
    match rest {
        [] => Some(address),
        [0x23, uleb @ ..] => {
            let mut value = 0u32;
            for (n, byte) in uleb.iter().enumerate().take(5) {
                value |= u32::from(byte & 0x7F) << (7 * n);
                if byte & 0x80 == 0 {
                    return (n + 1 == uleb.len()).then(|| address.wrapping_add(value));
                }
            }
            None
        }
        _ => None,
    }
}

/// `name` in `dir`, unless it is a whole path itself.
fn join(dir: &str, name: &str) -> String {
    if name.starts_with('/') || dir.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        dir.to_string()
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}
