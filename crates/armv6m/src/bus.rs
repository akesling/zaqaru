//! The physical address space: what answers at each address.
//!
//! No permission is checked here. Whether an access is *allowed* is the
//! MPU's question, and the machine asks it before it gets this far; the
//! bus answers only whether anything is there, and what it says.
//!
//! The G0's map, as much of it as the model has — sized here for the
//! G031; how much flash and RAM a board has is its [`Part`]'s:
//!
//! | range                       | what                                  |
//! |-----------------------------|---------------------------------------|
//! | `0x0000_0000..0x0001_0000`  | main flash, aliased (boot from flash) |
//! | `0x0800_0000..0x0801_0000`  | main flash, 64 KiB                    |
//! | `0x1FFF_0000..0x2000_0000`  | system memory and option bytes        |
//! | `0x2000_0000..0x2000_2000`  | SRAM, 8 KiB                           |
//! | `0x4000_0000..0x5000_1800`  | peripherals; see [`crate::stm32g0`]   |
//! | `0xE000_0000..0xE010_0000`  | the PPB; see [`crate::scs`]           |
//!
//! And the H743/753's:
//!
//! | range                       | what                                  |
//! |-----------------------------|---------------------------------------|
//! | `0x0000_0000..0x0001_0000`  | ITCM, 64 KiB                          |
//! | `0x0800_0000..0x0820_0000`  | main flash, two banks of 1 MiB        |
//! | `0x1FF0_0000..0x1FF2_0000`  | system memory: the UID, FLASH_SIZE    |
//! | `0x2000_0000..0x2002_0000`  | DTCM, 128 KiB                         |
//! | `0x2400_0000..0x2408_0000`  | AXI SRAM, 512 KiB                     |
//! | `0x3000_0000..0x3004_8000`  | SRAM1, 2 and 3, 288 KiB               |
//! | `0x3800_0000..0x3801_0000`  | SRAM4, 64 KiB                         |
//! | `0x3880_0000..0x3880_1000`  | backup SRAM, 4 KiB                    |
//! | `0x4000_0000..0x5C00_2000`  | peripherals; see [`crate::stm32h7`]   |
//! | `0xE000_0000..0xE010_0000`  | the PPB; see [`crate::scs`]           |

use crate::flash_h7;
use crate::gpio::Edge;
use crate::part::{Family, Part};
use crate::scs::Scs;
use crate::stm32g0::Stm32g0;
use crate::stm32h7::Stm32h7;
use crate::usart;

pub const FLASH_BASE: u32 = 0x0800_0000;
/// The G031's, which the tests are written against; a board's own are
/// its [`Part`]'s.
pub const FLASH_SIZE: usize = 64 * 1024;
pub const SRAM_BASE: u32 = 0x2000_0000;
pub const SRAM_SIZE: usize = 8 * 1024;
pub const AXI_SRAM_BASE: u32 = 0x2400_0000;
/// FLASHSIZE, in a G0's system memory: main flash in KiB, which a flash
/// algorithm reads to know the part.
const G0_FLASHSIZE: usize = 0x75E0;
/// An H7's UID and FLASH_SIZE, in its system memory.
const H7_UID: usize = 0x1_E800;
const H7_FLASHSIZE: usize = 0x1_E880;

/// Nothing answered: a bus fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmapped;

/// A RAM, at its address, with when each of its words was last written
/// and by what — what a memory view shows as heat, and a stack's deepest
/// reach.
#[derive(Clone, Debug)]
pub struct Ram {
    pub base: u32,
    pub bytes: Vec<u8>,
    /// When each word was last written, and by what; written through
    /// [`Bus::write`] and the DMA, which keep it, and not otherwise —
    /// what changes `bytes` directly, as a test setting up memory does,
    /// leaves no mark.
    pub marks: Vec<Mark>,
}

/// When a word of RAM was last written, and what wrote it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    /// The board's time in microseconds, plus one ([`write_stamp`]); 0
    /// for never.
    pub stamp: u64,
    /// The address of the instruction that stored to it, or one of
    /// [`BY_DMA`], [`BY_PROBE`], [`BY_EXCEPTION`].
    pub writer: u32,
}

/// A word written by a peripheral's DMA.
pub const BY_DMA: u32 = 0xFFFF_FFFF;
/// A word written by a debugger, through the debug port.
pub const BY_PROBE: u32 = 0xFFFF_FFFE;
/// A word written by the core itself, stacking an exception's frame.
pub const BY_EXCEPTION: u32 = 0xFFFF_FFFD;

/// The board's time, `now` picoseconds, as a [`Mark`] keeps it.
pub fn write_stamp(now: u64) -> u64 {
    now / 1_000_000 + 1
}

impl Ram {
    pub fn new(base: u32, size: usize) -> Ram {
        let words = size.div_ceil(4);
        Ram { base, bytes: vec![0; size], marks: vec![Mark::default(); words] }
    }

    /// The `len` bytes at `offset` written at `stamp` by `writer`.
    fn mark(&mut self, offset: usize, len: usize, stamp: u64, writer: u32) {
        if len == 0 {
            return;
        }
        for word in offset / 4..=(offset + len - 1) / 4 {
            self.marks[word] = Mark { stamp, writer };
        }
    }
}

/// Every write to a range of RAM, and how often each instruction wrote each
/// word anywhere, kept while someone asks: what a variable's history and an
/// audit of who writes what are made of. Off (the bus's `watch` is `None`)
/// it costs a test per write to RAM. Observation only, as the marks are.
#[derive(Clone, Debug, Default)]
pub struct Watch {
    /// The range whose writes are logged, `[base, base + length)`.
    pub base: u32,
    pub length: u32,
    /// Oldest first, at most [`LOGGED`].
    pub log: Vec<Written>,
    /// Writes to the range past the most `log` keeps.
    pub dropped: u64,
    /// When auditing: for each writer, the caller it was called from (lr,
    /// for what a helper such as `memcpy` wrote on its caller's behalf),
    /// and word, how many times it wrote there; at most [`SITES`] of them.
    pub sites: Option<std::collections::HashMap<(u32, u32, u32), u32>>,
}

/// A write to RAM, as a [`Watch`] logs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// The board's time, in picoseconds.
    pub time: u64,
    /// As a [`Mark`]'s.
    pub writer: u32,
    /// lr when the writer stored: who called the function storing.
    pub caller: u32,
    pub address: u32,
    pub size: u32,
    /// What was stored, for a store of up to a word; 0 for the DMA's.
    pub value: u32,
}

/// The most writes a [`Watch`] logs.
pub const LOGGED: usize = 4096;
/// The most writer, caller and word a [`Watch`] counts.
pub const SITES: usize = 1 << 20;

impl Watch {
    fn note(&mut self, written: Written) {
        let (start, end) = (u64::from(written.address), u64::from(written.address) + u64::from(written.size));
        if start < u64::from(self.base) + u64::from(self.length) && end > u64::from(self.base) {
            if self.log.len() < LOGGED {
                self.log.push(written);
            } else {
                self.dropped += 1;
            }
        }
        if let Some(sites) = &mut self.sites {
            for word in (written.address / 4)..=((written.address + written.size.max(1) - 1) / 4) {
                let key = (written.writer, written.caller, word * 4);
                if let Some(count) = sites.get_mut(&key) {
                    *count += 1;
                } else if sites.len() < SITES {
                    sites.insert(key, 1);
                }
            }
        }
    }
}

/// The chip around the core: its peripherals, by family.
#[derive(Clone, Debug)]
pub enum Soc {
    G0(Box<Stm32g0>),
    H7(Box<Stm32h7>),
}

#[derive(Clone, Debug)]
pub struct Bus {
    pub flash: Vec<u8>,
    /// Whether flash answers at zero too, as a G0 booting from it has it.
    flash_alias: bool,
    /// System memory holds ST's bootloader, the unique ID and the option
    /// bytes. Of it only the flash size and an H7's UID are modelled; the
    /// rest reads as erased flash.
    pub system: Vec<u8>,
    system_base: u32,
    /// The RAMs, the one used most first.
    pub rams: Vec<Ram>,
    pub scs: Scs,
    pub soc: Soc,
    /// The time it is, in picoseconds, for the peripherals whose state is
    /// a function of time; the machine sets it before an access reaches
    /// them.
    pub now: u64,
    /// What is writing: the address of the instruction storing, or one of
    /// the `BY_*`; whoever writes sets it first, as with `now`.
    pub writer: u32,
    /// lr as the writer stored, when it is an instruction: see [`Written`].
    pub caller: u32,
    /// What is being watched, if anything: see [`Watch`].
    pub watch: Option<Box<Watch>>,
    /// [`write_stamp`] of `stamped`: worked out again only when `now` has
    /// moved on, not at every store.
    stamp: u64,
    stamped: u64,
}

enum Target {
    Memory { region: Region, offset: usize },
    Scs,
    Soc,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    Flash,
    System,
    Ram(usize),
}

impl Bus {
    pub fn new(part: Part, image: &[u8]) -> Result<Bus, String> {
        if image.len() > part.flash {
            return Err(format!("image is {} bytes; the {}'s flash is {}", image.len(), part.name, part.flash));
        }
        let mut flash = vec![0xFF; part.flash];
        flash[..image.len()].copy_from_slice(image);
        let kib = ((part.flash / 1024) as u16).to_le_bytes();
        Ok(match part.family {
            Family::G0 => {
                let mut system = vec![0xFF; 64 * 1024];
                system[G0_FLASHSIZE..G0_FLASHSIZE + 2].copy_from_slice(&kib);
                let mut soc = Stm32g0::default();
                soc.dev_id = part.dev_id;
                Bus {
                    flash,
                    flash_alias: true,
                    system,
                    system_base: 0x1FFF_0000,
                    rams: vec![Ram::new(SRAM_BASE, part.sram)],
                    scs: Scs::default(),
                    soc: Soc::G0(Box::new(soc)),
                    now: 0,
                    writer: 0,
                    caller: 0,
                    watch: None,
                    stamp: 1,
                    stamped: 0,
                }
            }
            Family::H7 => {
                let mut system = vec![0xFF; 128 * 1024];
                system[H7_FLASHSIZE..H7_FLASHSIZE + 2].copy_from_slice(&kib);
                // A UID: the wafer's coordinates, its number, and its lot.
                system[H7_UID..H7_UID + 12].copy_from_slice(b"\x1a\x00\x2b\x00\x0eQZARU75");
                let ram = |base: u32, kib: usize| Ram::new(base, kib * 1024);
                Bus {
                    flash,
                    flash_alias: false,
                    system,
                    system_base: 0x1FF0_0000,
                    rams: vec![
                        ram(AXI_SRAM_BASE, 512),
                        ram(SRAM_BASE, 128),
                        ram(0x3000_0000, 288),
                        ram(0x3800_0000, 64),
                        ram(0x3880_0000, 4),
                        ram(0x0000_0000, 64),
                    ],
                    scs: Scs::default(),
                    soc: Soc::H7(Box::default()),
                    now: 0,
                    writer: 0,
                    caller: 0,
                    watch: None,
                    stamp: 1,
                    stamped: 0,
                }
            }
        })
    }

    /// The RAM `addr` is in: its number, and the offset in it.
    pub fn ram_at(&self, addr: u32) -> Option<(usize, usize)> {
        self.rams
            .iter()
            .position(|ram| addr >= ram.base && ((addr - ram.base) as usize) < ram.bytes.len())
            .map(|i| (i, (addr - self.rams[i].base) as usize))
    }

    /// When the word at `addr` was last written, and what wrote it; `None`
    /// if it is not RAM or was never written.
    pub fn last_write(&self, addr: u32) -> Option<Mark> {
        let (i, offset) = self.ram_at(addr)?;
        let mark = self.rams[i].marks[offset / 4];
        (mark.stamp != 0).then_some(mark)
    }

    /// The cells of `cell` bytes (a multiple of 4) of every RAM written at
    /// or after the stamp `since`: each RAM's number, the cell's number in
    /// it, and when it was last written. At, as well as after: a word
    /// written in the microsecond the last answer was given in — later in
    /// it, or while the board stood still — has that answer's stamp, and is
    /// not to be lost.
    pub fn written_since(&self, since: u64, cell: u32) -> Vec<(usize, u32, u64)> {
        let mut out = Vec::new();
        for (i, ram) in self.rams.iter().enumerate() {
            for (n, last) in Self::cells(&ram.marks, cell) {
                if last >= since && last != 0 {
                    out.push((i, n, last));
                }
            }
        }
        out
    }

    /// The cells of `cell` bytes (a multiple of 4) of `[base, base +
    /// length)` — of the one RAM `base` is in, and no further than its
    /// end — written at or after the stamp `since`: each cell's number from
    /// `base`, and when it was last written. `None` if `base` is not RAM.
    pub fn written_in(&self, since: u64, cell: u32, base: u32, length: u32) -> Option<Vec<(u32, u64)>> {
        let (i, offset) = self.ram_at(base)?;
        let marks = &self.rams[i].marks;
        let from = offset / 4;
        let to = (offset as u64 + length as u64).div_ceil(4).min(marks.len() as u64) as usize;
        Some(Self::cells(&marks[from..to.max(from)], cell).filter(|&(_, last)| last >= since && last != 0).collect())
    }

    /// Each cell of `cell` bytes of `marks`: its number, and the last
    /// stamp in it.
    fn cells(marks: &[Mark], cell: u32) -> impl Iterator<Item = (u32, u64)> + '_ {
        let words = (cell / 4).max(1) as usize;
        marks.chunks(words).enumerate().map(|(n, marks)| (n as u32, marks.iter().map(|m| m.stamp).max().unwrap_or(0)))
    }

    /// [`write_stamp`] of the time it is.
    fn stamp(&mut self) -> u64 {
        if self.stamped != self.now {
            self.stamped = self.now;
            self.stamp = write_stamp(self.now);
        }
        self.stamp
    }

    fn decode(&self, addr: u32) -> Option<Target> {
        let within = |base: u32, size: usize| {
            addr.checked_sub(base).filter(|&o| (o as usize) < size).map(|o| o as usize)
        };
        let flash = self.flash.len();
        if let Some(offset) = within(FLASH_BASE, flash) {
            return Some(Target::Memory { region: Region::Flash, offset });
        }
        for (i, ram) in self.rams.iter().enumerate() {
            if let Some(offset) = within(ram.base, ram.bytes.len()) {
                return Some(Target::Memory { region: Region::Ram(i), offset });
            }
        }
        if let Some(offset) = within(0, flash).filter(|_| self.flash_alias) {
            return Some(Target::Memory { region: Region::Flash, offset });
        }
        if let Some(offset) = within(self.system_base, self.system.len()) {
            return Some(Target::Memory { region: Region::System, offset });
        }
        if (0xE000_0000..0xE010_0000).contains(&addr) && !self.soc.contains(addr) {
            return Some(Target::Scs);
        }
        if self.soc.contains(addr) {
            return Some(Target::Soc);
        }
        None
    }

    fn memory(&self, region: Region) -> &[u8] {
        match region {
            Region::Flash => &self.flash,
            Region::System => &self.system,
            Region::Ram(i) => &self.rams[i].bytes,
        }
    }

    /// Reads `size` bytes (1, 2 or 4) at `addr`, which the caller has
    /// already checked is aligned.
    pub fn read(&mut self, addr: u32, size: u32) -> Result<u32, Unmapped> {
        match self.decode(addr).ok_or(Unmapped)? {
            Target::Memory { region, offset } => Ok(little_endian(self.memory(region), offset, size)),
            Target::Scs => Ok(subword(self.scs.read(addr & !3).ok_or(Unmapped)?, addr, size)),
            Target::Soc => Ok(subword(self.soc.read(addr & !3, size, self.now).ok_or(Unmapped)?, addr, size)),
        }
    }

    /// An instruction fetch: a halfword read, with the common case — code
    /// in flash at its own address — taken before the general decode.
    pub fn fetch(&mut self, addr: u32) -> Option<u32> {
        let offset = addr.wrapping_sub(FLASH_BASE) as usize;
        if offset < self.flash.len() - 1 {
            return Some(u16::from_le_bytes([self.flash[offset], self.flash[offset + 1]]) as u32);
        }
        self.read(addr, 2).ok()
    }

    /// A read as a debugger makes it: the same answer as [`Bus::read`],
    /// without the one side effect a read can have here (SysTick's
    /// COUNTFLAG), and without counting as software touching a peripheral.
    pub fn peek(&self, addr: u32, size: u32) -> Result<u32, Unmapped> {
        match self.decode(addr).ok_or(Unmapped)? {
            Target::Memory { region, offset } => Ok(little_endian(self.memory(region), offset, size)),
            Target::Scs => Ok(subword(self.scs.peek(addr & !3).ok_or(Unmapped)?, addr, size)),
            Target::Soc => Ok(subword(self.soc.peek(addr & !3, self.now), addr, size)),
        }
    }

    pub fn write(&mut self, addr: u32, size: u32, value: u32) -> Result<(), Unmapped> {
        let shift = 8 * (addr & 3);
        let mask = if size == 4 { u32::MAX } else { ((1 << (8 * size)) - 1) << shift };
        let placed = value << shift & mask;
        match self.decode(addr).ok_or(Unmapped)? {
            // A store to main flash is programming it, which the flash
            // interface decides; it answers in its status register, not
            // with a bus error.
            Target::Memory { region: Region::Flash, offset } => {
                self.soc.program(&mut self.flash, offset, size, value, self.now);
                Ok(())
            }
            Target::Memory { region: Region::System, .. } => Err(Unmapped),
            Target::Memory { region: Region::Ram(i), offset } => {
                let bytes = value.to_le_bytes();
                let (stamp, writer) = (self.stamp(), self.writer);
                let ram = &mut self.rams[i];
                match size {
                    1 => ram.bytes[offset] = bytes[0],
                    2 => ram.bytes[offset..offset + 2].copy_from_slice(&bytes[..2]),
                    _ => ram.bytes[offset..offset + 4].copy_from_slice(&bytes),
                }
                ram.mark(offset, size as usize, stamp, writer);
                if let Some(watch) = &mut self.watch {
                    watch.note(Written { time: self.now, writer, caller: self.caller, address: addr, size, value });
                }
                Ok(())
            }
            Target::Scs => {
                let word = if size != 4 && Scs::merges_subword(addr & !3) {
                    self.scs.peek(addr & !3).ok_or(Unmapped)? & !mask | placed
                } else {
                    placed
                };
                self.scs.write(addr & !3, word).ok_or(Unmapped)
            }
            Target::Soc => {
                let word = if size != 4 { self.soc.peek(addr & !3, self.now) & !mask | placed } else { placed };
                self.soc.write(addr & !3, word, size, self.now, &mut self.flash).ok_or(Unmapped)?;
                self.run_dma();
                Ok(())
            }
        }
    }

    /// `len` bytes of RAM at `addr`, if they are all in one RAM: what a DMA
    /// reaches.
    fn dma_slice(rams: &mut [Ram], addr: u32, len: usize) -> Option<&mut [u8]> {
        let ram = rams.iter_mut().find(|ram| addr >= ram.base && (addr - ram.base) as usize + len <= ram.bytes.len())?;
        let at = (addr - ram.base) as usize;
        Some(&mut ram.bytes[at..at + len])
    }

    /// `len` bytes of RAM at `addr`, as [`Bus::dma_slice`] found them,
    /// written by DMA at `now`.
    fn dma_wrote(rams: &mut [Ram], watch: &mut Option<Box<Watch>>, addr: u32, len: usize, now: u64) {
        if let Some(ram) = rams.iter_mut().find(|ram| addr >= ram.base && (addr - ram.base) as usize + len <= ram.bytes.len()) {
            ram.mark((addr - ram.base) as usize, len, write_stamp(now), BY_DMA);
            if let Some(watch) = watch {
                watch.note(Written { time: now, writer: BY_DMA, caller: 0, address: addr, size: len as u32, value: 0 });
            }
        }
    }

    fn dma_words(rams: &mut [Ram], addr: u32) -> Option<[u32; 4]> {
        let bytes = Self::dma_slice(rams, addr, 16)?;
        Some(std::array::from_fn(|i| u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap())))
    }

    /// The Ethernet DMA's work, as far as it can go: frames out of the
    /// transmit ring onto the wire, and frames from the wire into the
    /// receive ring. Run whenever software may have given it some — any
    /// store to a peripheral — and when a frame arrives.
    pub fn run_dma(&mut self) {
        use crate::eth_h7::{BUF1V, FD, LD, OWN};
        let Soc::H7(soc) = &mut self.soc else { return };
        let (rams, eth, watch) = (&mut self.rams, &mut soc.eth, &mut self.watch);
        if eth.tx_kicked && eth.tx_running() {
            eth.tx_kicked = false;
            let mut frame = Vec::new();
            // At most the ring, once round.
            for _ in 0..1024 {
                let at = eth.tx_current;
                let Some([buf1, buf2, tdes2, tdes3]) = Self::dma_words(rams, at) else { break };
                if tdes3 & OWN == 0 {
                    eth.tx_unavailable();
                    break;
                }
                // A context descriptor (CTXT, bit 30) carries, with VLTV, the
                // VLAN tag for the frames after it.
                if tdes3 & 1 << 30 != 0 && tdes3 & 1 << 16 != 0 {
                    eth.tx_vlan = Some(tdes3 as u16);
                }
                if tdes3 & 1 << 30 == 0 {
                    if tdes3 & FD != 0 {
                        frame.clear();
                    }
                    for (buffer, len) in [(buf1, tdes2 & 0x3FFF), (buf2, tdes2 >> 16 & 0x3FFF)] {
                        if let Some(bytes) = Self::dma_slice(rams, buffer, len as usize) {
                            frame.extend_from_slice(bytes);
                        }
                    }
                }
                let written = (tdes3 & !OWN).to_le_bytes();
                if let Some(slot) = Self::dma_slice(rams, at + 12, 4) {
                    slot.copy_from_slice(&written);
                    Self::dma_wrote(rams, watch, at + 12, 4, self.now);
                }
                eth.tx_advance();
                if tdes3 & 1 << 30 == 0 && tdes3 & LD != 0 {
                    let mut frame = std::mem::take(&mut frame);
                    // TDES2's VTIR: insert the context's tag.
                    if let (0b10, Some(tag)) = (tdes2 >> 14 & 3, eth.tx_vlan) {
                        frame = crate::eth_h7::tagged(&frame, tag);
                    }
                    eth.transmitted(frame, tdes2, self.now);
                }
            }
        }
        while eth.rx_running() && eth.arrived(self.now) {
            let at = eth.rx_current;
            let Some([buffer, _, _, rdes3]) = Self::dma_words(rams, at) else { break };
            if rdes3 & OWN == 0 || rdes3 & BUF1V == 0 {
                eth.rx_unavailable();
                eth.incoming.pop_front();
                break;
            }
            let (_, mut frame) = eth.incoming.pop_front().unwrap();
            // A tagged frame, stripped: its tag goes in RDES0.
            let mut rdes0 = 0;
            if eth.strips_vlan() {
                if let Some((tci, inner)) = crate::eth_h7::untagged(&frame) {
                    rdes0 = u32::from(tci);
                    frame = inner;
                }
            }
            frame.extend_from_slice(&crate::eth_h7::fcs(&frame).to_le_bytes());
            // A frame longer than a buffer would go on into the next
            // descriptor; Hubris's buffers hold the longest there is.
            let len = frame.len().min(eth.rx_buffer_size());
            if let Some(bytes) = Self::dma_slice(rams, buffer, len) {
                bytes.copy_from_slice(&frame[..len]);
                Self::dma_wrote(rams, watch, buffer, len, self.now);
            }
            // Written back: FD and LD, the length with its FCS, and RDES1's
            // IPv6-header-present bit set for what is IPv6.
            let ipv6 = frame.get(12..14) == Some(&[0x86, 0xDD]);
            let rdes1: u32 = if ipv6 { 1 << 5 } else { 0 };
            let words = [rdes0, rdes1, 0, FD | LD | 1 << 25 | len as u32 & 0x7FFF];
            if let Some(slot) = Self::dma_slice(rams, at, 16) {
                for (i, word) in words.iter().enumerate() {
                    slot[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
                }
                Self::dma_wrote(rams, watch, at, 16, self.now);
            }
            eth.rx_advance();
            eth.received(rdes3);
        }
    }

    /// A system reset of the chip around the core.
    pub fn reset_soc(&mut self) {
        match &mut self.soc {
            Soc::G0(soc) => soc.reset(),
            Soc::H7(soc) => soc.reset(&mut self.flash, self.now),
        }
    }
}

impl Soc {
    pub fn contains(&self, addr: u32) -> bool {
        match self {
            Soc::G0(_) => Stm32g0::contains(addr),
            Soc::H7(_) => Stm32h7::contains(addr),
        }
    }

    /// Whether a load at `addr` changes something (a FIFO popped) or reads
    /// something that changes by itself (a timer's counter): what a busy
    /// wait may not do and still be skipped (see `Machine::spin`).
    pub fn impure(&self, addr: u32) -> bool {
        match self {
            Soc::G0(_) => crate::stm32g0::USARTS.iter().any(|&(base, _)| addr == base + 0x24),
            Soc::H7(_) => Stm32h7::impure(addr),
        }
    }

    /// The clocks RCC is giving out.
    pub fn clocks(&self) -> crate::clock::Clocks {
        match self {
            Soc::G0(soc) => soc.clocks(),
            Soc::H7(soc) => soc.clocks,
        }
    }

    /// A read of `size` bytes at the word `addr`, at time `now`.
    pub fn read(&mut self, addr: u32, size: u32, now: u64) -> Option<u32> {
        match self {
            Soc::G0(soc) => soc.read(addr, now),
            Soc::H7(soc) => soc.read(addr, size, now),
        }
    }

    pub fn peek(&self, addr: u32, now: u64) -> u32 {
        match self {
            Soc::G0(soc) => soc.peek(addr, now),
            Soc::H7(soc) => soc.peek(addr, now),
        }
    }

    pub fn write(&mut self, addr: u32, value: u32, size: u32, now: u64, flash: &mut [u8]) -> Option<()> {
        match self {
            Soc::G0(soc) => soc.write(addr, value, now, flash),
            Soc::H7(soc) => soc.write(addr, value, size, now, flash),
        }
    }

    /// The time the next timed event in the peripherals is due at.
    pub fn due(&self) -> Option<u64> {
        match self {
            Soc::G0(soc) => soc.due(),
            Soc::H7(soc) => soc.due(),
        }
    }

    /// Brings the peripherals' timed state up to time `now`.
    pub fn advance(&mut self, now: u64) {
        match self {
            Soc::G0(soc) => soc.advance(now),
            Soc::H7(soc) => soc.advance(now),
        }
    }

    /// A store into main flash, which the flash interface programs or
    /// refuses.
    fn program(&mut self, flash: &mut [u8], offset: usize, size: u32, value: u32, now: u64) {
        match self {
            Soc::G0(soc) => soc.flash_interface.program(flash, offset, size, value),
            Soc::H7(soc) => {
                soc.flash_interface.now = now;
                soc.flash_interface.advance(now);
                soc.flash_interface.store(flash, offset % (2 * flash_h7::BANK), size, value)
            }
        }
    }

    /// The interrupt lines the peripherals assert, as a mask by IRQ number.
    pub fn interrupts(&self) -> [u32; 8] {
        match self {
            Soc::G0(soc) => soc.interrupts(),
            Soc::H7(soc) => soc.interrupts(),
        }
    }

    /// A byte arriving on USART `port`'s line at `now`; false if it was
    /// not taken.
    pub fn receive(&mut self, port: usize, byte: u8, now: u64) -> bool {
        match self {
            Soc::G0(soc) => soc.receive(port, byte, now),
            Soc::H7(soc) => soc.receive(port, byte, now),
        }
    }

    /// Something outside drives a pin to a level, or lets go of it.
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>, now: u64) {
        match self {
            Soc::G0(soc) => soc.drive(port, pin, level, now),
            Soc::H7(soc) => soc.drive(port, pin, level, now),
        }
    }

    /// What the peripherals produced since last asked: output edges and
    /// serial bytes.
    pub fn outputs(&mut self) -> (&mut Vec<Edge>, &mut Vec<usart::Byte>) {
        match self {
            Soc::G0(soc) => (&mut soc.edges, &mut soc.serial),
            Soc::H7(soc) => (&mut soc.edges, &mut soc.serial),
        }
    }

    /// Whether there are outputs waiting.
    pub fn has_outputs(&self) -> bool {
        match self {
            Soc::G0(soc) => !soc.edges.is_empty() || !soc.serial.is_empty(),
            Soc::H7(soc) => !soc.edges.is_empty() || !soc.serial.is_empty() || !soc.eth.outgoing.is_empty(),
        }
    }

    /// The frames the Ethernet controller put on the wire, if there is one.
    pub fn frames(&mut self) -> Option<&mut Vec<Vec<u8>>> {
        match self {
            Soc::G0(_) => None,
            Soc::H7(soc) => Some(&mut soc.eth.outgoing),
        }
    }

    /// Register addresses software touched that the model only pretends
    /// to have.
    pub fn unmodelled(&self) -> &std::collections::BTreeSet<u32> {
        match self {
            Soc::G0(soc) => &soc.unmodelled,
            Soc::H7(soc) => &soc.unmodelled,
        }
    }
}

/// `size` bytes at `offset`, little-endian, each size its own fixed-length
/// copy: a copy of a length known only at run time is a call to `memmove`
/// per load, and loads are a quarter of what a kernel executes.
fn little_endian(memory: &[u8], offset: usize, size: u32) -> u32 {
    match size {
        1 => memory[offset] as u32,
        2 => u16::from_le_bytes([memory[offset], memory[offset + 1]]) as u32,
        _ => u32::from_le_bytes(memory[offset..offset + 4].try_into().unwrap()),
    }
}

fn subword(word: u32, addr: u32, size: u32) -> u32 {
    let shifted = word >> (8 * (addr & 3));
    if size == 4 { shifted } else { shifted & ((1 << (8 * size)) - 1) }
}
