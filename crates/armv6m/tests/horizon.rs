//! The horizon (`Machine::horizon`): a point the machine will not run past,
//! by its own instruction count or its own time, whoever asks it to run —
//! for standing a board at an exact instant of its own.

use armv6m::bus::{FLASH_BASE, SRAM_BASE};
use armv6m::{Horizon, Machine, Outcome};

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

fn countdown() -> Machine {
    g031(
        &[
            0x4901, // ldr r1, =100000
            0x3901, // loop: subs r1, #1
            0xD1FD, // bne loop
            0xBE00, // bkpt #0
        ],
        &[100_000],
    )
}

/// Polls USART2's TC for a 40.8 ms frame: a busy wait the machine skips.
fn polling() -> Machine {
    g031(
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
    )
}

#[test]
fn a_machine_stops_just_after_its_horizon_instruction_and_goes_no_further() {
    let mut m = countdown();
    m.horizon = Horizon { instructions: 1001, ..Horizon::NONE };
    assert_eq!(m.run_until(u64::MAX, u64::MAX), Outcome::Budget);
    assert_eq!(m.retired, 1001);
    // The load, then 500 times round the loop.
    assert_eq!(m.cpu.r[1], 100_000 - 500);
    assert!(m.at_horizon());
    let time = m.time;
    assert_eq!(m.step(), Outcome::Budget, "not even a debugger's step");
    assert_eq!((m.retired, m.time), (1001, time));
}

#[test]
fn lifted_it_goes_on_as_if_it_had_never_stopped() {
    let mut free = countdown();
    free.run_until(u64::MAX, u64::MAX);
    let mut m = countdown();
    m.horizon = Horizon { instructions: 777, ..Horizon::NONE };
    m.run_until(u64::MAX, u64::MAX);
    m.horizon = Horizon::NONE;
    m.run_until(u64::MAX, u64::MAX);
    assert_eq!((m.retired, m.time, m.cpu.pc), (free.retired, free.time, free.cpu.pc));
}

#[test]
fn a_horizon_in_a_skipped_busy_wait_is_landed_on_exactly() {
    let mut free = polling();
    assert!(matches!(free.run_until(1000 * PS_PER_MS, u64::MAX), Outcome::Breakpoint { .. }));
    assert!(free.skipped > 0);
    // Halfway through the wait, in the middle of what is skipped.
    let target = free.retired / 2 + 1;
    let mut m = polling();
    m.horizon = Horizon { instructions: target, ..Horizon::NONE };
    m.run_until(1000 * PS_PER_MS, u64::MAX);
    assert_eq!(m.retired, target);
    assert!(m.time < free.time / 2 + PS_PER_MS, "about halfway: {} of {} ps", m.time, free.time);
    m.horizon = Horizon::NONE;
    assert!(matches!(m.run_until(1000 * PS_PER_MS, u64::MAX), Outcome::Breakpoint { .. }));
    assert_eq!((m.retired, m.time), (free.retired, free.time), "and it ends where the free run did");
}

#[test]
fn a_time_horizon_stands_where_a_run_to_that_time_does() {
    let mut free = polling();
    free.run_until(17 * PS_PER_MS, u64::MAX);
    let mut m = polling();
    m.horizon = Horizon { time: free.time, ..Horizon::NONE };
    // However it is asked to run, in whatever pieces.
    while !m.at_horizon() {
        m.run_until(u64::MAX, 3);
    }
    assert_eq!((m.retired, m.time, &m.cpu), (free.retired, free.time, &free.cpu));
}
