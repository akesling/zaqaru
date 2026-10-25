//! The H7's flash interface (RM0433 4): two banks of 1 MiB, each with its
//! own keys, control and status, programmed a 256-bit flash word at a time
//! and erased by 128 KiB sector or by bank; and the option bytes, of which
//! what matters is the bank swap.
//!
//! What a writer waits on and checks is what is modelled: the unlock
//! sequences, the write buffer filling with stores to one flash word and
//! programming it when the last byte lands (or FW forces it), START, EOP,
//! QW and the error flags — PGSERR for a store without PG, STRBERR for one
//! byte written twice, INCERR for a store to another word with the buffer
//! part-full — and the interrupt they raise. Every operation completes at
//! once: BSY and QW are never seen set.
//!
//! The banks are logical. Flash is the bus's 2 MiB, the first half at
//! `0x0800_0000`, and bank 1's registers drive the half there; a swap
//! takes effect at the next reset by exchanging the halves (see
//! [`Interface::reset`]), so the registers go on driving the half at their
//! address, as Hubris's update server expects of bank 2.

pub const BASE: u32 = 0x5200_2000;
pub const END: u32 = BASE + 0x2000;
pub const BANK: usize = 1024 * 1024;
pub const SECTOR: usize = 128 * 1024;
/// A flash word, the unit of programming.
pub const WORD: usize = 32;

const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const OPTKEY1: u32 = 0x0819_2A3B;
const OPTKEY2: u32 = 0x4C5D_6E7F;

// CR.
const LOCK: u32 = 1 << 0;
const PG: u32 = 1 << 1;
const SER: u32 = 1 << 2;
const BER: u32 = 1 << 3;
const FW: u32 = 1 << 6;
const START: u32 = 1 << 7;
const CR_RESET: u32 = LOCK | 0b11 << 4;
/// The interrupt enables, which sit at their flags' positions in SR.
const CR_INTERRUPTS: u32 = 0x0FEF_0000;

// SR.
const WBNE: u32 = 1 << 1;
const EOP: u32 = 1 << 16;
const WRPERR: u32 = 1 << 17;
const PGSERR: u32 = 1 << 18;
const STRBERR: u32 = 1 << 19;
const INCERR: u32 = 1 << 21;
/// The flags CCR clears.
const SR_CLEARABLE: u32 = 0x0FEF_0000;

// OPTCR.
const OPTLOCK: u32 = 1 << 0;
const OPTSTART: u32 = 1 << 1;
const SWAP_BANK: u32 = 1 << 31;
// OPTSR.
const SWAP_BANK_OPT: u32 = 1 << 31;
/// OPTSR as a part leaves the factory: RDP level 0, the watchdogs started
/// by software, no bank swap.
const OPTSR_FACTORY: u32 = 0x1386_AAF0;
/// OPTSR_PRG's writable bits: all of OPTSR_CUR but its status.
const OPTSR_WRITABLE: u32 = 0xA3FE_FFFC;

#[derive(Clone, Debug)]
struct Bank {
    cr: u32,
    sr: u32,
    key: u8,
    keyed_wrong: bool,
    /// The write buffer: the flash word it is for, its bytes, and which
    /// of them have been written.
    buffer: Option<(usize, [u8; WORD], u32)>,
    other: [u32; 0x40],
}

impl Default for Bank {
    fn default() -> Self {
        Bank { cr: CR_RESET, sr: 0, key: 0, keyed_wrong: false, buffer: None, other: [0; 0x40] }
    }
}

#[derive(Clone, Debug)]
pub struct Interface {
    acr: u32,
    banks: [Bank; 2],
    optcr: u32,
    optkey: u8,
    optsr_cur: u32,
    optsr_prg: u32,
}

impl Default for Interface {
    fn default() -> Self {
        Interface {
            acr: 0x37,
            banks: Default::default(),
            optcr: OPTLOCK,
            optkey: 0,
            optsr_cur: OPTSR_FACTORY,
            optsr_prg: OPTSR_FACTORY,
        }
    }
}

impl Interface {
    /// A system reset: the registers back to their reset values, the
    /// option bytes kept, and a swap they ask for taken — the halves of
    /// `flash` exchanged — if it has not been.
    pub fn reset(&mut self, flash: &mut [u8]) {
        let (optsr, swapped) = (self.optsr_cur, self.optcr & SWAP_BANK != 0);
        *self = Interface { optsr_cur: optsr, optsr_prg: optsr, ..Interface::default() };
        let swap = optsr & SWAP_BANK_OPT != 0;
        if swap != swapped && flash.len() == 2 * BANK {
            let (one, two) = flash.split_at_mut(BANK);
            one.swap_with_slice(two);
        }
        if swap {
            self.optcr |= SWAP_BANK;
        }
    }

    pub fn contains(addr: u32) -> bool {
        (BASE..END).contains(&addr)
    }

    /// Whether the interface's interrupt is asserted: an operation ended,
    /// or failed, with its interrupt enabled.
    pub fn interrupt(&self) -> bool {
        self.banks.iter().any(|bank| bank.sr & bank.cr & CR_INTERRUPTS != 0)
    }

    /// The register `offset` names, and the bank it is one of. The
    /// registers they share are bank 1's, mirrored in bank 2's block.
    fn decode(addr: u32) -> (usize, u32) {
        let offset = addr - BASE;
        ((offset >> 8 & 1) as usize, offset & 0xFF)
    }

    pub fn read(&self, addr: u32) -> u32 {
        let (b, offset) = Self::decode(addr);
        if addr - BASE >= 0x200 {
            return 0;
        }
        let bank = &self.banks[b];
        match offset {
            0x00 => self.acr,
            0x04 | 0x08 => 0,
            0x0C => bank.cr,
            0x10 => bank.sr | if bank.buffer.is_some() { WBNE } else { 0 },
            0x14 => 0,
            0x18 => self.optcr,
            0x1C => self.optsr_cur,
            0x20 => self.optsr_prg,
            // WPSN: no sector write-protected. PRAR, SCAR: start past end.
            0x38 | 0x3C => 0xFF,
            0x28 | 0x2C | 0x30 | 0x34 => 0x0000_00FF,
            0x40 | 0x44 => 0x1FF0_0800,
            offset => bank.other[(offset / 4) as usize],
        }
    }

    /// A register write; `flash` is main flash, which START erases.
    pub fn write(&mut self, addr: u32, value: u32, flash: &mut [u8]) {
        if addr - BASE >= 0x200 {
            return;
        }
        let (b, offset) = Self::decode(addr);
        match offset {
            0x00 => self.acr = value & 0x3F,
            0x04 => {
                let bank = &mut self.banks[b];
                match (bank.key, value) {
                    _ if bank.keyed_wrong => {}
                    (0, KEY1) => bank.key = 1,
                    (1, KEY2) => {
                        bank.key = 0;
                        bank.cr &= !LOCK;
                    }
                    _ => bank.keyed_wrong = true,
                }
            }
            0x08 => match (self.optkey, value) {
                (0, OPTKEY1) => self.optkey = 1,
                (1, OPTKEY2) => {
                    self.optkey = 0;
                    self.optcr &= !OPTLOCK;
                }
                _ => self.optkey = 0,
            },
            0x0C => self.control(b, value, flash),
            0x14 => self.banks[b].sr &= !(value & SR_CLEARABLE),
            0x18 => {
                if self.optcr & OPTLOCK != 0 {
                    return;
                }
                self.optcr = self.optcr & SWAP_BANK | value & !(SWAP_BANK | OPTSTART);
                if value & OPTSTART != 0 {
                    self.optsr_cur = self.optsr_cur & !OPTSR_WRITABLE | self.optsr_prg & OPTSR_WRITABLE;
                }
            }
            0x20 => {
                if self.optcr & OPTLOCK == 0 {
                    self.optsr_prg = value & OPTSR_WRITABLE;
                }
            }
            offset => self.banks[b].other[(offset / 4) as usize] = value,
        }
    }

    fn control(&mut self, b: usize, value: u32, flash: &mut [u8]) {
        let bank = &mut self.banks[b];
        if bank.cr & LOCK != 0 {
            return;
        }
        bank.cr = value & !(START | FW);
        if value & FW != 0 {
            if let Some((word, bytes, _)) = bank.buffer.take() {
                Self::program(bank, flash, word, &bytes);
            }
        }
        if value & START != 0 {
            let start = b * BANK;
            let Some(half) = flash.get_mut(start..start + BANK) else {
                bank.sr |= PGSERR;
                return;
            };
            if value & BER != 0 {
                half.fill(0xFF);
            } else if value & SER != 0 {
                let sector = (value >> 8 & 7) as usize;
                half[sector * SECTOR..(sector + 1) * SECTOR].fill(0xFF);
            } else {
                bank.sr |= PGSERR;
                return;
            }
            bank.sr |= EOP;
        }
    }

    /// A store of `size` bytes at `offset` into main flash: into the write
    /// buffer of the bank it falls in, programming the flash word when the
    /// buffer is full.
    pub fn store(&mut self, flash: &mut [u8], offset: usize, size: u32, value: u32) {
        let b = offset / BANK;
        let Some(bank) = self.banks.get_mut(b) else { return };
        if bank.cr & LOCK != 0 {
            bank.sr |= WRPERR;
            return;
        }
        if bank.cr & PG == 0 {
            bank.sr |= PGSERR;
            return;
        }
        let word = offset & !(WORD - 1);
        let (at, bytes, written) = bank.buffer.get_or_insert((word, [0xFF; WORD], 0));
        if *at != word {
            bank.sr |= INCERR;
            bank.buffer = None;
            return;
        }
        let first = offset - word;
        let lanes = ((1u32 << size) - 1) << first;
        if *written & lanes != 0 {
            bank.sr |= STRBERR;
            bank.buffer = None;
            return;
        }
        *written |= lanes;
        bytes[first..first + size as usize].copy_from_slice(&value.to_le_bytes()[..size as usize]);
        if *written == u32::MAX {
            let (word, bytes, _) = bank.buffer.take().unwrap();
            Self::program(bank, flash, word, &bytes);
        }
    }

    /// Programs a flash word. Programming only clears bits; a word that is
    /// not erased is the part's ECC's to complain about, which this has
    /// none of.
    fn program(bank: &mut Bank, flash: &mut [u8], word: usize, bytes: &[u8; WORD]) {
        for (cell, byte) in flash[word..word + WORD].iter_mut().zip(bytes) {
            *cell &= byte;
        }
        bank.sr |= EOP;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlocked() -> (Interface, Vec<u8>) {
        let mut f = Interface::default();
        let mut flash = vec![0xFF; 2 * BANK];
        for bank in [BASE, BASE + 0x100] {
            f.write(bank + 0x04, KEY1, &mut flash);
            f.write(bank + 0x04, KEY2, &mut flash);
            assert_eq!(f.read(bank + 0x0C) & LOCK, 0);
        }
        (f, flash)
    }

    #[test]
    fn a_flash_word_programs_when_its_last_byte_lands() {
        let (mut f, mut flash) = unlocked();
        f.write(BASE + 0x10C, PG | 0b11 << 4, &mut flash);
        let at = BANK + 0x40;
        for i in 0..7 {
            f.store(&mut flash, at + 4 * i, 4, 0x1111_1111 * i as u32);
        }
        assert_eq!(f.read(BASE + 0x110) & WBNE, WBNE, "the buffer waits");
        assert!(flash[at..at + WORD].iter().all(|&b| b == 0xFF));
        f.store(&mut flash, at + 28, 4, 0x7777_7777);
        assert_eq!(f.read(BASE + 0x110) & (WBNE | EOP), EOP);
        assert_eq!(flash[at + 4..at + 8], [0x11; 4]);

        f.store(&mut flash, at + 0x20, 1, 0);
        f.store(&mut flash, at + 0x40, 1, 0);
        assert_eq!(f.read(BASE + 0x110) & INCERR, INCERR, "another word with the buffer part-full");
        f.write(BASE + 0x114, SR_CLEARABLE, &mut flash);
        f.store(&mut flash, at + 0x60, 1, 0);
        f.write(BASE + 0x10C, PG | FW, &mut flash);
        assert_eq!(flash[at + 0x60..at + 0x62], [0, 0xFF], "FW programs a part-full buffer");
    }

    #[test]
    fn a_sector_erase_erases_its_sector_of_its_bank() {
        let (mut f, mut flash) = unlocked();
        flash.fill(0);
        f.write(BASE + 0x10C, SER | 3 << 8 | START, &mut flash);
        let sector = BANK + 3 * SECTOR;
        assert!(flash[sector..sector + SECTOR].iter().all(|&b| b == 0xFF));
        assert!(flash[3 * SECTOR..4 * SECTOR].iter().all(|&b| b == 0), "bank 1 untouched");
        assert_eq!(f.read(BASE + 0x110) & EOP, EOP);
        f.write(BASE + 0x0C, BER | START, &mut flash);
        assert!(flash[..BANK].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn a_bank_swap_takes_effect_at_reset() {
        let (mut f, mut flash) = unlocked();
        flash[0] = 1;
        flash[BANK] = 2;
        f.write(BASE + 0x08, OPTKEY1, &mut flash);
        f.write(BASE + 0x08, OPTKEY2, &mut flash);
        f.write(BASE + 0x20, f.read(BASE + 0x1C) | SWAP_BANK_OPT, &mut flash);
        f.write(BASE + 0x18, OPTSTART, &mut flash);
        assert_eq!(f.read(BASE + 0x1C) & SWAP_BANK_OPT, SWAP_BANK_OPT);
        assert_eq!(f.read(BASE + 0x18) & SWAP_BANK, 0, "not until reset");
        assert_eq!(flash[0], 1);
        f.reset(&mut flash);
        assert_eq!(f.read(BASE + 0x18) & SWAP_BANK, SWAP_BANK);
        assert_eq!((flash[0], flash[BANK]), (2, 1));
        f.reset(&mut flash);
        assert_eq!((flash[0], flash[BANK]), (2, 1), "a second reset leaves it swapped");
    }
}
