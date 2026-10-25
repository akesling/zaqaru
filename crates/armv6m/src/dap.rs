//! The debug port: an ADIv5 SW-DP with one AHB-AP behind it, as an
//! STM32G031's Cortex-M0+ has; or, on an H7, a DPv2 SW-DP with three:
//! the Cortex-M7's AHB-AP (AP0), the D3 domain's (AP1), and the debug APB
//! (AP2), where ST's DBGMCU is — which is where probe-rs enables the
//! debug clocks.
//!
//! This is what a probe talks to. A CMSIS-DAP probe's transfers are reads
//! and writes of these registers — DP registers by address, AP registers by
//! the AP and bank SELECT names and the address — and everything a
//! debugger does to the core is made of them: power up the debug domain
//! through CTRL/STAT, find the AP and the ROM table through IDR and BASE,
//! then reach memory and the core's debug registers ([`crate::debug`])
//! with TAR and DRW.
//!
//! Answers are immediate. On the wire an AP read is posted and its value
//! comes back with the next access or a read of RDBUFF; a CMSIS-DAP probe
//! hides that from its host, and so does this, while still answering
//! RDBUFF and RESEND the way a probe that reads them expects.
//!
//! A failed transfer is FAULT and sets STICKYERR, and while a sticky flag
//! is set every access but DPIDR, CTRL/STAT and ABORT is FAULT too, until
//! ABORT clears it — the protocol a probe recovers by.

use crate::Machine;

/// DPIDR: an ARM SW-DP, DPv1 with the minimal debug port extension, part
/// 0xBC — what RM0444 gives for the G0.
pub const DPIDR: u32 = 0x0BC1_1477;
/// The AHB-AP's IDR: a Cortex-M0+'s AHB-AP, revision 0.
pub const AP_IDR: u32 = 0x0477_0031;
/// BASE: the ROM table, present and in the ADIv5 format.
pub const AP_BASE: u32 = crate::debug::ROM_TABLE | 3;

/// An H7's DPIDR: an ARM SW-DP, DPv2, part 0xBA02 (RM0433 60.3).
pub const H7_DPIDR: u32 = 0x6BA0_2477;
/// TARGETID: ST's part 0x450. DLPIDR: SWD protocol version 1.
const H7_TARGETID: u32 = 0x0450_0041;
const H7_DLPIDR: u32 = 0x0000_0001;
/// The H7's three APs: two AHB-APs, of the M7's generation, and an APB-AP.
const H7_AHB_AP_IDR: u32 = 0x8477_0001;
const H7_APB_AP_IDR: u32 = 0x5477_0002;
/// The debug APB's ROM table, and what it lists: DBGMCU.
const APBD_ROM_TABLE: u32 = 0xE00E_0000;
const APBD_END: u32 = 0xE010_0000;

/// What one access port reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// The system, through the core's own bus.
    System,
    /// The debug APB.
    DebugApb,
}

/// An access port: its IDR, its BASE, and what it reaches.
struct Ap {
    idr: u32,
    base: u32,
    reach: Reach,
}

const G0_APS: &[Ap] = &[Ap { idr: AP_IDR, base: AP_BASE, reach: Reach::System }];
const H7_APS: &[Ap] = &[
    Ap { idr: H7_AHB_AP_IDR, base: crate::debug::CHIP_ROM_TABLE | 3, reach: Reach::System },
    // D3's AHB-AP reaches D3's memories and peripherals; here, the same
    // system, with no ROM table of its own.
    Ap { idr: H7_AHB_AP_IDR, base: 2, reach: Reach::System },
    Ap { idr: H7_APB_AP_IDR, base: APBD_ROM_TABLE | 3, reach: Reach::DebugApb },
];

fn aps(m: &Machine) -> &'static [Ap] {
    match m.part.family {
        crate::part::Family::G0 => G0_APS,
        crate::part::Family::H7 => H7_APS,
    }
}

/// A transfer's acknowledgement, as the wire gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ack {
    Ok,
    Wait,
    Fault,
}

// CTRL/STAT.
const ORUNDETECT: u32 = 1 << 0;
const STICKYORUN: u32 = 1 << 1;
const TRNMODE: u32 = 3 << 2;
const STICKYCMP: u32 = 1 << 4;
const STICKYERR: u32 = 1 << 5;
const WDATAERR: u32 = 1 << 7;
const MASKLANE: u32 = 0xF << 8;
const TRNCNT: u32 = 0xFFF << 12;
const CDBGRSTREQ: u32 = 1 << 26;
const CDBGPWRUPREQ: u32 = 1 << 28;
const CSYSPWRUPREQ: u32 = 1 << 30;

// ABORT.
const STKCMPCLR: u32 = 1 << 1;
const STKERRCLR: u32 = 1 << 2;
const WDERRCLR: u32 = 1 << 3;
const ORUNERRCLR: u32 = 1 << 4;

// The MEM-AP's registers, by bank and address.
const CSW: u8 = 0x00;
const TAR: u8 = 0x04;
const DRW: u8 = 0x0C;
const BD0: u8 = 0x10;
const CFG: u8 = 0xF4;
const BASE: u8 = 0xF8;
const IDR: u8 = 0xFC;

/// CSW's fields that write and read back: Size, AddrInc, and the bus
/// attributes in 31:24. DeviceEn, bit 6, always reads as one.
const CSW_WRITABLE: u32 = 0xFF00_0037;
const CSW_DEVICEEN: u32 = 1 << 6;
const CSW_RESET: u32 = 0x0300_0040;

#[derive(Clone, Debug)]
pub struct Port {
    /// CTRL/STAT's request and mode fields, as written.
    control: u32,
    /// CTRL/STAT's sticky flags.
    sticky: u32,
    select: u32,
    /// The last AP read's value, which RDBUFF and RESEND return.
    rdbuff: u32,
    csw: u32,
    tar: u32,
}

impl Default for Port {
    fn default() -> Self {
        Port { control: 0, sticky: 0, select: 0, rdbuff: 0, csw: CSW_RESET, tar: 0 }
    }
}

impl Port {
    /// A line reset — fifty clocks with SWDIO high, which every connection
    /// starts with. The DP's state survives one; nothing here needs it.
    pub fn line_reset(&mut self) {}

    /// Whether the debug domain is powered: a debugger asked, and was
    /// acknowledged, as a part with nothing to wait for always does.
    pub fn powered(&self) -> bool {
        self.control & CDBGPWRUPREQ != 0
    }

    /// Reads DP register `a` (0x0, 0x4, 0x8 or 0xC) of `m`'s debug port.
    /// On a DPv2, 0x4 is banked by SELECT's DPBANKSEL: CTRL/STAT, DLCR,
    /// TARGETID, DLPIDR, EVENTSTAT.
    pub fn dp_read(&mut self, m: &Machine, a: u8) -> Result<u32, Ack> {
        let v2 = m.part.family == crate::part::Family::H7;
        match a & 0xC {
            0x0 => Ok(if v2 { H7_DPIDR } else { DPIDR }),
            0x4 if v2 && self.select & 0xF != 0 => Ok(match self.select & 0xF {
                1 => 0x40,
                2 => H7_TARGETID,
                3 => H7_DLPIDR,
                _ => 0,
            }),
            0x4 => {
                let requests = self.control & (CSYSPWRUPREQ | CDBGPWRUPREQ | CDBGRSTREQ);
                Ok(self.control | requests << 1 | self.sticky)
            }
            _ if self.sticky != 0 => Err(Ack::Fault),
            // RESEND and RDBUFF: the last AP read, again.
            _ => Ok(self.rdbuff),
        }
    }

    /// Writes DP register `a`: ABORT, CTRL/STAT or SELECT. RDBUFF ignores
    /// writes.
    pub fn dp_write(&mut self, a: u8, value: u32) -> Result<(), Ack> {
        match a & 0xC {
            0x0 => {
                let cleared = [(STKCMPCLR, STICKYCMP), (STKERRCLR, STICKYERR), (WDERRCLR, WDATAERR), (ORUNERRCLR, STICKYORUN)];
                for (clear, flag) in cleared {
                    if value & clear != 0 {
                        self.sticky &= !flag;
                    }
                }
                Ok(())
            }
            _ if self.sticky != 0 => Err(Ack::Fault),
            0x4 => {
                // The sticky flags clear through ABORT on SWD, not here.
                self.control = value & (CSYSPWRUPREQ | CDBGPWRUPREQ | CDBGRSTREQ | TRNCNT | MASKLANE | TRNMODE | ORUNDETECT);
                Ok(())
            }
            0x8 => {
                self.select = value;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The selected AP, and register `a` in the selected bank.
    fn ap_register(&self, a: u8) -> (u32, u8) {
        (self.select >> 24, (self.select & 0xF0) as u8 | a & 0xC)
    }

    /// Reads AP register `a` of the AP SELECT names.
    pub fn ap_read(&mut self, m: &mut Machine, a: u8) -> Result<u32, Ack> {
        if self.sticky != 0 {
            return Err(Ack::Fault);
        }
        let (ap, register) = self.ap_register(a);
        // The APs past the last read as zero, which is how a debugger
        // scanning for APs knows it has found them all.
        let value = match aps(m).get(ap as usize) {
            None => 0,
            Some(port) => match register {
                CSW => self.csw & CSW_WRITABLE | CSW_DEVICEEN,
                TAR => self.tar,
                DRW => {
                    let size = self.size();
                    let addr = self.tar & !(size - 1);
                    let loaded = load(m, port.reach, addr, size).ok_or_else(|| self.error())?;
                    self.increment();
                    loaded << (8 * (addr & 3))
                }
                r if (BD0..BD0 + 0x10).contains(&r) => {
                    load(m, port.reach, self.tar & !0xF | (r - BD0) as u32, 4).ok_or_else(|| self.error())?
                }
                CFG => 0,
                BASE => port.base,
                IDR => port.idr,
                _ => 0,
            },
        };
        self.rdbuff = value;
        Ok(value)
    }

    /// Writes AP register `a` of the AP SELECT names.
    pub fn ap_write(&mut self, m: &mut Machine, a: u8, value: u32) -> Result<(), Ack> {
        if self.sticky != 0 {
            return Err(Ack::Fault);
        }
        let (ap, register) = self.ap_register(a);
        let Some(port) = aps(m).get(ap as usize) else {
            return Ok(());
        };
        match register {
            CSW => self.csw = value & CSW_WRITABLE,
            TAR => self.tar = value,
            DRW => {
                let size = self.size();
                let addr = self.tar & !(size - 1);
                if !store(m, port.reach, addr, size, value >> (8 * (addr & 3))) {
                    return Err(self.error());
                }
                self.increment();
            }
            r if (BD0..BD0 + 0x10).contains(&r) => {
                if !store(m, port.reach, self.tar & !0xF | (r - BD0) as u32, 4, value) {
                    return Err(self.error());
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The access size CSW.Size asks for, in bytes: 1, 2 or 4. The AHB-AP
    /// has no larger.
    fn size(&self) -> u32 {
        match self.csw & 7 {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }

    /// TAR after a DRW access: on by the access size when AddrInc asks for
    /// single increments, wrapping within the 1 KiB the AHB-AP's
    /// incrementer covers.
    fn increment(&mut self) {
        if self.csw >> 4 & 3 == 0b01 {
            self.tar = self.tar & !0x3FF | self.tar.wrapping_add(self.size()) & 0x3FF;
        }
    }

    fn error(&mut self) -> Ack {
        self.sticky |= STICKYERR;
        Ack::Fault
    }
}

/// A load through an AP. The debug APB has its ROM table, listing
/// DBGMCU, and DBGMCU; the rest of it reads as zero, as unclaimed APB space
/// does, and anything outside it is a bus error.
fn load(m: &mut Machine, reach: Reach, addr: u32, size: u32) -> Option<u32> {
    if reach == Reach::System {
        return m.debug_load(addr, size);
    }
    if !(APBD_ROM_TABLE..APBD_END).contains(&addr) {
        return None;
    }
    let dbgmcu = crate::stm32h7::DBGMCU_DEBUG;
    let word = match addr & !3 {
        a if a & !0xFFF == APBD_ROM_TABLE && a & 0xFFF >= 0xFD0 => {
            crate::debug::coresight_id(a & 0xFFF, 0, 0x20, u32::from(m.part.dev_id), 0x1)
        }
        a if a == APBD_ROM_TABLE => dbgmcu.wrapping_sub(APBD_ROM_TABLE) | 3,
        a if a == APBD_ROM_TABLE + 0xFCC => 1,
        a if (dbgmcu..dbgmcu + 0x80).contains(&a) => return m.debug_load(addr, size),
        _ => 0,
    };
    Some(word >> (8 * (addr & 3)) & if size == 4 { u32::MAX } else { (1 << (8 * size)) - 1 })
}

fn store(m: &mut Machine, reach: Reach, addr: u32, size: u32, value: u32) -> bool {
    if reach == Reach::System {
        return m.debug_store(addr, size, value);
    }
    let dbgmcu = crate::stm32h7::DBGMCU_DEBUG;
    match addr {
        a if (dbgmcu..dbgmcu + 0x80).contains(&a) => m.debug_store(addr, size, value),
        a => (APBD_ROM_TABLE..APBD_END).contains(&a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{FLASH_BASE, SRAM_BASE};
    use crate::debug::*;
    use crate::Outcome;

    /// A G031 running a loop that counts in r0: `movs r0, #0`, then
    /// `adds r0, #1` and `b` back to it, forever.
    fn counting() -> Machine {
        let mut image = vec![0u8; 0x110];
        image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
        image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
        for (i, halfword) in [0x2000u16, 0x3001, 0xE7FD].iter().enumerate() {
            image[0x100 + 2 * i..0x102 + 2 * i].copy_from_slice(&halfword.to_le_bytes());
        }
        Machine::stm32g031(&image).unwrap()
    }

    /// A port powered up, with CSW set for words, incrementing.
    fn connected(m: &Machine) -> Port {
        let mut port = Port::default();
        port.line_reset();
        let dpidr = port.dp_read(m, 0x0).unwrap();
        assert!(dpidr == DPIDR || dpidr == H7_DPIDR);
        port.dp_write(0x0, STKERRCLR | STKCMPCLR | WDERRCLR | ORUNERRCLR).unwrap();
        port.dp_write(0x4, CSYSPWRUPREQ | CDBGPWRUPREQ).unwrap();
        let status = port.dp_read(m, 0x4).unwrap();
        assert_eq!(status >> 28, 0xF, "both power-ups acknowledged: {status:08x}");
        port.dp_write(0x8, 0).unwrap();
        port
    }

    fn word(port: &mut Port, m: &mut Machine, addr: u32) -> u32 {
        port.ap_write(m, CSW, 0x2300_0012).unwrap();
        port.ap_write(m, TAR, addr).unwrap();
        port.ap_read(m, DRW).unwrap()
    }

    fn set_word(port: &mut Port, m: &mut Machine, addr: u32, value: u32) {
        port.ap_write(m, CSW, 0x2300_0012).unwrap();
        port.ap_write(m, TAR, addr).unwrap();
        port.ap_write(m, DRW, value).unwrap();
    }

    #[test]
    fn a_debugger_finds_one_ahb_ap_and_the_rom_table_behind_it() {
        let mut m = counting();
        let mut port = connected(&m);
        port.dp_write(0x8, 0xF0).unwrap();
        assert_eq!(port.ap_read(&mut m, IDR & 0xC), Ok(AP_IDR));
        assert_eq!(port.ap_read(&mut m, BASE & 0xC), Ok(AP_BASE));
        port.dp_write(0x8, 1 << 24 | 0xF0).unwrap();
        assert_eq!(port.ap_read(&mut m, IDR & 0xC), Ok(0), "no second AP");
        port.dp_write(0x8, 0).unwrap();

        let rom = AP_BASE & !0xFFF;
        let cid = |port: &mut Port, m: &mut Machine, base: u32| {
            (0..4).fold(0, |id, i| id | (word(port, m, base + 0xFF0 + 4 * i) & 0xFF) << (8 * i))
        };
        assert_eq!(cid(&mut port, &mut m, rom), 0xB105_100D, "a ROM table");
        let entries: Vec<u32> = (0..4).map(|i| word(&mut port, &mut m, rom + 4 * i)).collect();
        let components: Vec<u32> = entries
            .iter()
            .take_while(|&&e| e != 0)
            .map(|&e| rom.wrapping_add(e & !0xFFF))
            .collect();
        assert_eq!(components, [SCS, DWT, BPU]);
        for base in components {
            assert_eq!(cid(&mut port, &mut m, base), 0xB105_E00D, "a generic component at {base:08x}");
        }
        assert_eq!(word(&mut port, &mut m, 0xE000_ED00), 0x410C_C601, "CPUID: a Cortex-M0+");
    }

    /// An H753 running the same loop, from its flash's own address.
    fn h753_counting() -> Machine {
        let mut image = vec![0u8; 0x110];
        image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
        image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
        for (i, halfword) in [0x2000u16, 0x3001, 0xE7FD].iter().enumerate() {
            image[0x100 + 2 * i..0x102 + 2 * i].copy_from_slice(&halfword.to_le_bytes());
        }
        Machine::new(crate::part::STM32H753, &image).unwrap()
    }

    #[test]
    fn an_h7_has_three_aps_and_st_s_rom_table_names_the_part() {
        let mut m = h753_counting();
        let mut port = connected(&m);
        assert_eq!(port.dp_read(&m, 0x0), Ok(H7_DPIDR));
        port.dp_write(0x8, 2).unwrap();
        assert_eq!(port.dp_read(&m, 0x4), Ok(H7_TARGETID));
        port.dp_write(0x8, 0).unwrap();

        let idrs: Vec<u32> = (0..4)
            .map(|ap| {
                port.dp_write(0x8, ap << 24 | 0xF0).unwrap();
                port.ap_read(&mut m, IDR & 0xC).unwrap()
            })
            .collect();
        assert_eq!(idrs, [H7_AHB_AP_IDR, H7_AHB_AP_IDR, H7_APB_AP_IDR, 0]);
        port.dp_write(0x8, 0xF0).unwrap();
        let base = port.ap_read(&mut m, BASE & 0xC).unwrap() & !0xFFF;
        port.dp_write(0x8, 0).unwrap();

        // The chip's table: ST's, part 0x450, listing the M7's.
        let pid = |port: &mut Port, m: &mut Machine, base: u32| {
            let byte = |port: &mut Port, m: &mut Machine, at: u32| word(port, m, base + at) & 0xFF;
            (byte(port, m, 0xFE0) | byte(port, m, 0xFE4) << 8 | byte(port, m, 0xFE8) << 16, byte(port, m, 0xFD0))
        };
        assert_eq!(base, crate::debug::CHIP_ROM_TABLE);
        assert_eq!(pid(&mut port, &mut m, base), (0x0A_0450, 0), "JEP106 0x20, ST, and the DEV_ID");
        let m7 = base.wrapping_add(word(&mut port, &mut m, base) & !0xFFF);
        assert_eq!(m7, ROM_TABLE);
        let components: Vec<u32> =
            (0..5).map(|i| word(&mut port, &mut m, m7 + 4 * i)).take_while(|&e| e != 0).map(|e| m7.wrapping_add(e & !0xFFF)).collect();
        assert_eq!(components, [SCS, DWT, BPU, ITM]);
        assert_eq!(pid(&mut port, &mut m, BPU).0 & 0xFFF, 0x00E, "the M7's FPB");
        assert_eq!(word(&mut port, &mut m, BPU) >> 28, 1, "FP_CTRL.REV: an FPB v2");

        // DBGMCU, through the debug APB, where probe-rs enables the debug
        // clocks; and the same register from the core's side.
        port.dp_write(0x8, 2 << 24).unwrap();
        assert_eq!(word(&mut port, &mut m, 0xE00E_1000) & 0xFFF, 0x450);
        set_word(&mut port, &mut m, 0xE00E_1004, 0x0070_0007);
        port.ap_write(&mut m, TAR, 0x2000_0000).unwrap();
        assert_eq!(port.ap_read(&mut m, DRW), Err(Ack::Fault), "the debug APB reaches no memory");
        port.dp_write(0x0, STKERRCLR).unwrap();
        port.dp_write(0x8, 0).unwrap();
        assert_eq!(word(&mut port, &mut m, 0x5C00_1004), 0x0070_0007);
    }

    #[test]
    fn an_fpb_v2_comparator_stops_at_its_address() {
        let mut m = h753_counting();
        let mut port = connected(&m);
        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN);
        set_word(&mut port, &mut m, BPU, 0b11);
        set_word(&mut port, &mut m, BPU + 8 + 4 * 7, (FLASH_BASE + 0x102) | 1);
        assert_eq!(m.run(1000), Outcome::Stopped { pc: FLASH_BASE + 0x102 });
    }

    #[test]
    fn halting_reading_registers_and_stepping_go_through_the_debug_registers() {
        let mut m = counting();
        let mut port = connected(&m);
        assert_eq!(m.run(1000), Outcome::Budget);
        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN | C_HALT);
        assert_eq!(word(&mut port, &mut m, DHCSR) & S_HALT, S_HALT);
        assert_eq!(m.run(1000), Outcome::Halted, "a halted core runs nothing");
        let retired = m.retired;

        set_word(&mut port, &mut m, DCRSR, 0);
        let r0 = word(&mut port, &mut m, DCRDR);
        assert_eq!(r0, m.cpu.r[0]);
        assert!(r0 > 400, "r0 counted: {r0}");

        // Write r0 through DCRDR, step, and the step counts from it.
        let before = m.cpu.pc;
        set_word(&mut port, &mut m, DCRDR, 7);
        set_word(&mut port, &mut m, DCRSR, 1 << 16);
        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN | C_STEP | C_MASKINTS);
        assert_eq!(m.run(1000), Outcome::Halted);
        assert_eq!(m.retired, retired + 1, "one instruction");
        assert_ne!(m.cpu.pc, before);
        assert!(m.cpu.r[0] == 7 || m.cpu.r[0] == 8);
        assert_eq!(word(&mut port, &mut m, DFSR) & DFSR_HALTED, DFSR_HALTED);

        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN);
        assert_eq!(m.run(100), Outcome::Budget, "running again");
        assert!(m.cpu.r[0] > 40);
    }

    #[test]
    fn a_bpu_comparator_stops_the_core_before_the_instruction() {
        let mut m = counting();
        let mut port = connected(&m);
        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN);
        set_word(&mut port, &mut m, BPU, 0b11);
        // The `adds` at 0x0800_0102: the upper halfword of its word.
        set_word(&mut port, &mut m, BPU + 8, 0b10 << 30 | (FLASH_BASE + 0x100) | 1);
        assert_eq!(m.run(1000), Outcome::Stopped { pc: FLASH_BASE + 0x102 });
        assert!(m.halted());
        assert_eq!(word(&mut port, &mut m, DFSR) & DFSR_BKPT, DFSR_BKPT);
        assert_eq!(m.cpu.r[0], 0, "the `adds` has not run");
    }

    #[test]
    fn vector_catch_halts_a_reset_at_the_reset_vector() {
        let mut m = counting();
        let mut port = connected(&m);
        set_word(&mut port, &mut m, DHCSR, 0xA05F << 16 | C_DEBUGEN | C_HALT);
        set_word(&mut port, &mut m, DEMCR, VC_CORERESET);
        word(&mut port, &mut m, DHCSR);
        set_word(&mut port, &mut m, 0xE000_ED0C, 0x05FA_0004);
        assert_eq!(m.run(1000), Outcome::SystemReset, "a halted core still takes the reset it asked for");
        m.reset().unwrap();
        let status = word(&mut port, &mut m, DHCSR);
        assert_eq!(status & (S_HALT | S_RESET_ST), S_HALT | S_RESET_ST);
        assert_eq!(m.cpu.pc, FLASH_BASE + 0x100);
        assert_eq!(m.retired, 0);
    }

    #[test]
    fn memory_reads_increment_and_a_bus_error_is_sticky_until_aborted() {
        let mut m = counting();
        let mut port = connected(&m);
        port.ap_write(&mut m, CSW, 0x2300_0012).unwrap();
        port.ap_write(&mut m, TAR, FLASH_BASE + 0x100).unwrap();
        let words: Vec<u32> = (0..2).map(|_| port.ap_read(&mut m, DRW).unwrap()).collect();
        assert_eq!(words, [0x3001_2000, 0x0000_E7FD]);
        assert_eq!(port.dp_read(&m, 0xC), Ok(0x0000_E7FD), "RDBUFF");

        // Bytes land in their lane.
        port.ap_write(&mut m, CSW, 0x2300_0010).unwrap();
        port.ap_write(&mut m, TAR, FLASH_BASE + 0x101).unwrap();
        assert_eq!(port.ap_read(&mut m, DRW), Ok(0x20 << 8));

        // Writes to SRAM, then back.
        set_word(&mut port, &mut m, SRAM_BASE + 0x40, 0xDEAD_BEEF);
        assert_eq!(m.bus.rams[0].bytes[0x40..0x44], 0xDEAD_BEEFu32.to_le_bytes());

        port.ap_write(&mut m, TAR, 0x3000_0000).unwrap();
        assert_eq!(port.ap_read(&mut m, DRW), Err(Ack::Fault));
        assert_eq!(port.dp_read(&m, 0x4).unwrap() & STICKYERR, STICKYERR);
        assert_eq!(port.ap_read(&mut m, IDR & 0xC), Err(Ack::Fault), "everything faults while sticky");
        port.dp_write(0x0, STKERRCLR).unwrap();
        assert_eq!(word(&mut port, &mut m, FLASH_BASE), 0x2000_2000);
    }
}
