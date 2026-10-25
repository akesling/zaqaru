//! The H7's Ethernet controller (RM0433 63): the Synopsys MAC, its MTL,
//! and its DMA, with the Nucleo's LAN8742A PHY on MDIO.
//!
//! What a driver sees is modelled: DMAMR's soft reset, done at once; the
//! transmit and receive descriptor rings, which the DMA walks by their OWN
//! bits — a transmit descriptor's buffer becomes a frame on the wire and
//! the descriptor is handed back, a frame from the wire lands in the next
//! receive descriptor's buffer with its FCS and is handed over with FD, LD
//! and its length; DMACSR's status, its summary bits, and the interrupt
//! DMACIER makes of them; and MDIO, whose transactions complete at once, to
//! a PHY that has autonegotiated 100 Mbit/s full duplex with its partner.
//!
//! The DMA reaches memory through the bus, so the work is done there (see
//! `Bus::run_dma`); this module keeps the registers and says what is to
//! be done. Frames carry no timing: one sent is on the wire at once. The
//! checksum offload is not modelled, which Hubris's stack, computing its
//! own, does not notice; neither are the MAC's filters, which promiscuous
//! mode — what Hubris sets — passes everything through anyway.

use std::collections::VecDeque;

pub const BASE: u32 = 0x4002_8000;
pub const END: u32 = BASE + 0x2000;
pub const IRQ: u32 = 61;

// DMA registers, from BASE.
const DMAMR: u32 = 0x1000;
const DMAISR: u32 = 0x1008;
const DMACTXCR: u32 = 0x1104;
const DMACRXCR: u32 = 0x1108;
const DMACTXDLAR: u32 = 0x1114;
const DMACRXDLAR: u32 = 0x111C;
const DMACTXDTPR: u32 = 0x1120;
const DMACTXRLR: u32 = 0x112C;
const DMACRXRLR: u32 = 0x1130;
const DMACIER: u32 = 0x1134;
const DMACCATXDR: u32 = 0x1144;
const DMACCARXDR: u32 = 0x114C;
const DMACSR: u32 = 0x1160;
const DMACMFCR: u32 = 0x116C;
// MAC registers.
const MACVR: u32 = 0x0110;
const MACMDIOAR: u32 = 0x0200;
const MACMDIODR: u32 = 0x0204;

const SWR: u32 = 1 << 0;
const ST: u32 = 1 << 0;
const SR: u32 = 1 << 0;
// DMACSR.
const TI: u32 = 1 << 0;
const TBU: u32 = 1 << 2;
const RI: u32 = 1 << 6;
const RBU: u32 = 1 << 7;
const AIS: u32 = 1 << 14;
const NIS: u32 = 1 << 15;
/// The status bits NIS summarises: TI, TBU, RI, ERI; and those AIS does:
/// TPS, RBU, RPS, RWT, ETI, FBE, CDE.
const NORMAL: u32 = TI | TBU | RI | 1 << 11;
const ABNORMAL: u32 = 1 << 1 | RBU | 1 << 8 | 1 << 9 | 1 << 10 | 1 << 12 | 1 << 13;
// MDIOAR.
const MB: u32 = 1 << 0;

// Descriptor bits.
pub const OWN: u32 = 1 << 31;
pub const IOC: u32 = 1 << 30;
pub const FD: u32 = 1 << 29;
pub const LD: u32 = 1 << 28;
/// A receive descriptor's buffer 1 address is valid.
pub const BUF1V: u32 = 1 << 24;
/// A transmit descriptor's TDES2: interrupt on completion, and buffer 1's
/// length.
const TDES2_IOC: u32 = 1 << 31;

/// The LAN8742A on the Nucleo, at MDIO address 0.
#[derive(Clone, Debug)]
struct Phy {
    bcr: u32,
    anar: u32,
    other: [u16; 32],
}

impl Default for Phy {
    fn default() -> Self {
        Phy { bcr: 0x3100, anar: 0x01E1, other: [0; 32] }
    }
}

impl Phy {
    fn read(&self, register: u32) -> u16 {
        match register {
            0 => self.bcr as u16,
            // BSR: 100BASE-TX and 10BASE-T capable, autonegotiation done,
            // link up.
            1 => 0x782D,
            // The PHY identifier: Microchip's OUI, model 0x13, revision 1.
            2 => 0x0007,
            3 => 0xC131,
            4 => self.anar as u16,
            // The partner offers 100BASE-TX full duplex and the rest.
            5 => 0x45E1,
            6 => 0x0001,
            18 => 0x60E0,
            // PSCSR: autonegotiation done, 100BASE-TX full duplex.
            31 => 0x1058,
            r => self.other[r as usize % 32],
        }
    }

    fn write(&mut self, register: u32, value: u16) {
        match register {
            // A reset and an autonegotiation restart are done as soon as
            // asked for.
            0 if value & 0x8000 != 0 => *self = Phy::default(),
            0 => self.bcr = u32::from(value) & !(1 << 9),
            4 => self.anar = u32::from(value),
            r => self.other[r as usize % 32] = value,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Ethernet {
    registers: Vec<u32>,
    csr: u32,
    phy: Phy,
    /// The descriptors the DMA will look at next, by address.
    pub tx_current: u32,
    pub rx_current: u32,
    /// Whether the transmit DMA has been told there may be work.
    pub tx_kicked: bool,
    /// Frames from the wire waiting for the receive DMA, and frames the
    /// transmit DMA put on the wire, for whoever is at the other end.
    pub incoming: VecDeque<Vec<u8>>,
    pub outgoing: Vec<Vec<u8>>,
    missed: u32,
}

impl Ethernet {
    pub fn reset(&mut self) {
        let incoming = std::mem::take(&mut self.incoming);
        *self = Ethernet { registers: vec![0; 0x2000 / 4], incoming, ..Ethernet::default() };
        // MACCR's and MACMDIOAR's reset values are zero; the version
        // register is the Synopsys core's 4.2.
        self.registers[(MACVR / 4) as usize] = 0x0000_3042;
    }

    pub fn contains(addr: u32) -> bool {
        (BASE..END).contains(&addr)
    }

    fn reg(&self, offset: u32) -> u32 {
        self.registers.get((offset / 4) as usize).copied().unwrap_or(0)
    }

    fn set(&mut self, offset: u32, value: u32) {
        if let Some(slot) = self.registers.get_mut((offset / 4) as usize) {
            *slot = value;
        }
    }

    pub fn read(&self, addr: u32) -> u32 {
        let offset = addr - BASE;
        match offset {
            // SWR is done by the time anything reads it.
            DMAMR => self.reg(DMAMR) & !SWR,
            DMAISR => self.interrupt() as u32,
            DMACSR => self.csr,
            DMACCATXDR => self.tx_current,
            DMACCARXDR => self.rx_current,
            DMACMFCR => self.missed & 0x7FF,
            offset => self.reg(offset),
        }
    }

    pub fn write(&mut self, addr: u32, value: u32) {
        let offset = addr - BASE;
        match offset {
            DMAMR if value & SWR != 0 => self.reset(),
            // DMACSR: a one clears; NIS and AIS are cleared the same way,
            // and set again while what they summarise is.
            DMACSR => self.csr &= !value,
            DMACTXDLAR => {
                self.set(offset, value & !3);
                self.tx_current = value & !3;
            }
            DMACRXDLAR => {
                self.set(offset, value & !3);
                self.rx_current = value & !3;
            }
            DMACTXDTPR => {
                self.set(offset, value);
                self.tx_kicked = true;
            }
            DMACTXCR => {
                self.set(offset, value);
                self.tx_kicked |= value & ST != 0;
            }
            MACMDIOAR => {
                self.set(offset, value & !MB);
                if value & MB != 0 {
                    self.mdio(value);
                }
            }
            offset => self.set(offset, value),
        }
    }

    /// An MDIO transaction: PA in 25:21, RDA in 20:16, GOC in 3:2.
    fn mdio(&mut self, value: u32) {
        let (phy, register, op) = (value >> 21 & 0x1F, value >> 16 & 0x1F, value >> 2 & 3);
        let data = self.reg(MACMDIODR);
        match (phy, op) {
            (0, 0b01) => self.phy.write(register, data as u16),
            (0, 0b11) => self.set(MACMDIODR, data & !0xFFFF | u32::from(self.phy.read(register))),
            // Nothing else is on the bus: a read floats high.
            (_, 0b11) => self.set(MACMDIODR, data | 0xFFFF),
            _ => {}
        }
    }

    pub fn tx_running(&self) -> bool {
        self.reg(DMACTXCR) & ST != 0
    }

    pub fn rx_running(&self) -> bool {
        self.reg(DMACRXCR) & SR != 0
    }

    /// The size of the receive buffers in bytes, set centrally in
    /// DMACRXCR.RBSZ, bits 14:1.
    pub fn rx_buffer_size(&self) -> usize {
        (self.reg(DMACRXCR) >> 1 & 0x3FFF) as usize
    }

    /// The descriptor after `at` in a ring at `base` of `rlr` + 1.
    fn next(at: u32, base: u32, rlr: u32) -> u32 {
        let index = (at - base) / 16;
        if index >= rlr & 0x3FF { base } else { at + 16 }
    }

    pub fn tx_advance(&mut self) {
        self.tx_current = Self::next(self.tx_current, self.reg(DMACTXDLAR), self.reg(DMACTXRLR));
    }

    pub fn rx_advance(&mut self) {
        self.rx_current = Self::next(self.rx_current, self.reg(DMACRXDLAR), self.reg(DMACRXRLR));
    }

    /// A frame sent: TDES2 asked for an interrupt or not.
    pub fn transmitted(&mut self, frame: Vec<u8>, tdes2: u32) {
        self.outgoing.push(frame);
        if tdes2 & TDES2_IOC != 0 {
            self.status(TI);
        }
    }

    /// The transmit DMA found a descriptor it does not own.
    pub fn tx_unavailable(&mut self) {
        self.status(TBU);
    }

    pub fn received(&mut self, rdes3: u32) {
        if rdes3 & IOC != 0 {
            self.status(RI);
        }
    }

    /// The receive DMA had nowhere to put a frame: it is dropped.
    pub fn rx_unavailable(&mut self) {
        self.missed += 1;
        self.status(RBU);
    }

    /// Status bits set, and the summaries of those DMACIER enables: NIS
    /// and AIS say an enabled interrupt happened, not merely a status.
    fn status(&mut self, bits: u32) {
        self.csr |= bits;
        let enabled = bits & self.reg(DMACIER);
        if enabled & NORMAL != 0 {
            self.csr |= NIS;
        }
        if enabled & ABNORMAL != 0 {
            self.csr |= AIS;
        }
    }

    /// The interrupt: a summary set, with its enable (NIE, AIE).
    pub fn interrupt(&self) -> bool {
        self.csr & self.reg(DMACIER) & (NIS | AIS) != 0
    }
}

/// The Ethernet FCS: CRC-32, as IEEE 802.3 appends it.
pub fn fcs(frame: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in frame {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_reset_is_done_at_once_and_mdio_reaches_the_phy() {
        let mut eth = Ethernet::default();
        eth.reset();
        eth.write(BASE + DMAMR, SWR);
        assert_eq!(eth.read(BASE + DMAMR) & SWR, 0);
        eth.write(BASE + MACMDIOAR, 1 << 16 | 0b11 << 2 | MB);
        assert_eq!(eth.read(BASE + MACMDIOAR) & MB, 0, "done at once");
        assert_eq!(eth.read(BASE + MACMDIODR) & 0xFFFF, 0x782D, "link up, autonegotiated");
        eth.write(BASE + MACMDIOAR, 3 << 21 | 2 << 16 | 0b11 << 2 | MB);
        assert_eq!(eth.read(BASE + MACMDIODR) & 0xFFFF, 0xFFFF, "nobody at 3");
    }

    #[test]
    fn the_fcs_is_ieee_crc32() {
        assert_eq!(fcs(b"123456789"), 0xCBF4_3926);
    }
}
