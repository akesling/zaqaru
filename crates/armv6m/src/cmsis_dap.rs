//! A CMSIS-DAP v2 probe's firmware, with the board's debug port as its
//! target: what a probe-rs debugger talks to over USB.
//!
//! The protocol is packets. The host sends one command — a command byte
//! and its request — and the probe answers with one response, which starts
//! with the same command byte. Everything here is a function from one to
//! the other ([`Probe::command`]); carrying packets is the USB layer's
//! business, and so is when.
//!
//! What a probe has a wire for, this has a model of: DAP_Transfer and
//! DAP_TransferBlock are [`dap::Port`] accesses, nRESET on DAP_SWJ_Pins is
//! a system reset, and line resets and clock rates, which shape bits on
//! wires that are not here, are accepted and change nothing. SWD only: a
//! request for JTAG, or for SWO, is refused the way a probe without them
//! refuses it.
//!
//! [`DESCRIPTORS`] and [`STRINGS`] are the probe's USB identity: one
//! vendor-class interface whose string names it CMSIS-DAP, with a bulk OUT
//! and a bulk IN endpoint, which is how a debugger tells a v2 probe from
//! anything else plugged in.

use crate::Machine;
use crate::dap::{Ack, Port};

/// The largest packet either way. CMSIS-DAP lets a probe choose; nothing
/// here is a USB packet, so a large one costs nothing and saves round
/// trips — a TransferBlock of a thousand words in one exchange.
pub const PACKET_SIZE: usize = 4096;

/// The USB IDs: pid.codes' open-source vendor, and a product ID from its
/// test range, since this probe is nobody's hardware.
pub const VENDOR_ID: u16 = 0x1209;
pub const PRODUCT_ID: u16 = 0x0001;

const DAP_OK: u8 = 0x00;
const DAP_ERROR: u8 = 0xFF;

/// The probe's state between commands: the debug port, and what
/// DAP_TransferConfigure and a match-mask write left.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub port: Port,
    connected: bool,
    match_mask: u32,
    match_retry: u16,
    /// Whether nRESET is held low.
    in_reset: bool,
}

impl Probe {
    /// Answers one command packet.
    pub fn command(&mut self, m: &mut Machine, request: &[u8]) -> Vec<u8> {
        let mut response = Vec::with_capacity(64);
        self.answer(m, request, &mut response);
        response
    }

    /// Appends the response to `request`, and says how many bytes of it the
    /// command took — which DAP_ExecuteCommands needs to find the next one.
    fn answer(&mut self, m: &mut Machine, request: &[u8], out: &mut Vec<u8>) -> usize {
        let Some((&id, body)) = request.split_first() else { return 0 };
        out.push(id);
        let byte = |i: usize| body.get(i).copied().unwrap_or(0);
        let half = |i: usize| u16::from_le_bytes([byte(i), byte(i + 1)]);
        let word = |i: usize| u32::from_le_bytes([byte(i), byte(i + 1), byte(i + 2), byte(i + 3)]);
        let used = match id {
            // DAP_Info.
            0x00 => {
                info(byte(0), out);
                2
            }
            // DAP_HostStatus: the LEDs a probe has.
            0x01 => {
                out.push(DAP_OK);
                3
            }
            // DAP_Connect: 0 (the default) and 1 are SWD.
            0x02 => {
                let swd = byte(0) <= 1;
                self.connected = swd;
                out.push(swd as u8);
                2
            }
            // DAP_Disconnect.
            0x03 => {
                self.connected = false;
                out.push(DAP_OK);
                1
            }
            // DAP_TransferConfigure: idle cycles, WAIT retries, match retries.
            0x04 => {
                self.match_retry = half(3);
                out.push(DAP_OK);
                6
            }
            0x05 => 1 + self.transfer(m, body, out),
            0x06 => 1 + self.transfer_block(m, body, out),
            // DAP_TransferAbort has no response.
            0x07 => {
                out.pop();
                1
            }
            // DAP_WriteABORT.
            0x08 => {
                let ok = self.port.dp_write(0x0, word(1)).is_ok();
                out.push(if ok { DAP_OK } else { DAP_ERROR });
                6
            }
            // DAP_Delay.
            0x09 => {
                out.push(DAP_OK);
                3
            }
            // DAP_ResetTarget: no device-specific reset sequence.
            0x0A => {
                out.extend([DAP_OK, 0]);
                1
            }
            0x10 => {
                let pins = self.pins(m, byte(0), byte(1));
                out.push(pins);
                7
            }
            // DAP_SWJ_Clock.
            0x11 => {
                out.push(DAP_OK);
                5
            }
            // DAP_SWJ_Sequence: a count of bits (0 meaning 256), then them.
            0x12 => {
                let bits = if byte(0) == 0 { 256 } else { byte(0) as usize };
                if bits >= 50 {
                    self.port.line_reset();
                }
                out.push(DAP_OK);
                2 + bits.div_ceil(8)
            }
            // DAP_SWD_Configure.
            0x13 => {
                out.push(DAP_OK);
                2
            }
            // DAP_SWD_Sequence: output sequences are taken, input ones read
            // back as zeros.
            0x1D => {
                out.push(DAP_OK);
                let mut at = 1;
                for _ in 0..byte(0) {
                    let info = byte(at);
                    let bits = if info & 0x3F == 0 { 64 } else { (info & 0x3F) as usize };
                    at += 1;
                    if info & 0x80 != 0 {
                        out.extend(std::iter::repeat_n(0, bits.div_ceil(8)));
                    } else {
                        at += bits.div_ceil(8);
                    }
                }
                1 + at
            }
            // DAP_ExecuteCommands: a count, then that many commands.
            0x7F => {
                let count = byte(0);
                out.push(count);
                let mut at = 1;
                for _ in 0..count {
                    at += self.answer(m, &body[at.min(body.len())..], out);
                }
                1 + at
            }
            // JTAG, SWO, UART, and anything else: not this probe.
            _ => {
                out.pop();
                out.push(DAP_ERROR);
                request.len()
            }
        };
        used
    }

    /// DAP_SWJ_Pins: only nRESET, bit 7, does anything. Releasing it after
    /// holding it is a system reset. Answers the pins as read.
    fn pins(&mut self, m: &mut Machine, output: u8, select: u8) -> u8 {
        const NRESET: u8 = 1 << 7;
        if select & NRESET != 0 {
            let low = output & NRESET == 0;
            if self.in_reset && !low {
                let _ = m.reset();
            }
            self.in_reset = low;
        }
        // SWDIO high, nRESET as driven.
        1 << 1 | if self.in_reset { 0 } else { NRESET }
    }

    /// One register access, by a DAP_Transfer request byte: APnDP in bit
    /// 0, RnW in bit 1, A[3:2] in bits 3:2.
    fn access(&mut self, m: &mut Machine, request: u8, value: u32) -> Result<u32, Ack> {
        let a = request & 0xC;
        match (request & 1 != 0, request & 2 != 0) {
            (false, true) => self.port.dp_read(m, a),
            (false, false) => self.port.dp_write(a, value).map(|()| 0),
            (true, true) => self.port.ap_read(m, a),
            (true, false) => self.port.ap_write(m, a, value).map(|()| 0),
        }
    }

    /// DAP_Transfer: a DAP index, a count, and that many requests, each
    /// followed by a word if it writes or matches. The response is how many
    /// completed, the last one's acknowledgement, and the words read.
    /// Processing stops at the first that does not answer OK.
    fn transfer(&mut self, m: &mut Machine, body: &[u8], out: &mut Vec<u8>) -> usize {
        let count = body.get(1).copied().unwrap_or(0);
        let head = out.len();
        out.extend([0, 0]);
        let mut at = 2;
        let mut done = 0u8;
        let mut ack = 1u8;
        for _ in 0..count {
            let Some(&request) = body.get(at) else { break };
            at += 1;
            let reads = request & 2 != 0;
            let value_match = request & 1 << 4 != 0;
            let match_mask = request & 1 << 5 != 0;
            let operand = if !reads || value_match {
                let Some(bytes) = body.get(at..at + 4) else { break };
                at += 4;
                u32::from_le_bytes(bytes.try_into().unwrap())
            } else {
                0
            };
            if match_mask {
                self.match_mask = operand;
                done += 1;
                continue;
            }
            match self.access(m, request, operand) {
                Ok(value) if value_match => {
                    let mut value = value;
                    let mut tries = 0;
                    while value & self.match_mask != operand && tries < self.match_retry {
                        value = self.access(m, request, 0).unwrap_or(value);
                        tries += 1;
                    }
                    if value & self.match_mask != operand {
                        ack = 1 | 1 << 4;
                        break;
                    }
                    done += 1;
                }
                Ok(value) => {
                    if reads {
                        out.extend(value.to_le_bytes());
                    }
                    done += 1;
                }
                Err(e) => {
                    ack = code(e);
                    break;
                }
            }
        }
        out[head] = done;
        out[head + 1] = ack;
        at
    }

    /// DAP_TransferBlock: a DAP index, a count, one request byte, and for a
    /// write that many words — the same register again and again, which
    /// against DRW with TAR incrementing is a block of memory.
    fn transfer_block(&mut self, m: &mut Machine, body: &[u8], out: &mut Vec<u8>) -> usize {
        let count = u16::from_le_bytes([body.get(1).copied().unwrap_or(0), body.get(2).copied().unwrap_or(0)]);
        let request = body.get(3).copied().unwrap_or(0);
        let reads = request & 2 != 0;
        let head = out.len();
        out.extend([0, 0, 1]);
        let mut at = 4;
        let mut done = 0u16;
        for _ in 0..count {
            let operand = if reads {
                0
            } else {
                let Some(bytes) = body.get(at..at + 4) else { break };
                at += 4;
                u32::from_le_bytes(bytes.try_into().unwrap())
            };
            match self.access(m, request, operand) {
                Ok(value) => {
                    if reads {
                        out.extend(value.to_le_bytes());
                    }
                    done += 1;
                }
                Err(e) => {
                    out[head + 2] = code(e);
                    break;
                }
            }
        }
        out[head..head + 2].copy_from_slice(&done.to_le_bytes());
        at
    }
}

/// An acknowledgement as a transfer response's bits 2:0 give it.
fn code(ack: Ack) -> u8 {
    match ack {
        Ack::Ok => 1,
        Ack::Wait => 2,
        Ack::Fault => 4,
    }
}

/// DAP_Info's answers: a length, then that much.
fn info(id: u8, out: &mut Vec<u8>) {
    let string = |out: &mut Vec<u8>, s: &str| {
        out.push(s.len() as u8 + 1);
        out.extend(s.bytes());
        out.push(0);
    };
    match id {
        0x01 => string(out, "zaqaru"),
        0x02 => string(out, PRODUCT),
        0x03 => string(out, SERIAL),
        // The CMSIS-DAP protocol version this speaks.
        0x04 => string(out, "2.1.1"),
        0x05 => string(out, "STMicroelectronics"),
        0x06 => string(out, "STM32G031"),
        0x07 => string(out, "STMicroelectronics"),
        0x08 => string(out, "NUCLEO-G031K8"),
        // Capabilities: SWD, and nothing else.
        0xF0 => out.extend([1, 0x01]),
        0xFD => out.extend([4, 0, 0, 0, 0]),
        // One packet in flight at a time: every command is answered before
        // the next is read.
        0xFE => out.extend([1, 1]),
        0xFF => {
            out.push(2);
            out.extend((PACKET_SIZE as u16).to_le_bytes());
        }
        _ => out.push(0),
    }
}

const PRODUCT: &str = "zaqaru CMSIS-DAP";
const SERIAL: &str = "zaqaru-board-0";

/// The probe's USB strings, by index; 0 is the language list.
pub const STRINGS: [&str; 4] = ["", "zaqaru", PRODUCT, SERIAL];
/// Index 4: the interface's name, which is what marks it CMSIS-DAP.
pub const INTERFACE_STRING: &str = "zaqaru CMSIS-DAP v2";

/// The bulk endpoints: OUT 1 and IN 1 (0x81).
pub const OUT_ENDPOINT: u8 = 0x01;
pub const IN_ENDPOINT: u8 = 0x81;
/// High-speed bulk's packet size, which is what the endpoints declare.
pub const MAX_PACKET: u16 = 512;

/// The device descriptor, then the configuration descriptor with its one
/// interface and two endpoints, as `usbfs` hands them back on a read of
/// the device file.
pub fn descriptors() -> Vec<u8> {
    let mut d = Vec::new();
    let le = |v: u16| v.to_le_bytes();
    // Device: USB 2.0, class by interface, EP0 of 64, the IDs, release
    // 1.00, strings 1–3, one configuration.
    d.extend([18, 1]);
    d.extend(le(0x0200));
    d.extend([0, 0, 0, 64]);
    d.extend(le(VENDOR_ID));
    d.extend(le(PRODUCT_ID));
    d.extend(le(0x0100));
    d.extend([1, 2, 3, 1]);
    // Configuration 1: one interface, bus-powered, 100 mA.
    let total = 9 + 9 + 7 + 7;
    d.extend([9, 2]);
    d.extend(le(total));
    d.extend([1, 1, 0, 0x80, 50]);
    // Interface 0: vendor class, subclass 0, two endpoints, string 4.
    d.extend([9, 4, 0, 0, 2, 0xFF, 0, 0, 4]);
    for address in [OUT_ENDPOINT, IN_ENDPOINT] {
        d.extend([7, 5, address, 0x02]);
        d.extend(le(MAX_PACKET));
        d.push(0);
    }
    d
}

/// A string descriptor, as GET_DESCRIPTOR(STRING) returns it: index 0 is
/// the language list (US English), the rest UTF-16LE.
pub fn string_descriptor(index: u8) -> Option<Vec<u8>> {
    if index == 0 {
        return Some(vec![4, 3, 0x09, 0x04]);
    }
    let s = match index {
        4 => INTERFACE_STRING,
        i => *STRINGS.get(i as usize)?,
    };
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut d = vec![2 + 2 * units.len() as u8, 3];
    for unit in units {
        d.extend(unit.to_le_bytes());
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{FLASH_BASE, SRAM_BASE};

    fn machine() -> Machine {
        let mut image = vec![0u8; 0x110];
        image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
        image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
        image[0x100..0x102].copy_from_slice(&0xE7FEu16.to_le_bytes());
        Machine::stm32g031(&image).unwrap()
    }

    #[test]
    fn info_says_what_probe_rs_asks() {
        let mut m = machine();
        let mut probe = Probe::default();
        assert_eq!(probe.command(&mut m, &[0x00, 0xFF]), [0x00, 2, 0x00, 0x10]);
        assert_eq!(probe.command(&mut m, &[0x00, 0xFE]), [0x00, 1, 1]);
        assert_eq!(probe.command(&mut m, &[0x00, 0xF0]), [0x00, 1, 1]);
        assert_eq!(&probe.command(&mut m, &[0x00, 0x02])[2..], b"zaqaru CMSIS-DAP\0");
        assert_eq!(probe.command(&mut m, &[0x02, 0x00]), [0x02, 1], "SWD");
        assert_eq!(probe.command(&mut m, &[0x02, 0x02]), [0x02, 0], "no JTAG");
        assert_eq!(probe.command(&mut m, &[0x17, 0x00]), [0xFF]);
    }

    #[test]
    fn transfers_power_up_the_port_and_read_memory_in_blocks() {
        let mut m = machine();
        let mut probe = Probe::default();
        // Read DPIDR; write CTRL/STAT to power up; read it back.
        let mut request = vec![0x05, 0, 3, 0x02, 0x04];
        request.extend(0x5000_0000u32.to_le_bytes());
        request.push(0x06);
        let response = probe.command(&mut m, &request);
        assert_eq!(response[..3], [0x05, 3, 1]);
        assert_eq!(u32::from_le_bytes(response[3..7].try_into().unwrap()), crate::dap::DPIDR);
        assert_eq!(u32::from_le_bytes(response[7..11].try_into().unwrap()) >> 28, 0xF);

        // CSW for incrementing words, TAR at flash, then a block of DRW.
        let mut request = vec![0x05, 0, 2, 0x01];
        request.extend(0x2300_0012u32.to_le_bytes());
        request.push(0x05);
        request.extend(FLASH_BASE.to_le_bytes());
        assert_eq!(probe.command(&mut m, &request), [0x05, 2, 1]);
        let response = probe.command(&mut m, &[0x06, 0, 3, 0, 0x0F]);
        assert_eq!(response[..4], [0x06, 3, 0, 1]);
        let words: Vec<u32> = response[4..].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(words, [SRAM_BASE + 0x2000, FLASH_BASE + 0x101, 0]);
    }

    #[test]
    fn a_faulting_transfer_stops_the_batch_and_says_so() {
        let mut m = machine();
        let mut probe = Probe::default();
        let mut request = vec![0x05, 0, 3, 0x05];
        request.extend(0x3000_0000u32.to_le_bytes());
        request.extend([0x0F, 0x0F]);
        assert_eq!(probe.command(&mut m, &request), [0x05, 1, 4]);
    }

    #[test]
    fn the_descriptors_are_well_formed() {
        let d = descriptors();
        assert_eq!(d[0] as usize + u16::from_le_bytes([d[20], d[21]]) as usize, d.len());
        assert_eq!(string_descriptor(4).unwrap()[0] as usize, 2 + 2 * INTERFACE_STRING.len());
    }
}
