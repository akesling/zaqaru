//! The clock tree: what RCC's registers make of the oscillators, as the
//! reference manuals draw it (RM0433 8.5 for the H7s, RM0444 5.2 for the
//! G0s) — the system clock's source and the PLLs, the prescalers down to
//! the core, the buses and their timers, and the kernel clocks the
//! peripherals that have a choice run on.
//!
//! The oscillators run at their nominal frequencies: HSI at 64 MHz on an H7
//! and 16 MHz on a G0, CSI at 4 MHz, HSI48 at 48 MHz, LSI at 32 kHz, LSE at
//! 32.768 kHz, and HSE at the 8 MHz a Nucleo's ST-LINK gives its MCO.
//!
//! Time on the machine is kept in picoseconds (see [`Clocks::period`]):
//! every frequency here divides a second into a whole number of them, or
//! near enough.

/// Picoseconds in a second.
pub const PS: u64 = 1_000_000_000_000;

pub const HSE: u64 = 8_000_000;
const CSI: u64 = 4_000_000;
const HSI48: u64 = 48_000_000;
const LSI: u64 = 32_000;
const LSE: u64 = 32_768;

/// The clocks a chip's RCC is giving out, in hertz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clocks {
    /// The system clock, before the core's prescaler.
    pub sys: u64,
    /// The core's clock: what an instruction's cycle is.
    pub core: u64,
    /// The AHB's clock, and the memories' on it.
    pub hclk: u64,
    /// APB1 to APB4's clocks. A G0 has one APB, given as APB1 and APB2.
    pub pclk: [u64; 4],
    /// The timers' clocks on APB1 and APB2: twice their bus's when its
    /// prescaler divides.
    pub timers: [u64; 2],
    /// The kernel clocks a peripheral can be given instead of its bus's:
    /// PLL1Q to PLL3R (an H7's), HSI after its divider, and CSI.
    pub pll: [[u64; 3]; 3],
    pub hsi: u64,
    pub csi: u64,
}

impl Clocks {
    /// Picoseconds per cycle of a clock at `hz`, at least one; a stopped
    /// clock's cycle never ends.
    pub fn period(hz: u64) -> u64 {
        if hz == 0 { u64::MAX } else { (PS / hz).max(1) }
    }

    /// Out of reset: everything on HSI, nothing divided.
    pub fn reset(hsi: u64) -> Clocks {
        Clocks { sys: hsi, core: hsi, hclk: hsi, pclk: [hsi; 4], timers: [hsi; 2], pll: [[0; 3]; 3], hsi, csi: CSI }
    }

    /// An H7's, from its RCC registers, indexed by offset / 4.
    pub fn h7(rcc: &[u32]) -> Clocks {
        let r = |offset: usize| rcc[offset / 4];
        let cr = r(0x00);
        let hsi = 64_000_000 >> (cr >> 3 & 3);
        // PLLCKSELR: the PLLs' common source, and each one's DIVM.
        let cksel = r(0x28);
        let source = match cksel & 3 {
            0 => hsi,
            1 => CSI,
            2 => HSE,
            _ => 0,
        };
        let pllcfgr = r(0x2C);
        let mut pll = [[0; 3]; 3];
        for (n, outputs) in pll.iter_mut().enumerate() {
            // PLLn is on (CR's PLL1ON, PLL2ON, PLL3ON at 24, 26 and 28).
            if cr >> (24 + 2 * n) & 1 == 0 {
                continue;
            }
            let divm = u64::from(cksel >> (4 + 8 * n as u32) & 0x3F);
            if divm == 0 {
                continue;
            }
            let divr = r(0x30 + 8 * n);
            let fracr = r(0x34 + 8 * n);
            let n_mult = u64::from(divr & 0x1FF) + 1;
            // FRACN, when PLLnFRACEN says so: N plus a fraction of 8192.
            let fraction = if pllcfgr >> (4 * n as u32) & 1 != 0 { u64::from(fracr >> 3 & 0x1FFF) } else { 0 };
            let vco = source * (n_mult * 8192 + fraction) / (divm * 8192);
            // P, Q and R, each enabled by PLLCFGR's DIVxnEN.
            for (k, output) in outputs.iter_mut().enumerate() {
                let enabled = pllcfgr >> (16 + 3 * n as u32 + k as u32) & 1 != 0;
                let div = u64::from(divr >> [9, 16, 24][k] & 0x7F) + 1;
                if enabled {
                    *output = vco / div;
                }
            }
        }
        // CFGR's SW: HSI, CSI, HSE or PLL1P.
        let sys = match r(0x10) & 7 {
            0 => hsi,
            1 => CSI,
            2 => HSE,
            3 => pll[0][0],
            _ => hsi,
        };
        let d1 = r(0x18);
        let core = sys / ahb_divider(d1 >> 8);
        let hclk = core / ahb_divider(d1);
        let pclk3 = hclk / apb_divider(d1 >> 4);
        let d2 = r(0x1C);
        let pclk1 = hclk / apb_divider(d2 >> 4);
        let pclk2 = hclk / apb_divider(d2 >> 8);
        let pclk4 = hclk / apb_divider(r(0x20) >> 4);
        // CFGR's TIMPRE: the timers at up to four times their bus, rather
        // than twice, and never faster than the AHB.
        let timpre = r(0x10) >> 15 & 1 != 0;
        let timer = |pclk: u64, ppre: u32| match (apb_divider(ppre), timpre) {
            (1, _) => pclk,
            (2, true) | (4, true) => hclk,
            (_, true) => 4 * pclk,
            (_, false) => 2 * pclk,
        };
        let timers = [timer(pclk1, d2 >> 4), timer(pclk2, d2 >> 8)];
        Clocks { sys, core, hclk, pclk: [pclk1, pclk2, pclk3, pclk4], timers, pll, hsi, csi: CSI }
    }

    /// A G0's, from its RCC registers, indexed by offset / 4.
    pub fn g0(rcc: &[u32]) -> Clocks {
        let r = |offset: usize| rcc[offset / 4];
        let hsi16 = 16_000_000;
        let hsisys = hsi16 >> (r(0x00) >> 11 & 7);
        // PLLCFGR: the source, M, N and R.
        let pllcfgr = r(0x0C);
        let source = match pllcfgr & 3 {
            2 => hsi16,
            3 => HSE,
            _ => 0,
        };
        let m = u64::from(pllcfgr >> 4 & 7) + 1;
        let n = u64::from(pllcfgr >> 8 & 0x7F);
        let pllr = if r(0x00) >> 24 & 1 != 0 && pllcfgr >> 28 & 1 != 0 {
            source * n / m / (u64::from(pllcfgr >> 29 & 7) + 1)
        } else {
            0
        };
        let cfgr = r(0x08);
        let sys = match cfgr & 7 {
            0 => hsisys,
            1 => HSE,
            2 => pllr,
            3 => LSI,
            4 => LSE,
            _ => hsisys,
        };
        let hclk = sys / ahb_divider(cfgr >> 8);
        let ppre = cfgr >> 12;
        let pclk = hclk / apb_divider(ppre);
        let timer = if apb_divider(ppre) == 1 { pclk } else { 2 * pclk };
        Clocks {
            sys,
            core: hclk,
            hclk,
            pclk: [pclk, pclk, pclk, pclk],
            timers: [timer, timer],
            pll: [[0, 0, pllr], [0; 3], [0; 3]],
            hsi: hsi16,
            csi: 0,
        }
    }

    /// HSI48, for the kernel clocks that can take it.
    pub fn hsi48(&self) -> u64 {
        HSI48
    }
}

/// HPRE and D1CPRE's encoding: 0xxx divides by one, 1000 to 1111 by 2, 4,
/// 8, 16, 64, 128, 256 and 512.
fn ahb_divider(field: u32) -> u64 {
    match field & 0xF {
        f if f < 8 => 1,
        f => [2, 4, 8, 16, 64, 128, 256, 512][(f - 8) as usize],
    }
}

/// The APB prescalers' encoding: 0xx divides by one, 100 to 111 by 2, 4, 8
/// and 16.
fn apb_divider(field: u32) -> u64 {
    match field & 7 {
        f if f < 4 => 1,
        f => 1 << (f - 3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RCC as Hubris's H7 Nucleo startup leaves it: HSI / 4 into PLL1, ×50,
    /// P / 2 for 400 MHz, Q / 4, the AHB at half the core and every APB at
    /// half the AHB.
    #[test]
    fn hubris_s_h7_configuration_is_400_mhz_with_100_mhz_buses() {
        let mut rcc = [0u32; 0x100];
        rcc[0x00] = 0x0000_0025 | 1 << 24;
        rcc[0x10 / 4] = 3;
        rcc[0x18 / 4] = 0b1000 | 0b100 << 4;
        rcc[0x1C / 4] = 0b100 << 4 | 0b100 << 8;
        rcc[0x20 / 4] = 0b100 << 4;
        rcc[0x28 / 4] = 4 << 4;
        rcc[0x2C / 4] = 0b111 << 16;
        rcc[0x30 / 4] = (50 - 1) | (2 - 1) << 9 | (4 - 1) << 16 | (2 - 1) << 24;
        let c = Clocks::h7(&rcc);
        assert_eq!((c.sys, c.core, c.hclk), (400_000_000, 400_000_000, 200_000_000));
        assert_eq!(c.pclk, [100_000_000; 4]);
        assert_eq!(c.timers, [200_000_000; 2]);
        assert_eq!(c.pll[0], [400_000_000, 200_000_000, 400_000_000]);
        assert_eq!(Clocks::period(c.core), 2500);
    }

    #[test]
    fn an_h7_out_of_reset_runs_on_hsi() {
        let mut rcc = [0u32; 0x100];
        rcc[0x00] = 0x0000_0025;
        rcc[0x28 / 4] = 0x0202_0200;
        rcc[0x30 / 4] = 0x0101_0280;
        let c = Clocks::h7(&rcc);
        assert_eq!((c.core, c.hclk, c.pclk[0], c.pll[0][0]), (64_000_000, 64_000_000, 64_000_000, 0));
    }

    #[test]
    fn a_g0_on_its_pll_at_64_mhz() {
        let mut rcc = [0u32; 0x40];
        rcc[0x00] = 1 << 8 | 1 << 24;
        // HSI16 / 1 × 8 / 2.
        rcc[0x0C / 4] = 2 | 8 << 8 | 1 << 28 | 1 << 29;
        rcc[0x08 / 4] = 2 | 0b100 << 12;
        let c = Clocks::g0(&rcc);
        assert_eq!((c.sys, c.hclk, c.pclk[0], c.timers[0]), (64_000_000, 64_000_000, 32_000_000, 64_000_000));
        let mut reset = [0u32; 0x40];
        reset[0x00] = 1 << 8;
        assert_eq!(Clocks::g0(&reset).core, 16_000_000);
    }
}
