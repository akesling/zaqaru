//! The STM32 USART, as the G0s and the H7s both have it, as a serial console: what a byte written to TDR
//! becomes, what a byte arriving is, and when each raises its interrupt.
//!
//! Modelled at the level a driver sees, not the wire's: a byte written to
//! TDR is transmitted at once — TXE and TC never read clear — and handed to
//! whoever is listening as a [`Byte`]. A byte received waits in RDR with
//! RXNE set until it is read, behind a short queue standing in for the line;
//! a byte that arrives with the queue full is an overrun. The baud rate is
//! whatever BRR says and changes nothing.
//!
//! The interrupt is the reference manual's (RM0444 33.8): the USART's one
//! line is asserted while an enabled event holds — TXE with TXEIE, TC with
//! TCIE, RXNE or ORE with RXNEIE, and the FIFO thresholds with their own
//! enables, which the H7's driver uses — and the machine samples it into the NVIC
//! (see `Machine::sample_interrupts`). A level, not an edge, so a driver
//! that leaves TXEIE set with nothing to send is interrupted again, as on
//! the part.

use std::collections::VecDeque;

pub const SIZE: u32 = 0x400;

// CR1.
const UE: u32 = 1 << 0;
const RE: u32 = 1 << 2;
const TE: u32 = 1 << 3;
const RXNEIE: u32 = 1 << 5;
const TCIE: u32 = 1 << 6;
const TXEIE: u32 = 1 << 7;
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
/// The transmit FIFO is empty, and so at any threshold: a byte written
/// leaves at once.
const TXFE: u32 = 1 << 23;
const RXFT: u32 = 1 << 26;
const TXFT: u32 = 1 << 27;
// ICR.
const ORECF: u32 = 1 << 3;
const TCCF: u32 = 1 << 6;

/// How many received bytes wait behind RDR before one is lost.
const QUEUE: usize = 64;

/// A byte a USART transmitted: which one (0 for USART1), and the byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Byte {
    pub port: u8,
    pub byte: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Usart {
    cr1: u32,
    cr2: u32,
    cr3: u32,
    brr: u32,
    other: [u32; 4],
    /// TC, cleared by ICR and set again by the next transmission.
    tc: bool,
    overrun: bool,
    received: VecDeque<u8>,
}

impl Usart {
    pub fn reset(&mut self) {
        *self = Usart { tc: true, ..Usart::default() };
    }

    fn isr(&self) -> u32 {
        let mut isr = TXE | TXFE | TXFT;
        if self.tc {
            isr |= TC;
        }
        if !self.received.is_empty() {
            // One byte waiting is at the threshold for the queue standing
            // in for the FIFO.
            isr |= RXNE | RXFT;
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
            0x10 | 0x14 | 0x2C => self.other[(offset / 4 % 4) as usize],
            _ => 0,
        }
    }

    /// A register write; a byte to transmit if it was one.
    pub fn write(&mut self, offset: u32, value: u32) -> Option<u8> {
        match offset {
            0x00 => self.cr1 = value,
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
                self.tc = true;
                return Some(value as u8);
            }
            0x10 | 0x14 | 0x2C => self.other[(offset / 4 % 4) as usize] = value,
            _ => {}
        }
        None
    }

    /// A byte arriving on the line. Taken only while the receiver is on;
    /// one arriving with the queue full is lost, and says so in ORE.
    pub fn receive(&mut self, byte: u8) -> bool {
        if self.cr1 & (UE | RE) != UE | RE {
            return false;
        }
        if self.received.len() >= QUEUE {
            self.overrun = true;
            return false;
        }
        self.received.push_back(byte);
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

    #[test]
    fn a_byte_written_is_transmitted_and_txeie_is_a_level() {
        let mut u = Usart::default();
        u.reset();
        assert_eq!(u.write(0x28, b'x' as u32), None, "not enabled");
        u.write(0x00, UE | TE);
        assert_eq!(u.write(0x28, b'P' as u32), Some(b'P'));
        assert!(!u.interrupt());
        u.write(0x00, UE | TE | TXEIE);
        assert!(u.interrupt(), "TXE holds, so TXEIE interrupts");
        u.write(0x00, UE | TE);
        assert!(!u.interrupt());
    }

    #[test]
    fn a_byte_received_waits_in_rdr() {
        let mut u = Usart::default();
        u.reset();
        assert!(!u.receive(1), "the receiver is off");
        u.write(0x00, UE | RE | RXNEIE);
        assert!(u.receive(b'a'));
        assert!(u.interrupt());
        assert_eq!(u.read(0x1C, false) & RXNE, RXNE);
        assert_eq!(u.read(0x24, false), b'a' as u32, "a peek leaves it");
        assert_eq!(u.read(0x24, true), b'a' as u32);
        assert_eq!(u.read(0x1C, false) & RXNE, 0);
        assert!(!u.interrupt());
    }
}
