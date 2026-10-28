//! Enough of an STM32H743/753 around the Cortex-M7 for Hubris's H7 apps to
//! boot unmodified.
//!
//! What startup waits on is modelled as the reference manual (RM0433)
//! has it, settled at once: PWR's voltage scaling ready as soon as asked
//! for, RCC's oscillators and PLLs ready as soon as enabled, the system
//! clock switched as soon as switched, the flash wait states read back. Then
//! the peripherals a task drives: GPIO ports A to K (with the pins a board
//! drives from outside, such as the Nucleo's user button on PC13), EXTI and
//! SYSCFG's line routing for their interrupts, the USARTs, I2C1 to 4
//! ([`crate::i2c`]), SPI1 to 6 ([`crate::spi_h7`]), the general-purpose
//! timers as time bases ([`crate::timer`]), the RNG, DBGMCU, the flash
//! interface ([`crate::flash_h7`]), the QUADSPI with a NOR flash mapped
//! through it ([`crate::quadspi`]), and the LTDC with the panel it drives
//! ([`crate::ltdc`]). RCC's reset bits reset the peripherals they name.
//!
//! The parts wired to the board's buses ([`crate::devices`]) are attached
//! with [`Stm32h7::attach`]: an I2C part answers its address on its
//! controller; a SPI part is selected while its chip-select pin is an
//! output driven low, and a line it drives back (the RoT's ROT_IRQ) drives
//! that GPIO pin from outside.
//!
//! Everything else in the peripheral windows is a plain register file
//! that remembers what was written, and remembers that it was touched, as
//! the G0's is.

use std::collections::{BTreeMap, BTreeSet};

use crate::clock::Clocks;
use crate::devices::{SpiPart, Wiring};
use crate::eth_h7::{self, Ethernet};
use crate::flash_h7;
use crate::gpio::{Edge, Gpio};
use crate::i2c::I2c;
use crate::ltdc::Ltdc;
use crate::quadspi::Quadspi;
use crate::spi_h7::Spi;
use crate::timer::Timer;
use crate::trace::{Signal, SpiEvent, Trace};
use crate::usart::{self, Usart};

const RCC: u32 = 0x5802_4400;
const PWR: u32 = 0x5802_4800;
const GPIO: u32 = 0x5802_0000;
const PORTS: usize = 11;
const EXTI: u32 = 0x5800_0000;
const SYSCFG: u32 = 0x5800_0400;
const RNG: u32 = 0x4802_1800;
/// DBGMCU, as the processor reaches it, and as a debugger does on the
/// debug APB.
pub const DBGMCU: u32 = 0x5C00_1000;
pub const DBGMCU_DEBUG: u32 = 0xE00E_1000;

/// The H7's USARTs and UARTs, 1 to 8: their bases, and their interrupt
/// numbers.
pub const USARTS: [(u32, u32); 8] = [
    (0x4001_1000, 37),
    (0x4000_4400, 38),
    (0x4000_4800, 39),
    (0x4000_4C00, 52),
    (0x4000_5000, 53),
    (0x4001_1400, 71),
    (0x4000_7800, 82),
    (0x4000_7C00, 83),
];

/// I2C1 to 4: their bases, and their event and error interrupts.
pub const I2CS: [(u32, u32, u32); 4] =
    [(0x4000_5400, 31, 32), (0x4000_5800, 33, 34), (0x4000_5C00, 72, 73), (0x5800_1C00, 95, 96)];
/// SPI1 to 6: their bases and interrupts.
pub const SPIS: [(u32, u32); 6] =
    [(0x4001_3000, 35), (0x4000_3800, 36), (0x4000_3C00, 51), (0x4001_3400, 84), (0x4001_5000, 85), (0x5800_1400, 86)];
/// The general-purpose and basic timers: base, interrupt, and whether the
/// counter is 32 bits.
const TIMERS: [(u32, u32, bool); 9] = [
    (0x4000_0000, 28, true),
    (0x4000_0400, 29, false),
    (0x4000_0800, 30, false),
    (0x4000_0C00, 50, true),
    (0x4000_1000, 54, false),
    (0x4000_1400, 55, false),
    (0x4001_4000, 116, false),
    (0x4001_4400, 117, false),
    (0x4001_4800, 118, false),
];
/// HSI's frequency, which the H7 runs on out of reset.
const HSI: u64 = 64_000_000;
/// How many of [`TIMERS`] are on APB1; the rest are on APB2.
const APB1_TIMERS: usize = 6;
const PERIPHERAL: u32 = 0x400;

const FLASH_IRQ: u32 = 4;
/// EXTI lines 0 to 15 and the interrupts they raise: lines 0 to 4 one
/// each, 5 to 9 and 10 to 15 shared.
const EXTI_IRQS: [u32; 16] = [6, 7, 8, 9, 10, 23, 23, 23, 23, 23, 40, 40, 40, 40, 40, 40];

/// IDCODE: revision V of the H742/743/750/753 die.
const IDCODE: u32 = 0x2003_0450;

// RCC_RSR's flags.
const RSR_RMVF: u32 = 1 << 16;
const RSR_CPU: u32 = 1 << 17;
const RSR_D1: u32 = 1 << 19;
const RSR_D2: u32 = 1 << 20;
const RSR_BOR: u32 = 1 << 21;
const RSR_PIN: u32 = 1 << 22;
const RSR_POR: u32 = 1 << 23;
const RSR_SFT: u32 = 1 << 24;

/// RM0433 11.4: ports A and B come out of reset with their debug pins
/// configured, the rest all-analog.
fn port_reset(port: usize) -> Gpio {
    match port {
        0 => Gpio::new(0xABFF_FFFF, 0x0C00_0000, 0x6400_0000),
        1 => Gpio::new(0xFFFF_FEBF, 0x0000_00C0, 0x0000_0100),
        _ => Gpio::new(0xFFFF_FFFF, 0, 0),
    }
}

#[derive(Clone, Debug)]
pub struct Stm32h7 {
    rcc: [u32; 0x100],
    pwr: [u32; 0x20],
    gpio: Vec<Gpio>,
    syscfg: [u32; 0x100],
    exti: [u32; 0x40],
    /// The levels the GPIO pins were last seen at, for EXTI's edges.
    sensed: [u16; PORTS],
    rng: Rng,
    dbgmcu: [u32; 0x20],
    other: BTreeMap<u32, u32>,
    pub flash_interface: flash_h7::Interface,
    /// Register addresses outside the modelled peripherals that software
    /// read or wrote.
    pub unmodelled: BTreeSet<u32>,
    /// Output changes since the machine last drained them.
    pub edges: Vec<Edge>,
    pub usarts: [Usart; 8],
    /// Bytes the USARTs transmitted since the machine last drained them.
    pub serial: Vec<usart::Byte>,
    pub i2cs: [I2c; 4],
    pub spis: [Spi; 6],
    /// The parts on the SPI buses.
    pub spi_parts: Vec<SpiWired>,
    timers: Vec<Timer>,
    pub eth: Ethernet,
    pub quadspi: Quadspi,
    pub ltdc: Ltdc,
    /// What RCC's registers make of the oscillators, worked out again
    /// whenever they are written.
    pub clocks: Clocks,
    /// What crosses the pins and buses, for whoever is watching; and the
    /// pins' levels as it last saw them.
    pub trace: Trace,
    watched: [u16; PORTS],
}

/// A part on a SPI bus: its controller, its chip-select, and whether that
/// is selecting it now.
#[derive(Clone, Debug)]
pub struct SpiWired {
    pub controller: usize,
    pub cs: (usize, u8),
    pub part: SpiPart,
    selected: bool,
}

#[derive(Clone, Debug, Default)]
struct Rng {
    cr: u32,
    /// The generator: xorshift, seeded the same at every power-on, so that
    /// a run is a pure function of its image. Entropy it is not.
    state: u32,
}

impl Default for Stm32h7 {
    fn default() -> Self {
        let mut soc = Stm32h7 {
            rcc: [0; 0x100],
            pwr: [0; 0x20],
            gpio: (0..PORTS).map(port_reset).collect(),
            syscfg: [0; 0x100],
            exti: [0; 0x40],
            sensed: [0; PORTS],
            rng: Rng::default(),
            dbgmcu: [0; 0x20],
            other: BTreeMap::new(),
            flash_interface: flash_h7::Interface::default(),
            unmodelled: BTreeSet::new(),
            edges: Vec::new(),
            usarts: Default::default(),
            serial: Vec::new(),
            i2cs: Default::default(),
            spis: Default::default(),
            spi_parts: Vec::new(),
            timers: TIMERS.iter().map(|&(_, _, wide)| Timer::new(wide, Clocks::period(HSI))).collect(),
            eth: Ethernet::default(),
            quadspi: Quadspi::default(),
            ltdc: Ltdc::default(),
            clocks: Clocks::reset(HSI),
            trace: Trace::default(),
            watched: [0; PORTS],
        };
        soc.rcc[0xD0 / 4] = RSR_POR | RSR_PIN | RSR_BOR | RSR_D2 | RSR_D1 | RSR_CPU;
        soc.rng.state = 0x2545_F491;
        soc.reset_registers(0);
        soc
    }
}

impl Stm32h7 {
    /// A system reset: what software reset, and RSR saying so.
    pub fn reset(&mut self, flash: &mut [u8], now: u64) {
        self.rcc[0xD0 / 4] |= RSR_SFT | RSR_PIN | RSR_CPU;
        self.reset_registers(now);
        self.flash_interface.reset(flash);
        self.reclock(now);
    }

    /// Works the clocks out again from RCC, and gives the timers their new
    /// kernel clocks from `now`.
    fn reclock(&mut self, now: u64) {
        let clocks = Clocks::h7(&self.rcc);
        if clocks == self.clocks {
            return;
        }
        self.clocks = clocks;
        for (n, timer) in self.timers.iter_mut().enumerate() {
            let bus = usize::from(n >= APB1_TIMERS);
            timer.set_period(Clocks::period(clocks.timers[bus]), now);
        }
        // D2CCIP2R: USART1 and 6's kernel clock, and the rest's: their
        // APB, PLL2Q, PLL3Q, HSI, CSI or LSE.
        let ccip2 = self.rcc[0x54 / 4];
        for n in 0..self.usarts.len() {
            self.advance_usart(n, now);
            let (bus, sel) = if n == 0 || n == 5 { (1, ccip2 >> 3 & 7) } else { (0, ccip2 & 7) };
            let hz = match sel {
                0 => clocks.pclk[bus],
                1 => clocks.pll[1][1],
                2 => clocks.pll[2][1],
                3 => clocks.hsi,
                4 => clocks.csi,
                _ => 32_768,
            };
            self.usarts[n].set_clock(Clocks::period(hz));
        }
        // I2C1 to 3's kernel clock from D2CCIP2R's I2C123SEL, I2C4's from
        // D3CCIPR's I2C4SEL: their APB, PLL3R, HSI or CSI.
        let kernel = |sel: u32, pclk: u64| match sel & 3 {
            0 => pclk,
            1 => clocks.pll[2][2],
            2 => clocks.hsi,
            _ => clocks.csi,
        };
        for (n, i2c) in self.i2cs.iter_mut().enumerate() {
            i2c.advance(now);
            let hz = if n < 3 { kernel(ccip2 >> 12, clocks.pclk[0]) } else { kernel(self.rcc[0x58 / 4] >> 8, clocks.pclk[3]) };
            i2c.set_clock(Clocks::period(hz));
        }
        // SPI1 to 3's kernel clock from D2CCIP1R's SPI123SEL — PLL1Q, PLL2P,
        // PLL3P, I2S_CKIN, or per_ck, which is HSI unless CKPERSEL says
        // otherwise; SPI4 and 5's from SPI45SEL and SPI6's from D3CCIPR's
        // SPI6SEL — their APB, PLL2Q, PLL3Q, HSI, CSI or HSE.
        let ccip1 = self.rcc[0x50 / 4];
        let per = match self.rcc[0x4C / 4] >> 28 & 3 {
            0 => clocks.hsi,
            1 => clocks.csi,
            _ => crate::clock::HSE,
        };
        let peripheral = |sel: u32, pclk: u64| match sel & 7 {
            0 => pclk,
            1 => clocks.pll[1][1],
            2 => clocks.pll[2][1],
            3 => clocks.hsi,
            4 => clocks.csi,
            _ => crate::clock::HSE,
        };
        for n in 0..self.spis.len() {
            self.advance_spi(n, now);
            let hz = match n {
                0..=2 => match ccip1 >> 12 & 7 {
                    0 => clocks.pll[0][1],
                    1 => clocks.pll[1][0],
                    2 => clocks.pll[2][0],
                    3 => 0,
                    _ => per,
                },
                3 | 4 => peripheral(ccip1 >> 16, clocks.pclk[1]),
                _ => peripheral(self.rcc[0x58 / 4] >> 28, clocks.pclk[3]),
            };
            self.spis[n].set_clock(Clocks::period(hz));
        }
    }

    fn reset_registers(&mut self, now: u64) {
        let rsr = self.rcc[0xD0 / 4];
        self.rcc = [0; 0x100];
        self.rcc[0xD0 / 4] = rsr;
        // CR: HSI on, at 64 MHz. CFGR: HSI the system clock.
        self.rcc[0x00] = 0x0000_0025;
        self.rcc[0x28 / 4] = 0x0202_0200;
        self.rcc[0x2C / 4] = 0x01FF_0000;
        for divr in [0x30, 0x38, 0x40] {
            self.rcc[divr / 4] = 0x0101_0280;
        }
        self.pwr = [0; 0x20];
        // CR1, CR3 (LDO on, supply configuration unlocked), D3CR (VOS3).
        self.pwr[0x00] = 0xF000_C000;
        self.pwr[0x0C / 4] = 0x0000_0006;
        self.pwr[0x18 / 4] = 0x0000_4000;
        for (port, gpio) in self.gpio.iter_mut().enumerate() {
            gpio.reset_to(&port_reset(port));
        }
        self.syscfg = [0; 0x100];
        self.exti = [0; 0x40];
        // CPUIMR1: the direct lines, 22 and up, unmasked.
        self.exti[0x80 / 4] = 0xFFC0_0000;
        self.sensed = std::array::from_fn(|port| self.gpio[port].idr() as u16);
        self.watch_pins(now);
        self.rng.cr = 0;
        self.dbgmcu = [0; 0x20];
        self.other.clear();
        self.quadspi.reset();
        self.ltdc.reset();
        for usart in &mut self.usarts {
            usart.reset();
        }
        for i2c in &mut self.i2cs {
            i2c.reset();
        }
        for spi in &mut self.spis {
            spi.reset();
        }
        for timer in &mut self.timers {
            timer.reset();
        }
        self.eth.reset();
        self.chip_selects(now);
    }

    /// Wires a part to the board's buses.
    pub fn attach(&mut self, wiring: Wiring) -> Result<(), String> {
        match wiring {
            Wiring::I2c { controller, part } => {
                let i2c = self.i2cs.get_mut(controller).ok_or_else(|| format!("the H7 has no I2C{}", controller + 1))?;
                if i2c.targets.iter().any(|t| t.address() == part.address()) {
                    return Err(format!("I2C{} already has a part at 0x{:02x}", controller + 1, part.address()));
                }
                i2c.targets.push(part);
            }
            Wiring::Spi { controller, cs, part } => {
                if controller >= self.spis.len() {
                    return Err(format!("the H7 has no SPI{}", controller + 1));
                }
                self.spi_parts.push(SpiWired { controller, cs, part, selected: false });
                self.chip_selects(0);
            }
        }
        Ok(())
    }

    /// The switch between the MAC and the cable, if a KSZ8463 is wired:
    /// the MAC is its port 3 and the cable its port 1.
    pub fn switch(&self) -> Option<&crate::devices::Ksz8463> {
        self.spi_parts.iter().find_map(|w| match &w.part {
            SpiPart::Ksz8463(ksz) => Some(ksz),
            _ => None,
        })
    }

    /// The parts on the buses, as whoever is watching sees them.
    pub fn parts(&self) -> Vec<crate::devices::PartView> {
        use crate::devices::PartView;
        let i2c = self.i2cs.iter().enumerate().flat_map(|(n, i2c)| i2c.targets.iter().map(move |part| PartView::i2c(n, part)));
        let spi = self.spi_parts.iter().map(|w| PartView::spi(w.controller, w.cs, &w.part));
        i2c.chain(spi).collect()
    }

    /// What is wired to the buses, as the lines that would wire it.
    pub fn wired(&self) -> Vec<String> {
        let i2c = self.i2cs.iter().enumerate().flat_map(|(n, i2c)| {
            i2c.targets.iter().map(move |part| Wiring::I2c { controller: n, part: part.clone() }.describe())
        });
        let spi = self
            .spi_parts
            .iter()
            .map(|w| Wiring::Spi { controller: w.controller, cs: w.cs, part: w.part.clone() }.describe());
        i2c.chain(spi).collect()
    }

    /// The SPI parts' chip-selects, after any change to the pins: a part
    /// is selected while its pin is an output driven low (a board pulls an
    /// undriven one high), and the lines the parts drive back follow.
    fn chip_selects(&mut self, now: u64) {
        for wired in &mut self.spi_parts {
            let (port, pin) = wired.cs;
            let gpio = &self.gpio[port];
            let output = gpio.moder >> (2 * pin) & 3 == 0b01;
            let low = output && gpio.odr >> pin & 1 == 0;
            if low != wired.selected {
                wired.selected = low;
                let (controller, port, pin) = (wired.controller as u8, port as u8, pin);
                if low {
                    wired.part.select();
                    self.trace.record(now, Signal::Spi { controller, event: SpiEvent::Select { port, pin } });
                } else {
                    wired.part.deselect(now);
                    self.trace.record(now, Signal::Spi { controller, event: SpiEvent::Deselect { port, pin } });
                }
            }
            if let Some(((port, pin), level)) = wired.part.line() {
                self.gpio[port].drive(pin, Some(level));
            }
        }
        self.sense();
        self.watch_pins(now);
    }

    /// Which of a kind of peripheral an address is in, and the offset into
    /// it, from their bases.
    fn instance(bases: impl Iterator<Item = u32>, addr: u32) -> Option<(usize, u32)> {
        bases.enumerate().find(|&(_, base)| (base..base + PERIPHERAL).contains(&addr)).map(|(n, base)| (n, addr - base))
    }

    fn i2c(addr: u32) -> Option<(usize, u32)> {
        Self::instance(I2CS.iter().map(|i| i.0), addr)
    }

    fn spi(addr: u32) -> Option<(usize, u32)> {
        Self::instance(SPIS.iter().map(|s| s.0), addr)
    }

    fn timer(addr: u32) -> Option<(usize, u32)> {
        Self::instance(TIMERS.iter().map(|t| t.0), addr)
    }

    /// Whether a load at `addr` pops a FIFO — a USART's, I2C's or SPI's
    /// receive data, the RNG's — or reads a timer, whose counter changes by
    /// itself.
    pub fn impure(addr: u32) -> bool {
        USARTS.iter().any(|&(base, _)| addr == base + 0x24)
            || I2CS.iter().any(|&(base, _, _)| addr == base + 0x24)
            || SPIS.iter().any(|&(base, _)| addr == base + 0x30)
            || addr == RNG + 0x08
            || Self::timer(addr).is_some()
    }

    /// The time the next timed event is due at: a timer's update, a byte
    /// on a USART's line.
    pub fn due(&self) -> Option<u64> {
        let timers = self.timers.iter().filter_map(Timer::due);
        let usarts = self.usarts.iter().filter_map(Usart::due);
        let i2cs = self.i2cs.iter().filter_map(I2c::due);
        let spis = self.spis.iter().filter_map(Spi::due);
        let parts = self.spi_parts.iter().filter_map(|w| w.part.due());
        let rest = [self.flash_interface.due(), self.eth.due()].into_iter().flatten();
        timers.chain(usarts).chain(i2cs).chain(spis).chain(parts).chain(rest).min()
    }

    /// Brings the timed peripherals up to time `now`.
    pub fn advance(&mut self, now: u64) {
        for timer in &mut self.timers {
            timer.advance(now);
        }
        for n in 0..self.usarts.len() {
            self.advance_usart(n, now);
        }
        for n in 0..self.i2cs.len() {
            self.i2cs[n].advance(now);
            self.trace_i2c(n);
        }
        for n in 0..self.spis.len() {
            self.advance_spi(n, now);
        }
        for wired in &mut self.spi_parts {
            wired.part.advance(now);
        }
        self.chip_selects(now);
        self.flash_interface.advance(now);
        self.eth.advance(now);
    }

    /// SPI `n` brought up to `now`, exchanging each frame with whatever
    /// its chip-select selects.
    fn advance_spi(&mut self, n: usize, now: u64) {
        let (parts, trace) = (&mut self.spi_parts, &mut self.trace);
        let mut bus = |mosi: u8, at: u64| {
            let miso =
                parts.iter_mut().find(|w| w.controller == n && w.selected).map_or(0xFF, |w| w.part.exchange(mosi, at));
            trace.record(at, Signal::Spi { controller: n as u8, event: SpiEvent::Frame { mosi, miso } });
            miso
        };
        self.spis[n].advance(now, &mut bus);
    }

    /// A USART brought up to `now`, and what it finished sending passed on.
    fn advance_usart(&mut self, n: usize, now: u64) {
        let usart = &mut self.usarts[n];
        usart.advance(now);
        for byte in usart.sent.iter().copied() {
            self.trace.record(now, Signal::Usart { port: n as u8, received: false, byte });
        }
        for byte in usart.arrived.drain(..) {
            self.trace.record(now, Signal::Usart { port: n as u8, received: true, byte });
        }
        self.serial.extend(usart.sent.drain(..).map(|byte| usart::Byte { port: n as u8, byte }));
    }

    /// Whether `addr` is in the H7's peripheral windows: APB1 and 2, AHB1
    /// and 2, AXI's GPV and AHB3, APB3, APB4 and AHB4, and DBGMCU at both
    /// its addresses.
    pub fn contains(addr: u32) -> bool {
        matches!(
            addr,
            0x4000_0000..=0x4000_D3FF
                | 0x4001_0000..=0x4001_77FF
                | 0x4002_0000..=0x4007_FFFF
                | 0x4802_0000..=0x4802_2BFF
                | 0x5000_0000..=0x5000_3FFF
                | 0x5100_0000..=0x520F_FFFF
                | 0x5800_0000..=0x5800_6FFF
                | 0x5802_0000..=0x5802_6FFF
                | 0x5C00_1000..=0x5C00_1FFF
                | 0xE00E_1000..=0xE00E_1FFF
        )
    }

    fn usart(addr: u32) -> Option<(usize, u32)> {
        USARTS
            .iter()
            .position(|&(base, _)| (base..base + usart::SIZE).contains(&addr))
            .map(|n| (n, addr - USARTS[n].0))
    }

    fn port(addr: u32) -> Option<(usize, u32)> {
        let offset = addr.checked_sub(GPIO)?;
        let port = (offset / 0x400) as usize;
        (port < PORTS).then_some((port, offset % 0x400))
    }

    fn modelled(addr: u32) -> bool {
        (RCC..RCC + 0x400).contains(&addr)
            || (PWR..PWR + 0x80).contains(&addr)
            || Self::port(addr).is_some()
            || (EXTI..EXTI + 0x100).contains(&addr)
            || (SYSCFG..SYSCFG + 0x400).contains(&addr)
            || (RNG..RNG + 0x10).contains(&addr)
            || (DBGMCU..DBGMCU + 0x80).contains(&addr)
            || (DBGMCU_DEBUG..DBGMCU_DEBUG + 0x80).contains(&addr)
            || flash_h7::Interface::contains(addr)
            || Self::usart(addr).is_some()
            || Self::i2c(addr).is_some()
            || Self::spi(addr).is_some()
            || Self::timer(addr).is_some()
            || Ethernet::contains(addr)
            || Quadspi::contains(addr)
            || Ltdc::contains(addr)
    }

    /// The interrupt lines the peripherals assert, as a mask by IRQ number.
    pub fn interrupts(&self) -> [u32; 8] {
        let mut lines = [0; 8];
        let mut raise = |irq: u32| lines[(irq / 32) as usize] |= 1 << (irq % 32);
        for (n, usart) in self.usarts.iter().enumerate() {
            if usart.interrupt() {
                raise(USARTS[n].1);
            }
        }
        let pending = self.exti[0x88 / 4] & self.exti[0x80 / 4];
        for (line, &irq) in EXTI_IRQS.iter().enumerate() {
            if pending >> line & 1 != 0 {
                raise(irq);
            }
        }
        if self.flash_interface.interrupt() {
            raise(FLASH_IRQ);
        }
        for (n, i2c) in self.i2cs.iter().enumerate() {
            let (event, error) = i2c.interrupts();
            if event {
                raise(I2CS[n].1);
            }
            if error {
                raise(I2CS[n].2);
            }
        }
        for (n, spi) in self.spis.iter().enumerate() {
            if spi.interrupt() {
                raise(SPIS[n].1);
            }
        }
        for (n, timer) in self.timers.iter().enumerate() {
            if timer.interrupt() {
                raise(TIMERS[n].1);
            }
        }
        if self.eth.interrupt() {
            raise(eth_h7::IRQ);
        }
        lines
    }

    /// A byte arriving on USART `port`'s line at `now`; false if it was
    /// not taken.
    pub fn receive(&mut self, port: usize, byte: u8, now: u64) -> bool {
        self.usarts.get_mut(port).is_some_and(|usart| usart.receive(byte, now))
    }

    /// Something outside drives a pin, or lets go of it, at `now`.
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>, now: u64) {
        if let Some(gpio) = self.gpio.get_mut(port) {
            gpio.drive(pin, level);
            self.sense();
            self.watch_pins(now);
        }
    }

    /// The pins whose level changed since last looked at, for the trace.
    fn watch_pins(&mut self, now: u64) {
        for port in 0..PORTS {
            let level = self.gpio[port].idr() as u16;
            let changed = level ^ self.watched[port];
            self.watched[port] = level;
            for pin in (0..16).filter(|pin| changed >> pin & 1 != 0) {
                self.trace.record(now, Signal::Pin { port: port as u8, pin, high: level >> pin & 1 != 0 });
            }
        }
    }

    /// The GPIO ports' state: each one's levels, outputs and modes.
    pub fn ports(&self) -> Vec<(u16, u16, u32)> {
        self.gpio.iter().map(|gpio| (gpio.idr() as u16, gpio.odr as u16, gpio.moder)).collect()
    }

    /// What I2C `n` carried since last asked, onto the trace.
    fn trace_i2c(&mut self, n: usize) {
        for (time, event) in self.i2cs[n].events.drain(..) {
            self.trace.record(time, Signal::I2c { controller: n as u8, event });
        }
    }

    /// EXTI's edge detection: each GPIO line that changed level since last
    /// seen, on the port SYSCFG routes to its EXTI line, pends that line if
    /// its trigger for the edge is enabled.
    fn sense(&mut self) {
        for port in 0..PORTS {
            let now = self.gpio[port].idr() as u16;
            let changed = now ^ self.sensed[port];
            self.sensed[port] = now;
            for line in (0..16).filter(|line| changed >> line & 1 != 0) {
                let routed = self.syscfg[(0x08 / 4 + line / 4) as usize] >> (4 * (line % 4)) & 0xF;
                if routed as usize != port {
                    continue;
                }
                let trigger = if now >> line & 1 != 0 { self.exti[0x00] } else { self.exti[0x04 / 4] };
                if trigger >> line & 1 != 0 {
                    self.exti[0x88 / 4] |= 1 << line;
                }
            }
        }
    }

    /// A read of `size` bytes at the word `addr`, at cycle `now`.
    pub fn read(&mut self, addr: u32, size: u32, now: u64) -> Option<u32> {
        if !Self::contains(addr) {
            return None;
        }
        if !Self::modelled(addr) {
            self.unmodelled.insert(addr);
        }
        if let Some((n, offset)) = Self::usart(addr) {
            self.advance_usart(n, now);
            return Some(self.usarts[n].read(offset, true));
        }
        if let Some((n, offset)) = Self::i2c(addr) {
            self.i2cs[n].advance(now);
            let value = self.i2cs[n].read(offset, true, now);
            self.trace_i2c(n);
            return Some(value);
        }
        if let Some((n, offset)) = Self::spi(addr) {
            self.advance_spi(n, now);
            return Some(self.spis[n].read(offset, true, size));
        }
        if let Some((n, offset)) = Self::timer(addr) {
            self.timers[n].advance(now);
            return Some(self.timers[n].read(offset, now));
        }
        if addr == RNG + 0x08 {
            return Some(self.rng.next());
        }
        Some(self.peek(addr, now))
    }

    pub fn peek(&self, addr: u32, now: u64) -> u32 {
        if Ethernet::contains(addr) {
            return self.eth.read(addr);
        }
        if Quadspi::contains(addr) {
            return self.quadspi.read(addr - crate::quadspi::BASE);
        }
        if Ltdc::contains(addr) {
            return self.ltdc.read(addr - Ltdc::BASE);
        }
        if let Some((n, offset)) = Self::i2c(addr) {
            let mut i2c = self.i2cs[n].clone();
            i2c.advance(now);
            return i2c.read(offset, false, now);
        }
        if let Some((n, offset)) = Self::spi(addr) {
            // A look leaves the parts on the bus alone: what they would
            // answer is not known without asking them.
            let mut spi = self.spis[n].clone();
            spi.advance(now, &mut |_, _| 0xFF);
            return spi.read(offset, false, 4);
        }
        if let Some((n, offset)) = Self::timer(addr) {
            let mut timer = self.timers[n].clone();
            timer.advance(now);
            return timer.read(offset, now);
        }
        if (RCC..RCC + 0x400).contains(&addr) {
            return self.rcc_read(addr - RCC);
        }
        if (PWR..PWR + 0x80).contains(&addr) {
            let v = self.pwr[((addr - PWR) / 4) as usize];
            return match addr - PWR {
                // CSR1: ACTVOS is D3CR's VOS, and ready.
                0x04 => v & !0xE000 | self.pwr[0x18 / 4] & 0xC000 | 1 << 13,
                // D3CR: VOSRDY.
                0x18 => v | 1 << 13,
                _ => v,
            };
        }
        if let Some((port, offset)) = Self::port(addr) {
            return self.gpio[port].read(offset);
        }
        if (EXTI..EXTI + 0x100).contains(&addr) {
            return self.exti[((addr - EXTI) / 4) as usize];
        }
        if (SYSCFG..SYSCFG + 0x400).contains(&addr) {
            return self.syscfg[((addr - SYSCFG) / 4) as usize];
        }
        if (RNG..RNG + 0x10).contains(&addr) {
            return match addr - RNG {
                0x00 => self.rng.cr,
                // SR: DRDY whenever enabled, and never an error.
                0x04 => self.rng.cr >> 2 & 1,
                0x08 => self.rng.clone().next(),
                _ => 0,
            };
        }
        if let Some(offset) = Self::dbgmcu(addr) {
            return match offset {
                0x00 => IDCODE,
                offset => self.dbgmcu[(offset / 4) as usize],
            };
        }
        if flash_h7::Interface::contains(addr) {
            let mut interface = self.flash_interface.clone();
            interface.now = now;
            interface.advance(now);
            return interface.read(addr);
        }
        if let Some((n, offset)) = Self::usart(addr) {
            let mut usart = self.usarts[n].clone();
            usart.advance(now);
            return usart.read(offset, false);
        }
        self.other.get(&addr).copied().unwrap_or(0)
    }

    fn dbgmcu(addr: u32) -> Option<u32> {
        [DBGMCU, DBGMCU_DEBUG].iter().find(|&&base| (base..base + 0x80).contains(&addr)).map(|base| addr - base)
    }

    /// A register write of `size` bytes at the word `addr`, at cycle
    /// `now`; `flash` is main flash, which the flash interface erases.
    pub fn write(&mut self, addr: u32, value: u32, size: u32, now: u64, flash: &mut [u8]) -> Option<()> {
        if !Self::contains(addr) {
            return None;
        }
        if Ethernet::contains(addr) {
            self.eth.write(addr, value);
        } else if Quadspi::contains(addr) {
            self.quadspi.write(addr - crate::quadspi::BASE, value);
        } else if Ltdc::contains(addr) {
            self.ltdc.write(addr - Ltdc::BASE, value);
        } else if let Some((n, offset)) = Self::i2c(addr) {
            self.i2cs[n].advance(now);
            self.i2cs[n].write(offset, value, now);
            self.trace_i2c(n);
        } else if let Some((n, offset)) = Self::spi(addr) {
            self.advance_spi(n, now);
            self.spis[n].write(offset, value, size, now);
        } else if let Some((n, offset)) = Self::timer(addr) {
            self.timers[n].write(offset, value, now);
        } else if (RCC..RCC + 0x400).contains(&addr) {
            self.rcc_write(addr - RCC, value);
            self.reclock(now);
        } else if (PWR..PWR + 0x80).contains(&addr) {
            self.pwr[((addr - PWR) / 4) as usize] = value;
        } else if let Some((port, offset)) = Self::port(addr) {
            let gpio = &mut self.gpio[port];
            let before = gpio.odr;
            gpio.write(offset, value, false);
            if gpio.odr != before {
                self.edges.push(Edge { port: port as u8, before: before as u16, after: gpio.odr as u16 });
            }
            // What the buses were doing finishes before a chip-select moves.
            for n in 0..self.spis.len() {
                self.advance_spi(n, now);
            }
            self.chip_selects(now);
        } else if (EXTI..EXTI + 0x100).contains(&addr) {
            match addr - EXTI {
                // SWIER1: a one pends its line.
                0x08 => self.exti[0x88 / 4] |= value & 0x003F_FFFF,
                // CPUPR1: a one clears.
                0x88 => self.exti[0x88 / 4] &= !value,
                offset => self.exti[(offset / 4) as usize] = value,
            }
        } else if (SYSCFG..SYSCFG + 0x400).contains(&addr) {
            self.syscfg[((addr - SYSCFG) / 4) as usize] = value;
            // A line rerouted starts from its new port's level.
            self.sensed = std::array::from_fn(|port| self.gpio[port].idr() as u16);
        } else if (RNG..RNG + 0x10).contains(&addr) {
            if addr == RNG {
                self.rng.cr = value;
            }
        } else if let Some(offset) = Self::dbgmcu(addr) {
            if offset != 0 {
                self.dbgmcu[(offset / 4) as usize] = value;
            }
        } else if flash_h7::Interface::contains(addr) {
            self.flash_interface.now = now;
            self.flash_interface.advance(now);
            self.flash_interface.write(addr, value, flash);
        } else if let Some((n, offset)) = Self::usart(addr) {
            self.advance_usart(n, now);
            self.usarts[n].write(offset, value, now);
        } else {
            self.unmodelled.insert(addr);
            self.other.insert(addr, value);
        }
        Some(())
    }

    /// RCC with its oscillators settled: every ready bit reads as its
    /// enable, the system-clock switch status as the switch, the domains'
    /// clocks always ready. RSR and the CPU's enable registers have aliases
    /// at 0x130 and up.
    fn rcc_read(&self, offset: u32) -> u32 {
        let offset = Self::rcc_alias(offset);
        let v = self.rcc[(offset / 4) as usize];
        let follow = |v: u32, enable: u32, ready: u32| v & !(1 << ready) | (v >> enable & 1) << ready;
        match offset {
            // CR: HSI, CSI, HSI48, HSE, PLL1 to 3; D1CKRDY and D2CKRDY.
            0x00 => {
                let v = [(0, 2), (7, 8), (12, 13), (16, 17), (24, 25), (26, 27), (28, 29)]
                    .iter()
                    .fold(v, |v, &(enable, ready)| follow(v, enable, ready));
                v | 1 << 14 | 1 << 15
            }
            // CFGR: SWS follows SW.
            0x10 => v & !(7 << 3) | (v & 7) << 3,
            // BDCR: LSERDY. CSR: LSIRDY.
            0x70 | 0x74 => follow(v, 0, 1),
            _ => v,
        }
    }

    fn rcc_alias(offset: u32) -> u32 {
        match offset {
            0x130..=0x17F => offset - 0x60,
            _ => offset,
        }
    }

    fn rcc_write(&mut self, offset: u32, value: u32) {
        let offset = Self::rcc_alias(offset);
        let at = (offset / 4) as usize;
        match offset {
            // RSR: RMVF clears the flags; nothing else is writable.
            0xD0 => {
                if value & RSR_RMVF != 0 {
                    self.rcc[at] = 0;
                }
            }
            // A peripheral held in reset is at its reset values: Ethernet in
            // AHB1RSTR, the GPIO ports in AHB4RSTR, the I2Cs, SPIs, timers
            // and USARTs in the APB registers.
            0x80 => {
                self.rcc[at] = value;
                if value & 1 << 15 != 0 {
                    self.eth.reset();
                }
            }
            0x88 => {
                self.rcc[at] = value;
                for port in (0..PORTS).filter(|port| value >> port & 1 != 0) {
                    self.gpio[port].reset_to(&port_reset(port));
                }
            }
            0x8C..=0x9C => {
                self.rcc[at] = value;
                for peripheral in (0..32).filter(|bit| value >> bit & 1 != 0).filter_map(|bit| apb_reset(offset, bit)) {
                    match peripheral {
                        Reset::I2c(n) => self.i2cs[n].reset(),
                        Reset::Spi(n) => self.spis[n].reset(),
                        Reset::Timer(n) => self.timers[n].reset(),
                        Reset::Usart(n) => self.usarts[n].reset(),
                    }
                }
            }
            _ => self.rcc[at] = value,
        }
    }
}

/// A peripheral an APB reset bit names.
enum Reset {
    I2c(usize),
    Spi(usize),
    Timer(usize),
    Usart(usize),
}

/// What bit `bit` of the APB reset register at `offset` resets (RM0433
/// 8.7.31–35), of the peripherals modelled.
fn apb_reset(offset: u32, bit: u32) -> Option<Reset> {
    Some(match (offset, bit) {
        // APB1LRSTR.
        (0x90, 0..=5) => Reset::Timer(bit as usize),
        (0x90, 14) => Reset::Spi(1),
        (0x90, 15) => Reset::Spi(2),
        (0x90, 17) => Reset::Usart(1),
        (0x90, 18) => Reset::Usart(2),
        (0x90, 19) => Reset::Usart(3),
        (0x90, 20) => Reset::Usart(4),
        (0x90, 21) => Reset::I2c(0),
        (0x90, 22) => Reset::I2c(1),
        (0x90, 23) => Reset::I2c(2),
        (0x90, 30) => Reset::Usart(6),
        (0x90, 31) => Reset::Usart(7),
        // APB2RSTR.
        (0x98, 4) => Reset::Usart(0),
        (0x98, 5) => Reset::Usart(5),
        (0x98, 12) => Reset::Spi(0),
        (0x98, 13) => Reset::Spi(3),
        (0x98, 16) => Reset::Timer(6),
        (0x98, 17) => Reset::Timer(7),
        (0x98, 18) => Reset::Timer(8),
        (0x98, 20) => Reset::Spi(4),
        // APB4RSTR.
        (0x9C, 5) => Reset::Spi(5),
        (0x9C, 7) => Reset::I2c(3),
        _ => return None,
    })
}

impl Rng {
    fn next(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word accesses at cycle zero.
    struct Harness<'a>(&'a mut Stm32h7);

    impl Harness<'_> {
        fn read(&mut self, addr: u32) -> Option<u32> {
            self.0.read(addr, 4, 0)
        }

        fn write(&mut self, addr: u32, value: u32, flash: &mut [u8]) -> Option<()> {
            self.0.write(addr, value, 4, 0, flash)
        }

        fn reset(&mut self, flash: &mut [u8]) {
            self.0.reset(flash, 0)
        }

        fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
            self.0.drive(port, pin, level, 0)
        }

        fn interrupts(&self) -> [u32; 8] {
            self.0.interrupts()
        }
    }

    #[test]
    fn startup_finds_what_it_waits_for() {
        let mut soc = Stm32h7::default();
        let mut flash = vec![0xFF; 2 * flash_h7::BANK];
        let mut soc = Harness(&mut soc);
        assert_ne!(soc.read(PWR + 0x04).unwrap() & 1 << 13, 0, "ACTVOSRDY");
        soc.write(PWR + 0x18, 0b11 << 14, &mut flash);
        assert_ne!(soc.read(PWR + 0x18).unwrap() & 1 << 13, 0, "VOSRDY");
        soc.write(RCC, 0x25 | 1 << 24, &mut flash);
        assert_ne!(soc.read(RCC).unwrap() & 1 << 25, 0, "PLL1RDY");
        soc.write(RCC + 0x10, 3, &mut flash);
        assert_eq!(soc.read(RCC + 0x10).unwrap() >> 3 & 7, 3, "SWS: PLL1");
        assert_eq!(soc.read(DBGMCU).unwrap() & 0xFFF, 0x450);
        assert_eq!(soc.read(RCC + 0xD0).unwrap(), soc.read(RCC + 0x130).unwrap(), "C1_RSR aliases RSR");
        soc.write(RCC + 0x130, RSR_RMVF, &mut flash);
        assert_eq!(soc.read(RCC + 0xD0), Some(0));
        soc.reset(&mut flash);
        assert_eq!(soc.read(RCC + 0xD0), Some(RSR_SFT | RSR_PIN | RSR_CPU), "what Hubris's sys calls a system reset");
    }

    #[test]
    fn a_button_on_pc13_raises_exti15_10() {
        let mut soc = Stm32h7::default();
        let mut flash = vec![];
        let mut soc = Harness(&mut soc);
        // PC13 an input with a pull-down; EXTI13 from port C, rising.
        soc.write(GPIO + 2 * 0x400, 0xF3FF_FFFF, &mut flash);
        soc.write(GPIO + 2 * 0x400 + 0x0C, 0b10 << 26, &mut flash);
        soc.write(SYSCFG + 0x14, 2 << 4, &mut flash);
        soc.write(EXTI, 1 << 13, &mut flash);
        soc.write(EXTI + 0x80, 1 << 13, &mut flash);
        assert_eq!(soc.interrupts()[1] & 1 << 8, 0);
        soc.drive(2, 13, Some(true));
        assert_eq!(soc.read(GPIO + 2 * 0x400 + 0x10).unwrap() >> 13 & 1, 1);
        assert_ne!(soc.interrupts()[1] & 1 << 8, 0, "IRQ 40");
        soc.write(EXTI + 0x88, 1 << 13, &mut flash);
        assert_eq!(soc.interrupts()[1] & 1 << 8, 0);
        soc.drive(2, 13, Some(false));
        assert_eq!(soc.interrupts()[1] & 1 << 8, 0, "no falling trigger");
    }
}
