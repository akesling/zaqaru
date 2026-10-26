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

/// A RAM, at its address.
#[derive(Clone, Debug)]
pub struct Ram {
    pub base: u32,
    pub bytes: Vec<u8>,
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
                    rams: vec![Ram { base: SRAM_BASE, bytes: vec![0; part.sram] }],
                    scs: Scs::default(),
                    soc: Soc::G0(Box::new(soc)),
                    now: 0,
                }
            }
            Family::H7 => {
                let mut system = vec![0xFF; 128 * 1024];
                system[H7_FLASHSIZE..H7_FLASHSIZE + 2].copy_from_slice(&kib);
                // A UID: the wafer's coordinates, its number, and its lot.
                system[H7_UID..H7_UID + 12].copy_from_slice(b"\x1a\x00\x2b\x00\x0eQZARU75");
                let ram = |base: u32, kib: usize| Ram { base, bytes: vec![0; kib * 1024] };
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
                }
            }
        })
    }

    /// The RAM at `base`, if there is one there.
    pub fn ram(&mut self, base: u32) -> Option<&mut Vec<u8>> {
        self.rams.iter_mut().find(|ram| ram.base == base).map(|ram| &mut ram.bytes)
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
                let ram = &mut self.rams[i].bytes;
                match size {
                    1 => ram[offset] = bytes[0],
                    2 => ram[offset..offset + 2].copy_from_slice(&bytes[..2]),
                    _ => ram[offset..offset + 4].copy_from_slice(&bytes),
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
        let (rams, eth) = (&mut self.rams, &mut soc.eth);
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
                // A context descriptor (CTXT, bit 30) carries a VLAN tag for
                // the next; this model does not tag.
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
                }
                eth.tx_advance();
                if tdes3 & 1 << 30 == 0 && tdes3 & LD != 0 {
                    eth.transmitted(std::mem::take(&mut frame), tdes2, self.now);
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
            frame.extend_from_slice(&crate::eth_h7::fcs(&frame).to_le_bytes());
            // A frame longer than a buffer would go on into the next
            // descriptor; Hubris's buffers hold the longest there is.
            let len = frame.len().min(eth.rx_buffer_size());
            if let Some(bytes) = Self::dma_slice(rams, buffer, len) {
                bytes.copy_from_slice(&frame[..len]);
            }
            // Written back: FD and LD, the length with its FCS, and RDES1's
            // IPv6-header-present bit set for what is IPv6.
            let ipv6 = frame.get(12..14) == Some(&[0x86, 0xDD]);
            let rdes1: u32 = if ipv6 { 1 << 5 } else { 0 };
            let words = [0, rdes1, 0, FD | LD | 1 << 25 | len as u32 & 0x7FFF];
            if let Some(slot) = Self::dma_slice(rams, at, 16) {
                for (i, word) in words.iter().enumerate() {
                    slot[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
                }
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
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
        match self {
            Soc::G0(soc) => soc.drive(port, pin, level),
            Soc::H7(soc) => soc.drive(port, pin, level),
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
