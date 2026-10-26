//! The GDB remote serial protocol, as a debug probe would speak it for
//! this machine.
//!
//! What a probe offers a debugger is small: halt, run, step, read and
//! write memory and registers, stop at an address. This is that, as
//! packets, and nothing about sockets — [`Debugger::packet`] takes one
//! packet's payload and says what to answer, and whoever owns the
//! connection does the framing ([`frame`]) and the waiting.
//!
//! Two departures from what GDB itself assumes, both because the client
//! that matters here is Humility, which speaks to a probe:
//!
//! - **Memory is readable and writable while the target runs**, as it is
//!   over a Cortex-M's AHB-AP. GDB's all-stop protocol never asks; Humility
//!   reads task state while Hubris runs and writes hiffy's mailbox to make
//!   a call.
//! - **Register numbers are the DCRSR selectors** ARMv6-M's debug
//!   architecture names them by: r0–r15 are 0–15, xPSR 16, MSP 17, PSP 18,
//!   and 20 packs CONTROL and PRIMASK. The target description given to
//!   GDB says so, so GDB numbers them the same way.

use crate::debug::{register, set_register};
use crate::{Machine, Outcome};

/// What the target description tells GDB the registers are: the M-profile
/// core feature, plus the two banked stack pointers, numbered as `p` and
/// `g` number them.
const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>arm</architecture>
  <feature name="org.gnu.gdb.arm.m-profile">
    <reg name="r0" bitsize="32" regnum="0"/>
    <reg name="r1" bitsize="32"/>
    <reg name="r2" bitsize="32"/>
    <reg name="r3" bitsize="32"/>
    <reg name="r4" bitsize="32"/>
    <reg name="r5" bitsize="32"/>
    <reg name="r6" bitsize="32"/>
    <reg name="r7" bitsize="32"/>
    <reg name="r8" bitsize="32"/>
    <reg name="r9" bitsize="32"/>
    <reg name="r10" bitsize="32"/>
    <reg name="r11" bitsize="32"/>
    <reg name="r12" bitsize="32"/>
    <reg name="sp" bitsize="32" type="data_ptr"/>
    <reg name="lr" bitsize="32"/>
    <reg name="pc" bitsize="32" type="code_ptr"/>
    <reg name="xpsr" bitsize="32" regnum="16"/>
  </feature>
  <feature name="org.gnu.gdb.arm.m-system">
    <reg name="msp" bitsize="32" type="data_ptr" regnum="17"/>
    <reg name="psp" bitsize="32" type="data_ptr" regnum="18"/>
  </feature>
</target>
"#;

/// The registers a `g` packet carries, in order.
const G_REGISTERS: [u32; 19] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18];

/// What to do after a packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// Answer with this payload.
    Reply(String),
    /// Nothing now: the target is running, and the answer is the stop
    /// reply it gives when it next halts.
    Resumed,
    /// Answer, then close the connection; the target runs on.
    Detach(String),
}

/// A GDB client of the machine. Whether the target is halted is the
/// machine's own state ([`Machine::halted`]), which a debug port sees too.
#[derive(Debug, Default)]
pub struct Debugger;

impl Debugger {
    /// Halts the target, as an interrupt (`^C`) asks. The stop reply.
    pub fn interrupt(&mut self, m: &mut Machine) -> String {
        m.halt();
        "S02".into()
    }

    /// The stop reply for a run that stopped by itself, or `None` if the
    /// outcome is not a stop.
    pub fn stopped(&mut self, outcome: Outcome) -> Option<String> {
        let signal = match outcome {
            Outcome::Breakpoint { .. } | Outcome::Stopped { .. } => 5,
            Outcome::Lockup { .. } => 11,
            _ => return None,
        };
        Some(format!("S{signal:02x}"))
    }

    pub fn packet(&mut self, m: &mut Machine, packet: &str) -> Response {
        use Response::Reply;
        let reply = |s: &str| Reply(s.to_string());
        let (command, rest) = packet.split_at(packet.chars().next().map_or(0, |c| c.len_utf8()));
        match command {
            // Attaching: GDB expects a stopped target, as OpenOCD gives it.
            "?" => {
                m.halt();
                reply("S05")
            }
            "g" => Reply(G_REGISTERS.iter().map(|&n| hex32(register(m, n))).collect()),
            "p" => match u32::from_str_radix(rest, 16) {
                Ok(n) => Reply(hex32(register(m, n))),
                Err(_) => reply("E01"),
            },
            "P" => match rest.split_once('=').and_then(|(n, v)| Some((u32::from_str_radix(n, 16).ok()?, unhex32(v)?))) {
                Some((n, value)) if set_register(m, n, value) => reply("OK"),
                _ => reply("E01"),
            },
            "m" => match address_length(rest) {
                Some((addr, len)) => match m.debug_read(addr, len) {
                    Some(bytes) => Reply(bytes.iter().map(|b| format!("{b:02x}")).collect()),
                    None => reply("E01"),
                },
                None => reply("E01"),
            },
            "M" => {
                let write = rest.split_once(':').and_then(|(at, data)| {
                    let (addr, len) = address_length(at)?;
                    let bytes = unhex(data)?;
                    (bytes.len() == len).then_some((addr, bytes))
                });
                match write {
                    Some((addr, bytes)) if m.debug_write(addr, &bytes) => reply("OK"),
                    _ => reply("E01"),
                }
            }
            "c" => {
                if let Ok(addr) = u32::from_str_radix(rest, 16) {
                    m.cpu.pc = addr & !1;
                }
                m.resume();
                Response::Resumed
            }
            "s" => {
                let outcome = m.step();
                Reply(match outcome {
                    Outcome::Lockup { .. } => "S0b".into(),
                    _ => "S05".into(),
                })
            }
            "Z" | "z" => {
                let mut fields = rest.split(',');
                let kind = fields.next();
                let addr = fields.next().and_then(|a| u32::from_str_radix(a, 16).ok());
                match (kind, addr) {
                    (Some("0" | "1"), Some(addr)) => {
                        if command == "Z" {
                            m.breakpoints.insert(addr & !1);
                        } else {
                            m.breakpoints.remove(&(addr & !1));
                        }
                        reply("OK")
                    }
                    // Watchpoints: not offered.
                    _ => reply(""),
                }
            }
            "D" => {
                m.breakpoints.clear();
                m.resume();
                Response::Detach("OK".into())
            }
            "H" => reply("OK"),
            "q" => self.query(m, packet),
            // Everything else — X, vCont, vFile — is unsupported, which an
            // empty reply says, and GDB falls back.
            _ => reply(""),
        }
    }

    fn query(&mut self, m: &mut Machine, packet: &str) -> Response {
        let reply = |s: &str| Response::Reply(s.to_string());
        if packet.starts_with("qSupported") {
            return reply("PacketSize=4000;qXfer:features:read+");
        }
        if let Some(window) = packet.strip_prefix("qXfer:features:read:target.xml:") {
            let Some((offset, length)) = address_length(window) else { return reply("E01") };
            let xml = TARGET_XML.as_bytes();
            let start = (offset as usize).min(xml.len());
            let end = (start + length).min(xml.len());
            let more = if end < xml.len() { "m" } else { "l" };
            return Response::Reply(format!("{more}{}", String::from_utf8_lossy(&xml[start..end])));
        }
        if let Some(hex) = packet.strip_prefix("qRcmd,") {
            // `monitor reset`, the one monitor command a probe can't do
            // without.
            let command = unhex(hex).map(|b| String::from_utf8_lossy(&b).trim().to_string());
            return match command.as_deref() {
                Some("reset" | "reset halt") => {
                    let _ = m.reset();
                    m.halt();
                    reply("OK")
                }
                _ => reply(""),
            };
        }
        match packet {
            "qAttached" => reply("1"),
            "qC" => reply("QC1"),
            "qfThreadInfo" => reply("m1"),
            "qsThreadInfo" => reply("l"),
            _ => reply(""),
        }
    }
}

/// The command in a `monitor` packet (`qRcmd`), if that is what this is.
pub fn monitor(packet: &str) -> Option<String> {
    let hex = packet.strip_prefix("qRcmd,")?;
    unhex(hex).map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Text for the debugger's console, as an `O` packet carries it.
pub fn output(text: &str) -> String {
    let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
    format!("O{hex}")
}

/// A packet as it goes on the wire.
pub fn frame(payload: &str) -> String {
    let sum = payload.bytes().fold(0u8, |a, b| a.wrapping_add(b));
    format!("${payload}#{sum:02x}")
}

fn address_length(s: &str) -> Option<(u32, usize)> {
    let (a, l) = s.split_once(',')?;
    Some((u32::from_str_radix(a, 16).ok()?, usize::from_str_radix(l, 16).ok()?))
}

/// A register as GDB sends it: target byte order, which is little-endian.
fn hex32(value: u32) -> String {
    value.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Option<u32> {
    Some(u32::from_le_bytes(unhex(s)?.try_into().ok()?))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}
