//! What crosses the chip's pins and buses, recorded as it happens, for
//! whoever is watching: a probe on a pin, a bus, a serial line or the
//! Ethernet cable, as a logic analyzer clipped onto the board would see it.
//!
//! Each [`Record`] is stamped with the simulated time it happened at, in
//! picoseconds. A pin's record is a change of its level, whatever drives
//! it — its own output, or something outside such as a button or a part's
//! interrupt line. A bus's are its transactions as the wire carries them:
//! an I2C START with its address, each byte and its acknowledge, the STOP;
//! a SPI chip-select going low, each frame with what went each way, the
//! chip-select going high. A USART's are its bytes, each way, and
//! Ethernet's its frames, each way, as the cable carries them (through the
//! switch, if one is wired).
//!
//! Recording is off until someone turns it on ([`Trace::on`]); then the
//! records wait in [`Trace::records`] for whoever drains them.

/// Something that crossed a pin or a bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A GPIO pin's level changed: port 0 for A.
    Pin { port: u8, pin: u8, high: bool },
    /// On I2C `controller`, 0 for I2C1.
    I2c { controller: u8, event: I2cEvent },
    /// On SPI `controller`, 0 for SPI1.
    Spi { controller: u8, event: SpiEvent },
    /// A byte on USART `port`'s line, 0 for USART1, received or sent.
    Usart { port: u8, received: bool, byte: u8 },
    /// An Ethernet frame on the cable, received or sent: its first bytes,
    /// and how long it was.
    Eth { received: bool, len: u32, head: Vec<u8> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I2cEvent {
    /// A START, or a repeated one, and the address phase: acknowledged by a
    /// target or not.
    Start { address: u8, read: bool, ack: bool },
    /// A byte, written or read.
    Byte { byte: u8 },
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiEvent {
    /// A chip-select went low: its pin.
    Select { port: u8, pin: u8 },
    /// A frame: what went out on MOSI, and what came back on MISO.
    Frame { mosi: u8, miso: u8 },
    /// A chip-select went high again.
    Deselect { port: u8, pin: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Simulated time, in picoseconds.
    pub time: u64,
    pub signal: Signal,
}

/// How much of an Ethernet frame a record keeps: the headers, and the start
/// of what they carry.
pub const FRAME_HEAD: usize = 96;

#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub on: bool,
    pub records: Vec<Record>,
}

impl Trace {
    pub fn record(&mut self, time: u64, signal: Signal) {
        if self.on {
            self.records.push(Record { time, signal });
        }
    }

    /// An Ethernet frame, as a record keeps it.
    pub fn frame(&mut self, time: u64, received: bool, frame: &[u8]) {
        if self.on {
            let head = frame[..frame.len().min(FRAME_HEAD)].to_vec();
            self.record(time, Signal::Eth { received, len: frame.len() as u32, head });
        }
    }
}

impl Signal {
    /// The channel a probe watches it on: `PB0`, `I2C2`, `SPI3`, `USART1`,
    /// `ETH`.
    pub fn channel(&self) -> String {
        match self {
            Signal::Pin { port, pin, .. } => format!("P{}{pin}", (b'A' + port) as char),
            Signal::I2c { controller, .. } => format!("I2C{}", controller + 1),
            Signal::Spi { controller, .. } => format!("SPI{}", controller + 1),
            Signal::Usart { port, .. } => format!("USART{}", port + 1),
            Signal::Eth { .. } => "ETH".to_string(),
        }
    }
}
