//! What last wrote each word of RAM, and when (`Ram::written`,
//! `Ram::writer`): what a memory view shows as heat and who.

use armv6m::bus::{BY_PROBE, FLASH_BASE, SRAM_BASE, write_stamp};
use armv6m::{Machine, Outcome};

/// A G031 storing 5 at 0x2000_0100 from 0x0800_0104, then stopping.
fn storing() -> Machine {
    let mut image = vec![0u8; 0x200];
    image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
    image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
    let code: [u16; 4] = [
        0x4801, // ldr r0, =0x2000_0100
        0x2105, // movs r1, #5
        0x6001, // str r1, [r0]
        0xBE00, // bkpt #0
    ];
    for (i, halfword) in code.iter().enumerate() {
        image[0x100 + 2 * i..0x102 + 2 * i].copy_from_slice(&halfword.to_le_bytes());
    }
    image[0x108..0x10C].copy_from_slice(&(SRAM_BASE + 0x100).to_le_bytes());
    Machine::stm32g031(&image).unwrap()
}

#[test]
fn a_store_marks_its_word_with_when_and_the_instruction_that_made_it() {
    let mut m = storing();
    assert!(matches!(m.run_until(u64::MAX, 100), Outcome::Breakpoint { .. }));
    let (stamp, writer) = m.bus.last_write(SRAM_BASE + 0x102).expect("the word was written");
    assert_eq!(writer, FLASH_BASE + 0x104);
    assert!(stamp >= 1 && stamp <= write_stamp(m.time));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x104), None, "the next word wasn't");
    assert_eq!(m.bus.last_write(FLASH_BASE + 0x100), None, "nor is flash RAM");
}

#[test]
fn a_debugger_s_write_is_the_probe_s() {
    let mut m = storing();
    assert!(m.debug_write(SRAM_BASE + 0x200, &[1, 2, 3, 4, 5]));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x200).map(|(_, by)| by), Some(BY_PROBE));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x204).map(|(_, by)| by), Some(BY_PROBE));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x208), None);
}

#[test]
fn cells_written_since_a_stamp_are_found_and_no_others() {
    let mut m = storing();
    assert!(m.bus.written_since(0, 64).is_empty());
    m.run_until(u64::MAX, 100);
    let (stamp, _) = m.bus.last_write(SRAM_BASE + 0x100).unwrap();
    assert_eq!(m.bus.written_since(0, 64), [(SRAM_BASE, 0x100 / 64, stamp)]);
    assert!(m.bus.written_since(stamp, 64).is_empty());
}
