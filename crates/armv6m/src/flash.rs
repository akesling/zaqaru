//! The G0's flash interface (RM0444 3): how main flash is erased and
//! programmed, which is how a debugger flashes the part.
//!
//! A probe flashes an STM32 by loading a small algorithm into SRAM and
//! running it on the core; the algorithm is what talks to these registers.
//! So what matters is what the algorithm waits on and checks — the unlock
//! sequence, page and mass erase, double-word programming, BSY, EOP and
//! the error flags — and that programming refuses what the part refuses:
//! a double word that is not erased, a word written alone, a store that is
//! not a word, or one made without PG.
//!
//! Every operation completes at once: BSY is never seen set, which is what
//! an algorithm polling it finds on a part fast enough. Option bytes,
//! write protection, PCROP and fast programming are registers that read
//! back and do nothing.


pub const BASE: u32 = 0x4002_2000;
pub const END: u32 = BASE + 0x400;
/// A page, the unit of erase.
pub const PAGE: usize = 2048;

const KEY1: u32 = 0x4567_0123;
const KEY2: u32 = 0xCDEF_89AB;
const OPTKEY1: u32 = 0x0819_2A3B;
const OPTKEY2: u32 = 0x4C5D_6E7F;

// SR.
const EOP: u32 = 1 << 0;
const PROGERR: u32 = 1 << 3;
const PGAERR: u32 = 1 << 5;
const SIZERR: u32 = 1 << 6;
const PGSERR: u32 = 1 << 7;
const MISSERR: u32 = 1 << 8;
/// The flags a write of one clears: EOP and every error.
const SR_CLEARABLE: u32 = 0xC3FB;

// CR.
const PG: u32 = 1 << 0;
const PER: u32 = 1 << 1;
const MER1: u32 = 1 << 2;
/// MER2: no second bank on a G031, but an algorithm written for the parts
/// with one sets it alongside MER1.
const MER2: u32 = 1 << 15;
const STRT: u32 = 1 << 16;
const OPTSTRT: u32 = 1 << 17;
const EOPIE: u32 = 1 << 24;
const OPTLOCK: u32 = 1 << 30;
const LOCK: u32 = 1 << 31;

/// OPTR as a G031 leaves the factory: no read protection, both watchdogs
/// started by software.
const OPTR_FACTORY: u32 = 0xDFFF_E1AA;
/// The write-protection areas, as a part without any: start past end.
const WRP_NONE: u32 = 0x0000_00FF;

#[derive(Clone, Debug)]
pub struct Interface {
    acr: u32,
    sr: u32,
    cr: u32,
    /// How far through the unlock sequence KEYR is: 0, or 1 after KEY1.
    /// A wrong key locks it until reset, as on the part.
    key: u8,
    keyed_wrong: bool,
    optkey: u8,
    optr: u32,
    other: [u32; 0x100],
    /// The first word of a double word being programmed, and its offset.
    half: Option<(usize, u32)>,
}

impl Default for Interface {
    fn default() -> Self {
        let mut other = [0; 0x100];
        other[0x2C / 4] = WRP_NONE;
        other[0x30 / 4] = WRP_NONE;
        Interface {
            acr: 0x0004_0600,
            sr: 0,
            cr: LOCK | OPTLOCK,
            key: 0,
            keyed_wrong: false,
            optkey: 0,
            optr: OPTR_FACTORY,
            other,
            half: None,
        }
    }
}

impl Interface {
    pub fn reset(&mut self) {
        let optr = self.optr;
        *self = Interface { optr, ..Interface::default() };
    }

    pub fn contains(addr: u32) -> bool {
        (BASE..END).contains(&addr)
    }

    pub fn read(&self, addr: u32) -> u32 {
        match addr - BASE {
            0x00 => self.acr,
            0x10 => self.sr,
            0x14 => self.cr,
            0x20 => self.optr,
            // KEYR and OPTKEYR read as zero.
            0x08 | 0x0C => 0,
            offset => self.other[(offset / 4) as usize],
        }
    }

    /// A register write; `flash` is main flash, which STRT erases.
    pub fn write(&mut self, addr: u32, value: u32, flash: &mut [u8]) {
        match addr - BASE {
            0x00 => self.acr = value,
            0x08 => {
                match (self.key, value) {
                    _ if self.keyed_wrong => {}
                    (0, KEY1) => self.key = 1,
                    (1, KEY2) => {
                        self.key = 0;
                        self.cr &= !LOCK;
                    }
                    _ => self.keyed_wrong = true,
                }
            }
            0x0C => match (self.optkey, value) {
                _ if self.cr & LOCK != 0 => {}
                (0, OPTKEY1) => self.optkey = 1,
                (1, OPTKEY2) => {
                    self.optkey = 0;
                    self.cr &= !OPTLOCK;
                }
                _ => self.optkey = 0,
            },
            0x10 => self.sr &= !(value & SR_CLEARABLE),
            0x14 => self.control(value, flash),
            0x20 => {
                if self.cr & OPTLOCK == 0 {
                    self.optr = value;
                }
            }
            offset => self.other[(offset / 4) as usize] = value,
        }
    }

    fn control(&mut self, value: u32, flash: &mut [u8]) {
        if self.cr & LOCK != 0 {
            // Locked, only LOCK itself — already set — means anything.
            return;
        }
        if value & PG == 0 && self.half.take().is_some() {
            // A double word left half-written when programming ended.
            self.sr |= MISSERR;
        }
        self.cr = value & !(STRT | OPTSTRT);
        if value & LOCK != 0 {
            self.cr |= OPTLOCK;
        }
        if value & STRT != 0 {
            self.start(value, flash);
        }
    }

    /// STRT: a page erase, or a mass erase.
    fn start(&mut self, value: u32, flash: &mut [u8]) {
        let erases = value & (PER | MER1 | MER2);
        if erases == 0 || value & PG != 0 || erases & PER != 0 && erases & (MER1 | MER2) != 0 {
            self.sr |= PGSERR;
            return;
        }
        if value & PER != 0 {
            let page = (value >> 3 & 0x3F) as usize;
            match flash.get_mut(page * PAGE..(page + 1) * PAGE) {
                Some(bytes) => bytes.fill(0xFF),
                None => {
                    self.sr |= PGSERR;
                    return;
                }
            }
        } else {
            flash.fill(0xFF);
        }
        self.done();
    }

    fn done(&mut self) {
        if self.cr & EOPIE != 0 {
            self.sr |= EOP;
        }
    }

    /// A store to main flash at `offset`. Programming is by double word:
    /// the first word is held, and the second, at the next address,
    /// programs both — if both are erased, or the double word written is
    /// all zeros, which the part allows over anything.
    pub fn program(&mut self, flash: &mut [u8], offset: usize, size: u32, value: u32) {
        let offset = offset % flash.len();
        if self.cr & PG == 0 || self.cr & LOCK != 0 {
            self.sr |= PGSERR;
            return;
        }
        if size != 4 {
            self.sr |= SIZERR;
            self.half = None;
            return;
        }
        let Some((first, low)) = self.half.take() else {
            if offset % 8 != 0 {
                self.sr |= PGAERR;
            } else {
                self.half = Some((offset, value));
            }
            return;
        };
        if offset != first + 4 {
            self.sr |= PGAERR;
            return;
        }
        let double = (value as u64) << 32 | low as u64;
        let target = &mut flash[first..first + 8];
        if double != 0 && target.iter().any(|&b| b != 0xFF) {
            self.sr |= PROGERR;
            return;
        }
        target.copy_from_slice(&double.to_le_bytes());
        self.done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unlocked() -> (Interface, Vec<u8>) {
        let mut flash_if = Interface::default();
        let mut flash = vec![0xFF; crate::bus::FLASH_SIZE];
        flash_if.write(BASE + 0x08, KEY1, &mut flash);
        flash_if.write(BASE + 0x08, KEY2, &mut flash);
        assert_eq!(flash_if.read(BASE + 0x14) & LOCK, 0);
        (flash_if, flash)
    }

    #[test]
    fn a_double_word_programs_only_when_erased() {
        let (mut f, mut flash) = unlocked();
        f.write(BASE + 0x14, PG | EOPIE, &mut flash);
        f.program(&mut flash, 0x800, 4, 0x1122_3344);
        assert_eq!(flash[0x800], 0xFF, "the first word is held");
        f.program(&mut flash, 0x804, 4, 0x5566_7788);
        assert_eq!(flash[0x800..0x808], [0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]);
        assert_eq!(f.read(BASE + 0x10), EOP);

        f.program(&mut flash, 0x800, 4, 1);
        f.program(&mut flash, 0x804, 4, 2);
        assert_eq!(f.read(BASE + 0x10) & PROGERR, PROGERR, "not erased");
        f.write(BASE + 0x10, SR_CLEARABLE, &mut flash);

        f.program(&mut flash, 0x902, 2, 0);
        assert_eq!(f.read(BASE + 0x10) & SIZERR, SIZERR);
    }

    #[test]
    fn a_page_erase_erases_one_page_and_a_mass_erase_all() {
        let (mut f, mut flash) = unlocked();
        flash.fill(0);
        f.write(BASE + 0x14, PER | 3 << 3 | STRT, &mut flash);
        assert!(flash[3 * PAGE..4 * PAGE].iter().all(|&b| b == 0xFF));
        assert!(flash[2 * PAGE..3 * PAGE].iter().all(|&b| b == 0));
        assert_eq!(f.read(BASE + 0x14) & STRT, 0, "STRT clears itself");
        f.write(BASE + 0x14, MER1 | MER2 | STRT, &mut flash);
        assert!(flash.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn a_wrong_key_locks_until_reset() {
        let mut f = Interface::default();
        let mut flash = vec![0xFF; crate::bus::FLASH_SIZE];
        f.write(BASE + 0x08, KEY2, &mut flash);
        f.write(BASE + 0x08, KEY1, &mut flash);
        f.write(BASE + 0x08, KEY2, &mut flash);
        assert_eq!(f.read(BASE + 0x14) & LOCK, LOCK);
        f.write(BASE + 0x14, PER | STRT, &mut flash);
        f.reset();
        f.write(BASE + 0x08, KEY1, &mut flash);
        f.write(BASE + 0x08, KEY2, &mut flash);
        assert_eq!(f.read(BASE + 0x14) & LOCK, 0);
    }
}
