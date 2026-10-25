//! Which STM32G0 a board carries: what differs between the parts Hubris
//! runs on, as far as this model needs to know.
//!
//! The G0s Hubris has apps for share a core, a peripheral map and a flash
//! interface; what differs is how much flash and RAM there is, and the
//! device ID a debugger reads to tell them apart. So a part is those three,
//! and everything else is [`crate::stm32g0`] for all of them.

/// One part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// The family name, as a board configuration names it: `stm32g031`.
    pub name: &'static str,
    pub flash: usize,
    pub sram: usize,
    /// DBGMCU_IDCODE's DEV_ID.
    pub dev_id: u16,
}

pub const STM32G030: Part = Part { name: "stm32g030", flash: 64 * 1024, sram: 8 * 1024, dev_id: 0x466 };
pub const STM32G031: Part = Part { name: "stm32g031", flash: 64 * 1024, sram: 8 * 1024, dev_id: 0x466 };
pub const STM32G070: Part = Part { name: "stm32g070", flash: 128 * 1024, sram: 36 * 1024, dev_id: 0x460 };

pub const PARTS: [Part; 3] = [STM32G030, STM32G031, STM32G070];

impl Part {
    /// A part by its family name (`stm32g070`), or by a full ordering code
    /// as a Hubris archive or probe-rs names it (`STM32G070RBTx`), whose
    /// tenth character is the flash size: `6` 32 KiB, `8` 64 KiB, `B` 128.
    pub fn named(name: &str) -> Option<Part> {
        let lower = name.to_ascii_lowercase();
        let family = PARTS.iter().find(|part| lower.starts_with(part.name))?;
        let flash = match lower.as_bytes().get(10) {
            Some(b'6') => 32 * 1024,
            Some(b'8') => 64 * 1024,
            Some(b'b') => 128 * 1024,
            _ => family.flash,
        };
        Some(Part { flash, ..*family })
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
        assert_eq!(Part::named("stm32h753"), None);
    }
}
