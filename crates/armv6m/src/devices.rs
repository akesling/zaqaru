//! The parts wired to a board's buses, outside the chip: what answers an
//! I2C address, and what a SPI chip-select selects.
//!
//! Each is modelled as its datasheet has it, as far as a driver sees:
//!
//! - [`Tmp117`], TI's temperature sensor: 16-bit registers behind a
//!   pointer, the temperature in 1/128 °C, the device ID 0x0117.
//! - [`At24`], an AT24C02 EEPROM: 256 bytes behind an address byte, written
//!   a page of 8 at a time, read on from wherever the address is; for the
//!   5 ms after a write it is busy writing, and does not acknowledge.
//! - [`Registers`], a plain register file, as most sensors are.
//! - [`NorFlash`], a Winbond W25Q80 SPI NOR flash: its JEDEC ID, reads,
//!   write-enable, page programs that only clear bits, and sector, block
//!   and chip erases, each busy for its datasheet's typical time.
//! - [`Rot`], the root of trust a Hubris service processor talks to over
//!   SPI: the SP-to-RoT protocol (`drv/sprot-api`) as the LPC55's server
//!   answers it, with its ROT_IRQ line to say a response is ready, a while
//!   after the request.
//!
//! A board says what it has wired, a line each, in the form [`Wiring`]
//! parses: `i2c I2C2 0x48 tmp117`, `spi SPI1 PD14 w25q80`, `spi SPI3 PA15
//! rot PD0`.

/// A part on an I2C bus.
#[derive(Clone, Debug)]
pub enum I2cPart {
    Tmp117(Tmp117),
    At24(At24),
    Registers(Registers),
}

impl I2cPart {
    pub fn address(&self) -> u8 {
        match self {
            I2cPart::Tmp117(p) => p.address,
            I2cPart::At24(p) => p.address,
            I2cPart::Registers(p) => p.address,
        }
    }

    /// A START (or a repeated one) addressed to it at `now`, reading or
    /// writing: whether it acknowledges.
    pub fn start(&mut self, read: bool, now: u64) -> bool {
        match self {
            I2cPart::Tmp117(p) => p.start(read),
            I2cPart::At24(p) => return p.start(read, now),
            I2cPart::Registers(p) => p.start(read),
        }
        true
    }

    pub fn write(&mut self, byte: u8) {
        match self {
            I2cPart::Tmp117(p) => p.write(byte),
            I2cPart::At24(p) => p.write(byte),
            I2cPart::Registers(p) => p.write(byte),
        }
    }

    pub fn read(&mut self) -> u8 {
        match self {
            I2cPart::Tmp117(p) => p.read(),
            I2cPart::At24(p) => p.read(),
            I2cPart::Registers(p) => p.read(),
        }
    }

    /// The STOP that ends a transfer with it, at `now`.
    pub fn stop(&mut self, now: u64) {
        if let I2cPart::At24(p) = self {
            p.stop(now);
        }
    }

    pub fn describe(&self) -> String {
        match self {
            I2cPart::Tmp117(p) => format!("TMP117 at {:.2} °C", p.celsius()),
            I2cPart::At24(_) => "AT24C02 EEPROM, 256 bytes".to_string(),
            I2cPart::Registers(_) => "a register file".to_string(),
        }
    }
}

/// TI's TMP117: a pointer register, then 16-bit registers sent most
/// significant byte first.
#[derive(Clone, Debug)]
pub struct Tmp117 {
    pub address: u8,
    registers: [u16; 16],
    pointer: u8,
    addressing: bool,
    /// Which byte of the register the next one moved is.
    low: bool,
    high_byte: u8,
}

impl Tmp117 {
    const TEMPERATURE: usize = 0x00;
    const CONFIGURATION: usize = 0x01;
    const DEVICE_ID: usize = 0x0F;

    pub fn new(address: u8) -> Tmp117 {
        let mut registers = [0; 16];
        // Configuration: continuous conversion, 8 averaged, data ready.
        registers[Self::CONFIGURATION] = 0x2220;
        registers[0x02] = 0x6000;
        registers[0x03] = 0x8000;
        registers[Self::DEVICE_ID] = 0x0117;
        let mut part = Tmp117 { address, registers, pointer: 0, addressing: false, low: false, high_byte: 0 };
        part.set_celsius(25.0);
        part
    }

    pub fn celsius(&self) -> f64 {
        f64::from(self.registers[Self::TEMPERATURE] as i16) / 128.0
    }

    pub fn set_celsius(&mut self, celsius: f64) {
        self.registers[Self::TEMPERATURE] = ((celsius * 128.0).round() as i16) as u16;
    }

    fn start(&mut self, read: bool) {
        self.addressing = !read;
        self.low = false;
    }

    fn write(&mut self, byte: u8) {
        if self.addressing {
            self.pointer = byte & 0x0F;
            self.addressing = false;
        } else if !self.low {
            self.high_byte = byte;
            self.low = true;
        } else {
            let value = u16::from(self.high_byte) << 8 | u16::from(byte);
            // Configuration, the limits, the EEPROM and the offset are
            // written; the temperature, the EEPROM unlock's status and the
            // ID are not.
            if matches!(self.pointer, 0x01..=0x03 | 0x05..=0x08) {
                self.registers[self.pointer as usize] = value;
            }
            self.low = false;
        }
    }

    fn read(&mut self) -> u8 {
        let value = self.registers[self.pointer as usize];
        let byte = if self.low { value as u8 } else { (value >> 8) as u8 };
        self.low = !self.low;
        byte
    }
}

/// An AT24C02: 256 bytes, an address byte, pages of 8.
#[derive(Clone, Debug)]
pub struct At24 {
    pub address: u8,
    pub memory: Vec<u8>,
    pointer: u8,
    addressing: bool,
    /// Whether data has been written since the START: the STOP begins the
    /// write cycle.
    written: bool,
    /// When the write cycle ends.
    busy_until: u64,
}

impl At24 {
    const PAGE: u8 = 8;
    /// t_WR, the write cycle: 5 ms.
    const WRITE_CYCLE: u64 = 5_000_000_000;

    pub fn new(address: u8) -> At24 {
        At24 { address, memory: vec![0xFF; 256], pointer: 0, addressing: false, written: false, busy_until: 0 }
    }

    fn start(&mut self, read: bool, now: u64) -> bool {
        if now < self.busy_until {
            return false;
        }
        self.addressing = !read;
        self.written = false;
        true
    }

    fn write(&mut self, byte: u8) {
        if self.addressing {
            self.pointer = byte;
            self.addressing = false;
            return;
        }
        self.memory[self.pointer as usize] = byte;
        self.written = true;
        // A page write wraps within its page.
        let page = self.pointer & !(Self::PAGE - 1);
        self.pointer = page | (self.pointer.wrapping_add(1) & (Self::PAGE - 1));
    }

    fn read(&mut self) -> u8 {
        let byte = self.memory[self.pointer as usize];
        self.pointer = self.pointer.wrapping_add(1);
        byte
    }

    fn stop(&mut self, now: u64) {
        self.addressing = false;
        if std::mem::take(&mut self.written) {
            self.busy_until = now + Self::WRITE_CYCLE;
        }
    }
}

/// A register file: the first byte written sets its pointer, the rest are
/// stored from it, and a read answers from it, the pointer advancing
/// either way.
#[derive(Clone, Debug)]
pub struct Registers {
    pub address: u8,
    pub registers: [u8; 256],
    pointer: u8,
    addressing: bool,
}

impl Registers {
    pub fn new(address: u8) -> Registers {
        Registers { address, registers: [0; 256], pointer: 0, addressing: false }
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

/// A part on a SPI bus, selected by its chip-select going low.
#[derive(Clone, Debug)]
pub enum SpiPart {
    Flash(NorFlash),
    Rot(Rot),
}

impl SpiPart {
    pub fn select(&mut self) {
        match self {
            SpiPart::Flash(p) => p.select(),
            SpiPart::Rot(p) => p.select(),
        }
    }

    /// One frame, ending at `now`: what the controller sent on MOSI, and
    /// what the part answers on MISO.
    pub fn exchange(&mut self, mosi: u8, now: u64) -> u8 {
        match self {
            SpiPart::Flash(p) => p.exchange(mosi, now),
            SpiPart::Rot(p) => p.exchange(mosi),
        }
    }

    /// Chip-select going high again, at `now`.
    pub fn deselect(&mut self, now: u64) {
        match self {
            SpiPart::Flash(p) => p.deselect(now),
            SpiPart::Rot(p) => p.deselect(now),
        }
    }

    /// When the part next does something by itself: the RoT having its
    /// response ready.
    pub fn due(&self) -> Option<u64> {
        match self {
            SpiPart::Flash(_) => None,
            SpiPart::Rot(p) => p.ready_at,
        }
    }

    pub fn advance(&mut self, now: u64) {
        if let SpiPart::Rot(p) = self {
            p.advance(now);
        }
    }

    /// A line the part drives back to the chip, and its level: the RoT's
    /// ROT_IRQ, low when a response waits.
    pub fn line(&self) -> Option<((usize, u8), bool)> {
        match self {
            SpiPart::Flash(_) => None,
            SpiPart::Rot(p) => Some((p.irq, !p.responding)),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            SpiPart::Flash(p) => format!("W25Q80 flash, {} KiB", p.memory.len() / 1024),
            SpiPart::Rot(p) => format!(
                "root of trust (sprot v{}), ROT_IRQ on {}, {} requests answered",
                CURRENT_VERSION,
                pin_name(p.irq),
                p.stats[0]
            ),
        }
    }
}

/// A Winbond W25Q80: 1 MiB of NOR flash, and the commands to read, program
/// and erase it. A program or erase leaves it busy (status register 1's
/// BUSY) for the datasheet's typical time — 0.7 ms a page, 45 ms a 4 KiB
/// sector, 120 and 150 ms a 32 and 64 KiB block, 2 s the chip — and while
/// busy it takes nothing but a read of its status.
#[derive(Clone, Debug)]
pub struct NorFlash {
    pub memory: Vec<u8>,
    /// The command, its address bytes, and how many frames it has had.
    command: Option<u8>,
    address: u32,
    frames: usize,
    write_enabled: bool,
    busy_until: u64,
}

impl NorFlash {
    const SIZE: usize = 1 << 20;
    const JEDEC_ID: [u8; 3] = [0xEF, 0x40, 0x14];
    const UNIQUE_ID: [u8; 8] = [0xD1, 0x5A, 0x51, 0x00, 0x5A, 0x51, 0x00, 0x01];
    const MS: u64 = 1_000_000_000;

    pub fn new() -> NorFlash {
        NorFlash { memory: vec![0xFF; Self::SIZE], command: None, address: 0, frames: 0, write_enabled: false, busy_until: 0 }
    }

    fn select(&mut self) {
        self.command = None;
        self.frames = 0;
        self.address = 0;
    }

    fn exchange(&mut self, mosi: u8, now: u64) -> u8 {
        let busy = now < self.busy_until;
        let Some(command) = self.command else {
            // Busy, it hears only a read of its status.
            self.command = Some(if busy && mosi != 0x05 { 0x00 } else { mosi });
            match mosi {
                _ if busy => {}
                0x06 => self.write_enabled = true,
                0x04 => self.write_enabled = false,
                0x60 | 0xC7 if self.write_enabled => {
                    self.memory.fill(0xFF);
                    self.write_enabled = false;
                    self.busy_until = now + 2000 * Self::MS;
                }
                _ => {}
            }
            return 0xFF;
        };
        let n = self.frames;
        self.frames += 1;
        // Commands with a 24-bit address take it first.
        let addressed = matches!(command, 0x03 | 0x0B | 0x02 | 0x20 | 0x52 | 0xD8);
        if addressed && n < 3 {
            self.address = self.address << 8 | u32::from(mosi);
            if n == 2 {
                self.addressed(command, now);
            }
            return 0xFF;
        }
        let at = (self.address as usize) % Self::SIZE;
        match command {
            0x9F => Self::JEDEC_ID.get(n).copied().unwrap_or(0xFF),
            // Manufacturer and device ID, after three dummy bytes.
            0x90 => [0xEF, 0x13].get(n.wrapping_sub(3)).copied().unwrap_or(0xFF),
            0xAB => 0x13,
            0x4B => Self::UNIQUE_ID.get(n.wrapping_sub(4)).copied().unwrap_or(0xFF),
            // Status register 1: BUSY and WEL, which a program or erase
            // clears when it ends.
            0x05 if busy => 0x03,
            0x05 => u8::from(self.write_enabled) << 1,
            0x35 | 0x15 => 0,
            0x03 => {
                self.address = self.address.wrapping_add(1);
                self.memory[at]
            }
            // Fast read: one dummy byte after the address.
            0x0B if n == 3 => 0xFF,
            0x0B => {
                self.address = self.address.wrapping_add(1);
                self.memory[at]
            }
            0x02 if self.write_enabled => {
                self.memory[at] &= mosi;
                // A page program wraps within its 256-byte page.
                self.address = self.address & !0xFF | (self.address + 1) & 0xFF;
                0xFF
            }
            _ => 0xFF,
        }
    }

    /// The address has come: the erases happen now.
    fn addressed(&mut self, command: u8, now: u64) {
        let (size, ms) = match command {
            0x20 => (4 << 10, 45),
            0x52 => (32 << 10, 120),
            0xD8 => (64 << 10, 150),
            _ => return,
        };
        if self.write_enabled {
            let start = (self.address as usize % Self::SIZE) & !(size - 1);
            self.memory[start..start + size].fill(0xFF);
            self.write_enabled = false;
            self.busy_until = now + ms * Self::MS;
        }
    }

    fn deselect(&mut self, now: u64) {
        // A program begins at chip-select's rising edge, and clears WEL
        // when it ends.
        if self.command == Some(0x02) && self.frames > 3 && self.write_enabled {
            self.write_enabled = false;
            self.busy_until = now + 7 * Self::MS / 10;
        }
        self.command = None;
    }
}

impl Default for NorFlash {
    fn default() -> Self {
        NorFlash::new()
    }
}

/// The sprot protocol's version the RoT speaks, and the oldest it takes.
const CURRENT_VERSION: u32 = 6;
const MIN_VERSION: u32 = 2;
const BUF_SIZE: usize = 1024;

/// The RoT, as the LPC55's sprot server is to the SP: a request clocked
/// in under one chip-select, a response made, ROT_IRQ pulled low to say
/// so, and the response clocked out under the next. A chip-select pulsed
/// with nothing clocked while a response waits drops it.
///
/// It answers what an RoT can say without being one: its status, its I/O
/// counters and its lifecycle state; its boot state and boot information —
/// slot A active and valid, slot B and stage0's next slot erased; its
/// update block size; and the caboose of its slot A image and of stage0,
/// TLV-C as Hubris writes one (see [`caboose`]). The rest — updates,
/// attestation, dumps, the SP's watchdog — it refuses as a message type it
/// does not know, as an LPC55 built without them does.
#[derive(Clone, Debug)]
pub struct Rot {
    /// The ROT_IRQ line: (port, pin), 0 for port A.
    pub irq: (usize, u8),
    responding: bool,
    /// When the response being made will be ready.
    ready_at: Option<u64>,
    request: Vec<u8>,
    response: Vec<u8>,
    sent: usize,
    clocked: usize,
    /// RotIoStats, in its order: rx_received, rx_overrun,
    /// request_msg_too_large, csn_pulses, tx_underrun, rx_invalid,
    /// tx_incomplete, desynchronized.
    stats: [u32; 8],
}

impl Rot {
    /// How long the RoT takes from a request's chip-select going high to
    /// ROT_IRQ: its interrupt, its handling, the response packed.
    const LATENCY: u64 = 100_000_000;

    pub fn new(irq: (usize, u8)) -> Rot {
        Rot {
            irq,
            responding: false,
            ready_at: None,
            request: Vec::new(),
            response: Vec::new(),
            sent: 0,
            clocked: 0,
            stats: [0; 8],
        }
    }

    fn advance(&mut self, now: u64) {
        if self.ready_at.is_some_and(|at| at <= now) {
            self.ready_at = None;
            self.responding = true;
        }
    }

    fn select(&mut self) {
        self.clocked = 0;
        if !self.responding {
            self.request.clear();
        }
    }

    fn exchange(&mut self, mosi: u8) -> u8 {
        self.clocked += 1;
        if self.responding {
            let byte = self.response.get(self.sent).copied().unwrap_or(0);
            self.sent += 1;
            return byte;
        }
        if self.request.len() < BUF_SIZE {
            self.request.push(mosi);
        } else if self.request.len() == BUF_SIZE {
            self.stats[2] += 1;
        }
        0
    }

    fn deselect(&mut self, now: u64) {
        if self.responding {
            if self.clocked == 0 {
                self.stats[3] += 1;
            } else if self.sent < self.response.len() {
                self.stats[6] += 1;
            }
            self.responding = false;
            return;
        }
        if self.request.is_empty() || self.ready_at.is_some() {
            return;
        }
        self.response = self.answer();
        self.sent = 0;
        self.ready_at = Some(now + Self::LATENCY);
    }

    /// The response to the request clocked in: a `Result<RspBody,
    /// SprotError>`, hubpacked, behind a header and ahead of a CRC.
    fn answer(&mut self) -> Vec<u8> {
        const OK: u8 = 0;
        const ERR: u8 = 1;
        // SprotError::Protocol, and its SprotProtocolError.
        let protocol = |error: u8| vec![ERR, 0, error];
        let (invalid_crc, unsupported, bad_type, bad_length) = (0, 2, 3, 4);
        let request = &self.request;
        let body = if request.len() < 8 {
            self.stats[5] += 1;
            protocol(bad_length)
        } else {
            let version = u32::from_le_bytes(request[0..4].try_into().unwrap());
            let size = u16::from_le_bytes([request[4], request[5]]) as usize;
            match request.get(6..6 + size + 2) {
                _ if version < MIN_VERSION => protocol(unsupported),
                None => {
                    self.stats[5] += 1;
                    protocol(bad_length)
                }
                Some(rest) if crc16(&request[..6 + size]) != u16::from_le_bytes([rest[size], rest[size + 1]]) => {
                    self.stats[5] += 1;
                    protocol(invalid_crc)
                }
                Some(rest) => {
                    self.stats[0] += 1;
                    match rest[..size] {
                        // Status: RotStatus.
                        [0] => {
                            let mut body = vec![OK, 1];
                            body.extend(CURRENT_VERSION.to_le_bytes());
                            body.extend(MIN_VERSION.to_le_bytes());
                            body.extend((BUF_SIZE as u16).to_le_bytes());
                            body.extend((BUF_SIZE as u16).to_le_bytes());
                            body
                        }
                        // IoStats: RotIoStats.
                        [1] => {
                            let mut body = vec![OK, 2];
                            for counter in self.stats {
                                body.extend(counter.to_le_bytes());
                            }
                            body
                        }
                        // State(LifecycleState): Development.
                        [10, 0] => vec![OK, 10, OK, 0, 2],
                        // RotState: RotState::V2 { state }.
                        [2] => [&[OK, 3, 1][..], &boot_state()].concat(),
                        // Update(GetBlockSize): an LPC55's 512-byte page.
                        [3, 0] => [&[OK, 4, 0][..], &512u32.to_le_bytes()].concat(),
                        // Update(BootInfo), and VersionedBootInfo, V1 or V2.
                        [3, 7] => [&[OK, 4, 1][..], &boot_info(1)].concat(),
                        [3, 8, version] => {
                            let v = version.min(2);
                            [&[OK, 4, 2, v - 1][..], &boot_info(v.max(1))].concat()
                        }
                        // Caboose(Size, Read), and their component forms:
                        // Hubris's slots, and stage0's.
                        [6, 0, slot] => caboose_size(0, slot),
                        [6, 2, component, slot] => caboose_size(component, slot),
                        [6, 1, slot, ref read @ ..] if read.len() == 8 => caboose_read(0, slot, read, 1),
                        [6, 3, component, slot, ref read @ ..] if read.len() == 8 => {
                            caboose_read(component, slot, read, 3)
                        }
                        _ => protocol(bad_type),
                    }
                }
            }
        };
        let mut message = CURRENT_VERSION.to_le_bytes().to_vec();
        message.extend((body.len() as u16).to_le_bytes());
        message.extend(&body);
        let crc = crc16(&message);
        message.extend(crc.to_le_bytes());
        message
    }
}

/// FWIDs: slot A's and stage0's images, made up; an erased slot's, the
/// SHA3-256 of nothing, as the LPC55 measures one.
const FWID_A: [u8; 32] = *b"zaqaru emulated RoT slot A image";
const FWID_STAGE0: [u8; 32] = *b"zaqaru emulated RoT stage0 image";
const FWID_ERASED: [u8; 32] = [
    0xa7, 0xff, 0xc6, 0xf8, 0xbf, 0x1e, 0xd7, 0x66, 0x51, 0xc1, 0x47, 0x56, 0xa0, 0x61, 0xd6, 0x62, 0xf5, 0x80, 0xff, 0x4d,
    0xe4, 0x3b, 0x49, 0xfa, 0x82, 0xd8, 0x0a, 0x4b, 0x80, 0xf8, 0x43, 0x4a,
];
/// `Result<(), ImageError>`: Ok, or FirstPageErased.
const IMAGE_OK: [u8; 1] = [0];
const IMAGE_ERASED: [u8; 2] = [1, 1];

/// RotBootStateV2, hubpacked: slot A active; A, B, stage0, stage0next.
fn boot_state() -> Vec<u8> {
    let slots: [(&[u8; 32], &[u8]); 4] =
        [(&FWID_A, &IMAGE_OK), (&FWID_ERASED, &IMAGE_ERASED), (&FWID_STAGE0, &IMAGE_OK), (&FWID_ERASED, &IMAGE_ERASED)];
    let mut state = vec![0u8];
    for (digest, status) in slots {
        state.extend(digest);
        state.extend(status);
    }
    state
}

/// RotBootInfo (1) or RotBootInfoV2 (2), hubpacked: slot A active and
/// preferred, nothing pending or transient.
fn boot_info(version: u8) -> Vec<u8> {
    let preferences = [0u8, 0, 0, 0];
    if version == 1 {
        // Each slot's digest an Option: A's, and B's of its erased flash.
        return [&preferences[..], &[1], &FWID_A, &[1], &FWID_ERASED].concat();
    }
    let fwid = |digest: &[u8; 32]| [&[0u8][..], digest].concat();
    [
        &preferences[..],
        &fwid(&FWID_A),
        &fwid(&FWID_ERASED),
        &fwid(&FWID_STAGE0),
        &fwid(&FWID_ERASED),
        &IMAGE_OK,
        &IMAGE_ERASED,
        &IMAGE_OK,
        &IMAGE_ERASED,
    ]
    .concat()
}

/// The caboose of a component's (Hubris 0, stage0 1) slot (A 0, B 1), if
/// it has one: only the images there are have one.
fn caboose_of(component: u8, slot: u8) -> Option<Vec<u8>> {
    let name: &[u8] = match (component, slot) {
        (0, 0) => b"zaqaru-rot",
        (1, 0) => b"zaqaru-stage0",
        _ => return None,
    };
    let commit = [b'0'; 40];
    Some(caboose(&[(b"GITC", &commit), (b"BORD", b"rot-carrier-2"), (b"NAME", name), (b"VERS", b"0.0.0")]))
}

/// `RspBody::Caboose`: a `Result<CabooseRsp, RawCabooseError>`, its error
/// MissingCaboose when the slot has none.
fn caboose_size(component: u8, slot: u8) -> Vec<u8> {
    let response = if component == 0 { 0 } else { 2 };
    match caboose_of(component, slot) {
        Some(caboose) => [&[0, 7, 0, response][..], &(caboose.len() as u32).to_le_bytes()].concat(),
        None => vec![0, 7, 1, 2],
    }
}

/// A caboose read: `start` and `size`, and the bytes after the response,
/// as the LPC55 sends its blobs; InvalidRead if it reaches past the end.
fn caboose_read(component: u8, slot: u8, read: &[u8], response: u8) -> Vec<u8> {
    let Some(caboose) = caboose_of(component, slot) else { return vec![0, 7, 1, 2] };
    let start = u32::from_le_bytes(read[0..4].try_into().unwrap()) as usize;
    let size = u32::from_le_bytes(read[4..8].try_into().unwrap()) as usize;
    match caboose.get(start..start.saturating_add(size)) {
        Some(bytes) => [&[0, 7, 0, response][..], bytes].concat(),
        None => vec![0, 7, 1, 0],
    }
}

/// A caboose's contents: TLV-C chunks, each a tag, a length, the header's
/// checksum, the body padded to four bytes, and the body's CRC-32C.
pub fn caboose(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, body) in chunks {
        let len = body.len() as u32;
        let header = !u32::from_le_bytes(**tag).wrapping_mul(0x6b32_9f69).wrapping_add(len);
        out.extend(*tag);
        out.extend(len.to_le_bytes());
        out.extend(header.to_le_bytes());
        out.extend(*body);
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend(crc32c(body).to_le_bytes());
    }
    out
}

/// CRC-32C, the iSCSI polynomial, TLV-C's.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { crc >> 1 ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    !crc
}

/// CRC-16/XMODEM, sprot's.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { crc << 1 ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// A part, and where on the board it is wired.
#[derive(Clone, Debug)]
pub enum Wiring {
    /// On I2C `controller`, 0 for I2C1.
    I2c { controller: usize, part: I2cPart },
    /// On SPI `controller`, 0 for SPI1, selected by `cs` going low.
    Spi { controller: usize, cs: (usize, u8), part: SpiPart },
}

/// The parts [`Wiring::parse`] knows.
pub const PARTS: &str = "i2c parts are tmp117, at24c02 and registers; spi parts are w25q80 and rot";

impl Wiring {
    /// `i2c I2C2 0x48 tmp117`, `spi SPI1 PD14 w25q80`, `spi SPI3 PA15 rot
    /// PD0`: the bus, the controller, the address or chip-select, the part,
    /// and what else the part needs — the RoT, its ROT_IRQ pin.
    pub fn parse(line: &str) -> Result<Wiring, String> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let controller = |word: &str, prefix: &str| -> Result<usize, String> {
            word.to_ascii_uppercase()
                .strip_prefix(prefix)
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&n| n >= 1)
                .map(|n| n - 1)
                .ok_or_else(|| format!("`{word}`: expected {prefix}1, {prefix}2, …"))
        };
        match words.as_slice() {
            ["i2c", bus, address, part] => {
                let controller = controller(bus, "I2C")?;
                let address = address
                    .strip_prefix("0x")
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .filter(|&a| a < 0x80)
                    .ok_or_else(|| format!("`{address}`: expected a 7-bit address, `0x48`"))?;
                let part = match *part {
                    "tmp117" => I2cPart::Tmp117(Tmp117::new(address)),
                    "at24c02" => I2cPart::At24(At24::new(address)),
                    "registers" => I2cPart::Registers(Registers::new(address)),
                    other => return Err(format!("`{other}`: {PARTS}")),
                };
                Ok(Wiring::I2c { controller, part })
            }
            ["spi", bus, cs, part, rest @ ..] => {
                let controller = controller(bus, "SPI")?;
                let cs = parse_pin(cs).ok_or_else(|| format!("`{cs}`: expected a pin, `PD14`"))?;
                let part = match (*part, rest) {
                    ("w25q80", []) => SpiPart::Flash(NorFlash::new()),
                    ("rot", [irq]) => {
                        SpiPart::Rot(Rot::new(parse_pin(irq).ok_or_else(|| format!("`{irq}`: expected ROT_IRQ's pin"))?))
                    }
                    ("rot", _) => return Err("rot: expected its ROT_IRQ pin, `rot PD0`".into()),
                    (other, _) => return Err(format!("`{other}`: {PARTS}")),
                };
                Ok(Wiring::Spi { controller, cs, part })
            }
            _ => Err(format!("`{line}`: expected `i2c I2C2 0x48 tmp117` or `spi SPI1 PD14 w25q80`")),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Wiring::I2c { controller, part } => {
                format!("I2C{} 0x{:02x}  {}", controller + 1, part.address(), part.describe())
            }
            Wiring::Spi { controller, cs, part } => {
                format!("SPI{} {}  {}", controller + 1, pin_name(*cs), part.describe())
            }
        }
    }
}

/// `PD14`: the port, 0 for A, and the pin.
pub fn parse_pin(text: &str) -> Option<(usize, u8)> {
    let port = match text.as_bytes() {
        [b'P', port @ b'A'..=b'K', ..] => port - b'A',
        _ => return None,
    };
    let pin = text[2..].parse().ok().filter(|&pin: &u8| pin < 16)?;
    Some((port as usize, pin))
}

pub fn pin_name((port, pin): (usize, u8)) -> String {
    format!("P{}{}", (b'A' + port as u8) as char, pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tmp117_reads_its_id_and_temperature_msb_first() {
        let mut t = Tmp117::new(0x48);
        t.set_celsius(31.5);
        for (pointer, expected) in [(0x0F, [0x01, 0x17]), (0x00, [0x0F, 0xC0])] {
            t.start(false);
            t.write(pointer);
            t.start(true);
            assert_eq!([t.read(), t.read()], expected);
        }
        t.start(false);
        t.write(0x00);
        t.write(0x12);
        t.write(0x34);
        assert_eq!(t.celsius(), 31.5, "the temperature is read-only");
    }

    #[test]
    fn an_eeprom_page_write_wraps_within_its_page_and_then_is_busy() {
        let mut e = At24::new(0x50);
        assert!(e.start(false, 0));
        for byte in [0x06, 1, 2, 3, 4] {
            e.write(byte);
        }
        e.stop(1000);
        assert_eq!(&e.memory[0..8], &[3, 4, 0xFF, 0xFF, 0xFF, 0xFF, 1, 2]);
        assert!(!e.start(true, 1000 + At24::WRITE_CYCLE - 1), "still writing: no acknowledge");
        assert!(e.start(true, 1000 + At24::WRITE_CYCLE));
    }

    #[test]
    fn the_flash_answers_its_jedec_id_and_programs_only_clear_bits() {
        let mut f = NorFlash::new();
        // Each transfer a second after the last: the flash is never busy.
        let mut now = 0;
        let mut transfer = |f: &mut NorFlash, bytes: &[u8]| -> Vec<u8> {
            now += 1000 * NorFlash::MS;
            f.select();
            let out = bytes.iter().map(|&b| f.exchange(b, now)).collect();
            f.deselect(now);
            out
        };
        assert_eq!(transfer(&mut f, &[0x9F, 0, 0, 0]), [0xFF, 0xEF, 0x40, 0x14]);
        transfer(&mut f, &[0x02, 0, 0, 0, 0x0F]);
        assert_eq!(f.memory[0], 0xFF, "no write enable, no program");
        transfer(&mut f, &[0x06]);
        assert_eq!(transfer(&mut f, &[0x05, 0]), [0xFF, 0x02], "WEL");
        transfer(&mut f, &[0x02, 0, 0, 0, 0x0F, 0xF0]);
        assert_eq!(transfer(&mut f, &[0x03, 0, 0, 0, 0, 0]), [0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0xF0]);
        assert_eq!(transfer(&mut f, &[0x05, 0]), [0xFF, 0x00], "the program cleared WEL");
        transfer(&mut f, &[0x06]);
        transfer(&mut f, &[0x20, 0, 0, 0]);
        assert_eq!(f.memory[0], 0xFF, "erased");
    }

    #[test]
    fn the_flash_is_busy_while_it_programs() {
        let mut f = NorFlash::new();
        for (bytes, at) in [(&[0x06][..], 0), (&[0x02, 0, 0, 0, 0x0F][..], 0)] {
            f.select();
            bytes.iter().for_each(|&b| _ = f.exchange(b, at));
            f.deselect(at);
        }
        let status = |f: &mut NorFlash, at| {
            f.select();
            let s = [f.exchange(0x05, at), f.exchange(0, at)][1];
            f.deselect(at);
            s
        };
        assert_eq!(status(&mut f, NorFlash::MS / 2), 0x03, "BUSY and WEL");
        assert_eq!(status(&mut f, NorFlash::MS), 0x00);
    }

    /// A sprot request as the SP packs one.
    fn request(body: &[u8]) -> Vec<u8> {
        let mut message = 6u32.to_le_bytes().to_vec();
        message.extend((body.len() as u16).to_le_bytes());
        message.extend(body);
        let crc = crc16(&message);
        message.extend(crc.to_le_bytes());
        message
    }

    #[test]
    fn the_rot_answers_a_status_request_and_raises_rot_irq() {
        let mut rot = SpiPart::Rot(Rot::new((3, 0)));
        assert_eq!(rot.line(), Some(((3, 0), true)), "ROT_IRQ idles high");
        rot.select();
        for byte in request(&[0]) {
            rot.exchange(byte, 0);
        }
        rot.deselect(0);
        assert_eq!(rot.line(), Some(((3, 0), true)), "still working on it");
        assert_eq!(rot.due(), Some(Rot::LATENCY));
        rot.advance(Rot::LATENCY);
        assert_eq!(rot.line(), Some(((3, 0), false)), "a response waits");
        rot.select();
        let response: Vec<u8> = (0..23).map(|_| rot.exchange(0, Rot::LATENCY)).collect();
        rot.deselect(Rot::LATENCY);
        assert_eq!(rot.line(), Some(((3, 0), true)));
        let size = u16::from_le_bytes([response[4], response[5]]) as usize;
        assert_eq!(&response[6..6 + size], &[0, 1, 6, 0, 0, 0, 2, 0, 0, 0, 0, 4, 0, 4]);
        let crc = u16::from_le_bytes([response[6 + size], response[7 + size]]);
        assert_eq!(crc, crc16(&response[..6 + size]));
    }

    #[test]
    fn the_rot_refuses_a_bad_crc_and_what_it_does_not_know() {
        let mut rot = Rot::new((3, 0));
        let mut bad = request(&[0]);
        *bad.last_mut().unwrap() ^= 1;
        rot.request = bad;
        assert_eq!(&rot.answer()[6..9], &[1, 0, 0], "Err(Protocol(InvalidCrc))");
        // Update(Prep(ImageB)): no updates here.
        rot.request = request(&[3, 1, 2]);
        assert_eq!(&rot.answer()[6..9], &[1, 0, 3], "Err(Protocol(BadMessageType))");
    }

    #[test]
    fn the_rot_s_boot_state_and_caboose() {
        let mut rot = Rot::new((3, 0));
        rot.request = request(&[2]);
        let state = rot.answer();
        let size = u16::from_le_bytes([state[4], state[5]]) as usize;
        assert_eq!(size, 3 + 1 + 4 * 32 + 1 + 2 + 1 + 2, "RotState::V2, all four slots");
        rot.request = request(&[6, 0, 0]);
        let sized = rot.answer();
        let len = u32::from_le_bytes(sized[10..14].try_into().unwrap()) as usize;
        let mut read = vec![6, 1, 0];
        read.extend(0u32.to_le_bytes());
        read.extend((len as u32).to_le_bytes());
        rot.request = request(&read);
        let caboose = rot.answer();
        assert_eq!(&caboose[6..10], &[0, 7, 0, 1], "Ok(CabooseRsp::Read)");
        assert_eq!(&caboose[10..14], b"GITC");
        rot.request = request(&[6, 0, 1]);
        assert_eq!(&rot.answer()[6..10], &[0, 7, 1, 2], "slot B has none: MissingCaboose");
    }

    #[test]
    fn tlvc_checksums_are_tlvc_s() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        let chunk = caboose(&[(b"VERS", b"1.0")]);
        assert_eq!(chunk.len(), 12 + 4 + 4);
        let header = u32::from_le_bytes(chunk[8..12].try_into().unwrap());
        assert_eq!(header, !u32::from_le_bytes(*b"VERS").wrapping_mul(0x6b32_9f69).wrapping_add(3));
    }

    #[test]
    fn wiring_lines_parse() {
        let Wiring::Spi { controller, cs, part: SpiPart::Rot(rot) } = Wiring::parse("spi SPI3 PA15 rot PD0").unwrap() else {
            panic!()
        };
        assert_eq!((controller, cs, rot.irq), (2, (0, 15), (3, 0)));
        let Wiring::I2c { controller, part } = Wiring::parse("i2c I2C2 0x48 tmp117").unwrap() else { panic!() };
        assert_eq!((controller, part.address()), (1, 0x48));
        assert!(Wiring::parse("i2c I2C2 0x48 lm75").is_err());
    }
}
