//! Which STM32 a board carries: what differs between the parts Hubris
//! runs on, as far as this model needs to know.
//!
//! Two families. The G0s Hubris has apps for share a core, a peripheral
//! map and a flash interface; what differs is how much flash and RAM there
//! is, and the device ID a debugger reads to tell them apart. The H7s it
//! has apps for, the H743 and the H753, are one die as far as software
//! sees, the 753 adding the crypto block. So a part is its family and
//! those few numbers, and everything else is the family's: its
//! peripherals ([`crate::stm32g0`], [`crate::stm32h7`]) and its memory map
//! ([`crate::bus`]).

use crate::state::Core;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    G0,
    H7,
}

/// One part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// The family name, as a board configuration names it: `stm32g031`.
    pub name: &'static str,
    pub family: Family,
    pub flash: usize,
    /// The G0's one SRAM. An H7's RAMs are its family's map.
    pub sram: usize,
    /// DBGMCU_IDCODE's DEV_ID.
    pub dev_id: u16,
}

pub const STM32G030: Part = Part { name: "stm32g030", family: Family::G0, flash: 64 * 1024, sram: 8 * 1024, dev_id: 0x466 };
pub const STM32G031: Part = Part { name: "stm32g031", family: Family::G0, flash: 64 * 1024, sram: 8 * 1024, dev_id: 0x466 };
pub const STM32G070: Part = Part { name: "stm32g070", family: Family::G0, flash: 128 * 1024, sram: 36 * 1024, dev_id: 0x460 };
pub const STM32H743: Part = Part { name: "stm32h743", family: Family::H7, flash: 2048 * 1024, sram: 0, dev_id: 0x450 };
pub const STM32H753: Part = Part { name: "stm32h753", family: Family::H7, flash: 2048 * 1024, sram: 0, dev_id: 0x450 };

pub const PARTS: [Part; 5] = [STM32G030, STM32G031, STM32G070, STM32H743, STM32H753];

impl Part {
    /// A part by its family name (`stm32g070`), or by a full ordering code
    /// as a Hubris archive or probe-rs names it (`STM32G070RBTx`), whose
    /// tenth character is a G0's flash size: `6` 32 KiB, `8` 64 KiB, `B`
    /// 128.
    pub fn named(name: &str) -> Option<Part> {
        let lower = name.to_ascii_lowercase();
        let family = PARTS.iter().find(|part| lower.starts_with(part.name))?;
        if family.family == Family::H7 {
            return Some(*family);
        }
        let flash = match lower.as_bytes().get(10) {
            Some(b'6') => 32 * 1024,
            Some(b'8') => 64 * 1024,
            Some(b'b') => 128 * 1024,
            _ => family.flash,
        };
        Some(Part { flash, ..*family })
    }

    /// The processor the part is built around.
    pub fn core(&self) -> Core {
        match self.family {
            Family::G0 => Core::CortexM0Plus,
            Family::H7 => Core::CortexM7,
        }
    }

    /// Where the processor boots: the address its vector table is read
    /// from at reset, and VTOR's reset value. A G0 aliases flash at zero; an
    /// H7 boots from BOOT_ADD0, which leaves the factory as flash's own
    /// address.
    pub fn boot(&self) -> u32 {
        match self.family {
            Family::G0 => 0,
            Family::H7 => crate::bus::FLASH_BASE,
        }
    }

    /// The clock Hubris runs the part's core at, in cycles per
    /// microsecond: the G0 demos' 16 MHz HSI, the H7's 400 MHz PLL.
    pub fn mhz(&self) -> u64 {
        match self.family {
            Family::G0 => 16,
            Family::H7 => 400,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_is_named_by_family_or_ordering_code() {
        assert_eq!(Part::named("stm32g031"), Some(STM32G031));
        assert_eq!(Part::named("STM32G031Y8Yx"), Some(STM32G031));
        assert_eq!(Part::named("STM32G070RBTx"), Some(STM32G070));
        assert_eq!(Part::named("STM32G030F6Px").map(|p| (p.flash, p.sram)), Some((32 * 1024, 8 * 1024)));
        assert_eq!(Part::named("STM32H753ZITx"), Some(STM32H753));
        assert_eq!(Part::named("stm32h743"), Some(STM32H743));
        assert_eq!(Part::named("stm32f407"), None);
    }
}
