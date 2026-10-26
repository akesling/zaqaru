//! The STM32 I2C controller (RM0433 47, RM0444 32: the same block on the H7s
//! and the G0s), as a master, and the targets on its bus.
//!
//! Modelled at the level a driver sees: a START with CR2's address,
//! direction and byte count either finds a target that acknowledges or
//! NACKs — NACKF, and the STOP the controller sends after one — and then
//! each byte moves as the driver moves it: TXIS asks for the next byte to
//! send, RXNE holds one received, TC (or TCR, when RELOAD asks for more)
//! says the count ran out, AUTOEND's STOP or the driver's sets STOPF. BUSY
//! holds from START to STOP. The interrupts are the reference manual's:
//! the event line for TXIS, RXNE, TC, TCR, ADDR, NACKF and STOPF under their
//! enables, the error line for BERR, ARLO and the like under ERRIE.
//!
//! The bus takes its time. A bit is an SCL period, TIMINGR's SCLL and SCLH
//! low and high periods of its prescaled kernel clock (RM0433 47.4.9): the
//! START and address are ten of them, a byte and its acknowledge nine, the
//! STOP one, and what each one makes happen — TXIS for the next byte, RXNE,
//! NACKF, STOPF — happens when it ends. Nothing is ever lost in
//! arbitration or held low, so the error flags never set.
//!
//! The targets are the parts wired to the bus ([`crate::devices`]).

use crate::devices::I2cPart;
use crate::trace::I2cEvent;

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

/// What the bus is in the middle of, which happens when it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The START and the address byte: acknowledged or not.
    Address,
    /// A byte written, and its acknowledge.
    Sent(u8),
    /// A byte read.
    Received,
    Stop,
}

#[derive(Clone, Debug)]
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
    /// What the bus is doing, and when it is done.
    pending: Option<(u64, Step)>,
    /// Picoseconds per cycle of the kernel clock.
    clock: u64,
    pub targets: Vec<I2cPart>,
    /// What the bus carried, and when, for whoever is watching.
    pub events: Vec<(u64, I2cEvent)>,
}

impl Default for I2c {
    fn default() -> Self {
        I2c {
            cr1: 0,
            cr2: 0,
            oar1: 0,
            oar2: 0,
            timingr: 0,
            timeoutr: 0,
            isr: 0,
            rxdr: 0,
            with: None,
            left: 0,
            busy: false,
            pending: None,
            clock: crate::clock::Clocks::period(64_000_000),
            targets: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl I2c {
    /// A reset of the controller's registers. The targets are on the bus,
    /// not in the controller, and stay; the kernel clock is RCC's.
    pub fn reset(&mut self) {
        let targets = std::mem::take(&mut self.targets);
        *self = I2c { targets, clock: self.clock, ..I2c::default() };
    }

    /// The kernel clock, in picoseconds per cycle.
    pub fn set_clock(&mut self, period: u64) {
        self.clock = period;
    }

    /// An SCL period: SCLL + 1 and SCLH + 1 cycles of the kernel clock
    /// after PRESC. With TIMINGR never written, standard mode's 10 µs.
    fn bit(&self) -> u64 {
        if self.timingr == 0 {
            return 10_000_000;
        }
        let presc = u64::from(self.timingr >> 28) + 1;
        let low = u64::from(self.timingr & 0xFF) + 1;
        let high = u64::from(self.timingr >> 8 & 0xFF) + 1;
        (low + high) * presc * self.clock
    }

    fn after(&mut self, now: u64, bits: u64, step: Step) {
        self.pending = Some((now + bits * self.bit(), step));
    }

    /// When what the bus is doing ends.
    pub fn due(&self) -> Option<u64> {
        self.pending.map(|(at, _)| at)
    }

    /// Brings the bus up to `now`: whatever it was doing, done.
    pub fn advance(&mut self, now: u64) {
        while let Some((at, step)) = self.pending.filter(|&(at, _)| at <= now) {
            self.pending = None;
            match step {
                Step::Address => self.addressed(at),
                Step::Sent(byte) => {
                    self.events.push((at, I2cEvent::Byte { byte }));
                    if let Some(n) = self.with {
                        self.targets[n].write(byte);
                    }
                    self.left -= 1;
                    self.resume(at);
                }
                Step::Received => {
                    if let Some(n) = self.with {
                        self.rxdr = self.targets[n].read();
                        self.events.push((at, I2cEvent::Byte { byte: self.rxdr }));
                        self.left -= 1;
                        self.isr |= RXNE;
                    }
                }
                Step::Stop => {
                    self.events.push((at, I2cEvent::Stop));
                    self.isr |= STOPF;
                    self.busy = false;
                    if let Some(n) = self.with.take() {
                        self.targets[n].stop(at);
                    }
                }
            }
        }
    }

    fn isr(&self) -> u32 {
        let mut isr = self.isr | TXE;
        if self.busy {
            isr |= BUSY;
        }
        isr
    }

    pub fn read(&mut self, offset: u32, pop: bool, now: u64) -> u32 {
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
                    self.resume(now);
                }
                byte
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u32, value: u32, now: u64) {
        match offset {
            0x00 => {
                self.cr1 = value;
                if value & PE == 0 {
                    // PE clear is a software reset of the state machines.
                    self.isr = 0;
                    self.busy = false;
                    self.with = None;
                    self.pending = None;
                }
            }
            0x04 => self.control(value, now),
            0x08 => self.oar1 = value,
            0x0C => self.oar2 = value,
            0x10 => self.timingr = value,
            0x14 => self.timeoutr = value,
            // ISR: TXE and TXIS may be set by software, to flush TXDR and
            // to ask for a byte; neither changes anything here.
            0x18 => {}
            0x1C => self.isr &= !(value & ICR_CLEARS),
            0x28 => self.transmit(value as u8, now),
            _ => {}
        }
    }

    fn control(&mut self, value: u32, now: u64) {
        let reloading = self.isr & TCR != 0 && value & START == 0;
        self.cr2 = value & !(START | STOP);
        if self.cr1 & PE == 0 {
            return;
        }
        if value & START != 0 {
            // A START, or a repeated one: the address phase begins.
            self.isr &= !(TC | TCR | TXIS | RXNE);
            self.busy = true;
            self.after(now, 10, Step::Address);
        } else if reloading && value >> 16 & 0xFF != 0 {
            // NBYTES rewritten after TCR: the transfer goes on.
            self.isr &= !TCR;
            self.left = value >> 16 & 0xFF;
            self.resume(now);
        }
        if value & STOP != 0 {
            self.stop(now);
        }
    }

    /// The address phase ended: a target acknowledged, or nobody did.
    fn addressed(&mut self, at: u64) {
        let address = (self.cr2 >> 1 & 0x7F) as u8;
        let read = self.cr2 & RD_WRN != 0;
        self.with = self.targets.iter_mut().position(|t| t.address() == address && t.start(read, at));
        self.events.push((at, I2cEvent::Start { address, read, ack: self.with.is_some() }));
        match self.with {
            None => {
                // No acknowledge: NACKF, and the STOP that follows one.
                self.isr |= NACKF;
                self.stop(at);
            }
            Some(_) => {
                self.left = self.cr2 >> 16 & 0xFF;
                self.resume(at);
            }
        }
    }

    /// Asks for, or starts receiving, the next byte of the count, or ends
    /// it.
    fn resume(&mut self, now: u64) {
        if self.with.is_none() {
            return;
        }
        if self.left == 0 {
            self.counted_out(now);
        } else if self.cr2 & RD_WRN != 0 {
            self.after(now, 9, Step::Received);
        } else {
            self.isr |= TXIS;
        }
    }

    fn transmit(&mut self, byte: u8, now: u64) {
        if self.with.is_none() || self.isr & TXIS == 0 {
            return;
        }
        self.isr &= !TXIS;
        self.after(now, 9, Step::Sent(byte));
    }

    /// The count ran out: more to come, a STOP, or the driver's next move.
    fn counted_out(&mut self, now: u64) {
        if self.cr2 & RD_WRN != 0 && self.isr & RXNE != 0 {
            // The last byte waits to be read first.
            return;
        }
        if self.cr2 & RELOAD != 0 {
            self.isr |= TCR;
        } else if self.cr2 & AUTOEND != 0 {
            self.stop(now);
        } else {
            self.isr |= TC;
        }
    }

    fn stop(&mut self, now: u64) {
        self.isr &= !(TC | TCR | TXIS);
        self.after(now, 1, Step::Stop);
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

    /// 100 kHz: an SCL period of 10 µs.
    const BIT: u64 = 10_000_000;

    fn enabled() -> I2c {
        let mut i2c = I2c::default();
        let mut sensor = crate::devices::Registers::new(0x48);
        sensor.registers[0..2].copy_from_slice(&[0x19, 0x80]);
        i2c.targets.push(I2cPart::Registers(sensor));
        i2c.write(0x00, PE | TXIE | RXIE | NACKIE | STOPIE | TCIE | ERRIE, 0);
        i2c
    }

    /// Runs the bus until it has nothing left to do, from `now`.
    fn settle(i2c: &mut I2c, now: &mut u64) {
        while let Some(at) = i2c.due() {
            *now = at;
            i2c.advance(at);
        }
    }

    #[test]
    fn nobody_at_an_address_is_a_nack_and_a_stop() {
        let mut i2c = enabled();
        i2c.write(0x04, 0x50 << 1 | 1 << 16 | START, 0);
        assert_eq!(i2c.read(0x18, true, 0) & (BUSY | NACKF), BUSY, "the address is still going out");
        assert_eq!(i2c.due(), Some(10 * BIT));
        let mut now = 0;
        settle(&mut i2c, &mut now);
        assert_eq!(now, 11 * BIT, "the address, then the STOP");
        let isr = i2c.read(0x18, true, now);
        assert_eq!(isr & (NACKF | STOPF | BUSY | TXIS), NACKF | STOPF);
        assert!(i2c.interrupts().0);
        i2c.write(0x1C, NACKF | STOPF, now);
        assert!(!i2c.interrupts().0);
    }

    #[test]
    fn a_register_read_is_a_write_of_the_pointer_and_a_repeated_start() {
        let mut i2c = enabled();
        let mut now = 0;
        i2c.write(0x04, 0x48 << 1 | 1 << 16 | START, now);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x18, true, now) & (TXIS | BUSY), TXIS | BUSY);
        i2c.write(0x28, 0x00, now);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x18, true, now) & (TC | TXIS), TC, "the count ran out with no AUTOEND");
        i2c.write(0x04, 0x48 << 1 | RD_WRN | 2 << 16 | START, now);
        let mut read = vec![];
        loop {
            settle(&mut i2c, &mut now);
            if i2c.read(0x18, true, now) & RXNE == 0 {
                break;
            }
            read.push(i2c.read(0x24, true, now) as u8);
        }
        assert_eq!(read, [0x19, 0x80]);
        assert_eq!(now, (10 + 9 + 10 + 9 + 9) * BIT);
        assert_eq!(i2c.read(0x18, true, now) & TC, TC);
        i2c.write(0x04, STOP, now);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x18, true, now) & (STOPF | BUSY), STOPF);
    }

    #[test]
    fn reload_waits_for_the_count_to_be_rewritten() {
        let mut i2c = enabled();
        let mut now = 0;
        i2c.write(0x04, 0x48 << 1 | RD_WRN | RELOAD | 1 << 16 | START, now);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x24, true, now), 0x19);
        assert_eq!(i2c.read(0x18, true, now) & TCR, TCR);
        i2c.write(0x04, 0x48 << 1 | RD_WRN | AUTOEND | 1 << 16, now);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x24, true, now), 0x80);
        settle(&mut i2c, &mut now);
        assert_eq!(i2c.read(0x18, true, now) & (STOPF | BUSY), STOPF, "AUTOEND sent the STOP");
    }

    #[test]
    fn timingr_sets_the_bit() {
        let mut i2c = I2c::default();
        i2c.set_clock(10_000);
        // PRESC 1, SCLL 0x13, SCLH 0x0F: 2 × 36 cycles of 10 ns.
        i2c.write(0x10, 1 << 28 | 0x0F << 8 | 0x13, 0);
        assert_eq!(i2c.bit(), 2 * 36 * 10_000);
    }
}
