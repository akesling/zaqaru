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
//! | `0x4002_2000..0x4002_2400`  | the flash interface; see [`crate::flash`] |
//! | `0x4000_0000..0x5000_1800`  | peripherals; see [`crate::stm32g0`]   |
//! | `0xE000_0000..0xE010_0000`  | the PPB; see [`crate::scs`]           |

use crate::flash;
use crate::part::Part;
use crate::scs::Scs;
use crate::stm32g0::Stm32g0;

pub const FLASH_BASE: u32 = 0x0800_0000;
/// The G031's, which the tests are written against; a board's own are
/// its [`Part`]'s.
pub const FLASH_SIZE: usize = 64 * 1024;
pub const SYSTEM_BASE: u32 = 0x1FFF_0000;
pub const SYSTEM_SIZE: usize = 64 * 1024;
pub const SRAM_BASE: u32 = 0x2000_0000;
pub const SRAM_SIZE: usize = 8 * 1024;
/// FLASHSIZE, in system memory: main flash in KiB, which a flash
/// algorithm reads to know the part.
const FLASHSIZE: usize = 0x75E0;

/// Nothing answered: a bus fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmapped;

#[derive(Clone, Debug)]
pub struct Bus {
    pub flash: Vec<u8>,
    /// System memory holds ST's bootloader, the unique ID and the option
    /// bytes. Of it only FLASHSIZE is modelled; the rest reads as erased
    /// flash.
    pub system: Vec<u8>,
    pub sram: Vec<u8>,
    pub scs: Scs,
    pub flash_interface: flash::Interface,
    pub soc: Stm32g0,
}

enum Target {
    Memory { region: Region, offset: usize },
    Scs,
    FlashInterface,
    Soc,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    Flash,
    System,
    Sram,
}

impl Bus {
    pub fn new(part: Part, image: &[u8]) -> Result<Bus, String> {
        if image.len() > part.flash {
            return Err(format!("image is {} bytes; the {}'s flash is {}", image.len(), part.name, part.flash));
        }
        let mut flash = vec![0xFF; part.flash];
        flash[..image.len()].copy_from_slice(image);
        let mut system = vec![0xFF; SYSTEM_SIZE];
        system[FLASHSIZE..FLASHSIZE + 2].copy_from_slice(&((part.flash / 1024) as u16).to_le_bytes());
        let mut soc = Stm32g0::default();
        soc.dev_id = part.dev_id;
        Ok(Bus {
            flash,
            system,
            sram: vec![0; part.sram],
            scs: Scs::default(),
            flash_interface: flash::Interface::default(),
            soc,
        })
    }

    fn decode(&self, addr: u32) -> Option<Target> {
        let within = |base: u32, size: usize| {
            addr.checked_sub(base).filter(|&o| (o as usize) < size).map(|o| o as usize)
        };
        let flash = self.flash.len();
        if let Some(offset) = within(0, flash).or_else(|| within(FLASH_BASE, flash)) {
            return Some(Target::Memory { region: Region::Flash, offset });
        }
        if let Some(offset) = within(SYSTEM_BASE, SYSTEM_SIZE) {
            return Some(Target::Memory { region: Region::System, offset });
        }
        if let Some(offset) = within(SRAM_BASE, self.sram.len()) {
            return Some(Target::Memory { region: Region::Sram, offset });
        }
        if (0xE000_0000..0xE010_0000).contains(&addr) {
            return Some(Target::Scs);
        }
        if flash::Interface::contains(addr) {
            return Some(Target::FlashInterface);
        }
        if Stm32g0::contains(addr) {
            return Some(Target::Soc);
        }
        None
    }

    fn memory(&self, region: Region) -> &[u8] {
        match region {
            Region::Flash => &self.flash,
            Region::System => &self.system,
            Region::Sram => &self.sram,
        }
    }

    /// Reads `size` bytes (1, 2 or 4) at `addr`, which the caller has
    /// already checked is aligned.
    pub fn read(&mut self, addr: u32, size: u32) -> Result<u32, Unmapped> {
        match self.decode(addr).ok_or(Unmapped)? {
            Target::Memory { region, offset } => Ok(little_endian(self.memory(region), offset, size)),
            Target::Scs => Ok(subword(self.scs.read(addr & !3).ok_or(Unmapped)?, addr, size)),
            Target::FlashInterface => Ok(subword(self.flash_interface.read(addr & !3), addr, size)),
            Target::Soc => Ok(subword(self.soc.read(addr & !3).ok_or(Unmapped)?, addr, size)),
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
            Target::FlashInterface => Ok(subword(self.flash_interface.read(addr & !3), addr, size)),
            Target::Soc => Ok(subword(self.soc.peek(addr & !3), addr, size)),
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
                self.flash_interface.program(&mut self.flash, offset, size, value);
                Ok(())
            }
            Target::Memory { region: Region::System, .. } => Err(Unmapped),
            Target::Memory { region: Region::Sram, offset } => {
                let bytes = value.to_le_bytes();
                match size {
                    1 => self.sram[offset] = bytes[0],
                    2 => self.sram[offset..offset + 2].copy_from_slice(&bytes[..2]),
                    _ => self.sram[offset..offset + 4].copy_from_slice(&bytes),
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
            Target::FlashInterface => {
                let word = if size != 4 { self.flash_interface.read(addr & !3) & !mask | placed } else { placed };
                self.flash_interface.write(addr & !3, word, &mut self.flash);
                Ok(())
            }
            Target::Soc => {
                let word = if size != 4 { self.soc.peek(addr & !3) & !mask | placed } else { placed };
                self.soc.write(addr & !3, word).ok_or(Unmapped)
            }
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
