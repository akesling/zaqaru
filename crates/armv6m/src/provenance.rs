//! Where a value came from: a tag on every register and every byte of
//! memory saying what put it there, kept while someone wants to know and
//! not otherwise.
//!
//! A tag is a node of a history ([`Node`]): a read of a peripheral's data
//! register — a byte off a bus — is an origin, and every store of a tagged
//! value is a node of its own, pointing at the tag of what was stored. So
//! the tag on a byte of memory is the last store to it, and from there the
//! chain runs back through each copy to where the value came in from the
//! outside: `hiffy's buffer ← stored by the kernel's IPC copy ← loaded from
//! i2c_driver's buffer ← stored by i2c_driver ← I2C2's RXDR at 17.501839 s`.
//!
//! Tags follow values, not instructions: each instruction notes which
//! registers it read ([`crate::state::Cpu::reg`]) and what it loaded, and
//! when it is done each register it wrote takes the tag of the load that
//! produced its value — matched by value, which is what keeps LDM, POP and
//! a writeback's base apart — or else the union of the tags of the
//! registers it read: arithmetic on a tagged value is still that value's.
//! A store tags the bytes it writes with the tag of the register it stored,
//! found the same way. The hardware's own pushes and pops — an exception's
//! frame — carry the tags of the registers they save.
//!
//! Following everything ([`Provenance::everything`]), nothing is untagged:
//! what was in memory before following began — RAM, flash, a NOR, a
//! peripheral's register — is an origin the first time an instruction
//! loads it ([`What::Held`]), with the last write to it if it is RAM, which
//! is where to follow it on from in an earlier window; what each register
//! held is an origin of its own ([`What::Register`]); and a store of a
//! value made of nothing — an immediate — is a node with no parent. So a
//! value's history always ends somewhere that says what to do next.
//!
//! What this does not follow: control flow (a branch on a tagged value
//! leaves what it decides untagged), the FPU's registers, and DMA, whose
//! writes land untagged. Kept off, it costs a flag test per instruction.

use std::collections::HashMap;

use crate::bus::Mark;

/// A node of a value's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub what: What,
    /// The instruction that did it.
    pub pc: u32,
    /// When, in picoseconds of the board's time.
    pub time: u64,
    /// What it came from: none for an origin; the stored value's tag for a
    /// store; two tags for a merge.
    pub parents: [Tag; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// A read of a peripheral's data register — `I2C2 RXDR` — and what
    /// it gave.
    Read { addr: u32, value: u32 },
    /// A store of a tagged value to memory.
    Store { addr: u32, size: u32, value: u32 },
    /// A value made of two tagged ones.
    Merge,
    /// Bytes loaded together that came from different places: the tag of
    /// each, by its place in the value — what a word copy of bytes stored
    /// one by one carries, and what a byte's story follows its own lane of.
    Bytes { addr: u32, lanes: [Tag; 4] },
    /// Following everything: `size` bytes at `addr` that held `value`
    /// before following began, the first time one was loaded; and the last
    /// write to them then, if they are RAM.
    Held { addr: u32, size: u32, value: u32, written: Option<Mark> },
    /// Following everything: what register `n` held as following began.
    Register { n: u8, value: u32 },
}

/// A node's number, from 1; 0 is untagged.
pub type Tag = u32;

#[derive(Clone, Debug, Default)]
pub struct Provenance {
    /// The time it began: what came in before is untagged, unless
    /// following `everything`.
    pub since: u64,
    /// Whether everything is followed, not only what came in off a bus.
    pub everything: bool,
    pub regs: [Tag; 16],
    /// By byte address; an address not here is untagged.
    pub memory: HashMap<u32, Tag>,
    pub nodes: Vec<Node>,
    /// This instruction's loads: what each gave, its size, and its tag.
    loads: Vec<(u32, u32, Tag)>,
}

fn mask(size: u32) -> u32 {
    if size >= 4 { u32::MAX } else { (1 << (8 * size)) - 1 }
}

impl Provenance {
    pub fn new(since: u64) -> Provenance {
        Provenance { since, ..Provenance::default() }
    }

    /// Following everything from `since`, the registers holding `regs`.
    pub fn everything(since: u64, regs: &[u32; 16]) -> Provenance {
        let mut p = Provenance { since, everything: true, ..Provenance::default() };
        for (n, &value) in regs.iter().enumerate() {
            p.regs[n] = p.add(Node { what: What::Register { n: n as u8, value }, pc: regs[15], time: since, parents: [0, 0] });
        }
        p
    }

    pub fn node(&self, tag: Tag) -> Option<&Node> {
        self.nodes.get((tag as usize).checked_sub(1)?)
    }

    fn add(&mut self, node: Node) -> Tag {
        self.nodes.push(node);
        self.nodes.len() as Tag
    }

    /// One tag for several: the one, if they are all the same or untagged;
    /// a merge of them, if not.
    fn union(&mut self, tags: impl IntoIterator<Item = Tag>, pc: u32, time: u64) -> Tag {
        let mut out = 0;
        for tag in tags {
            if tag == 0 || tag == out {
                continue;
            }
            out = if out == 0 { tag } else { self.add(Node { what: What::Merge, pc, time, parents: [out, tag] }) };
        }
        out
    }

    /// The tag of the bytes `[addr, addr + size)`: theirs, if they share
    /// one; or what each was, lane by lane.
    pub fn of_memory(&mut self, addr: u32, size: u32, pc: u32, time: u64) -> Tag {
        let tags: Vec<Tag> = (0..size).map(|i| self.memory.get(&addr.wrapping_add(i)).copied().unwrap_or(0)).collect();
        if tags.iter().all(|&t| t == tags[0]) {
            return tags[0];
        }
        if size > 4 {
            return self.union(tags, pc, time);
        }
        let mut lanes = [0; 4];
        lanes[..tags.len()].copy_from_slice(&tags);
        let first = tags.iter().copied().find(|&t| t != 0).unwrap_or(0);
        self.add(Node { what: What::Bytes { addr, lanes }, pc, time, parents: [first, 0] })
    }

    /// Following everything, bytes loaded untagged are what they held
    /// before following began: an origin, kept on them.
    pub(crate) fn hold(&mut self, addr: u32, size: u32, value: u32, written: Option<Mark>, pc: u32, time: u64) {
        if !self.everything || (0..size).all(|i| self.memory.contains_key(&addr.wrapping_add(i))) {
            return;
        }
        let tag = self.add(Node { what: What::Held { addr, size, value: value & mask(size), written }, pc, time, parents: [0, 0] });
        for i in 0..size {
            self.memory.entry(addr.wrapping_add(i)).or_insert(tag);
        }
    }

    /// A new instruction.
    pub(crate) fn begin(&mut self) {
        self.loads.clear();
    }

    /// A load the instruction made: `data_register` when it read a
    /// peripheral's data register, which is an origin; `written`, the last
    /// write to what it read, if RAM.
    pub(crate) fn loaded(&mut self, addr: u32, size: u32, value: u32, data_register: bool, written: Option<Mark>, pc: u32, time: u64) {
        let tag = if data_register {
            self.add(Node { what: What::Read { addr, value }, pc, time, parents: [0, 0] })
        } else {
            self.hold(addr, size, value, written, pc, time);
            self.of_memory(addr, size, pc, time)
        };
        self.loads.push((value & mask(size), size, tag));
    }

    /// A store the instruction made, of the value the register `source`
    /// held — `None` if no register it read held it — with the tags of
    /// the registers it read in `read` as the fallback.
    pub(crate) fn stored(&mut self, addr: u32, size: u32, value: u32, source: Option<usize>, read: u16, pc: u32, time: u64) {
        let tag = match source {
            Some(n) => self.regs[n],
            None => {
                let tags: Vec<Tag> = (0..16).filter(|n| read & 1 << n != 0).map(|n| self.regs[n]).collect();
                self.union(tags, pc, time)
            }
        };
        // Following everything, a value made of nothing is still a store:
        // the code's own, a constant.
        let tag = if tag == 0 && !self.everything {
            0
        } else {
            self.add(Node { what: What::Store { addr, size, value: value & mask(size) }, pc, time, parents: [tag, 0] })
        };
        for i in 0..size {
            let byte = addr.wrapping_add(i);
            if tag == 0 {
                self.memory.remove(&byte);
            } else {
                self.memory.insert(byte, tag);
            }
        }
    }

    /// The instruction is done: each register it wrote, `written`, now
    /// holding `values[n]`, takes the tag of the load that gave its value,
    /// or the union of the registers it read.
    pub(crate) fn settle(&mut self, written: u16, read: u16, values: &[u32; 16], pc: u32, time: u64) {
        if written == 0 {
            return;
        }
        let from_registers: Vec<Tag> = (0..16).filter(|n| read & 1 << n != 0).map(|n| self.regs[n]).collect();
        let mut computed = None;
        for n in 0..16 {
            if written & 1 << n == 0 {
                continue;
            }
            let value = values[n];
            let loaded = self.loads.iter().position(|&(v, size, _)| value & mask(size) == v);
            self.regs[n] = match loaded {
                Some(i) => self.loads.remove(i).2,
                None => *computed.get_or_insert_with(|| self.union(from_registers.iter().copied(), pc, time)),
            };
        }
    }

    /// The history of the byte at `address`, newest first, each node with
    /// the byte it is about: through stores and loads of words, the lane
    /// the byte is in; through a merge — arithmetic — the first parent's,
    /// the byte no longer told apart. At most `depth` nodes.
    pub fn story(&self, address: u32, depth: usize) -> Vec<(Tag, &Node, u32)> {
        let mut out = Vec::new();
        let mut tag = self.memory.get(&address).copied().unwrap_or(0);
        // The byte followed, and its place in the value the next node holds.
        let (mut byte, mut lane) = (address, 0);
        while let Some(node) = self.node(tag) {
            let mut next = node.parents[0];
            match node.what {
                What::Store { addr, size, .. } => {
                    lane = byte.wrapping_sub(addr).min(size.saturating_sub(1));
                    out.push((tag, node, byte));
                }
                What::Bytes { addr, lanes } => {
                    byte = addr.wrapping_add(lane);
                    next = lanes[lane.min(3) as usize];
                    lane = 0;
                }
                What::Held { addr, size, .. } => out.push((tag, node, addr.wrapping_add(lane.min(size.saturating_sub(1))))),
                What::Merge => {
                    lane = 0;
                    out.push((tag, node, byte));
                }
                _ => out.push((tag, node, byte)),
            }
            if out.len() >= depth {
                break;
            }
            tag = next;
        }
        out
    }

    /// A value's history, newest first: the node `tag`, then what it came
    /// from — along each merge's first parent, the second noted — at most
    /// `depth` nodes.
    pub fn history(&self, tag: Tag, depth: usize) -> Vec<(Tag, &Node)> {
        let mut out = Vec::new();
        let mut at = tag;
        while let Some(node) = self.node(at) {
            out.push((at, node));
            if out.len() >= depth {
                break;
            }
            at = node.parents[0];
        }
        out
    }
}
