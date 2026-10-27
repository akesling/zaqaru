//! What last wrote each word of RAM, and when (`Ram::marks`): what a
//! memory view shows as heat and who.

use armv6m::bus::{BY_DMA, BY_EXCEPTION, BY_PROBE, FLASH_BASE, SRAM_BASE, Soc, write_stamp};
use armv6m::part::Part;
use armv6m::{Machine, Outcome};

/// A G031 running `code` from 0x0800_0100, with `literals` after it and an
/// SVCall handler, `bx lr`, at 0x0800_0180.
fn g031(code: &[u16], literals: &[u32]) -> Machine {
    let mut image = vec![0u8; 0x200];
    image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
    image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
    image[0x2C..0x30].copy_from_slice(&(FLASH_BASE + 0x180 | 1).to_le_bytes());
    image[0x180..0x182].copy_from_slice(&0x4770u16.to_le_bytes());
    let mut at = 0x100;
    for halfword in code {
        image[at..at + 2].copy_from_slice(&halfword.to_le_bytes());
        at += 2;
    }
    at = (at + 3) & !3;
    for word in literals {
        image[at..at + 4].copy_from_slice(&word.to_le_bytes());
        at += 4;
    }
    Machine::stm32g031(&image).unwrap()
}

/// Stores 5 at 0x2000_0100 from 0x0800_0104, then stops.
fn storing() -> Machine {
    g031(
        &[
            0x4801, // ldr r0, =0x2000_0100
            0x2105, // movs r1, #5
            0x6001, // str r1, [r0]
            0xBE00, // bkpt #0
        ],
        &[SRAM_BASE + 0x100],
    )
}

#[test]
fn a_store_marks_its_word_with_when_and_the_instruction_that_made_it() {
    let mut m = storing();
    assert!(matches!(m.run_until(u64::MAX, 100), Outcome::Breakpoint { .. }));
    let mark = m.bus.last_write(SRAM_BASE + 0x102).expect("the word was written");
    assert_eq!(mark.writer, FLASH_BASE + 0x104);
    assert!(mark.stamp >= 1 && mark.stamp <= write_stamp(m.time));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x104), None, "the next word wasn't");
    assert_eq!(m.bus.last_write(FLASH_BASE + 0x100), None, "nor is flash RAM");
}

#[test]
fn a_debugger_s_write_is_the_probe_s() {
    let mut m = storing();
    assert!(m.debug_write(SRAM_BASE + 0x200, &[1, 2, 3, 4, 5]));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x200).map(|mark| mark.writer), Some(BY_PROBE));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x204).map(|mark| mark.writer), Some(BY_PROBE));
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x208), None);
}

#[test]
fn an_exception_s_frame_is_the_core_s() {
    let mut m = g031(
        &[
            0xDF00, // svc #0
            0xBE00, // bkpt #0
        ],
        &[],
    );
    assert!(matches!(m.run_until(u64::MAX, 100), Outcome::Breakpoint { .. }));
    // Eight words below the initial stack pointer.
    for word in 0..8 {
        let at = SRAM_BASE + 0x2000 - 32 + 4 * word;
        assert_eq!(m.bus.last_write(at).map(|mark| mark.writer), Some(BY_EXCEPTION), "{at:#x}");
    }
    assert_eq!(m.bus.last_write(SRAM_BASE + 0x2000 - 36), None);
}

#[test]
fn a_frame_the_ethernet_dma_receives_is_the_dma_s() {
    let mut image = vec![0u8; 0x400];
    image[0..4].copy_from_slice(&0x2400_2000u32.to_le_bytes());
    image[4..8].copy_from_slice(&(FLASH_BASE + 0x200 | 1).to_le_bytes());
    let mut m = Machine::new(Part::named("stm32h753").unwrap(), &image).unwrap();
    let eth = armv6m::eth_h7::BASE;
    // One receive descriptor at 0x2400_0100, its buffer at 0x2400_0200,
    // owned by the DMA; the ring one long; receiving, 1536-byte buffers.
    let (descriptor, buffer) = (0x2400_0100, 0x2400_0200);
    m.bus.write(descriptor, 4, buffer).unwrap();
    m.bus.write(descriptor + 12, 4, armv6m::eth_h7::OWN | armv6m::eth_h7::BUF1V).unwrap();
    m.bus.write(eth + 0x111C, 4, descriptor).unwrap();
    m.bus.write(eth + 0x1130, 4, 0).unwrap();
    m.bus.write(eth + 0x1108, 4, 1 | 1536 << 1).unwrap();
    let Soc::H7(soc) = &mut m.bus.soc else { panic!("an H7") };
    soc.eth.arrive(vec![0xAA; 60], 0);
    m.bus.now = 1_000_000_000;
    m.bus.run_dma();
    assert_eq!(m.bus.peek(buffer, 1), Ok(0xAA), "the frame landed");
    for at in [buffer, buffer + 60, descriptor, descriptor + 12] {
        assert_eq!(m.bus.last_write(at).map(|mark| (mark.writer, mark.stamp)), Some((BY_DMA, write_stamp(1_000_000_000))), "{at:#x}");
    }
}

#[test]
fn cells_written_at_or_since_a_stamp_are_found_and_no_others() {
    let mut m = storing();
    assert!(m.bus.written_since(0, 64).is_empty());
    m.run_until(u64::MAX, 100);
    let stamp = m.bus.last_write(SRAM_BASE + 0x100).unwrap().stamp;
    assert_eq!(m.bus.written_since(0, 64), [(0, 0x100 / 64, stamp)]);
    // A write in the very microsecond an answer was given at is still news.
    assert_eq!(m.bus.written_since(stamp, 64), [(0, 0x100 / 64, stamp)]);
    assert!(m.bus.written_since(stamp + 1, 64).is_empty());
}

#[test]
fn a_probe_write_while_the_board_stands_still_is_not_lost() {
    let mut m = storing();
    let now = write_stamp(m.time);
    assert!(m.debug_write(SRAM_BASE + 0x300, &[9, 9, 9, 9]));
    assert_eq!(m.bus.written_since(now, 4), [(0, 0x300 / 4, now)]);
}

#[test]
fn stamps_do_not_saturate_on_a_long_run() {
    // Two hours of the board's time, in picoseconds: past what a 32-bit
    // count of microseconds holds.
    let two_hours = 2 * 3600 * 1_000_000_000_000u64;
    assert!(write_stamp(two_hours) > u32::MAX as u64);
    assert!(write_stamp(two_hours + 1_000_000) > write_stamp(two_hours));
}

#[test]
fn cells_of_part_of_a_ram_are_numbered_from_its_start() {
    let mut m = storing();
    m.run_until(u64::MAX, 100);
    let stamp = m.bus.last_write(SRAM_BASE + 0x100).unwrap().stamp;
    // From 0xF0, 8-byte cells: 0x100 is the third.
    assert_eq!(m.bus.written_in(0, 8, SRAM_BASE + 0xF0, 0x40), Some(vec![(2, stamp)]));
    assert_eq!(m.bus.written_in(0, 8, SRAM_BASE + 0x108, 0x40), Some(vec![]));
    assert_eq!(m.bus.written_in(0, 8, FLASH_BASE, 0x40), None, "flash is not RAM");
}

#[test]
fn part_of_a_ram_stops_at_its_end_however_long_it_is_asked_for() {
    let mut m = storing();
    let last = SRAM_BASE + m.bus.rams[0].bytes.len() as u32 - 4;
    assert!(m.debug_write(last, &[1, 2, 3, 4]));
    let stamp = m.bus.last_write(last).unwrap().stamp;
    assert_eq!(m.bus.written_in(0, 4, last, u32::MAX - 8), Some(vec![(0, stamp)]));
    assert_eq!(m.bus.written_in(0, 4, last, u32::MAX), Some(vec![(0, stamp)]));
}
