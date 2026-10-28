//! The H7's QUADSPI, as far as a NOR flash read through it in memory-mapped
//! mode (RM0433 23.3.10): set up so, the controller turns each read of its
//! window at `0x9000_0000` into a read command to the flash, and software
//! reads the flash as memory. That is how a board keeps more than its
//! internal flash holds — a game's data, say, four times the H753's 2 MiB —
//! and runs as if it were all in its address space.
//!
//! The registers remember what was written, as the part's do. The window
//! answers while the controller is enabled (CR's EN) in memory-mapped mode
//! (CCR's FMODE `0b11`) with a read command in CCR's INSTRUCTION, as far as
//! DCR's FSIZE says the flash goes; reading it otherwise is a bus fault, as
//! it is on the part, and so is any write to it. Past what was put on the
//! flash it reads erased.
//!
//! Not modelled: indirect and status-polling modes, which software that
//! only maps a flash does not use; the timeout counter; and the commands'
//! own timing, beyond what [`crate::timing`] charges for the window (with
//! the M7's data cache on, as Hubris has it, the window is cached).

use std::sync::Arc;

pub const BASE: u32 = 0x5200_5000;
pub const SIZE: u32 = 0x1000;
/// The memory-mapped window, and how far it can reach.
pub const WINDOW: u32 = 0x9000_0000;
pub const WINDOW_SIZE: usize = 0x1000_0000;

// CR, DCR, SR and CCR, by offset.
const CR: u32 = 0x00;
const DCR: u32 = 0x04;
const SR: u32 = 0x08;
const FCR: u32 = 0x0C;
const CCR: u32 = 0x14;
const EN: u32 = 1;
const MEMORY_MAPPED: u32 = 0b11;
const BUSY: u32 = 1 << 5;

/// What a NOR flash on the QUADSPI holds: what was put on it, the rest of
/// it erased. Shared, since it is never written, so that a machine copied
/// for a checkpoint does not copy megabytes.
#[derive(Clone)]
pub struct Nor {
    contents: Arc<[u8]>,
    size: usize,
}

impl std::fmt::Debug for Nor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Nor {{ {} of {} bytes }}", self.contents.len(), self.size)
    }
}

impl Nor {
    /// A flash of `size` bytes, a power of two, holding `contents` from
    /// its start.
    pub fn new(contents: &[u8], size: usize) -> Result<Nor, String> {
        if !size.is_power_of_two() || size > WINDOW_SIZE {
            return Err(format!("a NOR of {size} bytes: its size is a power of two, at most 256 MiB"));
        }
        if contents.len() > size {
            return Err(format!("{} bytes do not fit on a NOR of {size}", contents.len()));
        }
        Ok(Nor { contents: contents.into(), size })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// What was put on it; the rest of it is erased.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

#[derive(Clone, Debug, Default)]
pub struct Quadspi {
    /// CR to LPTR, as written.
    registers: [u32; 13],
    /// The flash wired to it, if there is one.
    pub nor: Option<Nor>,
}

impl Quadspi {
    pub fn contains(addr: u32) -> bool {
        (BASE..BASE + SIZE).contains(&addr)
    }

    /// A reset: the registers to theirs, the flash as it is.
    pub fn reset(&mut self) {
        self.registers = [0; 13];
    }

    fn register(&self, offset: u32) -> u32 {
        self.registers.get(offset as usize / 4).copied().unwrap_or(0)
    }

    /// How many bytes of the window answer now: none unless it is mapped.
    pub fn mapped(&self) -> usize {
        let Some(nor) = &self.nor else { return 0 };
        let ccr = self.register(CCR);
        let reading = matches!(ccr & 0xFF, 0x03 | 0x0B | 0x3B | 0xBB | 0x6B | 0xEB | 0x13 | 0x0C | 0x3C | 0xBC | 0x6C | 0xEC);
        if self.register(CR) & EN == 0 || ccr >> 26 & 3 != MEMORY_MAPPED || !reading {
            return 0;
        }
        let fsize = self.register(DCR) >> 16 & 0x1F;
        (2usize << fsize).min(nor.size())
    }

    pub fn read(&self, offset: u32) -> u32 {
        match offset {
            // In memory-mapped mode the controller stays busy, ready for
            // the next read.
            SR => {
                if self.mapped() != 0 {
                    BUSY
                } else {
                    0
                }
            }
            FCR => 0,
            offset => self.register(offset),
        }
    }

    pub fn write(&mut self, offset: u32, value: u32) {
        match offset {
            // SR is read-only, and FCR clears flags this never raises.
            SR | FCR => {}
            offset => {
                if let Some(register) = self.registers.get_mut(offset as usize / 4) {
                    *register = value;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped_over(contents: &[u8]) -> Quadspi {
        let mut qspi = Quadspi { nor: Some(Nor::new(contents, 1 << 24).unwrap()), ..Default::default() };
        qspi.write(DCR, 23 << 16);
        qspi.write(CR, EN);
        qspi.write(CCR, 0b11 << 26 | 0x6B);
        qspi
    }

    #[test]
    fn the_window_answers_only_when_mapped_with_a_read_command() {
        let mut qspi = mapped_over(b"IWAD");
        assert_eq!(qspi.mapped(), 1 << 24);
        assert_eq!(qspi.read(SR), BUSY);
        qspi.write(CCR, 0b01 << 26 | 0x6B);
        assert_eq!(qspi.mapped(), 0, "indirect read mode does not map");
        qspi.write(CCR, 0b11 << 26 | 0x02);
        assert_eq!(qspi.mapped(), 0, "a page program is not a read");
        qspi.write(CCR, 0b11 << 26 | 0xEB);
        qspi.write(CR, 0);
        assert_eq!(qspi.mapped(), 0, "disabled");
    }

    #[test]
    fn fsize_bounds_the_window() {
        let mut qspi = mapped_over(b"IWAD");
        qspi.write(DCR, 19 << 16);
        assert_eq!(qspi.mapped(), 1 << 20);
        assert_eq!(qspi.nor.as_ref().unwrap().contents(), b"IWAD");
    }
}
