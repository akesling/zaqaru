//! The busy-wait detector (`Machine::spin`): a loop polling a flag that
//! only a timed event can change is skipped to the event, and comes out
//! where running it would have; a loop that changes anything is not.

use armv6m::bus::{FLASH_BASE, SRAM_BASE};
use armv6m::{Machine, Outcome};

/// A G031 running `code` from flash at 0x100, with `literals` after it.
fn g031(code: &[u16], literals: &[u32]) -> Machine {
    let mut image = vec![0u8; 0x200];
    image[0..4].copy_from_slice(&(SRAM_BASE + 0x2000).to_le_bytes());
    image[4..8].copy_from_slice(&(FLASH_BASE + 0x100 | 1).to_le_bytes());
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

const PS_PER_MS: u64 = 1_000_000_000;

#[test]
fn polling_a_usart_for_tc_is_skipped_to_the_end_of_the_frame() {
    let mut m = g031(
        &[
            0x4805, // ldr r0, =USART2
            0x2109, // movs r1, #UE | TE
            0x6001, // str r1, [r0, #CR1]
            0x21FF, // movs r1, #255
            0x0209, // lsls r1, r1, #8: BRR 0xFF00, a 4 ms bit at 16 MHz
            0x60C1, // str r1, [r0, #BRR]
            0x2150, // movs r1, #'P'
            0x6281, // str r1, [r0, #TDR]
            0x69C2, // loop: ldr r2, [r0, #ISR]
            0x0652, // lsls r2, r2, #25: TC into N
            0xD5FC, // bpl loop
            0xBE00, // bkpt #0
        ],
        &[0x4000_4400],
    );
    let outcome = m.run_until(1000 * PS_PER_MS, u64::MAX);
    assert!(matches!(outcome, Outcome::Breakpoint { .. }), "{outcome:?}");
    // Ten bits of 0xFF00 cycles of 62.5 ns: 40.8 ms.
    let frame = 10 * 0xFF00 * 62_500;
    assert!(m.time >= frame && m.time < frame + PS_PER_MS / 100, "stopped at {} ps", m.time);
    assert!(m.skipped > frame * 9 / 10, "skipped {} ps", m.skipped);
    assert!(m.retired > 100_000, "the iterations skipped count as run: {}", m.retired);
}

#[test]
fn a_countdown_is_run_not_skipped() {
    let mut m = g031(
        &[
            0x4901, // ldr r1, =100000
            0x3901, // loop: subs r1, #1
            0xD1FD, // bne loop
            0xBE00, // bkpt #0
        ],
        &[100_000],
    );
    let outcome = m.run_until(1000 * PS_PER_MS, u64::MAX);
    assert!(matches!(outcome, Outcome::Breakpoint { .. }), "{outcome:?}");
    assert_eq!(m.skipped, 0);
    assert_eq!(m.retired, 1 + 2 * 100_000);
}

#[test]
fn polling_a_counter_that_runs_by_itself_is_not_skipped() {
    let mut m = g031(
        &[
            0x4802, // ldr r0, =SYST_CVR
            0x6801, // loop: ldr r1, [r0]
            0x2900, // cmp r1, #0
            0xD0FC, // beq loop
            0xBE00, // bkpt #0
        ],
        &[0xE000_E018],
    );
    // SysTick is off, so its counter reads zero for ever; the loop does
    // not end, and is not skipped either.
    assert_eq!(m.run_until(10 * PS_PER_MS, u64::MAX), Outcome::Budget);
    assert_eq!(m.skipped, 0);
}

