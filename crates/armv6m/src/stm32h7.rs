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
//! timers as time bases ([`crate::timer`]), the RNG, DBGMCU, and the flash
//! interface ([`crate::flash_h7`]). RCC's reset bits reset the peripherals
//! they name.
//!
//! Everything else in the peripheral windows is a plain register file
//! that remembers what was written, and remembers that it was touched, as
//! the G0's is.

use std::collections::{BTreeMap, BTreeSet};

use crate::flash_h7;
use crate::gpio::{Edge, Gpio};
use crate::i2c::I2c;
use crate::spi_h7::Spi;
use crate::timer::Timer;
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
/// Core cycles per tick of the timers' kernel clock: Hubris runs APB1 and
/// APB2 at a quarter of the core, and their timers at twice that.
const TIMER_DIVIDER: u64 = 2;
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
    timers: Vec<Timer>,
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
            timers: TIMERS.iter().map(|&(_, _, wide)| Timer::new(wide, TIMER_DIVIDER)).collect(),
        };
        soc.rcc[0xD0 / 4] = RSR_POR | RSR_PIN | RSR_BOR | RSR_D2 | RSR_D1 | RSR_CPU;
        soc.rng.state = 0x2545_F491;
        soc.reset_registers();
        soc
    }
}

impl Stm32h7 {
    /// A system reset: what software reset, and RSR saying so.
    pub fn reset(&mut self, flash: &mut [u8]) {
        self.rcc[0xD0 / 4] |= RSR_SFT | RSR_PIN | RSR_CPU;
        self.reset_registers();
        self.flash_interface.reset(flash);
    }

    fn reset_registers(&mut self) {
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
        self.rng.cr = 0;
        self.dbgmcu = [0; 0x20];
        self.other.clear();
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

    /// The cycle the next timed event is due at: a timer's update.
    pub fn due(&self) -> Option<u64> {
        self.timers.iter().filter_map(Timer::due).min()
    }

    /// Brings the timers up to cycle `now`.
    pub fn advance(&mut self, now: u64) {
        for timer in &mut self.timers {
            timer.advance(now);
        }
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
        lines
    }

    /// A byte arriving on USART `port`'s line; false if it was not taken.
    pub fn receive(&mut self, port: usize, byte: u8) -> bool {
        self.usarts.get_mut(port).is_some_and(|usart| usart.receive(byte))
    }

    /// Something outside drives a pin, or lets go of it.
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
        if let Some(gpio) = self.gpio.get_mut(port) {
            gpio.drive(pin, level);
            self.sense();
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
            return Some(self.usarts[n].read(offset, true));
        }
        if let Some((n, offset)) = Self::i2c(addr) {
            return Some(self.i2cs[n].read(offset, true));
        }
        if let Some((n, offset)) = Self::spi(addr) {
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
        if let Some((n, offset)) = Self::i2c(addr) {
            return self.i2cs[n].clone().read(offset, false);
        }
        if let Some((n, offset)) = Self::spi(addr) {
            return self.spis[n].clone().read(offset, false, 4);
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
            return self.flash_interface.read(addr);
        }
        if let Some((n, offset)) = Self::usart(addr) {
            return self.usarts[n].clone().read(offset, false);
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
        if let Some((n, offset)) = Self::i2c(addr) {
            self.i2cs[n].write(offset, value);
        } else if let Some((n, offset)) = Self::spi(addr) {
            self.spis[n].write(offset, value, size);
        } else if let Some((n, offset)) = Self::timer(addr) {
            self.timers[n].write(offset, value, now);
        } else if (RCC..RCC + 0x400).contains(&addr) {
            self.rcc_write(addr - RCC, value);
        } else if (PWR..PWR + 0x80).contains(&addr) {
            self.pwr[((addr - PWR) / 4) as usize] = value;
        } else if let Some((port, offset)) = Self::port(addr) {
            let gpio = &mut self.gpio[port];
            let before = gpio.odr;
            gpio.write(offset, value, false);
            if gpio.odr != before {
                self.edges.push(Edge { port: port as u8, before: before as u16, after: gpio.odr as u16 });
            }
            self.sense();
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
            self.flash_interface.write(addr, value, flash);
        } else if let Some((n, offset)) = Self::usart(addr) {
            if let Some(byte) = self.usarts[n].write(offset, value) {
                self.serial.push(usart::Byte { port: n as u8, byte });
            }
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
            // A peripheral held in reset is at its reset values: the GPIO
            // ports in AHB4RSTR, the I2Cs, SPIs, timers and USARTs in the
            // APB registers.
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
            self.0.reset(flash)
        }

        fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
            self.0.drive(port, pin, level)
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
