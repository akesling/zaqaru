//! An STM32 GPIO port, as the G0s and the H7s both have it (RM0444 7,
//! RM0433 11): the same registers at the same offsets, but for the G0's BRR.
//!
//! A pin reads in IDR what drives it: its own output when it is one, else
//! whatever outside the chip drives it — a button, say (see
//! [`Gpio::drive`]) — else its pull. An undriven, unpulled input reads low.

/// A change in a port's output data register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// 0 for port A, 1 for B, and so on.
    pub port: u8,
    pub before: u16,
    pub after: u16,
}

#[derive(Clone, Debug)]
pub struct Gpio {
    pub moder: u32,
    pub otyper: u32,
    pub ospeedr: u32,
    pub pupdr: u32,
    pub odr: u32,
    pub lckr: u32,
    pub afr: [u32; 2],
    /// The pins something outside drives, and the levels it drives them to.
    pub driven: u16,
    pub levels: u16,
}

impl Gpio {
    /// A port out of reset, with the reset values its family gives it.
    pub fn new(moder: u32, ospeedr: u32, pupdr: u32) -> Gpio {
        Gpio { moder, otyper: 0, ospeedr, pupdr, odr: 0, lckr: 0, afr: [0; 2], driven: 0, levels: 0 }
    }

    /// A reset of the port's registers. What drives it from outside is
    /// outside, and stays.
    pub fn reset_to(&mut self, reset: &Gpio) {
        *self = Gpio { driven: self.driven, levels: self.levels, ..reset.clone() };
    }

    pub fn idr(&self) -> u32 {
        (0..16)
            .map(|pin| {
                let level = match (self.moder >> (2 * pin) & 3, self.pupdr >> (2 * pin) & 3) {
                    (0b01, _) => self.odr >> pin & 1,
                    _ if self.driven >> pin & 1 != 0 => u32::from(self.levels >> pin & 1),
                    (0b11, _) => 0,
                    (_, 0b01) => 1,
                    _ => 0,
                };
                level << pin
            })
            .sum()
    }

    /// Something outside drives `pin` to `level`, or lets go of it.
    pub fn drive(&mut self, pin: u8, level: Option<bool>) {
        let bit = 1 << pin;
        match level {
            Some(high) => {
                self.driven |= bit;
                self.levels = self.levels & !bit | if high { bit } else { 0 };
            }
            None => self.driven &= !bit,
        }
    }

    pub fn read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => self.moder,
            0x04 => self.otyper,
            0x08 => self.ospeedr,
            0x0C => self.pupdr,
            0x10 => self.idr(),
            0x14 => self.odr,
            0x1C => self.lckr,
            0x20 => self.afr[0],
            0x24 => self.afr[1],
            // BSRR and BRR are write-only.
            _ => 0,
        }
    }

    /// A register write. `brr` says whether the port has a BRR at 0x28.
    pub fn write(&mut self, offset: u32, value: u32, brr: bool) {
        match offset {
            0x00 => self.moder = value,
            0x04 => self.otyper = value & 0xFFFF,
            0x08 => self.ospeedr = value,
            0x0C => self.pupdr = value,
            0x14 => self.odr = value & 0xFFFF,
            // BSRR: set wins over reset when a write asks for both.
            0x18 => self.odr = (self.odr & !(value >> 16)) | (value & 0xFFFF),
            0x1C => self.lckr = value,
            0x20 => self.afr[0] = value,
            0x24 => self.afr[1] = value,
            0x28 if brr => self.odr &= !(value & 0xFFFF),
            _ => {}
        }
    }
}
