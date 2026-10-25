//! The STM32 I2C controller (RM0433 47, RM0444 32: the same block on the H7s
//! and the G0s), as a master, and the targets on its bus.
//!
//! Modelled at the level a driver sees: a START with CR2's address,
//! direction and byte count either finds a target that acknowledges or
//! NACKs — NACKF, and the STOP the controller sends after one — and then
//! each byte moves as soon as the driver moves it: TXIS asks for the next
//! byte to send, RXNE holds one received, TC (or TCR, when RELOAD asks for
//! more) says the count ran out, AUTOEND's STOP or the driver's sets STOPF.
//! BUSY holds from START to STOP. The interrupts are the reference manual's:
//! the event line for TXIS, RXNE, TC, TCR, ADDR, NACKF and STOPF under their
//! enables, the error line for BERR, ARLO and the like under ERRIE.
//!
//! The bus's timing is not modelled: a byte takes no time. Nothing is ever
//! lost in arbitration or held low, so the error flags never set.
//!
//! A target is a register file, as most sensors and EEPROMs are: the first
//! byte written sets its pointer, the rest are stored from it, and a read
//! answers from it, the pointer advancing either way.

// CR1.
const PE: u32 = 1 << 0;
const TXIE: u32 = 1 << 1;
const RXIE: u32 = 1 << 2;
const ADDRIE: u32 = 1 << 3;
const NACKIE: u32 = 1 << 4;
const STOPIE: u32 = 1 << 5;
const TCIE: u32 = 1 << 6;
const ERRIE: u32 = 1 << 7;
// CR2.
const RD_WRN: u32 = 1 << 10;
const START: u32 = 1 << 13;
const STOP: u32 = 1 << 14;
const RELOAD: u32 = 1 << 24;
const AUTOEND: u32 = 1 << 25;
// ISR.
const TXE: u32 = 1 << 0;
const TXIS: u32 = 1 << 1;
const RXNE: u32 = 1 << 2;
const ADDR: u32 = 1 << 3;
const NACKF: u32 = 1 << 4;
const STOPF: u32 = 1 << 5;
const TC: u32 = 1 << 6;
const TCR: u32 = 1 << 7;
const ERRORS: u32 = 0x3F00;
const BUSY: u32 = 1 << 15;
/// ICR's clearable flags, at their ISR positions.
const ICR_CLEARS: u32 = ADDR | NACKF | STOPF | ERRORS;

/// A target on the bus: a register file at a 7-bit address.
#[derive(Clone, Debug)]
pub struct Target {
    pub address: u8,
    pub registers: [u8; 256],
    pointer: u8,
    /// Whether the next byte written sets the pointer.
    addressing: bool,
}

impl Target {
    pub fn new(address: u8) -> Target {
        Target { address, registers: [0; 256], pointer: 0, addressing: false }
    }

    fn start(&mut self, read: bool) {
        self.addressing = !read;
    }

    fn write(&mut self, byte: u8) {
        if self.addressing {
            self.pointer = byte;
            self.addressing = false;
        } else {
            self.registers[self.pointer as usize] = byte;
            self.pointer = self.pointer.wrapping_add(1);
        }
    }

    fn read(&mut self) -> u8 {
        let byte = self.registers[self.pointer as usize];
        self.pointer = self.pointer.wrapping_add(1);
        byte
    }
}

#[derive(Clone, Debug, Default)]
pub struct I2c {
    cr1: u32,
    cr2: u32,
    oar1: u32,
    oar2: u32,
    timingr: u32,
    timeoutr: u32,
    /// ISR's flags, but TXE and BUSY, which are computed.
    isr: u32,
    rxdr: u8,
    /// The transfer in progress: the target it is with, if one answered,
    /// and how many bytes of the count are left.
    with: Option<usize>,
    left: u32,
    busy: bool,
    pub targets: Vec<Target>,
}

impl I2c {
    /// A reset of the controller's registers. The targets are on the bus,
    /// not in the controller, and stay.
    pub fn reset(&mut self) {
        let targets = std::mem::take(&mut self.targets);
        *self = I2c { targets, ..I2c::default() };
    }

    fn isr(&self) -> u32 {
        let mut isr = self.isr | TXE;
        if self.busy {
            isr |= BUSY;
        }
        isr
    }

    pub fn read(&mut self, offset: u32, pop: bool) -> u32 {
        match offset {
            0x00 => self.cr1,
            0x04 => self.cr2,
            0x08 => self.oar1,
            0x0C => self.oar2,
            0x10 => self.timingr,
            0x14 => self.timeoutr,
            0x18 => self.isr(),
            0x24 => {
                let byte = u32::from(self.rxdr);
                if pop && self.isr & RXNE != 0 {
                    self.isr &= !RXNE;
                    self.next_read();
                }
                byte
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u32, value: u32) {
        match offset {
            0x00 => {
                self.cr1 = value;
                if value & PE == 0 {
                    // PE clear is a software reset of the state machines.
                    self.isr = 0;
                    self.busy = false;
                    self.with = None;
                }
            }
            0x04 => self.control(value),
            0x08 => self.oar1 = value,
            0x0C => self.oar2 = value,
            0x10 => self.timingr = value,
            0x14 => self.timeoutr = value,
            // ISR: TXE and TXIS may be set by software, to flush TXDR and
            // to ask for a byte; neither changes anything here.
            0x18 => {}
            0x1C => self.isr &= !(value & ICR_CLEARS),
            0x28 => self.transmit(value as u8),
            _ => {}
        }
    }

    fn control(&mut self, value: u32) {
        let reloading = self.isr & TCR != 0 && value & START == 0;
        self.cr2 = value & !(START | STOP);
        if self.cr1 & PE == 0 {
            return;
        }
        if value & START != 0 {
            self.start();
        } else if reloading && value >> 16 & 0xFF != 0 {
            // NBYTES rewritten after TCR: the transfer goes on.
            self.isr &= !TCR;
            self.left = value >> 16 & 0xFF;
            self.resume();
        }
        if value & STOP != 0 {
            self.stop();
        }
    }

    /// A START, or a repeated one: the address phase.
    fn start(&mut self) {
        self.isr &= !(TC | TCR | TXIS | RXNE);
        self.busy = true;
        let address = (self.cr2 >> 1 & 0x7F) as u8;
        let read = self.cr2 & RD_WRN != 0;
        self.with = self.targets.iter().position(|t| t.address == address);
        match self.with {
            None => {
                // No acknowledge: NACKF, and the STOP that follows one.
                self.isr |= NACKF;
                self.stop();
            }
            Some(n) => {
                self.targets[n].start(read);
                self.left = self.cr2 >> 16 & 0xFF;
                self.resume();
            }
        }
    }

    /// Asks for, or delivers, the next byte of the count, or ends it.
    fn resume(&mut self) {
        if self.left == 0 {
            self.counted_out();
            return;
        }
        if self.cr2 & RD_WRN != 0 {
            self.next_read();
        } else {
            self.isr |= TXIS;
        }
    }

    fn next_read(&mut self) {
        let Some(n) = self.with else { return };
        if self.left == 0 {
            self.counted_out();
            return;
        }
        self.rxdr = self.targets[n].read();
        self.left -= 1;
        self.isr |= RXNE;
    }

    fn transmit(&mut self, byte: u8) {
        let Some(n) = self.with.filter(|_| self.isr & TXIS != 0) else { return };
        self.isr &= !TXIS;
        self.targets[n].write(byte);
        self.left -= 1;
        self.resume();
    }

    /// The count ran out: more to come, a STOP, or the driver's next move.
    fn counted_out(&mut self) {
        if self.cr2 & RD_WRN != 0 && self.isr & RXNE != 0 {
            // The last byte waits to be read first.
            return;
        }
        if self.cr2 & RELOAD != 0 {
            self.isr |= TCR;
        } else if self.cr2 & AUTOEND != 0 {
            self.stop();
        } else {
            self.isr |= TC;
        }
    }

    fn stop(&mut self) {
        self.isr = self.isr & !(TC | TCR | TXIS) | STOPF;
        self.busy = false;
        self.with = None;
    }

    /// The event interrupt line, and the error line.
    pub fn interrupts(&self) -> (bool, bool) {
        let isr = self.isr;
        let enabled = |flag: u32, enable: u32| isr & flag != 0 && self.cr1 & enable != 0;
        let event = enabled(TXIS, TXIE)
            || enabled(RXNE, RXIE)
            || enabled(ADDR, ADDRIE)
            || enabled(NACKF, NACKIE)
            || enabled(STOPF, STOPIE)
            || enabled(TC | TCR, TCIE);
        (self.cr1 & PE != 0 && event, self.cr1 & PE != 0 && enabled(ERRORS, ERRIE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> I2c {
        let mut i2c = I2c::default();
        let mut sensor = Target::new(0x48);
        sensor.registers[0..2].copy_from_slice(&[0x19, 0x80]);
        i2c.targets.push(sensor);
        i2c.write(0x00, PE | TXIE | RXIE | NACKIE | STOPIE | TCIE | ERRIE);
        i2c
    }

    #[test]
    fn nobody_at_an_address_is_a_nack_and_a_stop() {
        let mut i2c = enabled();
        i2c.write(0x04, 0x50 << 1 | 1 << 16 | START);
        let isr = i2c.read(0x18, true);
        assert_eq!(isr & (NACKF | STOPF | BUSY | TXIS), NACKF | STOPF);
        assert!(i2c.interrupts().0);
        i2c.write(0x1C, NACKF | STOPF);
        assert!(!i2c.interrupts().0);
    }

    #[test]
    fn a_register_read_is_a_write_of_the_pointer_and_a_repeated_start() {
        let mut i2c = enabled();
        i2c.write(0x04, 0x48 << 1 | 1 << 16 | START);
        assert_eq!(i2c.read(0x18, true) & (TXIS | BUSY), TXIS | BUSY);
        i2c.write(0x28, 0x00);
        assert_eq!(i2c.read(0x18, true) & (TC | TXIS), TC, "the count ran out with no AUTOEND");
        i2c.write(0x04, 0x48 << 1 | RD_WRN | 2 << 16 | START);
        let mut read = vec![];
        while i2c.read(0x18, true) & RXNE != 0 {
            read.push(i2c.read(0x24, true) as u8);
        }
        assert_eq!(read, [0x19, 0x80]);
        assert_eq!(i2c.read(0x18, true) & TC, TC);
        i2c.write(0x04, STOP);
        assert_eq!(i2c.read(0x18, true) & (STOPF | BUSY), STOPF);
    }

    #[test]
    fn reload_waits_for_the_count_to_be_rewritten() {
        let mut i2c = enabled();
        i2c.write(0x04, 0x48 << 1 | RD_WRN | RELOAD | 1 << 16 | START);
        assert_eq!(i2c.read(0x24, true), 0x19);
        assert_eq!(i2c.read(0x18, true) & TCR, TCR);
        i2c.write(0x04, 0x48 << 1 | RD_WRN | AUTOEND | 1 << 16);
        assert_eq!(i2c.read(0x24, true), 0x80);
        assert_eq!(i2c.read(0x18, true) & (STOPF | BUSY), STOPF, "AUTOEND sent the STOP");
    }
}
