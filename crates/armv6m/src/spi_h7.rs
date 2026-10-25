//! The H7's SPI (RM0433 50), as a master: the block with TSIZE, FIFOs and
//! end-of-transfer, unlike the G0's.
//!
//! A frame written to TXDR while the transfer runs (SPE and CSTART) is
//! shifted out at once, and the frame shifted in with it lands in the
//! receive FIFO. TXP holds while frames of TSIZE are still to be written,
//! RXP while a frame waits to be read, and when the TSIZE-th frame has gone
//! EOT, TXTF and TXC say so; IER's enables make them the interrupt.
//!
//! What answers on MISO is the peripheral on the bus. None is modelled
//! yet: an absent device reads as the line idling high, 0xFF.
//!
//! Frames are bytes (DSIZE up to 8 bits), which is what Hubris's drivers
//! use; a word access to TXDR or RXDR moves four of them, as packing does.

use std::collections::VecDeque;

// CR1.
const SPE: u32 = 1 << 0;
const CSTART: u32 = 1 << 9;
// SR.
const RXP: u32 = 1 << 0;
const TXP: u32 = 1 << 1;
const DXP: u32 = 1 << 2;
const EOT: u32 = 1 << 3;
const TXTF: u32 = 1 << 4;
const TXC: u32 = 1 << 12;
const RXWNE: u32 = 1 << 15;
/// The flags IFCR clears: EOT, TXTF, UDR, OVR, CRCE, TIFRE, MODF, TSERF,
/// SUSP.
const IFCR_CLEARS: u32 = 0x0FF8;

const FIFO: usize = 16;

#[derive(Clone, Debug, Default)]
pub struct Spi {
    cr1: u32,
    cr2: u32,
    cfg1: u32,
    cfg2: u32,
    ier: u32,
    /// SR's sticky flags: EOT, TXTF and the errors.
    flags: u32,
    other: [u32; 4],
    /// Frames sent in this transfer.
    sent: u32,
    received: VecDeque<u8>,
    /// Every frame the bus carried out, for whoever is looking.
    pub transmitted: Vec<u8>,
}

impl Spi {
    pub fn reset(&mut self) {
        *self = Spi::default();
    }

    fn running(&self) -> bool {
        self.cr1 & (SPE | CSTART) == SPE | CSTART
    }

    fn tsize(&self) -> u32 {
        self.cr2 & 0xFFFF
    }

    fn sr(&self) -> u32 {
        let mut sr = self.flags;
        let room = self.tsize() == 0 || self.sent < self.tsize();
        if self.cr1 & SPE != 0 && room && self.received.len() < FIFO {
            sr |= TXP;
        }
        if !self.received.is_empty() {
            sr |= RXP;
        }
        if sr & (TXP | RXP) == TXP | RXP {
            sr |= DXP;
        }
        if self.received.len() >= 4 {
            sr |= RXWNE;
        }
        // RXPLVL: the frames short of a word.
        sr |= ((self.received.len() % 4) as u32) << 13;
        if !self.running() || self.flags & EOT != 0 {
            sr |= TXC;
        }
        let left = self.tsize().saturating_sub(self.sent);
        sr | left << 16
    }

    pub fn read(&mut self, offset: u32, pop: bool, size: u32) -> u32 {
        match offset {
            0x00 => self.cr1,
            0x04 => self.cr2,
            0x08 => self.cfg1,
            0x0C => self.cfg2,
            0x10 => self.ier,
            0x14 => self.sr(),
            0x30 => {
                let frames = size.min(4) as usize;
                let mut word = 0;
                for i in 0..frames {
                    let byte = if pop { self.received.pop_front() } else { self.received.get(i).copied() };
                    word |= u32::from(byte.unwrap_or(0)) << (8 * i);
                }
                word
            }
            0x40..=0x4F => self.other[((offset - 0x40) / 4) as usize],
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u32, value: u32, size: u32) {
        match offset {
            0x00 => {
                let starting = value & CSTART != 0 && !self.running();
                self.cr1 = value;
                if value & SPE == 0 {
                    // SPE clear ends the transfer and empties the FIFOs.
                    self.cr1 &= !CSTART;
                    self.received.clear();
                    self.sent = 0;
                }
                if starting {
                    self.sent = 0;
                }
            }
            0x04 => self.cr2 = value,
            0x08 => self.cfg1 = value,
            0x0C => self.cfg2 = value,
            0x10 => self.ier = value,
            0x18 => self.flags &= !(value & IFCR_CLEARS),
            0x20 => {
                for i in 0..size.min(4) {
                    self.shift((value >> (8 * i)) as u8);
                }
            }
            0x40..=0x4F => self.other[((offset - 0x40) / 4) as usize] = value,
            _ => {}
        }
    }

    /// One frame out, and the one MISO carried in with it.
    fn shift(&mut self, byte: u8) {
        if self.cr1 & SPE == 0 || self.tsize() != 0 && self.sent >= self.tsize() {
            return;
        }
        self.transmitted.push(byte);
        if self.received.len() < FIFO {
            self.received.push_back(0xFF);
        }
        self.sent += 1;
        if self.tsize() != 0 && self.sent == self.tsize() {
            self.flags |= EOT | TXTF;
            self.cr1 &= !CSTART;
        }
    }

    pub fn interrupt(&self) -> bool {
        self.sr() & self.ier & 0x7FF != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_of_tsize_frames_ends_in_eot() {
        let mut spi = Spi::default();
        spi.write(0x04, 3, 4);
        spi.write(0x00, SPE, 4);
        spi.write(0x00, SPE | CSTART, 4);
        assert_eq!(spi.read(0x14, false, 4) & (TXP | EOT), TXP);
        for byte in [1, 2, 3] {
            spi.write(0x20, byte, 1);
        }
        let sr = spi.read(0x14, false, 4);
        assert_eq!(sr & (TXP | EOT | TXTF | RXP), EOT | TXTF | RXP);
        assert_eq!(sr >> 13 & 3, 3, "three frames short of a word");
        spi.write(0x10, 1 << 3, 4);
        assert!(spi.interrupt(), "EOTIE");
        assert_eq!(spi.read(0x30, true, 1), 0xFF);
        spi.write(0x18, EOT | TXTF, 4);
        assert!(!spi.interrupt());
        assert_eq!(spi.transmitted, [1, 2, 3]);
    }
}
