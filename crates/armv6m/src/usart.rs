//! The STM32 USART, as the G0s and the H7s both have it, as a serial
//! console: what a byte written to TDR becomes, what a byte arriving is,
//! and when each raises its interrupt.
//!
//! Bytes take the time the wire takes. A frame is a start bit, the data
//! bits CR1's M says (parity among them), and CR2's stop bits, each bit
//! the time BRR (with OVER8) and PRESC give the USART's kernel clock — so
//! a driver that writes TDR sees TXE clear until the shift register takes
//! the byte, and TC clear until its stop bit ends; then the byte is handed
//! to whoever is listening as a [`Byte`]. With FIFOEN, TDR and RDR are
//! eight-deep FIFOs, with their full, empty and threshold flags (RM0433
//! 48.5.3, RM0444 33.5.3).
//!
//! A byte arriving from outside goes onto the line, behind any still
//! arriving, and lands in RDR (or the receive FIFO) when its own frame has
//! passed; one that lands with no room is lost, and says so in ORE.
//!
//! The interrupt is the reference manual's (RM0444 33.8): the USART's one
//! line is asserted while an enabled event holds — TXE with TXEIE, TC with
//! TCIE, RXNE or ORE with RXNEIE, and the FIFO thresholds with their own
//! enables, which the H7's driver uses — and the machine samples it into
//! the NVIC (see `Machine::sample_interrupts`). A level, not an edge, so a
//! driver that leaves TXEIE set with nothing to send is interrupted again,
//! as on the part.

use std::collections::VecDeque;

pub const SIZE: u32 = 0x400;

// CR1.
const UE: u32 = 1 << 0;
const RE: u32 = 1 << 2;
const TE: u32 = 1 << 3;
const RXNEIE: u32 = 1 << 5;
const TCIE: u32 = 1 << 6;
const TXEIE: u32 = 1 << 7;
const M0: u32 = 1 << 12;
const OVER8: u32 = 1 << 15;
const M1: u32 = 1 << 28;
const FIFOEN: u32 = 1 << 29;
// CR3: the FIFO thresholds' interrupts.
const TXFTIE: u32 = 1 << 23;
const RXFTIE: u32 = 1 << 28;
// ISR.
const ORE: u32 = 1 << 3;
const RXNE: u32 = 1 << 5;
const TC: u32 = 1 << 6;
const TXE: u32 = 1 << 7;
const TEACK: u32 = 1 << 21;
const REACK: u32 = 1 << 22;
const TXFE: u32 = 1 << 23;
const RXFF: u32 = 1 << 24;
const RXFT: u32 = 1 << 26;
const TXFT: u32 = 1 << 27;
// ICR.
const ORECF: u32 = 1 << 3;
const TCCF: u32 = 1 << 6;

/// The FIFOs' depth, with FIFOEN.
const FIFO: usize = 8;
/// How many bytes the far end may queue on the line before it is told no.
const LINE: usize = 4096;
/// PRESC's dividers.
const PRESCALERS: [u64; 12] = [1, 2, 4, 6, 8, 10, 12, 16, 32, 64, 128, 256];

/// A byte a USART transmitted: which one (0 for USART1), and the byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Byte {
    pub port: u8,
    pub byte: u8,
}

#[derive(Clone, Debug)]
pub struct Usart {
    cr1: u32,
    cr2: u32,
    cr3: u32,
    brr: u32,
    presc: u32,
    other: [u32; 4],
    /// Picoseconds per cycle of the kernel clock.
    clock: u64,
    /// TDR, or the transmit FIFO: what is waiting for the shift register.
    tx: VecDeque<u8>,
    /// The byte in the shift register, and when its last stop bit ends.
    shifting: Option<(u8, u64)>,
    /// TC, cleared by ICR and by a byte written, set when the line goes
    /// idle.
    tc: bool,
    /// Bytes on their way in, and when the first's frame ends.
    line: VecDeque<u8>,
    line_at: u64,
    /// RDR, or the receive FIFO.
    received: VecDeque<u8>,
    overrun: bool,
    /// What has finished going out on the line, for whoever is listening.
    pub sent: Vec<u8>,
}

impl Default for Usart {
    fn default() -> Self {
        Usart {
            cr1: 0,
            cr2: 0,
            cr3: 0,
            brr: 0,
            presc: 0,
            other: [0; 4],
            clock: crate::clock::Clocks::period(16_000_000),
            tx: VecDeque::new(),
            shifting: None,
            tc: true,
            line: VecDeque::new(),
            line_at: 0,
            received: VecDeque::new(),
            overrun: false,
            sent: Vec::new(),
        }
    }
}

impl Usart {
    /// A reset of the USART's registers; its kernel clock is RCC's.
    pub fn reset(&mut self) {
        *self = Usart { clock: self.clock, ..Usart::default() };
    }

    /// The kernel clock, in picoseconds per cycle.
    pub fn set_clock(&mut self, period: u64) {
        self.clock = period;
    }

    fn depth(&self) -> usize {
        if self.cr1 & FIFOEN != 0 { FIFO } else { 1 }
    }

    /// A bit's time, in picoseconds: BRR's kernel clocks, halved with
    /// OVER8, after PRESC.
    fn bit(&self) -> u64 {
        let brr = u64::from(self.brr.max(16));
        let clocks = if self.cr1 & OVER8 != 0 { ((brr & 0xFFF0) | (brr & 7) << 1) / 2 } else { brr };
        let presc = PRESCALERS.get(self.presc as usize & 0xF).copied().unwrap_or(256);
        clocks * presc * self.clock
    }

    /// A frame's time: start, data and stop bits.
    fn frame(&self) -> u64 {
        let data = match (self.cr1 & M1 != 0, self.cr1 & M0 != 0) {
            (false, false) => 8,
            (false, true) => 9,
            _ => 7,
        };
        // In half bits: 1, 0.5, 2 or 1.5 stop bits.
        let stop = [2, 1, 4, 3][(self.cr2 >> 12 & 3) as usize];
        (2 * (1 + data) + stop) * self.bit() / 2
    }

    /// The shift register takes the next byte waiting, if it is free.
    fn shift_next(&mut self, from: u64) {
        if self.shifting.is_none() {
            if let Some(byte) = self.tx.pop_front() {
                self.shifting = Some((byte, from + self.frame()));
            }
        }
    }

    /// When the next byte finishes going out or coming in.
    pub fn due(&self) -> Option<u64> {
        let out = self.shifting.map(|(_, done)| done);
        let into = (!self.line.is_empty()).then_some(self.line_at);
        out.into_iter().chain(into).min()
    }

    /// Brings the line up to `now`: the bytes whose frames have ended
    /// sent, or received.
    pub fn advance(&mut self, now: u64) {
        while let Some((byte, done)) = self.shifting.filter(|&(_, done)| done <= now) {
            self.sent.push(byte);
            self.shifting = None;
            self.shift_next(done);
            if self.shifting.is_none() {
                self.tc = true;
            }
        }
        while !self.line.is_empty() && self.line_at <= now {
            let byte = self.line.pop_front().unwrap();
            if self.received.len() < self.depth() {
                self.received.push_back(byte);
            } else {
                self.overrun = true;
            }
            self.line_at += self.frame();
        }
    }

    fn threshold(field: u32) -> usize {
        [1, 2, 4, 6, 7, 8].get(field as usize & 7).copied().unwrap_or(8)
    }

    fn isr(&self) -> u32 {
        let depth = self.depth();
        let mut isr = 0;
        // TXE (TXFNF with the FIFO): room in TDR.
        if self.tx.len() < depth {
            isr |= TXE;
        }
        if self.tx.is_empty() {
            isr |= TXFE;
        }
        // TXFT: as many empty places as TXFTCFG asks for.
        if depth - self.tx.len() >= Self::threshold(self.cr3 >> 29).min(depth) {
            isr |= TXFT;
        }
        if self.tc {
            isr |= TC;
        }
        if !self.received.is_empty() {
            isr |= RXNE;
        }
        if self.received.len() >= Self::threshold(self.cr3 >> 25).min(depth) {
            isr |= RXFT;
        }
        if self.received.len() == depth {
            isr |= RXFF;
        }
        if self.overrun {
            isr |= ORE;
        }
        if self.cr1 & TE != 0 {
            isr |= TEACK;
        }
        if self.cr1 & RE != 0 {
            isr |= REACK;
        }
        isr
    }

    /// A register read; `pop` says whether this is a read with effects,
    /// which for RDR takes the byte.
    pub fn read(&mut self, offset: u32, pop: bool) -> u32 {
        match offset {
            0x00 => self.cr1,
            0x04 => self.cr2,
            0x08 => self.cr3,
            0x0C => self.brr,
            0x1C => self.isr(),
            0x24 => u32::from(match pop {
                true => self.received.pop_front(),
                false => self.received.front().copied(),
            }
            .unwrap_or(0)),
            0x2C => self.presc,
            0x10 | 0x14 => self.other[(offset / 4 % 4) as usize],
            _ => 0,
        }
    }

    /// A register write, at time `now`.
    pub fn write(&mut self, offset: u32, value: u32, now: u64) {
        match offset {
            0x00 => {
                self.cr1 = value;
                if value & UE == 0 {
                    // UE clear stops the USART and empties its FIFOs.
                    self.tx.clear();
                    self.shifting = None;
                    self.received.clear();
                    self.tc = true;
                }
            }
            0x04 => self.cr2 = value,
            0x08 => self.cr3 = value,
            0x0C => self.brr = value & 0xFFFF,
            0x20 => {
                if value & ORECF != 0 {
                    self.overrun = false;
                }
                if value & TCCF != 0 {
                    self.tc = false;
                }
            }
            0x28 if self.cr1 & (UE | TE) == UE | TE => {
                if self.tx.len() < self.depth() {
                    self.tx.push_back(value as u8);
                    self.tc = false;
                    self.shift_next(now);
                }
            }
            0x2C => self.presc = value & 0xF,
            0x10 | 0x14 => self.other[(offset / 4 % 4) as usize] = value,
            _ => {}
        }
    }

    /// A byte the far end sends, at `now`: onto the line, behind any still
    /// arriving. Taken only while the receiver is on, and while the far
    /// end's queue has room.
    pub fn receive(&mut self, byte: u8, now: u64) -> bool {
        if self.cr1 & (UE | RE) != UE | RE || self.line.len() >= LINE {
            return false;
        }
        if self.line.is_empty() {
            self.line_at = now + self.frame();
        }
        self.line.push_back(byte);
        true
    }

    /// Whether the USART's interrupt line is asserted.
    pub fn interrupt(&self) -> bool {
        let isr = self.isr();
        self.cr1 & UE != 0
            && (self.cr1 & TXEIE != 0 && isr & TXE != 0
                || self.cr1 & TCIE != 0 && isr & TC != 0
                || self.cr1 & RXNEIE != 0 && isr & (RXNE | ORE) != 0
                || self.cr3 & TXFTIE != 0 && isr & TXFT != 0
                || self.cr3 & RXFTIE != 0 && isr & RXFT != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 115200 baud from a 100 MHz kernel clock: BRR 868, a bit of 8.68 µs.
    fn at_115200() -> Usart {
        let mut u = Usart::default();
        u.set_clock(10_000);
        u.reset();
        u.write(0x0C, 868, 0);
        u
    }

    #[test]
    fn a_byte_takes_its_frame_to_go_and_txeie_is_a_level() {
        let mut u = at_115200();
        u.write(0x28, b'x' as u32, 0);
        assert_eq!(u.due(), None, "not enabled");
        u.write(0x00, UE | TE, 0);
        u.write(0x28, b'P' as u32, 0);
        let frame = 10 * 868 * 10_000;
        assert_eq!(u.due(), Some(frame));
        assert_eq!(u.read(0x1C, false) & (TXE | TC), TXE, "TDR is free again; the line is not");
        u.write(0x28, b'Q' as u32, 0);
        assert_eq!(u.read(0x1C, false) & TXE, 0, "TDR holds Q");
        u.advance(frame);
        assert_eq!(u.sent, b"P");
        assert_eq!(u.read(0x1C, false) & (TXE | TC), TXE);
        u.advance(2 * frame);
        assert_eq!(u.sent, b"PQ");
        assert_eq!(u.read(0x1C, false) & TC, TC, "idle");
        assert!(!u.interrupt());
        u.write(0x00, UE | TE | TXEIE, 2 * frame);
        assert!(u.interrupt(), "TXE holds, so TXEIE interrupts");
    }

    #[test]
    fn a_byte_received_waits_its_frame_then_in_rdr() {
        let mut u = at_115200();
        assert!(!u.receive(1, 0), "the receiver is off");
        u.write(0x00, UE | RE | RXNEIE, 0);
        assert!(u.receive(b'a', 0));
        assert!(u.receive(b'b', 0));
        assert!(!u.interrupt(), "still on the line");
        let frame = 10 * 868 * 10_000;
        u.advance(frame);
        assert!(u.interrupt());
        assert_eq!(u.read(0x24, false), b'a' as u32, "a peek leaves it");
        assert_eq!(u.read(0x24, true), b'a' as u32);
        assert_eq!(u.read(0x1C, false) & RXNE, 0);
        u.advance(2 * frame);
        assert_eq!(u.read(0x24, true), b'b' as u32);
    }

    #[test]
    fn a_byte_landing_with_rdr_full_is_an_overrun() {
        let mut u = at_115200();
        u.write(0x00, UE | RE, 0);
        u.receive(1, 0);
        u.receive(2, 0);
        u.advance(1 << 40);
        assert_eq!(u.read(0x1C, false) & (RXNE | ORE), RXNE | ORE);
    }
}
