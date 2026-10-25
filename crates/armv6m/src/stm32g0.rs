//! Enough of an STM32G0 around the core for Hubris's G0 demos to boot
//! unmodified.
//!
//! The G031 demo touches two peripherals: RCC, to clock and un-reset the
//! GPIO ports, and GPIO, to drive the Nucleo's one user LED (PC6, active
//! low). The G070 demo adds USART1, for `ping`'s serial output (see
//! [`crate::usart`]). RCC and GPIO are modelled as registers with their
//! documented reset values and the few read-back behaviours software waits
//! on — a clock's ready bit following its enable, BSRR setting and clearing
//! ODR. Everything else in
//! the peripheral windows is a plain register file that remembers what was
//! written, and remembers that it was touched, so a run can say which
//! peripherals it would need next rather than guessing.

use std::collections::{BTreeMap, BTreeSet};

use crate::flash;
pub use crate::gpio::Edge;
use crate::gpio::Gpio;
use crate::usart::{self, Usart};

const RCC: u32 = 0x4002_1000;
const DBGMCU_IDCODE: u32 = 0x4001_5800;
const IOPORT: u32 = 0x5000_0000;
const PORTS: usize = 6;

/// The two USARTs a G0 has: their bases, and their interrupt numbers.
pub const USARTS: [(u32, u32); 2] = [(0x4001_3800, 27), (0x4000_4400, 28)];

/// REV_ID 0x1000, above the part's DEV_ID.
const REV_ID: u32 = 0x1000;

/// RM0444 7.4: port A comes out of reset with its SWD pins configured,
/// the rest of the ports all-analog.
fn port_reset(port: usize) -> Gpio {
    match port {
        0 => Gpio::new(0xEBFF_FFFF, 0x0C00_0000, 0x2400_0000),
        _ => Gpio::new(0xFFFF_FFFF, 0, 0),
    }
}

#[derive(Clone, Debug)]
pub struct Stm32g0 {
    rcc: [u32; 0x100],
    gpio: Vec<Gpio>,
    other: BTreeMap<u32, u32>,
    /// Register addresses outside RCC and GPIO that software read or
    /// wrote: the peripherals a run reached that this model only pretends
    /// to have.
    pub unmodelled: BTreeSet<u32>,
    /// Output changes since the machine last drained them.
    pub edges: Vec<Edge>,
    pub usarts: [Usart; 2],
    /// Bytes the USARTs transmitted since the machine last drained them.
    pub serial: Vec<usart::Byte>,
    /// The part's DEV_ID, which DBGMCU_IDCODE reports.
    pub dev_id: u16,
    pub flash_interface: flash::Interface,
}

impl Default for Stm32g0 {
    fn default() -> Self {
        let mut soc = Stm32g0 {
            rcc: [0; 0x100],
            gpio: Vec::new(),
            other: BTreeMap::new(),
            unmodelled: BTreeSet::new(),
            edges: Vec::new(),
            usarts: Default::default(),
            serial: Vec::new(),
            dev_id: crate::part::STM32G031.dev_id,
            flash_interface: flash::Interface::default(),
        };
        soc.reset();
        soc
    }
}

impl Stm32g0 {
    pub fn reset(&mut self) {
        self.rcc = [0; 0x100];
        // RCC_CR: HSI16 on (its ready bit follows; see `rcc_read`).
        self.rcc[0] = 1 << 8;
        if self.gpio.is_empty() {
            self.gpio = (0..PORTS).map(port_reset).collect();
        }
        for (port, gpio) in self.gpio.iter_mut().enumerate() {
            gpio.reset_to(&port_reset(port));
        }
        self.flash_interface.reset();
        self.other.clear();
        for usart in &mut self.usarts {
            usart.reset();
        }
    }

    /// Which USART, and the offset into it, an address names.
    fn usart(addr: u32) -> Option<(usize, u32)> {
        USARTS
            .iter()
            .position(|&(base, _)| (base..base + usart::SIZE).contains(&addr))
            .map(|n| (n, addr - USARTS[n].0))
    }

    /// The interrupt lines the peripherals assert, as a mask by IRQ number.
    pub fn interrupts(&self) -> [u32; 8] {
        let mut lines = 0;
        for (n, usart) in self.usarts.iter().enumerate() {
            if usart.interrupt() {
                lines |= 1 << USARTS[n].1;
            }
        }
        [lines, 0, 0, 0, 0, 0, 0, 0]
    }

    /// A byte arriving on USART `port`'s line; false if it was not taken.
    pub fn receive(&mut self, port: usize, byte: u8) -> bool {
        self.usarts.get_mut(port).is_some_and(|usart| usart.receive(byte))
    }

    /// Whether `addr` is in the G0's peripheral windows: APB, AHB, and the
    /// single-cycle IOPORT bus the GPIOs sit on.
    pub fn contains(addr: u32) -> bool {
        matches!(addr, 0x4000_0000..=0x4002_63FF | 0x5000_0000..=0x5000_17FF)
    }

    /// Something outside drives a pin, or lets go of it.
    pub fn drive(&mut self, port: usize, pin: u8, level: Option<bool>) {
        if let Some(gpio) = self.gpio.get_mut(port) {
            gpio.drive(pin, level);
        }
    }

    pub fn read(&mut self, addr: u32) -> Option<u32> {
        if !Self::contains(addr) {
            return None;
        }
        if !self.modelled(addr) {
            self.unmodelled.insert(addr);
        }
        if let Some((n, offset)) = Self::usart(addr) {
            return Some(self.usarts[n].read(offset, true));
        }
        Some(self.peek(addr))
    }

    pub fn peek(&self, addr: u32) -> u32 {
        if (RCC..RCC + 0x400).contains(&addr) {
            return self.rcc_read(addr - RCC);
        }
        if let Some((gpio, offset)) = self.port(addr) {
            return gpio.read(offset);
        }
        if flash::Interface::contains(addr) {
            return self.flash_interface.read(addr);
        }
        if addr == DBGMCU_IDCODE {
            return REV_ID << 16 | u32::from(self.dev_id);
        }
        if let Some((n, offset)) = Self::usart(addr) {
            // A peek is a read without effects, and only RDR has any; the
            // `Usart` is cloned rather than mutated to say so.
            return self.usarts[n].clone().read(offset, false);
        }
        self.other.get(&addr).copied().unwrap_or(0)
    }

    /// A register write; `flash` is main flash, which the flash
    /// interface erases.
    pub fn write(&mut self, addr: u32, value: u32, flash: &mut [u8]) -> Option<()> {
        if !Self::contains(addr) {
            return None;
        }
        if flash::Interface::contains(addr) {
            self.flash_interface.write(addr, value, flash);
            return Some(());
        }
        if (RCC..RCC + 0x400).contains(&addr) {
            self.rcc[((addr - RCC) / 4) as usize] = value;
            return Some(());
        }
        if let Some((n, offset)) = Self::usart(addr) {
            if let Some(byte) = self.usarts[n].write(offset, value) {
                self.serial.push(usart::Byte { port: n as u8, byte });
            }
            return Some(());
        }
        if addr >= IOPORT {
            let port = ((addr - IOPORT) / 0x400) as usize;
            let gpio = &mut self.gpio[port];
            let before = gpio.odr;
            gpio.write((addr - IOPORT) % 0x400, value, true);
            if gpio.odr != before {
                self.edges.push(Edge { port: port as u8, before: before as u16, after: gpio.odr as u16 });
            }
            return Some(());
        }
        self.unmodelled.insert(addr);
        self.other.insert(addr, value);
        Some(())
    }

    fn modelled(&self, addr: u32) -> bool {
        (RCC..RCC + 0x400).contains(&addr)
            || addr >= IOPORT
            || addr == DBGMCU_IDCODE
            || flash::Interface::contains(addr)
            || Self::usart(addr).is_some()
    }

    fn port(&self, addr: u32) -> Option<(&Gpio, u32)> {
        if addr < IOPORT {
            return None;
        }
        let offset = addr - IOPORT;
        Some((&self.gpio[(offset / 0x400) as usize], offset % 0x400))
    }

    /// RCC with its oscillators settled: every ready bit reads as its
    /// enable bit, and the system-clock switch status as the switch. A real
    /// part takes microseconds to get there; software polls until it has,
    /// and here the first poll succeeds.
    fn rcc_read(&self, offset: u32) -> u32 {
        let v = self.rcc[(offset / 4) as usize];
        let follow = |v: u32, enable: u32, ready: u32| v & !(1 << ready) | (v >> enable & 1) << ready;
        match offset {
            // CR: HSIRDY, HSERDY, PLLRDY.
            0x00 => follow(follow(follow(v, 8, 10), 16, 17), 24, 25),
            // CFGR: SWS follows SW.
            0x08 => v & !(7 << 3) | (v & 7) << 3,
            // BDCR: LSERDY. CSR: LSIRDY.
            0x5C | 0x60 => follow(v, 0, 1),
            _ => v,
        }
    }
}
