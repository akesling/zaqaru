//! The H7's SPI (RM0433 50), as a master: the block with TSIZE, FIFOs and
//! end-of-transfer, unlike the G0's.
//!
//! A frame written to TXDR while the transfer runs (SPE and CSTART) goes
//! into the transmit FIFO, and the shift register takes frames from it one
//! at a time: each takes its DSIZE bits at SCK — the kernel clock divided
//! by MBR's power of two — and CFG2's MIDI idle clocks after it, and when
//! it has gone the frame shifted in with it lands in the receive FIFO. TXP
//! holds while the FIFO has room and frames of TSIZE are still to be
//! written, RXP while a frame waits to be read, and when the TSIZE-th frame
//! has gone EOT, TXTF and TXC say so; IER's enables make them the
//! interrupt.
//!
//! What answers on MISO is the part on the bus its chip-select selects
//! ([`crate::devices`]), which whoever owns the controller passes to it as
//! the exchange of each frame, at the time the frame ends; with nothing
//! selected, the line idles high, 0xFF.
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

/// The bus, as the controller sees it: the frame it sent at a time, and
/// the frame that came back.
pub type Exchange<'a> = dyn FnMut(u8, u64) -> u8 + 'a;

#[derive(Clone, Debug)]
pub struct Spi {
    cr1: u32,
    cr2: u32,
    cfg1: u32,
    cfg2: u32,
    ier: u32,
    /// SR's sticky flags: EOT, TXTF and the errors.
    flags: u32,
    other: [u32; 4],
    /// Frames sent in this transfer, and written for it.
    sent: u32,
    written: u32,
    /// The transmit FIFO, and the frame in the shift register with when it
    /// is out.
    tx: VecDeque<u8>,
    shifting: Option<(u8, u64)>,
    received: VecDeque<u8>,
    /// Picoseconds per cycle of the kernel clock.
    clock: u64,
}

impl Default for Spi {
    fn default() -> Self {
        Spi {
            cr1: 0,
            cr2: 0,
            cfg1: 0,
            cfg2: 0,
            ier: 0,
            flags: 0,
            other: [0; 4],
            sent: 0,
            written: 0,
            tx: VecDeque::new(),
            shifting: None,
            received: VecDeque::new(),
            clock: crate::clock::Clocks::period(64_000_000),
        }
    }
}

impl Spi {
    /// A reset of the registers; the kernel clock is RCC's.
    pub fn reset(&mut self) {
        *self = Spi { clock: self.clock, ..Spi::default() };
    }

    /// The kernel clock, in picoseconds per cycle.
    pub fn set_clock(&mut self, period: u64) {
        self.clock = period;
    }

    fn running(&self) -> bool {
        self.cr1 & (SPE | CSTART) == SPE | CSTART
    }

    fn tsize(&self) -> u32 {
        self.cr2 & 0xFFFF
    }

    /// A frame's time on the wire: DSIZE + 1 bits at SCK, and MIDI's idle
    /// SCK cycles after.
    fn frame(&self) -> u64 {
        let sck = (2 << (self.cfg1 >> 28 & 7)) * self.clock;
        let bits = u64::from(self.cfg1 & 0x1F) + 1;
        let midi = u64::from(self.cfg2 >> 4 & 0xF);
        (bits.max(4) + midi) * sck
    }

    fn sr(&self) -> u32 {
        let mut sr = self.flags;
        let room = self.tsize() == 0 || self.written < self.tsize();
        if self.cr1 & SPE != 0 && room && self.tx.len() < FIFO && self.received.len() < FIFO {
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

    /// When the frame in the shift register is out.
    pub fn due(&self) -> Option<u64> {
        self.shifting.map(|(_, at)| at)
    }

    /// Brings the bus up to `now`: each frame whose time has passed sent,
    /// and its answer received.
    pub fn advance(&mut self, now: u64, exchange: &mut Exchange) {
        while let Some((byte, at)) = self.shifting.filter(|&(_, at)| at <= now) {
            self.shifting = None;
            let answer = exchange(byte, at);
            if self.received.len() < FIFO {
                self.received.push_back(answer);
            }
            self.sent += 1;
            if self.tsize() != 0 && self.sent == self.tsize() {
                self.flags |= EOT | TXTF;
                self.cr1 &= !CSTART;
            }
            self.shift_next(at);
        }
    }

    fn shift_next(&mut self, from: u64) {
        if self.shifting.is_none() && self.running() {
            if let Some(byte) = self.tx.pop_front() {
                self.shifting = Some((byte, from + self.frame()));
            }
        }
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

    /// A write at time `now`.
    pub fn write(&mut self, offset: u32, value: u32, size: u32, now: u64) {
        match offset {
            0x00 => {
                let starting = value & CSTART != 0 && !self.running();
                self.cr1 = value;
                if value & SPE == 0 {
                    // SPE clear ends the transfer and empties the FIFOs.
                    self.cr1 &= !CSTART;
                    self.received.clear();
                    self.tx.clear();
                    self.shifting = None;
                    self.sent = 0;
                    self.written = 0;
                }
                if starting {
                    self.sent = 0;
                    self.written = 0;
                    self.shift_next(now);
                }
            }
            0x04 => self.cr2 = value,
            0x08 => self.cfg1 = value,
            0x0C => self.cfg2 = value,
            0x10 => self.ier = value,
            0x18 => self.flags &= !(value & IFCR_CLEARS),
            0x20 => {
                for i in 0..size.min(4) {
                    let full = self.tsize() != 0 && self.written >= self.tsize();
                    if self.cr1 & SPE == 0 || full || self.tx.len() >= FIFO {
                        break;
                    }
                    self.tx.push_back((value >> (8 * i)) as u8);
                    self.written += 1;
                }
                self.shift_next(now);
            }
            0x40..=0x4F => self.other[((offset - 0x40) / 4) as usize] = value,
            _ => {}
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
    fn a_transfer_of_tsize_frames_takes_its_time_and_ends_in_eot() {
        let mut spi = Spi::default();
        spi.set_clock(5_000);
        let mut sent = Vec::new();
        let mut bus = |byte: u8, at: u64| {
            sent.push((byte, at));
            !byte
        };
        // MBR 0b011: SCK at the kernel clock over 16; 8-bit frames.
        spi.write(0x08, 0b011 << 28 | 7, 4, 0);
        spi.write(0x04, 3, 4, 0);
        spi.write(0x00, SPE, 4, 0);
        spi.write(0x00, SPE | CSTART, 4, 0);
        assert_eq!(spi.read(0x14, false, 4) & (TXP | EOT), TXP);
        for byte in [1, 2, 3] {
            spi.write(0x20, byte, 1, 0);
        }
        assert_eq!(spi.read(0x14, false, 4) & (TXP | RXP), 0, "all written, none back yet");
        let frame = 8 * 16 * 5_000;
        assert_eq!(spi.due(), Some(frame));
        spi.advance(3 * frame, &mut bus);
        let sr = spi.read(0x14, false, 4);
        assert_eq!(sr & (TXP | EOT | TXTF | RXP), EOT | TXTF | RXP);
        assert_eq!(sr >> 13 & 3, 3, "three frames short of a word");
        spi.write(0x10, 1 << 3, 4, 0);
        assert!(spi.interrupt(), "EOTIE");
        assert_eq!(spi.read(0x30, true, 1), 0xFE, "the bus's answer to the first");
        spi.write(0x18, EOT | TXTF, 4, 0);
        assert!(!spi.interrupt());
        assert_eq!(sent, [(1, frame), (2, 2 * frame), (3, 3 * frame)]);
    }
}
