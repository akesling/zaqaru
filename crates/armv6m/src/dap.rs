//! The debug port: an ADIv5 SW-DP with one AHB-AP behind it, as an
//! STM32G031's Cortex-M0+ has.
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

    /// Reads DP register `a` (0x0, 0x4, 0x8 or 0xC).
    pub fn dp_read(&mut self, a: u8) -> Result<u32, Ack> {
        match a & 0xC {
            0x0 => Ok(DPIDR),
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
        // Only AP 0 exists; the rest read as zero, which is how a debugger
        // scanning for APs knows it has found them all.
        let value = if ap != 0 {
            0
        } else {
            match register {
                CSW => self.csw & CSW_WRITABLE | CSW_DEVICEEN,
                TAR => self.tar,
                DRW => {
                    let size = self.size();
                    let addr = self.tar & !(size - 1);
                    let loaded = m.debug_load(addr, size).ok_or_else(|| self.error())?;
                    self.increment();
                    loaded << (8 * (addr & 3))
                }
                r if (BD0..BD0 + 0x10).contains(&r) => {
                    m.debug_load(self.tar & !0xF | (r - BD0) as u32, 4).ok_or_else(|| self.error())?
                }
                CFG => 0,
                BASE => AP_BASE,
                IDR => AP_IDR,
                _ => 0,
            }
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
        if ap != 0 {
            return Ok(());
        }
        match register {
            CSW => self.csw = value & CSW_WRITABLE,
            TAR => self.tar = value,
            DRW => {
                let size = self.size();
                let addr = self.tar & !(size - 1);
                if !m.debug_store(addr, size, value >> (8 * (addr & 3))) {
                    return Err(self.error());
                }
                self.increment();
            }
            r if (BD0..BD0 + 0x10).contains(&r) => {
                if !m.debug_store(self.tar & !0xF | (r - BD0) as u32, 4, value) {
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
    fn connected() -> Port {
        let mut port = Port::default();
        port.line_reset();
        assert_eq!(port.dp_read(0x0), Ok(DPIDR));
        port.dp_write(0x0, STKERRCLR | STKCMPCLR | WDERRCLR | ORUNERRCLR).unwrap();
        port.dp_write(0x4, CSYSPWRUPREQ | CDBGPWRUPREQ).unwrap();
        let status = port.dp_read(0x4).unwrap();
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
        let mut port = connected();
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

    #[test]
    fn halting_reading_registers_and_stepping_go_through_the_debug_registers() {
        let mut m = counting();
        let mut port = connected();
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
        let mut port = connected();
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
        let mut port = connected();
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
        let mut port = connected();
        port.ap_write(&mut m, CSW, 0x2300_0012).unwrap();
        port.ap_write(&mut m, TAR, FLASH_BASE + 0x100).unwrap();
        let words: Vec<u32> = (0..2).map(|_| port.ap_read(&mut m, DRW).unwrap()).collect();
        assert_eq!(words, [0x3001_2000, 0x0000_E7FD]);
        assert_eq!(port.dp_read(0xC), Ok(0x0000_E7FD), "RDBUFF");

        // Bytes land in their lane.
        port.ap_write(&mut m, CSW, 0x2300_0010).unwrap();
        port.ap_write(&mut m, TAR, FLASH_BASE + 0x101).unwrap();
        assert_eq!(port.ap_read(&mut m, DRW), Ok(0x20 << 8));

        // Writes to SRAM, then back.
        set_word(&mut port, &mut m, SRAM_BASE + 0x40, 0xDEAD_BEEF);
        assert_eq!(m.bus.rams[0].bytes[0x40..0x44], 0xDEAD_BEEFu32.to_le_bytes());

        port.ap_write(&mut m, TAR, 0x3000_0000).unwrap();
        assert_eq!(port.ap_read(&mut m, DRW), Err(Ack::Fault));
        assert_eq!(port.dp_read(0x4).unwrap() & STICKYERR, STICKYERR);
        assert_eq!(port.ap_read(&mut m, IDR & 0xC), Err(Ack::Fault), "everything faults while sticky");
        port.dp_write(0x0, STKERRCLR).unwrap();
        assert_eq!(word(&mut port, &mut m, FLASH_BASE), 0x2000_2000);
    }
}
