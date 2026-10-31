//! A microcontroller board on the container's bench: an emulated STM32 (see
//! the `armv6m` crate) that the kernel runs beside the processes, with a
//! debug probe the processes can reach.
//!
//! An image asks for one by containing `/etc/zaqaru/board`:
//!
//! ```text
//! chip   stm32g031        # or stm32g030, stm32g070, stm32h743, stm32h753,
//!                         # or an ordering code
//! led    PC6 low          # optional: the part's Nucleo's, by default
//! image  /hubris/final.bin
//! probe  127.0.0.1:3333
//! i2c    I2C2 0x48 tmp117     # optional, on an H7: the parts wired to its
//! spi    SPI1 PD14 w25q80     # buses (see `armv6m::devices`)
//! ```
//!
//! The board is kernel state, like the socket arena: one per container,
//! shared by every process, kept across `fork` and `execve`. That is the
//! point of having it here rather than in a process. A process that
//! emulated it would be interpreted twice over — every ARM instruction as
//! the hundreds of x86 instructions an emulator spends on it — and would
//! pace itself with timed waits; here it is plain Rust in the module, a
//! few instructions of the host per instruction of the board, and it keeps
//! time with the container's own clock.
//!
//! # Time
//!
//! The board keeps simulated time (`armv6m::Machine::time`, in
//! picoseconds), each instruction costing cycles of the core's clock as
//! RCC sets it. By default a second of it passes per second of the
//! container's monotonic clock; [`Speed`] scales that, or lets the board
//! run as fast as it can with its idle time skipped, and it can be paused
//! and moved on by a given time or to its next event — from `/dev/board`
//! (`speed`, `pause`, `resume`, `advance`) and from a debugger's `monitor`
//! commands, which are the same words.
//!
//! It is advanced at the points the scheduler already crosses to the host
//! — a process finishing its slice, and the container going idle — and so
//! at points that are a pure function of execution, against clock readings
//! that are store answers and so on the tape. A recorded run replays the
//! board too. A halted board's time stands still, as SysTick's does under
//! a debugger, and a board that falls more than [`MAX_BEHIND_NS`] behind
//! gives the time up rather than racing to make it up.
//!
//! # The probe
//!
//! A listening socket the kernel holds, on the configured address. A
//! `connect` to it is an ordinary loopback connection, queued on the
//! listener like any other; the board takes it off the queue as `accept`
//! would and serves the GDB remote protocol (`armv6m::gdb`) on its two
//! rings. One debugger at a time: a second connection replaces the first,
//! which is what a probe with one debug port does.
//!
//! And a second probe, on USB: a CMSIS-DAP v2 probe (`armv6m::cmsis_dap`)
//! on the board's debug port, which the board serves as a USB provider
//! (see [`crate::usb`]) under the id `board`. That is the one a debugger
//! built on probe-rs finds by itself, as it would find a probe plugged in.
//! Both reach the same core, and see the same halts.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use armv6m::cmsis_dap::{self, Probe};

use armv6m::gdb::{self, Debugger, Response};
use armv6m::gpio::Edge;
use armv6m::{Event, Machine, Outcome};

use crate::ring::End;

mod dwarf;
mod index;
mod watch;
pub use dwarf::ReadFile;

/// What a value's story is of: the byte at an address, or what a core
/// register (`r0`–`r12`) holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    Address(u32),
    Register(u8),
}

impl Subject {
    /// `0x2400_1000`, `603979776`, or `r3`.
    pub fn parse(text: &str, number: impl Fn(&str) -> Option<u64>) -> Option<Subject> {
        match text.strip_prefix('r') {
            Some(n) => n.parse::<u8>().ok().filter(|&n| n < 13).map(Subject::Register),
            None => number(text).map(|a| Subject::Address(a as u32)),
        }
    }
}
use crate::socket::{Address, Endpoint, State};

/// The most of the board's memory one read of it serves.
pub const MEMORY_SERVED: u32 = 4096;

/// The most cells a `board/writes` answer has: the cells asked for are
/// made larger, if they must be, to keep within it.
pub const CELLS_SERVED: usize = 65536;

/// The cell size, a multiple of 4, that covers `bytes` in at most
/// [`CELLS_SERVED`] cells, and is no smaller than `asked`.
fn cell_for(asked: u32, bytes: usize) -> u32 {
    let least = bytes.div_ceil(CELLS_SERVED).next_multiple_of(4) as u32;
    (asked.clamp(4, 1 << 20) & !3).max(least)
}

/// What Hubris's kernel fills a task's stack with when it starts the task.
const STACK_PAINT: u32 = 0xbadd_cafe;

/// Where an image asks for a board.
pub const CONFIGURATION: &[u8] = b"/etc/zaqaru/board";

/// How far behind the container's clock the board may fall before it gives
/// the time up.
pub const MAX_BEHIND_NS: u64 = 250_000_000;

/// How often an idle container wakes to move a running board on. Nothing a
/// process can see depends on it; it bounds how stale the board's state is
/// when nobody is looking, and how bunched its events are.
pub const TICK_NS: u64 = 20_000_000;

/// The most of the board's time one advance runs, catching up: a board
/// behind the container's clock catches up a step at a time, waking the
/// container again at once until it has, rather than all at once. Each
/// step hands the host the board as it is, and a panel on a board running
/// a game shows each frame a step makes; a catch-up of a quarter of a
/// second in one go is seven frames of Doom drawn as one.
pub const STEP_NS: u64 = 20_000_000;

/// How fast the board's simulated time runs against the container's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// Simulated time at this many thousandths of the container's: 1000 is
    /// real time.
    Scaled(u64),
    /// As much as the container can run: instructions in proportion to
    /// the container's time, and the board's idle time skipped.
    Max,
}

impl Speed {
    /// `1`, `0.25`, `10`, `max`.
    fn parse(text: &str) -> Option<Speed> {
        if text == "max" {
            return Some(Speed::Max);
        }
        let factor: f64 = text.trim_end_matches(['x', '×']).parse().ok()?;
        let permille = (factor * 1000.0).round();
        (1.0..=1_000_000.0).contains(&permille).then_some(Speed::Scaled(permille as u64))
    }

    fn show(&self) -> String {
        match self {
            Speed::Scaled(permille) => format!("{}×", *permille as f64 / 1000.0),
            Speed::Max => "max".to_string(),
        }
    }
}

/// At [`Speed::Max`], instructions the board runs per microsecond of the
/// container's time, and the most and least it runs at a time.
const MAX_RATE: u64 = 50;
const MAX_INSTRUCTIONS: u64 = 5_000_000;
const MIN_INSTRUCTIONS: u64 = 10_000;

/// `10ms`, `250us`, `2s`, `1000cycles`, in picoseconds at a clock whose
/// cycle is `period` of them.
fn parse_duration(text: &str, period: u64) -> Option<u64> {
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "ps" => 1.0,
        "ns" => 1e3,
        "us" | "µs" => 1e6,
        "ms" | "" => 1e9,
        "s" => 1e12,
        "cycle" | "cycles" => period as f64,
        _ => return None,
    };
    let ps = number * scale;
    (ps >= 0.0 && ps < 1e18).then_some(ps as u64)
}

/// A frequency as a person reads it: `400 MHz`, `32.768 kHz`.
fn hz(hz: u64) -> String {
    match hz {
        0 => "off".to_string(),
        hz if hz % 1_000_000 == 0 => format!("{} MHz", hz / 1_000_000),
        hz if hz >= 1_000_000 => format!("{:.3} MHz", hz as f64 / 1e6),
        hz => format!("{} kHz", hz as f64 / 1e3),
    }
}

/// The board, as the kernel holds it: absent until the first look, then
/// either nothing (no configuration) or a board.
#[derive(Default)]
pub enum Slot {
    #[default]
    Unlooked,
    Absent,
    Present(Box<Board>),
}

pub type Shared = Rc<RefCell<Slot>>;

pub struct Board {
    pub machine: Machine,
    pub debugger: Debugger,
    pub chip: String,
    pub image: String,
    /// The Hubris archive the image came out of, if anyone said: the
    /// configuration's `archive`, or an `image` that is one.
    pub archive: Option<String>,
    /// What the archive says the board's memory is, read the first time
    /// someone asks and kept, under the archive's path.
    map: RefCell<Option<(String, Rc<Result<armv6m::hubris::Image, String>>)>>,
    /// What each owner's DWARF in the archive says, read as it is asked
    /// for: see `dwarf`.
    debug: RefCell<dwarf::Cache>,
    /// The archive whose tasks' stacks the trace was given.
    stacks_for: Option<String>,
    /// The listener the probe answers on.
    listener: u32,
    pub probe: Address,
    client: Option<Client>,
    /// The monotonic instant simulated time zero corresponds to, at the
    /// speed it runs at; `None` until the first reading.
    origin: Option<u64>,
    pub speed: Speed,
    /// Whether simulated time stands still, a debugger or not.
    pub paused: bool,
    /// How fast simulated time has actually been going against the
    /// container's: measured over a second or so, from the reading and the
    /// simulated time at its start.
    rate: (u64, u64),
    ratio: f64,
    /// Whether the last advance left the board behind the container's
    /// clock: see [`STEP_NS`].
    behind: bool,
    /// The last monotonic reading the board was moved on at.
    pub now: u64,
    /// The board's LEDs, and whether each is lit, if it has been driven.
    pub leds: Vec<(Led, Option<bool>)>,
    /// Which board this is, as Hubris names it: `nucleo-h753zi`,
    /// `gimletlet-2`; empty if nobody has said.
    pub name: String,
    /// What has crossed the chip's pins and buses, numbered, the latest
    /// [`TRACE_KEPT`]; and the number the next will have.
    trace: VecDeque<(u64, armv6m::trace::Record, u64)>,
    /// The container's instant — its instructions retired, the axis of
    /// its timeline — at the crossing the board is being moved on at: what
    /// each record collected there is stamped with. The board runs only at
    /// crossings, so every record of one run of it has the same.
    stamp: u64,
    trace_next: u64,
    /// What the USARTs have sent, the latest [`SERIAL_KEPT`] bytes or so.
    pub serial: Vec<u8>,
    pub faults: u64,
    /// Lines for the console, not yet written.
    pub console: Vec<String>,
    /// The USB probe, and the responses it has for the host to read.
    usb_probe: Probe,
    usb_responses: VecDeque<Vec<u8>>,
    /// The host at the other end of the board's Ethernet cable, and the
    /// latest of what it heard, numbered.
    pub host: armv6m::net::Peer,
    pub heard: VecDeque<(u64, String)>,
    heard_count: u64,
    pings: u16,
    /// The parts wired to the buses, as configuration lines, for a part
    /// put on the bench to be wired the same; and the configuration's own,
    /// for `rewire`.
    pub wiring: Vec<String>,
    configured: Vec<String>,
    /// Datagrams the board sent to a group rather than to the host —
    /// udpbroadcast's, every second — counted, and the latest, apart from
    /// what was sent to the host.
    multicast: u64,
    last_multicast: Option<String>,
    /// The NOR flash on the QUADSPI, if the configuration put one there:
    /// kept to put on whatever part goes on the bench next.
    nor: Option<armv6m::quadspi::Nor>,
    /// The USART a keyboard is wired to, as `keys` types on it (0 for
    /// USART1).
    pub keyboard: Option<usize>,
}

/// How many trace records the board keeps, and serves at a time.
const TRACE_KEPT: usize = 500_000;
const TRACE_SERVED: usize = 5_000;

/// How many of what the host heard the board keeps.
const HEARD_KEPT: usize = 16;

/// The debugger connected to the probe.
struct Client {
    socket: u32,
    /// What the debugger writes, which the board reads.
    receive: u32,
    /// What the board writes, which the debugger reads.
    transmit: u32,
    /// Bytes read that do not make a whole packet yet.
    partial: Vec<u8>,
    /// Replies the transmit ring had no room for yet.
    outbox: Vec<u8>,
}

/// What a configuration file says, before anything is loaded.
#[derive(Debug, PartialEq, Eq)]
pub struct Configuration {
    pub chip: String,
    pub image: String,
    /// The Hubris build archive the image came out of, if there is one:
    /// what says what the board's memory is (see [`Board::whatis_json`]).
    pub archive: Option<String>,
    pub probe: (u32, u16),
    pub leds: Vec<Led>,
    /// The parts wired to the buses, a line each: `i2c I2C2 0x48 tmp117`.
    pub wiring: Vec<String>,
    /// Which board this is, as Hubris names it: `nucleo-h753zi`.
    pub name: String,
    /// A file to put on a 16 MiB NOR flash on the QUADSPI, for the image to
    /// map: a game's data, say.
    pub nor: Option<String>,
    /// The USART a keyboard is wired to, sending scan codes: 0 for USART1.
    pub keyboard: Option<usize>,
}

/// A user LED on the board: which pin, and which level lights it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Led {
    /// 0 for port A.
    pub port: u8,
    pub pin: u8,
    pub active_low: bool,
    /// What it lights, for the page to draw.
    pub color: &'static str,
}

impl Led {
    /// The LEDs of the board a part comes on, as Hubris's own LED driver
    /// has them: the G031 Nucleo's PC6, the G070 Nucleo's PA5 (lit high, as
    /// the Nucleo-64 wires it), the G030 oxcon2023g0 badge's PB7, and the
    /// Nucleo-144 H7s' green PB0, red PB14 and yellow PE1.
    pub fn of(part: &str) -> Vec<Led> {
        let led = |port: u8, pin: u8, active_low: bool, color: &'static str| Led { port, pin, active_low, color };
        match part {
            "stm32g031" => vec![led(2, 6, true, "green")],
            "stm32g070" => vec![led(0, 5, false, "green")],
            "stm32g030" => vec![led(1, 7, true, "green")],
            "stm32h743" | "stm32h753" => {
                vec![led(1, 0, false, "green"), led(1, 14, false, "red"), led(4, 1, false, "yellow")]
            }
            _ => Vec::new(),
        }
    }

    /// `PC6 low`, `PA5 high`, `PB14 high red`: the pin, the level that
    /// lights it, and its colour, green if not said.
    fn parse(text: &str) -> Option<Led> {
        let mut words = text.split_whitespace();
        let (port, pin) = parse_pin(words.next()?)?;
        let active_low = match words.next().unwrap_or("high") {
            "low" => true,
            "high" => false,
            _ => return None,
        };
        let color = match words.next().unwrap_or("green") {
            "green" => "green",
            "red" => "red",
            "yellow" => "yellow",
            "blue" => "blue",
            "white" => "white",
            _ => return None,
        };
        Some(Led { port, pin, active_low, color })
    }

    fn name(&self) -> String {
        format!("P{}{}", (b'A' + self.port) as char, self.pin)
    }
}

/// `PC13`: a port, 0 for A, and a pin.
fn parse_pin(text: &str) -> Option<(u8, u8)> {
    let port = match text.as_bytes() {
        [b'P', port @ b'A'..=b'K', ..] => port - b'A',
        _ => return None,
    };
    let pin = text[2..].parse().ok().filter(|&pin: &u8| pin < 16)?;
    Some((port, pin))
}

const MODELLED: &str = "the modelled parts are stm32g030, stm32g031, stm32g070, stm32h743 and stm32h753";

/// How much of what the USARTs sent the board keeps: the tail, for the
/// page's serial panel.
const SERIAL_KEPT: usize = 16 * 1024;

/// Parses `/etc/zaqaru/board`: one `key value` per line, `#` comments.
pub fn parse(text: &str) -> Result<Configuration, String> {
    let (mut chip, mut image, mut probe, mut led, mut archive) = (None, None, None, None, None);
    let mut wiring = Vec::new();
    let mut name = String::new();
    let (mut nor, mut keyboard) = (None, None);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(char::is_whitespace)
            .map(|(k, v)| (k, v.trim()))
            .ok_or_else(|| format!("`{line}`: expected `key value`"))?;
        match key {
            "chip" => chip = Some(value.to_string()),
            "name" => name = value.to_string(),
            "image" => image = Some(value.to_string()),
            "archive" => archive = Some(value.to_string()),
            "probe" => probe = Some(parse_address(value).ok_or_else(|| format!("probe `{value}`: expected a.b.c.d:port"))?),
            "led" => led = Some(Led::parse(value).ok_or_else(|| format!("led `{value}`: expected a pin and a level, `PC6 low`"))?),
            "i2c" | "spi" => {
                let line = format!("{key} {value}");
                armv6m::devices::Wiring::parse(&line)?;
                wiring.push(line);
            }
            "nor" => nor = Some(value.to_string()),
            "keyboard" => keyboard = Some(parse_usart(value).ok_or_else(|| format!("keyboard `{value}`: expected a USART, `USART3`"))?),
            _ => return Err(format!("unknown key `{key}`")),
        }
    }
    let chip = chip.unwrap_or_else(|| "stm32g031".to_string());
    let part = armv6m::part::Part::named(&chip)
        .ok_or_else(|| format!("chip `{chip}`: {MODELLED}"))?;
    Ok(Configuration {
        chip: part.name.to_string(),
        image: image.ok_or("no `image`")?,
        archive,
        probe: probe.unwrap_or((crate::socket::INADDR_LOOPBACK, 3333)),
        leds: led.map_or_else(|| Led::of(part.name), |led| vec![led]),
        wiring,
        name,
        nor,
        keyboard,
    })
}

/// `USART3` or `UART7`: its number from 0.
fn parse_usart(text: &str) -> Option<usize> {
    let upper = text.to_ascii_uppercase();
    let n: usize = upper.strip_prefix("USART").or_else(|| upper.strip_prefix("UART"))?.parse().ok()?;
    (1..=8).contains(&n).then_some(n - 1)
}

/// Standard base64, for bytes in JSON.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk.iter().enumerate().fold(0u32, |word, (i, &b)| word | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(word >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// What the host heard, as a line: a datagram's text and where from, or
/// an echo reply.
fn heard_line(heard: &armv6m::net::Heard) -> String {
    use armv6m::net::{Heard, show};
    match heard {
        Heard::Udp { from, source, port, payload, .. } => {
            let text = String::from_utf8_lossy(payload);
            let printable = text.chars().all(|c| !c.is_control() || c == '\n');
            let body = if printable { format!("{:?}", text.trim_end_matches('\n')) } else { format!("{} bytes", payload.len()) };
            format!("{body} from [{}]:{source} to port {port}", show(from))
        }
        Heard::Pong { from, sequence } => format!("an echo reply from [{}], sequence {sequence}", show(from)),
    }
}

fn lit_json(lit: Option<bool>) -> String {
    use crate::system::server::quoted;
    match lit {
        Some(true) => quoted("on"),
        Some(false) => quoted("off"),
        None => "null".into(),
    }
}

fn core_name(core: armv6m::state::Core) -> &'static str {
    match core {
        armv6m::state::Core::CortexM0Plus => "Cortex-M0+",
        armv6m::state::Core::CortexM7 => "Cortex-M7",
    }
}

fn parse_address(text: &str) -> Option<(u32, u16)> {
    let (address, port) = text.rsplit_once(':')?;
    let mut packed = 0u32;
    let mut parts = 0;
    for part in address.split('.') {
        packed = (packed << 8) | u32::from(part.parse::<u8>().ok()?);
        parts += 1;
    }
    (parts == 4).then_some(())?;
    Some((packed, port.parse().ok()?))
}

/// What byte `offset` of a Doom WAD's lump `name` is, where its format
/// says: a colour map's light level and index, a palette's entry, a flat's
/// or a picture's pixel. `between(marker)` says whether the lump is between
/// the markers `marker_START` and `marker_END` (`F` for flats).
fn wad_byte(name: &str, lump: &[u8], offset: usize, between: impl Fn(&str) -> bool) -> Option<String> {
    match name {
        "COLORMAP" => return Some(format!("light map {}, colour {:#04x}", offset / 256, offset % 256)),
        "PLAYPAL" => return Some(format!("palette {}, colour {:#04x}, {}", offset / 768, offset % 768 / 3, ["red", "green", "blue"][offset % 3])),
        _ => {}
    }
    if lump.len() == 4096 && between("F") {
        return Some(format!("flat pixel ({}, {})", offset % 64, offset / 64));
    }
    // A picture: width, height and offsets, then where each column's posts
    // are; each post its first row, its length, a pad byte, its pixels and
    // another pad.
    let half = |at: usize| Some(u16::from_le_bytes(lump.get(at..at + 2)?.try_into().ok()?) as usize);
    let word = |at: usize| Some(u32::from_le_bytes(lump.get(at..at + 4)?.try_into().ok()?) as usize);
    let (width, height) = (half(0)?, half(2)?);
    if width == 0 || width > 1024 || height == 0 || height > 1024 || 8 + 4 * width > lump.len() {
        return None;
    }
    for x in 0..width {
        let mut at = word(8 + 4 * x)?;
        if at >= lump.len() {
            return None;
        }
        while let Some(&top) = lump.get(at) {
            if top == 0xFF {
                break;
            }
            let length = *lump.get(at + 1)? as usize;
            let pixels = at + 3;
            if offset >= pixels && offset < pixels + length {
                return Some(format!("picture {width}×{height}, pixel ({x}, {})", top as usize + offset - pixels));
            }
            at = pixels + length + 1;
        }
    }
    None
}

/// Whether lump `n` of a WAD is between `marker_START` (or `markerS_START`)
/// and `marker_END`.
fn lump_between(wad: &[u8], lumps: usize, directory: usize, n: usize, marker: &str) -> bool {
    let name = |i: usize| -> String {
        let at = directory + 16 * i + 8;
        wad.get(at..at + 8).map_or(String::new(), |b| b.iter().take_while(|&&c| c != 0).map(|&c| c as char).collect())
    };
    let (start, end) = (format!("{marker}_START"), format!("{marker}_END"));
    let (start2, end2) = (format!("{marker}{marker}_START"), format!("{marker}{marker}_END"));
    (0..n).rev().map(name).find_map(|lump| {
        if lump == start || lump == start2 {
            Some(true)
        } else if lump == end || lump == end2 {
            Some(false)
        } else {
            None
        }
    }) == Some(true)
        && (n + 1..lumps).map(name).any(|lump| lump == end || lump == end2)
}

#[cfg(test)]
mod wad {
    use super::wad_byte;

    #[test]
    fn a_wad_s_bytes_are_named_by_their_lump_s_format() {
        assert_eq!(wad_byte("COLORMAP", &[], 0xc4e, |_| false).as_deref(), Some("light map 12, colour 0x4e"));
        assert_eq!(wad_byte("PLAYPAL", &[], 768 + 5, |_| false).as_deref(), Some("palette 1, colour 0x01, blue"));
        assert_eq!(wad_byte("FLOOR4_8", &[0; 4096], 64 * 3 + 5, |m| m == "F").as_deref(), Some("flat pixel (5, 3)"));
        // A 2×4 picture: column 0 a post of rows 1 and 2, column 1 none.
        let mut picture = vec![2, 0, 4, 0, 0, 0, 0, 0];
        picture.extend_from_slice(&16u32.to_le_bytes());
        picture.extend_from_slice(&23u32.to_le_bytes());
        picture.extend_from_slice(&[1, 2, 0, 0xAA, 0xBB, 0, 0xFF, 0xFF]);
        assert_eq!(wad_byte("STCFN068", &picture, 20, |_| false).as_deref(), Some("picture 2×4, pixel (0, 2)"));
        assert_eq!(wad_byte("STCFN068", &picture, 2, |_| false), None, "the header");
    }
}

impl Board {
    /// A command written to `/dev/board`, a line at a time:
    ///
    /// - `part NAME` puts a different part on the bench — `stm32g070`,
    ///   `stm32h753`, or an ordering code such as a Hubris archive names
    ///   (`STM32G070RBTx`) — erased, and held halted as a probe holds a
    ///   blank part, for `humility flash` to program;
    /// - `image TEXT` says what is in its flash now, for the panel;
    /// - `pin PC13 high`, `low` or `float` drives a pin from outside the
    ///   chip, or lets it go: a button pressed and released;
    /// - `udp PORT TEXT` sends a datagram to the board's port from the host
    ///   at the other end of its Ethernet cable, and `ping` an echo
    ///   request; what comes back is in the board's state;
    /// - `temperature 31.5` is what the temperature sensors on its buses
    ///   read, in degrees Celsius;
    /// - `speed`, `pause`, `resume` and `advance`: see
    ///   [`Board::clock_command`];
    /// - `name gimletlet-2` says which board this is, and `leds PG2 high
    ///   green, PG3 high` what its LEDs are — for a board other than the
    ///   bench's own;
    /// - `wire LINE` wires a part to the board's buses now, as a board
    ///   file's `i2c` and `spi` lines do; `unwire` leaves the next part put
    ///   on the bench with nothing wired, and `rewire` with what the board
    ///   file wires — for a board other than the bench's own;
    /// - `keys 1c 9c` types on the keyboard the board file wires, if it
    ///   wires one: those bytes arrive on its USART, scan codes as an AT
    ///   keyboard sends them.
    pub fn command(&mut self, line: &str) -> Result<(), String> {
        let (verb, rest) = line.trim().split_once(char::is_whitespace).unwrap_or((line.trim(), ""));
        match verb {
            "part" => {
                let part = armv6m::part::Part::named(rest.trim())
                    .ok_or_else(|| format!("part `{}`: {MODELLED}", rest.trim()))?;
                let mut machine = Machine::new(part, &[])?;
                machine.halt();
                machine.trace().on = true;
                self.name.clear();
                if matches!(machine.bus.soc, armv6m::bus::Soc::H7(_)) {
                    if let Some(nor) = &self.nor {
                        machine.attach_nor(nor.clone())?;
                    }
                    for line in &self.wiring {
                        let wired = armv6m::devices::Wiring::parse(line).and_then(|w| machine.attach(w));
                        if let Err(why) = wired {
                            self.console.push(format!("board: {line}: {why}"));
                        }
                    }
                }
                self.machine = machine;
                self.chip = part.name.to_string();
                self.image = "(erased)".to_string();
                self.leds = Led::of(part.name).into_iter().map(|led| (led, None)).collect();
                self.host = armv6m::net::Peer::default();
                self.heard.clear();
                self.multicast = 0;
                self.last_multicast = None;
                self.serial.clear();
                self.faults = 0;
                self.usb_probe = Probe::default();
                self.usb_responses.clear();
                self.rebase(self.now);
                self.rate = (self.now, 0);
                self.console.push(format!(
                    "board: now an {} with {} KiB of flash, erased and halted",
                    part.name.to_ascii_uppercase(),
                    part.flash / 1024
                ));
                Ok(())
            }
            "name" => {
                self.name = rest.trim().to_string();
                Ok(())
            }
            "leds" => {
                let leds: Option<Vec<Led>> = rest.split(',').filter(|led| !led.trim().is_empty()).map(Led::parse).collect();
                let leds = leds.ok_or_else(|| format!("leds `{}`: expected `PG2 high green, PG3 high red`", rest.trim()))?;
                self.leds = leds.into_iter().map(|led| (led, None)).collect();
                Ok(())
            }
            "image" if !rest.trim().is_empty() => {
                self.image = rest.trim().to_string();
                // A Hubris archive, as the door flashes: it says what the
                // board's memory is too.
                if self.image.ends_with(".zip") {
                    self.archive = Some(self.image.clone());
                }
                Ok(())
            }
            "udp" => {
                let (port, text) = rest.trim().split_once(char::is_whitespace).unwrap_or((rest.trim(), ""));
                let port = port.parse().map_err(|_| format!("udp `{port}`: expected a port, then the text"))?;
                let frames = self.host.udp(port, text.as_bytes());
                self.send_frames(frames)
            }
            "ping" => {
                self.pings = self.pings.wrapping_add(1);
                let frames = self.host.ping(self.pings);
                self.send_frames(frames)
            }
            "speed" | "pause" | "resume" | "advance" => self.clock_command(verb, rest.trim()).map(|_| ()),
            "wire" => {
                let line = rest.trim();
                self.machine.attach(armv6m::devices::Wiring::parse(line)?)?;
                self.wiring.push(line.to_string());
                Ok(())
            }
            "unwire" => {
                self.wiring.clear();
                Ok(())
            }
            "rewire" => {
                self.wiring = self.configured.clone();
                Ok(())
            }
            "temperature" => {
                let celsius: f64 =
                    rest.trim().parse().map_err(|_| format!("temperature `{}`: expected degrees Celsius", rest.trim()))?;
                let mut sensors = 0;
                for part in self.machine.i2c_parts_mut() {
                    if let armv6m::devices::I2cPart::Tmp117(sensor) = part {
                        sensor.set_celsius(celsius);
                        sensors += 1;
                    }
                }
                if sensors == 0 {
                    return Err("there is no temperature sensor on the board's buses".into());
                }
                Ok(())
            }
            "pin" => {
                let (pin, level) = rest.trim().split_once(char::is_whitespace).unwrap_or((rest.trim(), ""));
                let (port, pin) = parse_pin(pin).ok_or_else(|| format!("pin `{pin}`: expected a pin, `PC13`"))?;
                let level = match level.trim() {
                    "high" => Some(true),
                    "low" => Some(false),
                    "float" => None,
                    other => return Err(format!("`{other}`: a pin is driven `high`, `low` or `float`")),
                };
                self.machine.drive(port as usize, pin, level);
                Ok(())
            }
            "keys" => {
                let port = self.keyboard.ok_or("the board has no keyboard")?;
                let hex: String = rest.split_whitespace().collect();
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
                    .collect::<Option<Vec<u8>>>()
                    .ok_or_else(|| format!("keys `{}`: expected scan codes in hex, `1c 9c`", rest.trim()))?;
                // A byte the USART is not ready for is lost, as a key typed
                // at a board not listening is.
                for byte in bytes {
                    self.machine.receive(port, byte);
                }
                Ok(())
            }
            "" => Ok(()),
            other => Err(format!("`{other}`: the board takes `part NAME`, `image TEXT`, `pin PIN LEVEL`, `udp PORT TEXT`, `ping`, `keys HEX`, `temperature C`, `speed X`, `pause`, `resume`, `advance D`, `wire LINE`, `unwire` and `rewire`")),
        }
    }

    /// Frames onto the board's Ethernet, from the host.
    fn send_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<(), String> {
        for frame in frames {
            if !self.machine.deliver(frame) {
                return Err(format!("the {} has no Ethernet", self.chip));
            }
        }
        Ok(())
    }

    /// The board's clock, from a command's verb and what follows it:
    ///
    /// - `speed X` runs simulated time at X times the container's (`1`,
    ///   `0.25`, `10`), or `speed max` as fast as the container can;
    /// - `pause` stops it, a debugger or not, and `resume` lets it go;
    /// - `advance D` moves it on by D — `10ms`, `250us`, `2s`, `1000cycles`
    ///   — or `advance next` to whatever is next due: SysTick, a timer, a
    ///   transfer finishing;
    /// - `time` says where it is.
    ///
    /// What it says back is a line for whoever asked.
    pub fn clock_command(&mut self, verb: &str, rest: &str) -> Result<String, String> {
        match verb {
            "speed" if rest.is_empty() => {}
            "speed" => {
                self.speed = Speed::parse(rest).ok_or_else(|| format!("speed `{rest}`: a factor, `1` or `0.25`, or `max`"))?;
                self.rebase(self.now);
                self.rate = (self.now, self.machine.time);
            }
            "pause" => self.paused = true,
            "resume" => {
                self.paused = false;
                self.rebase(self.now);
            }
            "advance" => {
                if self.machine.halted() {
                    return Err("the core is halted: its debugger moves it".into());
                }
                let to = if rest == "next" {
                    self.machine.next_due().ok_or("nothing is due: no timer is running")?
                } else {
                    let by = parse_duration(rest, self.machine.period)
                        .ok_or_else(|| format!("advance `{rest}`: a time, `10ms`, `250us`, `1000cycles`, or `next`"))?;
                    self.machine.time.saturating_add(by)
                };
                let outcome = self.machine.run_until(to, u64::MAX);
                self.collect();
                self.after(outcome);
                self.rebase(self.now);
            }
            "time" => {}
            other => return Err(format!("`{other}`: the clock takes `speed X`, `pause`, `resume`, `advance D` and `time`")),
        }
        Ok(self.clock_line())
    }

    /// Where the board's clock is: simulated time, the speed asked for and
    /// the speed it is managing, and the clocks RCC gives out.
    pub fn clock_line(&self) -> String {
        let state = if self.paused { ", paused" } else if self.machine.halted() { ", halted" } else { "" };
        format!(
            "time {:.6} s simulated, speed {}{state}, managing {:.3}× the container's; {}\n",
            self.machine.time as f64 / 1e12,
            self.speed.show(),
            self.ratio,
            self.clocks_text(),
        )
    }

    fn clocks_text(&self) -> String {
        let c = self.machine.bus.soc.clocks();
        let pclk: Vec<String> = match self.machine.bus.soc {
            armv6m::bus::Soc::H7(_) => c.pclk.iter().map(|&f| hz(f)).collect(),
            armv6m::bus::Soc::G0(_) => vec![hz(c.pclk[0])],
        };
        format!("core {}, AHB {}, APB {}", hz(c.core), hz(c.hclk), pclk.join(" / "))
    }

    /// The instant, in monotonic nanoseconds, by which an idle container
    /// should wake to move the board on — `None` when it is halted, since
    /// then only a debugger moves it, or paused; at once when it runs as
    /// fast as it can.
    pub fn wake(&self) -> Option<u64> {
        if self.machine.halted() || self.paused {
            return None;
        }
        Some(match self.speed {
            Speed::Max => self.now,
            // Still catching up: at once.
            Speed::Scaled(_) if self.behind => self.now,
            Speed::Scaled(_) => self.now + TICK_NS,
        })
    }

    /// Brings simulated time up to `now`.
    fn advance(&mut self, now: u64) {
        let origin = *self.origin.get_or_insert(now);
        let last = std::mem::replace(&mut self.now, now);
        // A halted or paused board's time stands still; a reset asked for
        // while halted is still taken.
        if self.paused || self.machine.halted() && !self.machine.bus.scs.reset_requested {
            self.rebase(now);
            self.rate = (now, self.machine.time);
            return;
        }
        let outcome = match self.speed {
            // Elapsed nanoseconds at so many thousandths are picoseconds.
            Speed::Scaled(permille) => {
                let due = now.saturating_sub(origin) * permille;
                let behind = due.saturating_sub(self.machine.time);
                let limit = MAX_BEHIND_NS * permille;
                let step = STEP_NS * permille;
                let outcome = self.machine.run_until(self.machine.time + behind.min(step), u64::MAX);
                if behind > limit {
                    self.rebase(now);
                }
                self.behind = behind > step && behind <= limit;
                outcome
            }
            Speed::Max => {
                let instructions = (now.saturating_sub(last) / 1000 * MAX_RATE).clamp(MIN_INSTRUCTIONS, MAX_INSTRUCTIONS);
                let outcome = self.machine.run_until(u64::MAX, instructions);
                self.rebase(now);
                outcome
            }
        };
        if now.saturating_sub(self.rate.0) >= 1_000_000_000 {
            let simulated = self.machine.time.saturating_sub(self.rate.1) as f64 / 1000.0;
            self.ratio = simulated / now.saturating_sub(self.rate.0) as f64;
            self.rate = (now, self.machine.time);
        }
        self.collect();
        self.after(outcome);
    }

    /// What a run's ending means: a reset taken, or a halt reported.
    fn after(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Budget | Outcome::Asleep | Outcome::Halted => {}
            Outcome::SystemReset => self.take_reset(),
            outcome => {
                let time = self.machine.time;
                self.machine.trace().record(time, armv6m::trace::Signal::Board { what: format!("halted: {outcome:?}") });
                // A halt a debugger with halting debug on asked for — its
                // breakpoint, or a flash algorithm returning to one — is the
                // debugger's business, and a flash writes one per page; a
                // lockup is everyone's.
                if !self.machine.debug.enabled || matches!(outcome, Outcome::Lockup { .. }) {
                    self.console.push(format!("{} halted: {outcome:?}", self.stamp()));
                }
                if let Some(stop) = self.debugger.stopped(outcome) {
                    self.reply(&gdb::frame(&stop));
                }
            }
        }
    }

    /// A system reset, as SYSRESETREQ asks for one.
    fn take_reset(&mut self) {
        self.console.push(format!("{} system reset", self.stamp()));
        let time = self.machine.time;
        self.machine.trace().record(time, armv6m::trace::Signal::Board { what: "reset".into() });
        if let Err(e) = self.machine.reset() {
            self.console.push(format!("board: reset failed: {e}"));
        }
    }

    /// Restarts the clock from where the board is: simulated time now
    /// corresponds to `now`, at the speed it runs at.
    fn rebase(&mut self, now: u64) {
        let permille = match self.speed {
            Speed::Scaled(permille) => permille,
            Speed::Max => 1000,
        };
        self.origin = Some(now.saturating_sub(self.machine.time / permille));
    }

    /// Simulated milliseconds at `time`, in picoseconds.
    fn ms(&self, time: u64) -> f64 {
        time as f64 / 1e9
    }

    fn stamp(&self) -> String {
        format!("[board {:>10.3} ms]", self.ms(self.machine.time))
    }

    /// Turns the machine's events into console lines and board state.
    fn collect(&mut self) {
        let records: Vec<armv6m::trace::Record> = self.machine.trace().records.drain(..).collect();
        for record in records {
            self.trace.push_back((self.trace_next, record, self.stamp));
            self.trace_next += 1;
        }
        if self.trace.len() > TRACE_KEPT {
            self.trace.drain(..self.trace.len() - TRACE_KEPT);
        }
        let events: Vec<Event> = self.machine.events.drain(..).collect();
        for event in events {
            match event {
                // Only an LED's edges reach the console: an H7 Hubris toggles
                // its profiling pins at every system call.
                Event::Pin { time, edge: Edge { port, before, after } } => {
                    let at = self.ms(time);
                    let several = self.leds.len() > 1;
                    for (led, lit) in &mut self.leds {
                        if port != led.port || (before ^ after) & 1 << led.pin == 0 {
                            continue;
                        }
                        let on = (after >> led.pin & 1 != 0) != led.active_low;
                        *lit = Some(on);
                        let which = if several { format!(" {}", led.name()) } else { String::new() };
                        self.console.push(format!(
                            "[board {at:>10.3} ms] GPIO{} 0x{before:04x} -> 0x{after:04x}  LED{which} {}",
                            (b'A' + port) as char,
                            if on { "on" } else { "off" }
                        ));
                    }
                }
                // Kept for the serial panel rather than put on the console:
                // a demo that prints a line a millisecond would drown it.
                Event::Serial { byte, .. } => {
                    self.serial.push(byte.byte);
                    if self.serial.len() > 2 * SERIAL_KEPT {
                        self.serial.drain(..self.serial.len() - SERIAL_KEPT);
                    }
                }
                // What the board sends reaches the host, which may answer.
                Event::Frame { time, frame } => {
                    let (replies, heard) = self.host.receive(&frame);
                    for reply in replies {
                        self.machine.deliver(reply);
                    }
                    for heard in heard {
                        if let armv6m::net::Heard::Udp { multicast: true, .. } = heard {
                            self.multicast += 1;
                            self.last_multicast = Some(heard_line(&heard));
                            continue;
                        }
                        self.heard_count += 1;
                        let line = heard_line(&heard);
                        self.console.push(format!("[board {:>10.3} ms] the host heard {line}", self.ms(time)));
                        self.heard.push_back((self.heard_count, line));
                        if self.heard.len() > HEARD_KEPT {
                            self.heard.pop_front();
                        }
                    }
                }
                Event::Fault { time, pc, fault, mode, privileged } => {
                    self.faults += 1;
                    let at = self.ms(time);
                    self.console.push(format!(
                        "[board {at:>10.3} ms] fault at pc=0x{pc:08x} ({mode:?}, {}): {fault:?}",
                        if privileged { "privileged" } else { "unprivileged" }
                    ));
                }
            }
        }
    }

    /// Queues bytes for the debugger.
    fn reply(&mut self, bytes: &str) {
        if let Some(client) = &mut self.client {
            client.outbox.extend_from_slice(bytes.as_bytes());
        }
    }

    /// Answers whatever whole packets the debugger has sent.
    fn converse(&mut self, input: &[u8]) {
        let Some(client) = &mut self.client else { return };
        client.partial.extend_from_slice(input);
        let mut buffer = std::mem::take(&mut client.partial);
        let mut at = 0;
        while at < buffer.len() {
            match buffer[at] {
                0x03 => {
                    at += 1;
                    if !self.machine.halted() {
                        let stop = self.debugger.interrupt(&mut self.machine);
                        self.reply(&gdb::frame(&stop));
                    }
                }
                b'$' => {
                    // A packet is `$payload#xx`; wait for all of it.
                    let Some(hash) = buffer[at..].iter().position(|&b| b == b'#') else { break };
                    if at + hash + 3 > buffer.len() {
                        break;
                    }
                    let payload = String::from_utf8_lossy(&buffer[at + 1..at + hash]).into_owned();
                    at += hash + 3;
                    // The board's clock answers its own `monitor` commands,
                    // which a probe would not know.
                    if let Some(command) = gdb::monitor(&payload) {
                        let (verb, rest) = command.split_once(' ').unwrap_or((&command, ""));
                        if matches!(verb, "speed" | "pause" | "resume" | "advance" | "time") {
                            let said = self.clock_command(verb, rest.trim()).unwrap_or_else(|why| format!("{why}\n"));
                            self.reply(&format!("+{}{}", gdb::frame(&gdb::output(&said)), gdb::frame("OK")));
                            self.collect();
                            continue;
                        }
                    }
                    let was_halted = self.machine.halted();
                    match self.debugger.packet(&mut self.machine, &payload) {
                        Response::Reply(reply) => self.reply(&format!("+{}", gdb::frame(&reply))),
                        Response::Resumed => self.reply("+"),
                        Response::Detach(reply) => self.reply(&format!("+{}", gdb::frame(&reply))),
                    }
                    if was_halted && !self.machine.halted() {
                        self.rebase(self.now);
                    }
                    // A debugger's write can change a pin.
                    self.collect();
                }
                // Acks, and anything between packets.
                _ => at += 1,
            }
        }
        buffer.drain(..at);
        if let Some(client) = &mut self.client {
            client.partial = buffer;
        }
    }

    /// Lets go of a halted board when its debugger goes, as unplugging a
    /// probe does.
    fn release(&mut self) {
        self.machine.breakpoints.clear();
        let halted = self.machine.halted();
        self.machine.debug.release();
        if halted {
            self.machine.resume();
            self.rebase(self.now);
        }
    }

    /// Brings the board up to `now`, as the next service would.
    pub fn catch_up(&mut self, now: u64) {
        self.advance(now);
    }

    /// A read of the USB probe's provider paths: see [`crate::usb`].
    pub fn usb_read(&mut self, path: &[&[u8]], into: &mut Vec<u8>) -> crate::abi::StoreOutcome {
        use crate::abi::StoreOutcome::{Absent, Failed, Present};
        match path {
            [b"descriptors"] => into.extend(cmsis_dap::descriptors()),
            [b"strings", index] => {
                let text = match std::str::from_utf8(index).ok().and_then(|i| i.parse::<usize>().ok()) {
                    Some(4) => cmsis_dap::INTERFACE_STRING,
                    Some(i) if (1..cmsis_dap::STRINGS.len()).contains(&i) => cmsis_dap::STRINGS[i],
                    _ => return Absent,
                };
                into.extend(text.as_bytes());
            }
            [b"speed"] => into.extend(b"480"),
            [b"endpoints", b"81", b"pending", _] => match self.usb_responses.pop_front() {
                Some(response) => into.extend(response),
                None => return Absent,
            },
            // No class or vendor requests: a stall.
            _ => return Failed,
        }
        Present
    }

    /// A write to the USB probe: a command, answered at once, as the
    /// probe's firmware would before it read the next.
    pub fn usb_write(&mut self, path: &[&[u8]], data: &[u8]) -> crate::abi::StoreOutcome {
        if path != [&b"endpoints"[..], b"01", b"tx"] {
            return crate::abi::StoreOutcome::Failed;
        }
        let was_halted = self.machine.halted();
        let response = self.usb_probe.command(&mut self.machine, data);
        if !response.is_empty() {
            self.usb_responses.push_back(response);
        }
        // A reset asked for through AIRCR happens now, as it does on the
        // part, rather than when the board is next moved on: a debugger
        // that resets and then asks whether the core is halted at the
        // vector expects the answer to be yes.
        if self.machine.bus.scs.reset_requested {
            self.take_reset();
        }
        // And a step, which a core takes in a cycle: a debugger polls for
        // the halt after it for a hundred milliseconds, not for however long
        // until the board is next moved on.
        if self.machine.debug.stepping {
            self.machine.run(1);
        }
        if was_halted && !self.machine.halted() {
            self.rebase(self.now);
        }
        // A debugger's write can change a pin.
        self.collect();
        crate::abi::StoreOutcome::Present
    }

    /// The board's state as the container's store serves it: what the
    /// page's board panel shows.
    pub fn json(&self) -> String {
        use crate::system::server::quoted;
        let m = &self.machine;
        let cpu = &m.cpu;
        let hex = |value: u32| quoted(&format!("0x{value:08x}"));
        let mut registers: Vec<String> = (0..13).map(|n| format!(r#""r{n}":{}"#, hex(cpu.r[n]))).collect();
        registers.push(format!(r#""sp":{}"#, hex(cpu.sp())));
        registers.push(format!(r#""lr":{}"#, hex(cpu.r[14])));
        registers.push(format!(r#""pc":{}"#, hex(cpu.pc)));
        registers.push(format!(r#""xpsr":{}"#, hex(cpu.xpsr())));
        registers.push(format!(r#""msp":{}"#, hex(cpu.msp)));
        registers.push(format!(r#""psp":{}"#, hex(cpu.psp)));
        let exceptions: Vec<String> = m
            .taken
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(n, count)| format!("{}:{count}", quoted(&armv6m::scs::exception::name(n as u32))))
            .collect();
        let probe = match &self.probe {
            Address::Inet { address, port } => {
                format!("{}.{}.{}.{}:{port}", address >> 24, address >> 16 & 0xff, address >> 8 & 0xff, address & 0xff)
            }
            _ => String::new(),
        };
        format!(
            r#"{{"name":{},"chip":{},"core":{},"image":{},"state":{},"speed":{},"paused":{},"ratio":{:.4},"skipped_ms":{:.3},"clocks":{{{}}},"mode":{},"privileged":{},"simulated_ms":{:.3},"time_ps":{},"instructions":{},"led":{},"led_pin":{},"leds":[{}],"faults":{},"probe":{},"debugger":{},"serial":{},"heard":[{}],"wired":[{}],"gpio":[{}],"parts":[{}],"trace_next":{},"display":{},"keyboard":{},"exceptions":{{{}}},"registers":{{{}}}}}"#,
            quoted(&self.name),
            quoted(&self.chip),
            quoted(core_name(m.core)),
            quoted(&self.image),
            quoted(if self.machine.halted() { "halted" } else { "running" }),
            quoted(&self.speed.show()),
            self.paused,
            self.ratio,
            m.skipped as f64 / 1e9,
            {
                let c = m.bus.soc.clocks();
                let list = |v: &[u64]| v.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
                format!(r#""core":{},"hclk":{},"pclk":[{}],"timers":[{}]"#, c.core, c.hclk, list(&c.pclk), list(&c.timers))
            },
            quoted(&format!("{:?}", cpu.mode)),
            cpu.privileged(),
            self.ms(m.time),
            m.time,
            m.retired,
            lit_json(self.leds.first().and_then(|&(_, lit)| lit)),
            self.leds.first().map_or("null".into(), |(led, _)| quoted(&led.name())),
            self.leds
                .iter()
                .map(|(led, lit)| {
                    format!(r#"{{"pin":{},"color":{},"state":{}}}"#, quoted(&led.name()), quoted(led.color), lit_json(*lit))
                })
                .collect::<Vec<_>>()
                .join(","),
            self.faults,
            quoted(&probe),
            self.client.is_some(),
            // The tail, as text: enough for the panel to show, and no more
            // than a read of the lens should carry.
            quoted(&String::from_utf8_lossy(&self.serial[self.serial.len().saturating_sub(4096)..])),
            self.heard.iter().map(|(_, line)| quoted(line)).collect::<Vec<_>>().join(","),
            m.wired().iter().map(|line| quoted(line)).collect::<Vec<_>>().join(","),
            m.ports()
                .iter()
                .enumerate()
                .map(|(n, (idr, odr, moder))| {
                    format!(r#"{{"port":"{}","idr":{idr},"odr":{odr},"moder":{moder}}}"#, (b'A' + n as u8) as char)
                })
                .collect::<Vec<_>>()
                .join(","),
            m.parts()
                .iter()
                .map(|part| {
                    let values: Vec<String> =
                        part.values.iter().map(|(name, value)| format!("{}:{}", quoted(name), quoted(value))).collect();
                    format!(
                        r#"{{"bus":{},"at":{},"kind":{},"line":{},"values":{{{}}}}}"#,
                        quoted(&part.bus),
                        quoted(&part.at),
                        quoted(part.kind),
                        part.line.as_deref().map_or("null".into(), quoted),
                        values.join(",")
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
            self.trace_next,
            m.screen().map_or("null".into(), |(screen, frames)| format!(
                r#"{{"width":{},"height":{},"frames":{frames}}}"#,
                screen.width, screen.height
            )),
            self.keyboard.map_or("null".into(), |port| quoted(&format!("USART{}", port + 1))),
            exceptions.join(","),
            registers.join(","),
        )
    }

    /// What the panel on the board's LTDC shows, if it has been shown a
    /// frame since `since` (a count of frames): its size, and its pixels in
    /// base64 — palette indices and the palette, RGB, or RGB. Just the count
    /// if nothing is new, and `off` if there is no panel or it is dark.
    pub fn display_json(&self, since: u64) -> String {
        let frames = self.machine.frames_shown();
        if frames == since {
            return format!(r#"{{"frame":{frames}}}"#);
        }
        let Some((screen, frames)) = self.machine.screen() else {
            return format!(r#"{{"frame":{frames},"off":true}}"#);
        };
        let (format, palette, pixels) = match &screen.pixels {
            armv6m::ltdc::Pixels::Indexed { indices, palette } => {
                let rgb: Vec<u8> = palette.iter().flat_map(|c| [(c >> 16) as u8, (c >> 8) as u8, *c as u8]).collect();
                ("indexed", base64(&rgb), base64(indices))
            }
            armv6m::ltdc::Pixels::Rgb(rgb) => ("rgb", String::new(), base64(rgb)),
        };
        format!(
            r#"{{"frame":{frames},"width":{},"height":{},"format":"{format}","palette":"{palette}","pixels":"{pixels}"}}"#,
            screen.width, screen.height
        )
    }

    /// The memory the board's display controller scans out, as JSON: each
    /// layer's frame — its address, a line's pitch in bytes, its width and
    /// height in pixels, its format, and its colour lookup table as RGB in
    /// base64, if it has one on — what shows memory that is a picture as
    /// one, and says which pixel each of its bytes is.
    pub fn scanned_json(&self) -> String {
        let layers: Vec<String> = self
            .machine
            .scanned()
            .iter()
            .map(|layer| {
                let clut = layer.clut.as_ref().map_or("null".into(), |clut| {
                    let rgb: Vec<u8> = clut.iter().flat_map(|c| [(c >> 16) as u8, (c >> 8) as u8, *c as u8]).collect();
                    format!(r#""{}""#, base64(&rgb))
                });
                format!(
                    r#"{{"address":{},"pitch":{},"width":{},"height":{},"bytes":{},"format":"{}","clut":{clut}}}"#,
                    layer.address,
                    layer.pitch,
                    layer.width,
                    layer.height,
                    layer.format.bytes(),
                    layer.format.name()
                )
            })
            .collect();
        format!(r#"{{"layers":[{}]}}"#, layers.join(","))
    }

    /// Gives the trace the tasks' stacks, from the board's archive, once
    /// for each archive: what says which task the core returns to.
    pub fn give_trace_the_stacks(&mut self, read: impl FnOnce(&[u8]) -> Result<Vec<u8>, crate::errno::Errno>) {
        if self.stacks_for == self.archive {
            return;
        }
        self.stacks_for = self.archive.clone();
        let stacks = match self.memory_map(read).as_deref() {
            Some(Ok(image)) => image
                .tasks
                .iter()
                .filter_map(|task| {
                    let (base, size) = task.stack?;
                    Some((base, size, task.index? as u16))
                })
                .collect(),
            _ => Vec::new(),
        };
        let trace = self.machine.trace();
        trace.stacks = stacks;
        trace.task = None;
    }

    /// What the board's archive says its memory is, read through `read`
    /// the first time it is asked for and kept until the archive changes;
    /// `None` when nobody has said what the archive is.
    pub fn memory_map(
        &self,
        read: impl FnOnce(&[u8]) -> Result<Vec<u8>, crate::errno::Errno>,
    ) -> Option<Rc<Result<armv6m::hubris::Image, String>>> {
        let archive = self.archive.as_ref()?;
        if let Some((path, map)) = &*self.map.borrow()
            && path == archive
        {
            return Some(Rc::clone(map));
        }
        let map = Rc::new(
            read(archive.as_bytes())
                .map_err(|errno| format!("{archive}: {errno:?}"))
                .and_then(|bytes| armv6m::hubris::Image::read(&bytes).map_err(|why| format!("{archive}: {why}"))),
        );
        *self.map.borrow_mut() = Some((archive.clone(), Rc::clone(&map)));
        Some(map)
    }

    /// The board's memory as its archive lays it out, as JSON: the
    /// kernel, and each task in the task table's order, each with its
    /// sections, its stack and how many symbols name what is in it.
    pub fn tasks_json(&self, map: Option<&Result<armv6m::hubris::Image, String>>) -> String {
        use crate::system::server::quoted;
        let archive = self.archive.as_deref().map_or("null".into(), quoted);
        let image = match map {
            None => return format!(r#"{{"archive":null,"error":"the board's configuration names no archive"}}"#),
            Some(Err(why)) => return format!(r#"{{"archive":{archive},"error":{}}}"#, quoted(why)),
            Some(Ok(image)) => image,
        };
        let owner = |o: &armv6m::hubris::Owner| {
            let sections: Vec<String> = o
                .sections
                .iter()
                .map(|s| {
                    format!(
                        r#"{{"name":{},"base":{},"size":{},"writable":{},"executable":{}}}"#,
                        quoted(&s.name),
                        s.base,
                        s.size,
                        s.writable,
                        s.executable
                    )
                })
                .collect();
            let stack = o.stack.map_or("null".into(), |(base, size)| {
                // How deep it has ever gone: where the kernel painted a
                // task's stack when it started it, as `humility
                // stackmargin` finds it, the lowest word no longer holding
                // the paint; elsewhere, the lowest word ever written.
                let bus = &self.machine.bus;
                let words = || (base..base + size).step_by(4);
                let painted = words().any(|a| bus.peek(a, 4) == Ok(STACK_PAINT));
                let deepest = if painted {
                    words().find(|&a| bus.peek(a, 4) != Ok(STACK_PAINT))
                } else {
                    words().find(|&a| bus.last_write(a).is_some())
                };
                let deepest = deepest.map_or("null".into(), |a| a.to_string());
                format!(r#"{{"base":{base},"size":{size},"deepest":{deepest}}}"#)
            });
            let index = o.index.map_or("null".into(), |n| n.to_string());
            format!(
                r#"{{"name":{},"index":{index},"stack":{stack},"sections":[{}],"symbols":{}}}"#,
                quoted(&o.name),
                sections.join(","),
                o.symbols.len()
            )
        };
        let tasks: Vec<String> = image.tasks.iter().map(owner).collect();
        format!(
            r#"{{"archive":{archive},"app":{},"kernel":{},"tasks":[{}]}}"#,
            quoted(&image.name),
            owner(&image.kernel),
            tasks.join(",")
        )
    }

    /// What `address` is, as JSON: the memory or peripheral it is in, by
    /// the chip's memory map; whose it is — the kernel's or a task's — and
    /// which part, by the archive; the function or static it falls in;
    /// and all of it as a line a person reads,
    /// `i2c_driver .bss: drv_stm32xx_i2c_server::BUF + 0x4 (AXI SRAM + 0xd41c)`.
    pub fn whatis_json(&self, address: u32, map: Option<&Result<armv6m::hubris::Image, String>>) -> String {
        use crate::system::server::quoted;
        let block = armv6m::names::block(&self.machine.bus.soc, address);
        let place = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(address));
        let text = self.describe(address, map);
        let block = block.map_or("null".into(), |(name, base)| {
            format!(r#"{{"name":{},"base":{base},"offset":{}}}"#, quoted(name), address - base)
        });
        let (owner, task, part, symbol) = match &place {
            None => ("null".into(), "null".into(), "null".into(), "null".into()),
            Some(place) => (
                quoted(&place.owner.name),
                place.owner.index.map_or("null".into(), |n| n.to_string()),
                quoted(&place.part),
                place.symbol.map_or("null".into(), |(s, offset)| {
                    format!(
                        r#"{{"name":{},"base":{},"size":{},"offset":{offset},"function":{}}}"#,
                        quoted(&s.name),
                        s.base,
                        s.size,
                        s.function
                    )
                }),
            ),
        };
        let error = match map {
            Some(Err(why)) => quoted(why),
            _ => "null".into(),
        };
        format!(
            r#"{{"address":{address},"text":{},"block":{block},"owner":{owner},"task":{task},"part":{part},"symbol":{symbol},"error":{error}}}"#,
            quoted(&text)
        )
    }

    /// What `address` is, as a line a person reads:
    /// `i2c_driver .bss: drv_stm32xx_i2c_server::BUF + 0x4 (AXI SRAM + 0xd41c)`.
    pub fn describe(&self, address: u32, map: Option<&Result<armv6m::hubris::Image, String>>) -> String {
        let block = armv6m::names::block(&self.machine.bus.soc, address);
        let place = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(address));
        let mut text = String::new();
        if let Some(place) = &place {
            text += &format!("{} {}", place.owner.name, place.part);
            if let Some((symbol, offset)) = place.symbol {
                text += &format!(": {}", symbol.name);
                if offset > 0 {
                    text += &format!(" + {offset:#x}");
                }
            }
        }
        if let Some((name, base)) = block {
            let at = if address > base { format!("{name} + {:#x}", address - base) } else { name.to_string() };
            text = if text.is_empty() { at } else { format!("{text} ({at})") };
        }
        if let Some(lump) = self.wad_lump(address) {
            text = if text.is_empty() { format!("the NOR: {lump}") } else { format!("{text}: {lump}") };
        }
        if text.is_empty() {
            text = format!("{address:#010x}: nothing the chip or the archive names");
        }
        text
    }

    /// What a NOR holding a WAD — Doom's — has at `address`, if it is in
    /// one: the lump, by its name in the WAD's directory, and how far in.
    fn wad_lump(&self, address: u32) -> Option<String> {
        let armv6m::bus::Soc::H7(soc) = &self.machine.bus.soc else { return None };
        let wad = soc.quadspi.nor.as_ref()?.contents();
        let offset = address.checked_sub(armv6m::quadspi::WINDOW)? as usize;
        if offset >= wad.len() || wad.len() < 12 || !matches!(&wad[..4], b"IWAD" | b"PWAD") {
            return None;
        }
        let word = |at: usize| Some(u32::from_le_bytes(wad.get(at..at + 4)?.try_into().ok()?) as usize);
        let (lumps, directory) = (word(4)?, word(8)?);
        if offset < 12 {
            return Some("the WAD's header".to_string());
        }
        if offset >= directory && offset < directory + 16 * lumps {
            return Some(format!("the WAD's directory, entry {}", (offset - directory) / 16));
        }
        (0..lumps).find_map(|n| {
            let entry = directory + 16 * n;
            let (at, size) = (word(entry)?, word(entry + 4)?);
            if offset < at || offset >= at + size {
                return None;
            }
            let name = wad.get(entry + 8..entry + 16)?;
            let name: String = name.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
            let lump = &wad[at..(at + size).min(wad.len())];
            let what = wad_byte(&name, lump, offset - at, |marker| lump_between(wad, lumps, directory, n, marker));
            let place = if offset > at { format!("WAD lump {name} + {:#x}", offset - at) } else { format!("WAD lump {name}") };
            Some(match what {
                Some(what) => format!("{place}: {what}"),
                None => place,
            })
        })
    }

    /// Where the code at `pc` is: its owner, and the function and how far
    /// in — `i2c_driver: drv_stm32xx_i2c::I2cController::read + 0x12`.
    fn code_at(&self, pc: u32, map: Option<&Result<armv6m::hubris::Image, String>>) -> String {
        let place = map.and_then(|m| m.as_ref().ok()).and_then(|image| image.whatis(pc));
        match place {
            Some(place) => match place.symbol {
                Some((symbol, 0)) => format!("{}: {}", place.owner.name, symbol.name),
                Some((symbol, offset)) => format!("{}: {} + {offset:#x}", place.owner.name, symbol.name),
                None => format!("{} {}", place.owner.name, place.part),
            },
            None => format!("{pc:#010x}"),
        }
    }

    /// Follows where values go from now on, or stops: see
    /// `armv6m::provenance`. Answers since when it is following.
    pub fn follow_values(&mut self, on: bool) -> String {
        self.machine.follow_values(on);
        match &self.machine.provenance {
            Some(p) => format!(r#"{{"following":true,"since_ns":{}}}"#, p.since / 1000),
            None => r#"{"following":false}"#.to_string(),
        }
    }

    /// Follows everything from the board's time `from`, in picoseconds:
    /// see `armv6m::Machine::follow_everything_from`.
    pub fn follow_everything_from(&mut self, from: u64) -> String {
        self.machine.follow_everything_from(from);
        format!(r#"{{"following":"everything","from_ns":{}}}"#, from / 1000)
    }

    /// Where the value at `address` came from, as JSON: newest first, each
    /// step what it was — a store, where to and by which code; a merge of
    /// two values, the other's history beside it; a read of a peripheral's
    /// data register, and the trace record of the byte it gave — with its
    /// time in the board's nanoseconds. Empty when nothing is being
    /// followed, or the byte came from nowhere followed.
    pub fn why_json(&self, subject: Subject, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false,"steps":[]}"#.to_string();
        };
        let (story, about) = match subject {
            Subject::Address(address) => (p.story(address, 32), format!(r#""address":{address},"what":{}"#, quoted(&self.describe(address, map)))),
            // What it holds now, to tell that this is the instant asked about.
            Subject::Register(n) => (p.register_story(n as usize, 32), format!(r#""register":"r{n}","value":{},"what":"r{n}""#, self.machine.cpu.reg(n as usize))),
        };
        let story: Vec<_> = story.into_iter().map(|(_, node, byte)| (node, byte)).collect();
        let steps = self.why_steps(p, story, map, read, 32);
        format!(r#"{{"following":true,"everything":{},"since_ns":{},{about},"steps":[{}]}}"#, p.everything, p.since / 1000, steps.join(","))
    }

    /// Where the value at `address` came from, as `why_json` says, and
    /// everything else it depended on (`armv6m::provenance::Provenance::
    /// influences`), each as a step — what was held since before and the
    /// write that put it there, what came in off a bus, a register's, a
    /// constant — with `via` saying whether it chose which value (`address`)
    /// or went into it (`value`). Pointers, and constants the code loaded
    /// from beside itself, which say only which table, are left out.
    pub fn influences_json(&self, subject: Subject, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use armv6m::provenance::What;
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false,"steps":[],"influences":[]}"#.to_string();
        };
        // Memory's addresses, not what a value that happens to be large —
        // a negative number — is: from flash up, what the chip maps.
        let pointer = |value: u32| value >= 0x0800_0000 && self.machine.bus.is_memory(value);
        // The kernel's copies between tasks, and memcpy and its kin, move
        // bytes: where they read is where the bytes were, said already.
        let moves = |pc: u32| {
            let code = self.code_at(pc, map);
            code.starts_with("kernel") || ["memcpy", "memmove", "copy_forward", "copy_backward", "compiler_builtins::mem"].iter().any(|n| code.contains(n))
        };
        let all = match subject {
            Subject::Address(address) => p.influences(address, 128, &moves),
            Subject::Register(n) => p.register_influences(n as usize, 128, &moves),
        };
        let total = all.len();
        let leaves: Vec<String> = all
            .into_iter()
            .filter(|(_, node, ..)| match node.what {
                What::Held { addr, size, value, .. } => addr.wrapping_sub(node.pc) >= 4096 && !(size == 4 && pointer(value)),
                What::Register { value, .. } => !pointer(value),
                What::Store { size: 4, value, .. } => !pointer(value),
                _ => true,
            })
            .take(64)
            .filter_map(|(_, node, byte, via)| {
                let step = self.why_steps(p, vec![(node, byte)], map, read, 4).pop()?;
                Some(format!(r#"{{"via":"{}",{}"#, if via { "address" } else { "value" }, &step[1..]))
            })
            .collect();
        let why = self.why_json(subject, map, read);
        format!(r#"{},"influences":[{}],"considered":{total}}}"#, &why[..why.len() - 1], leaves.join(","))
    }

    /// Who made each byte of `[address, address + length)`, as JSON: along
    /// each byte's own way back, the code that last stored it other than to
    /// move it — a copy, a kernel's between tasks — or, made before the
    /// window, the code whose write put it where it was then held. The
    /// makers, each with its code and line, and runs of bytes `[offset,
    /// length, maker]`, maker -1 for a byte whose maker is not known.
    pub fn makers_json(&self, address: u32, length: u32, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        use armv6m::provenance::What;
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false,"makers":[],"runs":[]}"#.to_string();
        };
        let mut moving = std::collections::HashMap::new();
        let mut moves = |pc: u32| {
            *moving.entry(pc).or_insert_with(|| {
                let code = self.code_at(pc, map);
                code.starts_with("kernel") || ["memcpy", "memmove", "copy_forward", "copy_backward", "compiler_builtins::mem"].iter().any(|n| code.contains(n))
            })
        };
        let mut makers: Vec<u32> = Vec::new();
        let mut runs: Vec<(u32, u32, i64)> = Vec::new();
        for offset in 0..length.min(1 << 20) {
            let mut maker = None;
            for (_, node, _) in p.story(address.wrapping_add(offset), 32) {
                match node.what {
                    What::Store { .. } if !moves(node.pc) => maker = Some(node.pc),
                    What::Held { written: Some(mark), .. } if mark.writer < armv6m::bus::BY_EXCEPTION && !moves(mark.writer) => maker = Some(mark.writer),
                    _ => continue,
                }
                break;
            }
            let index = maker.map_or(-1, |pc| {
                makers.iter().position(|&m| m == pc).unwrap_or_else(|| {
                    makers.push(pc);
                    makers.len() - 1
                }) as i64
            });
            match runs.last_mut() {
                Some((_, n, last)) if *last == index => *n += 1,
                _ => runs.push((offset, 1, index)),
            }
        }
        let makers: Vec<String> = makers
            .iter()
            .map(|&pc| {
                format!(
                    r#"{{"pc":{pc},"code":{},"lines":[{}]}}"#,
                    quoted(&self.code_at(pc, map)),
                    self.lines_at(pc, map, read).iter().map(dwarf::line_json).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        let runs: Vec<String> = runs.iter().map(|(o, n, m)| format!("[{o},{n},{m}]")).collect();
        format!(r#"{{"following":true,"since_ns":{},"address":{address},"length":{length},"makers":[{}],"runs":[{}]}}"#, p.since / 1000, makers.join(","), runs.join(","))
    }

    /// Follows the `length` bytes at `address` forward from the board's
    /// time `from`, in picoseconds: see
    /// `armv6m::Machine::follow_forward_from`.
    pub fn follow_forward_from(&mut self, from: u64, address: u32, length: u32) -> String {
        self.machine.follow_forward_from(from, address, length);
        format!(r#"{{"following":"forward","from_ns":{},"address":{address},"length":{length}}}"#, from / 1000)
    }

    /// Where the values followed have reached, as JSON: the runs of the
    /// board's memory holding something made of them, by address — each
    /// with what it is, the code that last stored there and when — at most
    /// `limit` runs, and how many bytes in all.
    pub fn reached_json(&self, limit: usize, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false,"runs":[]}"#.to_string();
        };
        let mut bytes: Vec<(u32, u32)> = p.memory.iter().map(|(&a, &t)| (a, t)).collect();
        bytes.sort_unstable();
        // Runs of bytes next to each other, stored by the same code.
        let pc = |tag: u32| p.node(tag).map_or(0, |n| n.pc);
        let mut runs: Vec<(u32, u32, u32)> = Vec::new(); // (start, length, tag)
        for (addr, tag) in bytes.iter().copied() {
            match runs.last_mut() {
                Some((start, length, last)) if *start + *length == addr && pc(*last) == pc(tag) => *length += 1,
                _ => runs.push((addr, 1, tag)),
            }
        }
        let shown: Vec<String> = runs
            .iter()
            .take(limit)
            .map(|&(start, length, tag)| {
                let node = p.node(tag);
                let by = match node {
                    Some(node) if matches!(node.what, armv6m::provenance::What::Store { .. }) => format!(
                        r#""t":{},"code":{},"lines":[{}]"#,
                        node.time / 1000,
                        quoted(&self.code_at(node.pc, map)),
                        self.lines_at(node.pc, map, read).iter().map(dwarf::line_json).collect::<Vec<_>>().join(",")
                    ),
                    _ => r#""t":null,"code":"where it began""#.to_string(),
                };
                format!(r#"{{"addr":{start},"length":{length},"where":{},{by}}}"#, quoted(&self.describe_typed(start, map, read)))
            })
            .collect();
        // And every place it went on the way, still there or not: each
        // store of something made of it, runs of them by the code that
        // stored, first when.
        let mut stores: Vec<(u32, u32, u32, u64)> = p
            .nodes
            .iter()
            .filter_map(|node| match node.what {
                armv6m::provenance::What::Store { addr, size, .. } => Some((node.pc, addr, size, node.time)),
                _ => None,
            })
            .collect();
        stores.sort_unstable_by_key(|&(pc, addr, ..)| (pc, addr));
        let mut went: Vec<(u32, u32, u32, u64, u64)> = Vec::new(); // (pc, start, end, first, writes)
        for (pc, addr, size, time) in stores {
            match went.last_mut() {
                Some((last, _, end, first, writes)) if *last == pc && addr <= *end => {
                    *end = (*end).max(addr + size);
                    *first = (*first).min(time);
                    *writes += 1;
                }
                _ => went.push((pc, addr, addr + size, time, 1)),
            }
        }
        went.sort_unstable_by_key(|&(_, _, _, first, _)| first);
        let total = went.len();
        let went: Vec<String> = went
            .into_iter()
            .take(limit)
            .map(|(pc, start, end, first, writes)| {
                let still = (start..end).any(|a| p.memory.contains_key(&a));
                format!(
                    r#"{{"addr":{start},"length":{},"t":{},"writes":{writes},"still":{still},"where":{},"code":{},"lines":[{}]}}"#,
                    end - start,
                    first / 1000,
                    quoted(&self.describe_typed(start, map, read)),
                    quoted(&self.code_at(pc, map)),
                    self.lines_at(pc, map, read).iter().map(dwarf::line_json).collect::<Vec<_>>().join(",")
                )
            })
            .collect();
        format!(
            r#"{{"following":true,"since_ns":{},"bytes":{},"runs":{},"shown":[{}],"places":{total},"went":[{}]}}"#,
            p.since / 1000,
            bytes.len(),
            runs.len(),
            shown.join(","),
            went.join(",")
        )
    }

    /// What of the board's memory holds values that came in from outside
    /// since following began, as JSON: runs of bytes with the same history,
    /// each with where it is, what it holds, and the read it came from — at
    /// most `limit` runs, by address.
    pub fn tagged_json(&self, limit: usize, map: Option<&Result<armv6m::hubris::Image, String>>) -> String {
        use crate::system::server::quoted;
        use armv6m::provenance::What;
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false,"runs":[]}"#.to_string();
        };
        let mut bytes: Vec<(u32, u32)> = p.memory.iter().map(|(&a, &t)| (a, t)).collect();
        bytes.sort_unstable();
        let mut runs: Vec<(u32, u32, u32)> = Vec::new(); // (start, length, tag)
        for (addr, tag) in bytes {
            match runs.last_mut() {
                Some((start, length, last)) if *last == tag && *start + *length == addr => *length += 1,
                _ => runs.push((addr, 1, tag)),
            }
        }
        let total = runs.len();
        let out: Vec<String> = runs
            .into_iter()
            .take(limit)
            .map(|(start, length, tag)| {
                let origin = p.history(tag, 64).last().map(|&(_, node)| node.clone());
                let origin = match origin {
                    Some(node) => match node.what {
                        What::Read { addr, value } => format!(
                            r#"{{"register":{},"value":{value},"t":{}}}"#,
                            quoted(&armv6m::names::data_register(&self.machine.bus.soc, addr).unwrap_or_default()),
                            node.time / 1000
                        ),
                        _ => "null".into(),
                    },
                    None => "null".into(),
                };
                let held: Vec<String> = (0..length.min(16))
                    .map(|i| self.machine.bus.peek(start + i, 1).map_or("--".into(), |b| format!("{b:02x}")))
                    .collect();
                format!(
                    r#"{{"addr":{start},"length":{length},"hex":"{}","where":{},"origin":{origin}}}"#,
                    held.join(""),
                    quoted(&self.describe(start, map))
                )
            })
            .collect();
        format!(r#"{{"following":true,"since_ns":{},"runs":{total},"shown":[{}]}}"#, p.since / 1000, out.join(","))
    }

    /// One node of a value's history as JSON: what it was — a store, a
    /// read off a bus, what was held since before, a register's, a merge
    /// and its operation, a load — with its value, where, its code and
    /// line, and when. `byte`, the one of a store's or a hold's that is
    /// asked about.
    fn node_json(&self, node: &armv6m::provenance::Node, byte: u32, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        use armv6m::provenance::What;
        let lines: Vec<String> = self.lines_at(node.pc, map, read).iter().map(dwarf::line_json).collect();
        // The byte followed, where it is one of those the node is about.
        let within = |addr: u32, size: u32| if byte.wrapping_sub(addr) < size { byte } else { addr };
        let common = format!(r#""byte":{byte},"t":{},"pc":{},"code":{},"lines":[{}]"#, node.time / 1000, node.pc, quoted(&self.code_at(node.pc, map)), lines.join(","));
        match node.what {
            What::Store { addr, size, value } => format!(r#"{{"what":"store","addr":{addr},"size":{size},"value":{value},"where":{},{common}}}"#, quoted(&self.describe_typed(within(addr, size), map, read))),
            What::Read { addr, value } => {
                let register = armv6m::names::data_register(&self.machine.bus.soc, addr).unwrap_or_default();
                let record = self.record_of(&register, value, node.time);
                format!(
                    r#"{{"what":"read","addr":{addr},"value":{value},"register":{},"record":{},{common}}}"#,
                    quoted(&register),
                    record.map_or("null".into(), |(n, t)| format!(r#"{{"n":{n},"t":{}}}"#, t / 1000))
                )
            }
            What::Held { addr, size, value, written } => format!(
                r#"{{"what":"held","addr":{addr},"size":{size},"value":{value},"where":{},"ram":{},"written":{},{common}}}"#,
                quoted(&self.describe_typed(within(addr, size), map, read)),
                self.machine.bus.ram_at(addr).is_some(),
                written.map_or("null".into(), |mark| format!(r#"{{"stamp":{},"run":{},"by":{}}}"#, mark.stamp, mark.run, self.writer_json(mark.writer, map, read)))
            ),
            What::Register { n, value } => format!(
                r#"{{"what":"register","register":"{}","value":{value},{common}}}"#,
                match n {
                    13 => "sp".to_string(),
                    14 => "lr".to_string(),
                    15 => "pc".to_string(),
                    n => format!("r{n}"),
                }
            ),
            What::Merge { value, constant } => {
                // An address of a register and a register: how far the
                // second is shifted.
                let half = |at: u32| self.machine.bus.peek(at, 2).unwrap_or(0);
                let first = half(node.pc);
                let second = if armv6m::ops::wide(first) { half(node.pc.wrapping_add(2)) } else { 0 };
                let shift = armv6m::ops::index_shift(first, second).map_or(String::new(), |k| format!(r#""shift":{k},"#));
                // A second operand shifted: `add.w r4, r4, r0, lsr #26`.
                let shifted = armv6m::ops::shifted_operand(first, second).map_or(String::new(), |(kind, n)| format!(r#""rshift":["{kind}",{n}],"#));
                // And the constant it worked with: what it added, shifted by.
                let imm = match armv6m::ops::immediate(first, second) {
                    Some(armv6m::ops::Immediate::Value(v)) => format!(r#""imm":{v},"#),
                    Some(armv6m::ops::Immediate::Bits { lsb, width }) => format!(r#""bits":[{lsb},{width}],"#),
                    // Or one a register held, that the code made before.
                    None => constant.map_or(String::new(), |v| format!(r#""imm":{v},"#)),
                };
                format!(r#"{{"what":"merge","op":"{}",{shift}{shifted}{imm}"value":{},{common}}}"#, armv6m::ops::operation(first, second), value.map_or("null".into(), |v| v.to_string()))
            }
            What::Load { addr, value } => format!(r#"{{"what":"load","addr":{addr},"value":{value},"where":{},{common}}}"#, quoted(&self.describe_typed(addr, map, read))),
            What::Bytes { addr, value, .. } => format!(r#"{{"what":"bytes","addr":{addr},"value":{value},"where":{},{common}}}"#, quoted(&self.describe_typed(addr, map, read))),
        }
    }

    /// How the value at an address, in a register, or of a node of the
    /// window's history (`node`, its tag) was made, as a tree of JSON
    /// (`armv6m::provenance::Provenance::derivation`): each node with
    /// what it was (`node_json`), the copies it was before, how many
    /// times the same code made it of itself before, and what it was
    /// made of — `role` `value` part of it, `chose` what chose an entry of
    /// a table, `before` its own value before the repeats. A node left
    /// out is `cut`, to be asked about by its tag; one said already,
    /// `same`.
    pub fn derive_json(&self, subject: Option<Subject>, node: Option<u32>, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        use armv6m::provenance::{Derived, Role, What};
        let Some(p) = &self.machine.provenance else {
            return r#"{"following":false}"#.to_string();
        };
        let (tag, about) = match (subject, node) {
            (Some(Subject::Address(address)), _) => (p.memory.get(&address).copied().unwrap_or(0), format!(r#""address":{address},"what":{}"#, quoted(&self.describe(address, map)))),
            (Some(Subject::Register(n)), _) => (p.regs[n as usize], format!(r#""register":"r{n}","value":{},"what":"r{n}""#, self.machine.cpu.reg(n as usize))),
            (None, Some(tag)) => (tag, format!(r#""node":{tag}"#)),
            (None, None) => (0, String::new()),
        };
        // Memory's addresses, not what a value that happens to be large —
        // a negative number — is: from flash up, what the chip maps.
        let pointer = |value: u32| value >= 0x0800_0000 && self.machine.bus.is_memory(value);
        let says_nothing = |node: &armv6m::provenance::Node| match node.what {
            What::Held { addr, size, value, .. } => addr.wrapping_sub(node.pc) < 4096 || (size == 4 && pointer(value)),
            What::Register { n: 13 | 15, .. } => true,
            What::Register { value, .. } => pointer(value),
            // A pointer the code made and kept, or loaded: which struct,
            // which table.
            What::Store { size: 4, value, .. } | What::Load { value, .. } | What::Bytes { value, .. } => pointer(value),
            _ => false,
        };
        let tree = p.derivation(tag, 40, 400, &says_nothing);
        fn json(board: &Board, p: &armv6m::provenance::Provenance, d: &Derived, role: Role, byte: u32, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
            let Some(node) = p.node(d.tag) else { return "null".into() };
            let role = match role {
                Role::Value => "value",
                Role::Chose => "chose",
                Role::Again => "before",
            };
            let mut out = format!(r#"{{"tag":{},"role":"{role}","step":{}"#, d.tag, board.node_json(node, byte, map, read));
            if !d.copies.is_empty() {
                let copies: Vec<String> = d.copies.iter().filter_map(|&c| p.node(c)).map(|n| board.node_json(n, 0, map, read)).collect();
                out += &format!(r#","copies":[{}]"#, copies.join(","));
            }
            if d.repeats > 0 {
                out += &format!(r#","repeats":{},"earliest":{}"#, d.repeats, p.node(d.earliest).map_or("null".into(), |n| board.node_json(n, 0, map, read)));
            }
            if d.cut {
                out += r#","cut":true"#;
            }
            if d.same {
                out += r#","same":true"#;
            }
            if d.pointer {
                out += r#","pointer":true"#;
            }
            let operands: Vec<String> = d.operands.iter().map(|(role, o)| json(board, p, o, *role, 0, map, read)).collect();
            format!(r#"{out},"operands":[{}]}}"#, operands.join(","))
        }
        let byte = match subject {
            Some(Subject::Address(address)) => address,
            _ => 0,
        };
        let root = if tag == 0 { "null".to_string() } else { json(self, p, &tree, Role::Value, byte, map, read) };
        let comma = if about.is_empty() { "" } else { "," };
        format!(r#"{{"following":true,"everything":{},"since_ns":{}{comma}{about},"tree":{root}}}"#, p.everything, p.since / 1000)
    }

    /// A history's steps as JSON: each node, with the byte it is about.
    fn why_steps(
        &self,
        p: &armv6m::provenance::Provenance,
        story: Vec<(&armv6m::provenance::Node, u32)>,
        map: Option<&Result<armv6m::hubris::Image, String>>,
        read: ReadFile,
        depth: usize,
    ) -> Vec<String> {
        use armv6m::provenance::What;
        let mut out = Vec::new();
        for (node, byte) in story {
            out.push(match node.what {
                What::Store { .. } | What::Read { .. } | What::Held { .. } | What::Register { .. } => self.node_json(node, byte, map, read),
                What::Merge { .. } => {
                    let other = p.history(node.parents[1], depth / 2).into_iter().map(|(_, node)| (node, 0)).collect();
                    let other = self.why_steps(p, other, map, read, depth / 2);
                    let step = self.node_json(node, byte, map, read);
                    format!(r#"{{"other":[{}],{}"#, other.join(","), &step[1..])
                }
                // Followed through, lane by lane: never a step of its own.
                What::Bytes { .. } => continue,
                // What the address it was read from was worked out from:
                // the values that chose which entry of a table it was, each
                // where it came into the window. A constant the code loaded
                // from beside itself — a table's address — says nothing.
                What::Load { .. } => {
                    let leaves: Vec<(&armv6m::provenance::Node, u32)> = p
                        .origins(node.parents[1], 8)
                        .into_iter()
                        .filter(|(_, leaf)| !matches!(leaf.what, What::Held { addr, .. } if addr.wrapping_sub(leaf.pc) < 4096))
                        // Nor does a pointer — which table, which the load's
                        // own address already says — only what indexed it.
                        .filter(|(_, leaf)| !matches!(leaf.what, What::Held { size: 4, value, .. } | What::Register { value, .. } if value >= 0x0800_0000 && self.machine.bus.is_memory(value)))
                        .map(|(_, leaf)| (leaf, if let What::Held { addr, .. } = leaf.what { addr } else { 0 }))
                        .collect();
                    if leaves.is_empty() {
                        continue;
                    }
                    let leaves = self.why_steps(p, leaves, map, read, depth / 2);
                    let step = self.node_json(node, byte, map, read);
                    format!(r#"{{"address":[{}],{}"#, leaves.join(","), &step[1..])
                }
            });
        }
        out
    }

    /// The trace record that brought the byte a read of `register` — `I2C2
    /// RXDR` — gave: the last of its channel's at or before `time` that
    /// carried `value`. Its number, and its time.
    fn record_of(&self, register: &str, value: u32, time: u64) -> Option<(u64, u64)> {
        use armv6m::trace::{I2cEvent, Signal, SpiEvent};
        let channel = register.split(' ').next()?;
        self.trace
            .iter()
            .rev()
            .filter(|(_, record, _)| record.time <= time && record.signal.channel() == channel)
            .find(|(_, record, _)| match &record.signal {
                Signal::I2c { event: I2cEvent::Byte { byte }, .. } => u32::from(*byte) == value & 0xFF,
                Signal::Usart { received: true, byte, .. } => u32::from(*byte) == value & 0xFF,
                Signal::Spi { event: SpiEvent::Frame { miso, .. }, .. } => u32::from(*miso) == value & 0xFF,
                _ => false,
            })
            .map(|(n, record, _)| (*n, record.time))
    }

    /// The statics in `owner`'s RAM — the kernel's, or a task's by name —
    /// as JSON, by address: each one's name, address and size.
    pub fn symbols_json(&self, owner: &str, map: Option<&Result<armv6m::hubris::Image, String>>) -> Option<String> {
        use crate::system::server::quoted;
        let image = map?.as_ref().ok()?;
        let owner = if owner == "kernel" { &image.kernel } else { image.owner(owner)? };
        let in_ram = |base: u32| owner.sections.iter().any(|s| s.writable && base >= s.base && base < s.base + s.size);
        let mut statics: Vec<&armv6m::hubris::Symbol> = owner.symbols.iter().filter(|s| !s.function && in_ram(s.base)).collect();
        statics.sort_by_key(|s| (s.base, std::cmp::Reverse(s.size)));
        let out: Vec<String> = statics
            .iter()
            .map(|s| format!(r#"{{"name":{},"base":{},"size":{}}}"#, quoted(&s.name), s.base, s.size))
            .collect();
        Some(format!(r#"{{"owner":{},"statics":[{}]}}"#, quoted(&owner.name), out.join(",")))
    }

    /// When each word of RAM from `address`, `length` bytes, was last
    /// written and what wrote it, as JSON: the board's time now, in the
    /// same units — microseconds, plus one — and for each word its stamp
    /// (0: never) and the writer's number in `writers`, which says each
    /// one's code — `i2c_driver: main + 0x80e` — or `probe`, `dma` or
    /// `exception` for what wrote without an instruction.
    pub fn written_json(&self, address: u32, length: u32, map: Option<&Result<armv6m::hubris::Image, String>>, read: ReadFile) -> String {
        use crate::system::server::quoted;
        use armv6m::bus::{BY_DMA, BY_EXCEPTION, BY_PROBE};
        // Every word the bytes asked for touch, from the one the first is in.
        let words = (length.min(MEMORY_SERVED) + (address & 3)).div_ceil(4);
        let address = address & !3;
        let mut writers: Vec<u32> = Vec::new();
        let mut out = Vec::with_capacity(words as usize);
        for n in 0..words {
            match self.machine.bus.last_write(address.wrapping_add(4 * n)) {
                None => out.push("0,-1".to_string()),
                Some(armv6m::bus::Mark { stamp, writer: by, .. }) => {
                    let index = writers.iter().position(|&w| w == by).unwrap_or_else(|| {
                        writers.push(by);
                        writers.len() - 1
                    });
                    out.push(format!("{stamp},{index}"));
                }
            }
        }
        let writers: Vec<String> = writers
            .iter()
            .map(|&by| match by {
                BY_PROBE => r#"{"code":"the probe"}"#.into(),
                BY_DMA => r#"{"code":"DMA"}"#.into(),
                BY_EXCEPTION => r#"{"code":"the core, stacking an exception"}"#.into(),
                // And where in the source, innermost first.
                pc => format!(
                    r#"{{"pc":{pc},"code":{},"lines":[{}]}}"#,
                    quoted(&self.code_at(pc, map)),
                    self.lines_at(pc, map, read).iter().map(dwarf::line_json).collect::<Vec<_>>().join(",")
                ),
            })
            .collect();
        format!(
            r#"{{"address":{address},"now":{},"words":[{}],"writers":[{}]}}"#,
            armv6m::bus::write_stamp(self.machine.time),
            out.join(","),
            writers.join(",")
        )
    }

    /// Which cells of `cell` bytes of the board's RAMs were written at or
    /// after the stamp `since` (see [`Board::written_json`]), as JSON: the
    /// RAMs, the board's time now, and each cell's RAM, number and last
    /// stamp, flat, three numbers a cell — what a picture of memory lit by
    /// its writes reads, again and again, for what changed. The cells are
    /// made large enough that there are at most [`CELLS_SERVED`] of them,
    /// and the answer says how large they were made.
    pub fn writes_json(&self, since: u64, cell: u32) -> String {
        let bus = &self.machine.bus;
        let total: usize = bus.rams.iter().map(|ram| ram.bytes.len()).sum();
        let cell = cell_for(cell, total);
        let rams: Vec<String> = bus
            .rams
            .iter()
            .map(|ram| {
                let name = armv6m::names::block(&bus.soc, ram.base).map_or("RAM", |(name, _)| name);
                format!(r#"{{"base":{},"length":{},"name":"{name}"}}"#, ram.base, ram.bytes.len())
            })
            .collect();
        let mut cells = Vec::new();
        for (ram, n, stamp) in bus.written_since(since, cell) {
            cells.push(format!("{ram},{n},{stamp}"));
        }
        format!(
            r#"{{"now":{},"cell":{cell},"rams":[{}],"cells":[{}]}}"#,
            armv6m::bus::write_stamp(self.machine.time),
            rams.join(","),
            cells.join(",")
        )
    }

    /// As [`Board::writes_json`], for `[base, base + length)` of one RAM
    /// alone — no further than its end — cells numbered from `base`: what a
    /// picture zoomed in on part of a RAM reads, at a finer grain. A `base`
    /// in no RAM answers with no RAMs and no cells.
    pub fn writes_in_json(&self, since: u64, cell: u32, base: u32, length: u32) -> String {
        let base = base & !3;
        let bus = &self.machine.bus;
        let now = armv6m::bus::write_stamp(self.machine.time);
        let Some((i, offset)) = bus.ram_at(base) else {
            return format!(r#"{{"now":{now},"cell":{},"rams":[],"cells":[]}}"#, cell.max(4) & !3);
        };
        let length = length.min((bus.rams[i].bytes.len() - offset) as u32);
        let cell = cell_for(cell, length as usize);
        let name = armv6m::names::block(&bus.soc, base).map_or("RAM", |(name, _)| name);
        let cells: Vec<String> =
            bus.written_in(since, cell, base, length).unwrap_or_default().into_iter().map(|(n, stamp)| format!("0,{n},{stamp}")).collect();
        format!(
            r#"{{"now":{now},"cell":{cell},"rams":[{{"base":{base},"length":{length},"name":"{name}"}}],"cells":[{}]}}"#,
            cells.join(",")
        )
    }

    /// Up to [`MEMORY_SERVED`] bytes of the board's memory from `address`, as
    /// a debugger reads them: RAM, flash and the peripherals' registers,
    /// without a read's side effects. As JSON, hex, two digits a byte and
    /// `--` for one nothing answers at.
    pub fn memory_json(&self, address: u32, length: u32) -> String {
        let length = length.min(MEMORY_SERVED);
        let mut hex = String::with_capacity(2 * length as usize);
        for n in 0..length {
            match self.machine.bus.peek(address.wrapping_add(n), 1) {
                Ok(byte) => hex.push_str(&format!("{byte:02x}")),
                Err(_) => hex.push_str("--"),
            }
        }
        format!(r#"{{"address":{address},"length":{length},"hex":"{hex}"}}"#)
    }

    /// A digest of everything the board's memory holds — each RAM and the
    /// flash, FNV-1a 64 — with where the board stands, for telling two runs
    /// of it apart: a replay must arrive at the same digests.
    pub fn digest_json(&self) -> String {
        fn fnv(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| (hash ^ b as u64).wrapping_mul(0x0100_0000_01b3))
        }
        let bus = &self.machine.bus;
        let rams: Vec<String> = bus
            .rams
            .iter()
            .map(|ram| format!(r#"{{"base":{},"length":{},"fnv":"{:016x}"}}"#, ram.base, ram.bytes.len(), fnv(&ram.bytes)))
            .collect();
        format!(
            r#"{{"instructions":{},"time_ps":{},"flash":"{:016x}","rams":[{}]}}"#,
            self.machine.retired,
            self.machine.time,
            fnv(&bus.flash),
            rams.join(",")
        )
    }

    /// What has crossed the pins and buses, from record `since` on — or,
    /// with `since` negative, the last that many — on the channels asked
    /// for (`PB0`, `I2C2`, `SPI3`, `USART1`, `ETH`; all, if none), at most
    /// [`TRACE_SERVED`] of them, as JSON: the records, the number of the
    /// next, and the oldest still kept. Times are in nanoseconds of
    /// simulated time.
    pub fn trace_json(&self, since: i64, channels: &[&str]) -> String {
        use crate::system::server::quoted;
        use armv6m::trace::{I2cEvent, Signal, SpiEvent};
        let first = self.trace.front().map_or(self.trace_next, |(n, _, _)| *n);
        let wanted = |record: &armv6m::trace::Record| {
            let channel = record.signal.channel();
            channels.is_empty() || channels.iter().any(|c| c.eq_ignore_ascii_case(&channel))
        };
        // The last so many: from where counting back that many that match
        // stops.
        let since = if since < 0 {
            let tail = since.unsigned_abs().min(TRACE_SERVED as u64) as usize;
            self.trace.iter().rev().filter(|(_, record, _)| wanted(record)).take(tail).last().map_or(self.trace_next, |(n, _, _)| *n)
        } else {
            since as u64
        };
        let mut out = Vec::new();
        // Not past the end: a reader whose cursor is ahead of it — one that
        // read a later instant of the board and now reads an earlier one —
        // learns where it is.
        let mut next = since.max(first).min(self.trace_next);
        for (n, record, at) in self.trace.iter().skip(since.saturating_sub(first) as usize) {
            if out.len() == TRACE_SERVED {
                break;
            }
            next = n + 1;
            if !wanted(record) {
                continue;
            }
            let channel = record.signal.channel();
            let what = match &record.signal {
                Signal::Pin { high, .. } => format!(r#""high":{high}"#),
                Signal::I2c { event, .. } => match event {
                    I2cEvent::Start { address, read, ack } => {
                        format!(r#""ev":"start","addr":{address},"read":{read},"ack":{ack}"#)
                    }
                    I2cEvent::Byte { byte } => format!(r#""ev":"byte","b":{byte}"#),
                    I2cEvent::Stop => r#""ev":"stop""#.to_string(),
                },
                Signal::Spi { event, .. } => match event {
                    SpiEvent::Select { port, pin } => {
                        format!(r#""ev":"select","cs":"P{}{pin}""#, (b'A' + port) as char)
                    }
                    SpiEvent::Frame { mosi, miso } => format!(r#""ev":"frame","mosi":{mosi},"miso":{miso}"#),
                    SpiEvent::Deselect { port, pin } => {
                        format!(r#""ev":"deselect","cs":"P{}{pin}""#, (b'A' + port) as char)
                    }
                },
                Signal::Usart { received, byte, .. } => format!(r#""rx":{received},"b":{byte}"#),
                Signal::Eth { received, len, head } => {
                    let hex: String = head.iter().map(|b| format!("{b:02x}")).collect();
                    format!(r#""rx":{received},"len":{len},"head":{}"#, quoted(&hex))
                }
                Signal::Exception { number } => format!(
                    r#""exc":{number},"name":{}"#,
                    quoted(&armv6m::scs::exception::name(*number as u32))
                ),
                Signal::Syscall { r11, r4, r5 } => format!(r#""r11":{r11},"r4":{r4},"r5":{r5}"#),
                Signal::Task { index, psp } => {
                    format!(r#""task":{},"psp":{psp}"#, index.map_or("null".into(), |n| n.to_string()))
                }
                Signal::Board { what } => format!(r#""what":{}"#, quoted(what)),
            };
            out.push(format!(r#"{{"n":{n},"t":{},"at":{at},"ch":{},{what}}}"#, record.time / 1000, quoted(&channel)));
        }
        format!(r#"{{"first":{first},"next":{next},"records":[{}]}}"#, out.join(","))
    }

    /// The board's state, as the console's `board` command and the store
    /// show it.
    pub fn status(&self) -> String {
        let m = &self.machine;
        let state = match (self.machine.halted(), self.paused) {
            (true, _) => format!("halted at pc=0x{:08x}", m.cpu.pc),
            (false, true) => "paused".to_string(),
            (false, false) => "running".to_string(),
        };
        let leds: Vec<String> = self
            .leds
            .iter()
            .map(|(led, lit)| {
                let state = match lit {
                    Some(true) => "on",
                    Some(false) => "off",
                    None => "never driven",
                };
                format!("LED {:<6}{state}", led.name())
            })
            .collect();
        let leds = if leds.is_empty() { "LED         none".to_string() } else { leds.join("\n") };
        let mut network = String::new();
        if matches!(m.bus.soc, armv6m::bus::Soc::H7(_)) {
            let board = self.host.board.map_or("not heard from yet".into(), |(_, ip)| armv6m::net::show(&ip));
            network = format!("network     the host is {}, the board {board}\n", armv6m::net::show(&self.host.ip));
            if let Some(line) = &self.last_multicast {
                network += &format!("multicast   {} datagrams to all nodes, the latest {line}\n", self.multicast);
            }
            for (n, line) in &self.heard {
                network += &format!("heard #{n:<4} {line}\n");
            }
        }
        let taken: Vec<String> = m
            .taken
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .map(|(n, count)| format!("{} {count}", armv6m::scs::exception::name(n as u32)))
            .collect();
        let wired: String = m.wired().iter().map(|line| format!("wired       {line}\n")).collect();
        format!(
            "board       {} (emulated {}), in the kernel\nimage       {}\nstate       {state}\ntime        {:.6} s simulated, {} instructions\nspeed       {}, managing {:.3}× the container's clock; {:.3} s of busy waits skipped\nclocks      {}\n{leds}\nfaults      {}\nexceptions  {}\n{wired}{network}",
            self.chip,
            core_name(m.core),
            self.image,
            self.ms(m.time) / 1000.0,
            m.retired,
            self.speed.show(),
            self.ratio,
            m.skipped as f64 / 1e12,
            self.clocks_text(),
            self.faults,
            taken.join(", "),
        )
    }
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// Moves the board on: takes any new debugger off the probe's queue,
    /// answers what the debugger sent, runs the board up to the clock, and
    /// sends what it has to say. Answers whether bytes moved toward a
    /// process — which is whether something parked may now run.
    ///
    /// Kernel state only, like the edge pump it runs beside: the board,
    /// rings, the socket arena, and the store's clock and console.
    pub fn service_board(&mut self, at: u64) -> bool {
        self.look_for_board_now();
        let board = Rc::clone(&self.board);
        let mut slot = board.borrow_mut();
        let Slot::Present(board) = &mut *slot else {
            drop(slot);
            return self.service_usb();
        };

        board.stamp = at;
        self.accept_debugger(board);
        board.give_trace_the_stacks(|path| self.read_file(path));
        let input = match &board.client {
            Some(client) => {
                let queued = self.rings.borrow().queued(client.receive);
                let mut bytes = vec![0u8; queued];
                self.rings.borrow_mut().take(client.receive, &mut bytes);
                bytes
            }
            None => Vec::new(),
        };
        if !input.is_empty() {
            board.converse(&input);
        }
        if let Some(now) = self.monotonic() {
            board.advance(now);
        }
        let moved = self.flush_debugger(board);
        self.hang_up_if_gone(board);

        for line in board.console.drain(..) {
            let mut line = line.into_bytes();
            line.push(b'\n');
            self.store.write(crate::paths::CONSOLE_STDERR, &line);
        }
        drop(slot);
        // The USB probe's answers, and any other provider's, are what a
        // parked process may be waiting for too.
        self.service_usb() || moved
    }

    /// Looks for the board now rather than at the next service, for
    /// whoever needs to know whether there is one.
    pub(crate) fn look_for_board_now(&mut self) {
        if matches!(*self.board.borrow(), Slot::Unlooked) {
            let slot = self.look_for_board();
            *self.board.borrow_mut() = slot;
        }
    }

    /// When an idle container must wake to move the board on.
    pub fn board_wake(&self) -> Option<u64> {
        match &*self.board.borrow() {
            Slot::Present(board) => board.wake(),
            _ => None,
        }
    }

    /// Reads `/etc/zaqaru/board` and, if it asks for one, builds the board
    /// and puts its probe's listener in the socket arena.
    fn look_for_board(&mut self) -> Slot {
        let Ok(text) = self.read_file(CONFIGURATION) else {
            return Slot::Absent;
        };
        let configuration = match parse(&String::from_utf8_lossy(&text)) {
            Ok(configuration) => configuration,
            Err(why) => {
                self.board_says(&format!("board: {}: {why}", String::from_utf8_lossy(CONFIGURATION)));
                return Slot::Absent;
            }
        };
        let firmware = match self.read_file(configuration.image.as_bytes()) {
            Ok(firmware) => firmware,
            Err(errno) => {
                self.board_says(&format!("board: {}: {errno:?}", configuration.image));
                return Slot::Absent;
            }
        };
        let part = armv6m::part::Part::named(&configuration.chip).expect("parse checked the part");
        let mut machine = match Machine::new(part, &firmware) {
            Ok(machine) => machine,
            Err(why) => {
                self.board_says(&format!("board: {}: {why}", configuration.image));
                return Slot::Absent;
            }
        };
        // Recording what crosses the pins and buses, for a probe to show.
        machine.trace().on = true;
        let mut nor = None;
        if let Some(path) = &configuration.nor {
            let made = self
                .read_file(path.as_bytes())
                .map_err(|errno| format!("{errno:?}"))
                .and_then(|contents| armv6m::quadspi::Nor::new(&contents, 16 << 20));
            match made.and_then(|made| machine.attach_nor(made.clone()).map(|()| made)) {
                Ok(made) => nor = Some(made),
                Err(why) => self.board_says(&format!("board: nor {path}: {why}")),
            }
        }
        for line in &configuration.wiring {
            let wired = armv6m::devices::Wiring::parse(line).and_then(|w| machine.attach(w));
            if let Err(why) = wired {
                self.board_says(&format!("board: {line}: {why}"));
            }
        }
        let (address, port) = configuration.probe;
        let probe = Address::Inet { address, port };
        let listener = {
            let mut sockets = self.sockets.borrow_mut();
            let listener = sockets.create(crate::socket::family::INET, crate::socket::kind::STREAM);
            if let Some(socket) = sockets.get_mut(listener) {
                socket.state = State::Listening {
                    address: probe.clone(),
                    backlog: 4,
                    queue: Default::default(),
                };
            }
            listener
        };
        // Announced as a guest's listener is, so that the host can reach the
        // probe through the edge too, as a debugger on the far side of a
        // published port would.
        self.register_listener(port);
        self.board_says(&format!(
            "board: {} running {}, probe on {}.{}.{}.{}:{port}",
            configuration.chip,
            configuration.image,
            address >> 24,
            address >> 16 & 0xff,
            address >> 8 & 0xff,
            address & 0xff,
        ));
        Slot::Present(Box::new(Board {
            machine,
            debugger: Debugger::default(),
            chip: configuration.chip,
            image: configuration.image,
            archive: configuration.archive,
            map: RefCell::new(None),
            debug: RefCell::default(),
            stacks_for: None,
            listener,
            probe,
            client: None,
            origin: None,
            speed: Speed::Scaled(1000),
            paused: false,
            rate: (0, 0),
            ratio: 0.0,
            behind: false,
            now: 0,
            leds: configuration.leds.into_iter().map(|led| (led, None)).collect(),
            name: configuration.name,
            trace: VecDeque::new(),
            stamp: 0,
            trace_next: 0,
            serial: Vec::new(),
            faults: 0,
            console: Vec::new(),
            usb_probe: Probe::default(),
            usb_responses: VecDeque::new(),
            host: armv6m::net::Peer::default(),
            heard: VecDeque::new(),
            heard_count: 0,
            pings: 0,
            configured: configuration.wiring.clone(),
            wiring: configuration.wiring,
            multicast: 0,
            last_multicast: None,
            nor,
            keyboard: configuration.keyboard,
        }))
    }

    fn board_says(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.store.write(crate::paths::CONSOLE_STDERR, &bytes);
    }

    /// The whole of a regular file in the container's filesystem.
    pub(crate) fn read_file(&self, path: &[u8]) -> Result<Vec<u8>, crate::errno::Errno> {
        let root = self.vfs.root();
        let vnode = self.vfs.resolve(root, path, crate::vfs::Lookup::FOLLOW)?;
        let inode = self.vfs.inode(vnode)?;
        if !inode.is_regular() {
            return Err(crate::errno::Errno::Invalid);
        }
        let filesystem = self.vfs.filesystem_of(vnode)?;
        Ok(filesystem.contents(&inode, vnode.inode)?.to_vec())
    }

    /// Takes the newest connection off the probe's queue; an older one is
    /// hung up on, as a probe with one debug port would.
    fn accept_debugger(&mut self, board: &mut Board) {
        loop {
            let Some(socket) = self.sockets.borrow_mut().dequeue(board.listener) else { return };
            let Some(Endpoint { receive, transmit, .. }) = self.sockets.borrow().endpoint(socket).cloned() else {
                continue;
            };
            if let Some(old) = board.client.take() {
                self.close_debugger(old);
            }
            board.client = Some(Client { socket, receive, transmit, partial: Vec::new(), outbox: Vec::new() });
            // Whatever the last debugger left, the next finds it running.
            board.release();
        }
    }

    /// Moves queued replies into the debugger's ring, as far as it has room.
    fn flush_debugger(&mut self, board: &mut Board) -> bool {
        let Some(client) = &mut board.client else { return false };
        if client.outbox.is_empty() {
            return false;
        }
        let given = self.rings.borrow_mut().give(client.transmit, &client.outbox);
        client.outbox.drain(..given);
        given > 0
    }

    /// Lets go of a debugger that has closed its end: no writer left on what
    /// it sends, and nothing of it unread.
    fn hang_up_if_gone(&mut self, board: &mut Board) {
        let gone = match &board.client {
            Some(client) => {
                let rings = self.rings.borrow();
                rings.writers(client.receive) == 0 && rings.queued(client.receive) == 0
            }
            None => false,
        };
        if gone {
            if let Some(client) = board.client.take() {
                self.close_debugger(client);
            }
            board.release();
        }
    }

    /// The board's end of a connection, closed as a process's `close` would.
    fn close_debugger(&mut self, client: Client) {
        {
            let mut rings = self.rings.borrow_mut();
            rings.release(client.receive, End::Read);
            rings.release(client.transmit, End::Write);
        }
        let retired = self.sockets.borrow_mut().release(client.socket);
        crate::socket::detach_endpoint(&mut self.rings.borrow_mut(), retired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_reads_as_written() {
        let parsed = parse("# the G031 demo\nchip stm32g031\nimage /hubris/final.bin\nprobe 127.0.0.1:3333\n").unwrap();
        assert_eq!(
            parsed,
            Configuration {
                chip: "stm32g031".into(),
                image: "/hubris/final.bin".into(),
                archive: None,
                probe: (0x7F00_0001, 3333),
                leds: vec![Led { port: 2, pin: 6, active_low: true, color: "green" }],
                wiring: vec![],
                name: String::new(),
                nor: None,
                keyboard: None,
            }
        );
        let wired = parse("chip stm32h753\nimage /f.bin\ni2c I2C2 0x48 tmp117\nspi SPI3 PA15 rot PD0\n").unwrap();
        assert_eq!(wired.wiring, ["i2c I2C2 0x48 tmp117", "spi SPI3 PA15 rot PD0"]);
        assert!(parse("image /f.bin\ni2c I2C2 0x48 lm75").is_err());
        let doom = parse("chip stm32h753\nimage /f.bin\nnor /hubris/doom1.wad\nkeyboard USART3\n").unwrap();
        assert_eq!((doom.nor.as_deref(), doom.keyboard), (Some("/hubris/doom1.wad"), Some(2)));
        assert!(parse("image /f.bin\nkeyboard SPI1").is_err());
    }

    #[test]
    fn base64_is_the_standard_alphabet_padded() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xFF, 0xEF]), "/+8=");
    }

    #[test]
    fn speeds_and_durations_read_as_written() {
        assert_eq!(Speed::parse("1"), Some(Speed::Scaled(1000)));
        assert_eq!(Speed::parse("0.25×"), Some(Speed::Scaled(250)));
        assert_eq!(Speed::parse("max"), Some(Speed::Max));
        assert_eq!(Speed::parse("0"), None);
        assert_eq!(parse_duration("10ms", 2500), Some(10_000_000_000));
        assert_eq!(parse_duration("250us", 2500), Some(250_000_000));
        assert_eq!(parse_duration("1000cycles", 2500), Some(2_500_000));
        assert_eq!(parse_duration("2 fortnights", 2500), None);
        assert_eq!(hz(400_000_000), "400 MHz");
        assert_eq!(hz(32_768), "32.768 kHz");
    }

    #[test]
    fn a_part_brings_its_board_s_led_unless_told_otherwise() {
        let g070 = parse("chip STM32G070RBTx\nimage /f.bin").unwrap();
        assert_eq!((g070.chip.as_str(), g070.leds), ("stm32g070", vec![Led { port: 0, pin: 5, active_low: false, color: "green" }]));
        let told = parse("chip stm32g031\nimage /f.bin\nled PA12 low").unwrap();
        assert_eq!(told.leds, vec![Led { port: 0, pin: 12, active_low: true, color: "green" }]);
        assert_eq!(Led::parse("PB14 high red"), Some(Led { port: 1, pin: 14, active_low: false, color: "red" }));
        assert_eq!(Led::parse("PB14 high mauve"), None);
        assert!(parse("image /f.bin\nled PZ9 low").is_err());
        let h753 = parse("chip STM32H753ZITx\nimage /f.bin").unwrap();
        assert_eq!((h753.chip.as_str(), h753.leds.len()), ("stm32h753", 3));
    }

    #[test]
    fn the_probe_and_chip_have_defaults_and_the_image_does_not() {
        assert_eq!(parse("image /f.bin").unwrap().probe, (crate::socket::INADDR_LOOPBACK, 3333));
        assert!(parse("chip stm32g031").is_err());
        assert!(parse("image /f.bin\nchip stm32f407").is_err());
        assert!(parse("image /f.bin\nflavour vanilla").is_err());
    }
}
