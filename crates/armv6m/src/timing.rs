//! What an instruction costs, in cycles of the core's clock.
//!
//! An instruction costs one cycle, and its stalls on top: kept in half
//! cycles (see `Machine::stall`), since a Cortex-M7's 64-bit load/store
//! path moves two registers a cycle.
//!
//! - Each data access costs a cycle on the M0+ — its loads and stores take
//!   two, LDM, STM, PUSH and POP one and one a register (Cortex-M0+ TRM,
//!   table 3.1) — but none through the single-cycle IOPORT; and half a
//!   cycle on the M7, which pairs them, and whose single loads stall about
//!   that often on their use.
//! - A taken branch refills the pipeline: one cycle more on either core,
//!   two for an M0+'s BL; and on a G0 without its prefetch, the flash's
//!   wait states too.
//! - Where an access goes costs what the bus there does, in core cycles at
//!   the clocks RCC gives out: an APB access a few cycles of the AHB to
//!   the bridge and of the APB beyond it, an AHB peripheral's a few of the
//!   AHB's; flash its LATENCY in AHB cycles, and the H7's SRAMs an AHB
//!   cycle or two — unless the M7's caches are on (CCR's IC and DC), which
//!   this takes to hit. The M7's stores go into its write buffer and cost
//!   nothing more. TCM costs nothing.
//! - Divides, the M7's integer ones two to twelve cycles by the quotient's
//!   size and its floating-point ones 14 cycles single and 29 double; an
//!   M0+'s MSR, MRS and barriers three.
//! - Exception entry 15 cycles on an M0+ and 12 on an M7, return 13 and 10,
//!   their stacking included.
//!
//! What is not modelled: the M7's dual issue, which would make some pairs
//! of instructions cost one cycle between them; branch prediction's misses;
//! cache misses, and the lazy stacking of the FP context.

use crate::clock::Clocks;
use crate::part::Family;

/// The wait states a machine's buses have now, in half cycles of the
/// core's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    /// A data access's own cost.
    access: u64,
    /// A load from APB1 to APB4, and from an AHB peripheral.
    apb: [u64; 4],
    ahb: u64,
    /// Flash: a data load, and an instruction fetch, a halfword's worth.
    flash_data: u64,
    flash_fetch: u64,
    /// The H7's SRAMs outside the TCMs.
    sram: u64,
    /// What a taken branch costs, and a BL more on an M0+.
    pub branch: u64,
    pub bl: u64,
    /// Exception entry and return.
    pub entry: u64,
    pub exit: u64,
    family: Option<Family>,
    /// Whether the M7 posts its stores.
    posted: bool,
}

impl Timing {
    /// From the clocks, the flash's ACR, and SCB's CCR.
    pub fn new(family: Family, clocks: &Clocks, acr: u32, ccr: u32) -> Timing {
        // Core cycles per cycle of a clock, at least one, in half cycles.
        let per = |hz: u64| 2 * (clocks.core / hz.max(1)).max(1);
        match family {
            Family::H7 => {
                let (icache, dcache) = (ccr >> 17 & 1 != 0, ccr >> 16 & 1 != 0);
                let latency = u64::from(acr & 0xF) + 1;
                let ahb = per(clocks.hclk);
                Timing {
                    access: 1,
                    apb: clocks.pclk.map(|pclk| 2 * ahb + 2 * per(pclk)),
                    ahb: 3 * ahb,
                    flash_data: if dcache { 0 } else { latency * ahb },
                    // A 64-bit AXI read holds four halfwords.
                    flash_fetch: if icache { 0 } else { latency * ahb / 4 },
                    sram: if dcache { 0 } else { ahb },
                    branch: 2,
                    bl: 0,
                    entry: 2 * 12,
                    exit: 2 * 10,
                    family: Some(family),
                    posted: true,
                }
            }
            Family::G0 => {
                let latency = u64::from(acr & 7);
                let (prefetch, icache) = (acr >> 8 & 1 != 0, acr >> 9 & 1 != 0);
                // A 32-bit flash read holds two halfwords.
                let fetch = if prefetch || icache { 0 } else { latency };
                Timing {
                    access: 2,
                    apb: [per(clocks.pclk[0]); 4],
                    ahb: per(clocks.hclk),
                    flash_data: 2 * latency,
                    flash_fetch: fetch,
                    sram: 0,
                    branch: 2 + if icache { 0 } else { 2 * latency },
                    bl: 2,
                    entry: 2 * 15,
                    exit: 2 * 13,
                    family: Some(family),
                    posted: false,
                }
            }
        }
    }

    /// What a data access at `addr` costs, beyond its instruction's cycle.
    pub fn data(&self, addr: u32, store: bool) -> u64 {
        let wait = match (self.family, addr) {
            (_, 0xE000_0000..) => 0,
            (Some(Family::G0), 0x5000_0000..=0x5000_17FF) => return 0,
            (Some(Family::G0), 0x0000_0000..=0x1FFF_FFFF) => self.flash_data,
            (Some(Family::G0), 0x4002_0000..=0x4002_63FF) => self.ahb,
            (Some(Family::G0), 0x4000_0000..=0x4001_FFFF) => self.apb[0],
            (Some(Family::H7), addr) => match addr {
                0x0000_0000..=0x0000_FFFF | 0x2000_0000..=0x2001_FFFF => 0,
                0x0800_0000..=0x1FFF_FFFF => self.flash_data,
                0x2400_0000..=0x3FFF_FFFF => self.sram,
                0x4000_0000..=0x4000_FFFF => self.apb[0],
                0x4001_0000..=0x4001_FFFF => self.apb[1],
                0x5000_0000..=0x50FF_FFFF => self.apb[2],
                0x5800_0000..=0x5801_FFFF | 0x5C00_0000..=0x5CFF_FFFF => self.apb[3],
                0x4002_0000..=0x5FFF_FFFF => self.ahb,
                _ => 0,
            },
            _ => 0,
        };
        self.access + if store && self.posted { 0 } else { wait }
    }

    /// What fetching a halfword at `addr` costs.
    pub fn fetch(&self, addr: u32) -> u64 {
        match (self.family, addr) {
            (Some(Family::G0), 0x0000_0000..=0x1FFF_FFFF) => self.flash_fetch,
            (Some(Family::H7), 0x0800_0000..=0x1FFF_FFFF) => self.flash_fetch,
            _ => 0,
        }
    }
}

/// An integer divide's extra half cycles on the M7: two to twelve cycles
/// in all, by how many bits the quotient has.
pub fn divide(dividend: u32, divisor: u32) -> u64 {
    let bits = (divisor.leading_zeros() as u64).saturating_sub(dividend.leading_zeros() as u64) + 1;
    let cycles = 2 + (bits.min(32) - 1) * 10 / 31;
    2 * (cycles - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hubris_h7() -> Clocks {
        Clocks {
            sys: 400_000_000,
            core: 400_000_000,
            hclk: 200_000_000,
            pclk: [100_000_000; 4],
            timers: [200_000_000; 2],
            pll: [[0; 3]; 3],
            hsi: 64_000_000,
            csi: 4_000_000,
        }
    }

    #[test]
    fn an_h7_s_buses_at_hubris_s_clocks() {
        let t = Timing::new(Family::H7, &hubris_h7(), 2, 0);
        // An APB load: two AHB cycles (4 core) and two APB (8 core).
        assert_eq!(t.data(0x4000_4400, false), 1 + 2 * 12);
        assert_eq!(t.data(0x4000_4400, true), 1, "posted");
        assert_eq!(t.data(0x2000_0000, false), 1, "DTCM");
        assert_eq!(t.data(0x0800_0000, false), 1 + 2 * 6, "three AHB cycles of flash");
        let cached = Timing::new(Family::H7, &hubris_h7(), 2, 3 << 16);
        assert_eq!(cached.data(0x0800_0000, false), 1);
        assert_eq!((t.fetch(0x0800_0000), cached.fetch(0x0800_0000)), (3, 0));
    }

    #[test]
    fn divides_take_two_to_twelve_cycles() {
        assert_eq!(divide(7, 7), 2 * 1);
        assert_eq!(divide(u32::MAX, 1), 2 * 11);
    }
}
