//! Instruction and system-model tests, as programs.
//!
//! Each test is Thumb assembly, assembled by clang for a Cortex-M0+ and
//! placed in flash behind a vector table built from its labels: `reset`
//! is the entry point, and `hard_fault`, `svc`, `pendsv` and `systick`
//! become handlers when the program defines them. A program ends with a
//! `bkpt`, whose immediate says which way it ended.

use std::collections::HashMap;
use std::process::Command;

use armv6m::bus::{FLASH_BASE, SRAM_BASE, SRAM_SIZE};
use armv6m::scs::exception::*;
use armv6m::{Access, Event, Fault, Machine, Outcome};
use object::{Object, ObjectSection, ObjectSymbol};

/// Where the code goes, past a vector table with room for every
/// exception.
const CODE: u32 = 0x100;

fn assemble(source: &str) -> (Vec<u8>, HashMap<String, u32>) {
    let dir = std::env::temp_dir().join(format!("armv6m-isa-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    let asm = dir.join("test.s");
    let obj = dir.join("test.o");
    std::fs::write(&asm, format!(".syntax unified\n.thumb\n.text\n{source}\n")).unwrap();
    let status = Command::new("clang")
        .args(["--target=thumbv6m-none-eabi", "-mcpu=cortex-m0plus", "-c"])
        .arg(&asm)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("clang, to assemble the test programs");
    assert!(status.success(), "assembly failed");
    let data = std::fs::read(&obj).unwrap();
    let file = object::File::parse(&*data).unwrap();
    let text = file.section_by_name(".text").unwrap();
    assert!(
        text.relocations().next().is_none(),
        "test programs must assemble without relocations"
    );
    let symbols = file
        .symbols()
        .filter(|s| s.section_index() == Some(text.index()) && !s.name().unwrap().starts_with('$'))
        .map(|s| (s.name().unwrap().to_string(), s.address() as u32))
        .collect();
    (text.data().unwrap().to_vec(), symbols)
}

fn machine(source: &str) -> Machine {
    let (code, symbols) = assemble(source);
    let mut image = vec![0u8; CODE as usize + code.len()];
    image[CODE as usize..].copy_from_slice(&code);
    let mut vector = |n: u32, value: u32| image[4 * n as usize..4 * n as usize + 4].copy_from_slice(&value.to_le_bytes());
    vector(0, SRAM_BASE + SRAM_SIZE as u32);
    for (n, label) in [(1, "reset"), (HARD_FAULT, "hard_fault"), (SVCALL, "svc"), (PENDSV, "pendsv"), (SYSTICK, "systick")] {
        if let Some(offset) = symbols.get(label) {
            vector(n, (FLASH_BASE + CODE + offset) | 1);
        }
    }
    Machine::stm32g031(&image).unwrap()
}

fn run(m: &mut Machine) -> Outcome {
    m.run(10_000_000)
}

fn ended(m: &mut Machine, imm: u8) {
    match run(m) {
        Outcome::Breakpoint { imm: got, .. } if got == imm => {}
        other => panic!("expected bkpt #{imm}, got {other:?}; events {:?}", m.events),
    }
}

#[test]
fn add_and_subtract_flags() {
    let mut m = machine(
        "
        reset:
            movs r0, #1
            lsls r0, r0, #31      @ 0x8000_0000, C from the bit shifted out: 0
            adds r1, r0, r0       @ 0, carry out, signed overflow
            mrs r2, apsr
            subs r3, r1, #1       @ borrow: C clear
            mrs r4, apsr
            movs r5, #5
            cmp r5, #5            @ equal: Z, and no borrow, so C
            mrs r6, apsr
            bkpt #0
        ",
    );
    ended(&mut m, 0);
    // N Z C V in bits 31:28.
    assert_eq!(m.cpu.r[1], 0);
    assert_eq!(m.cpu.r[2] >> 28, 0b0111);
    assert_eq!(m.cpu.r[3], u32::MAX);
    assert_eq!(m.cpu.r[4] >> 28, 0b1000);
    assert_eq!(m.cpu.r[6] >> 28, 0b0110);
}

#[test]
fn sixty_four_bit_arithmetic_through_the_carry() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0xFFFFFFFF   @ low half of a
            movs r1, #0           @ high half of a
            movs r2, #1           @ low half of b
            movs r3, #0
            adds r0, r0, r2
            adcs r1, r1, r3       @ a + b = 0x1_0000_0000
            movs r4, #0
            movs r5, #1           @ c = 0x1_0000_0000
            movs r6, #1
            movs r7, #0
            subs r4, r4, r6
            sbcs r5, r5, r7       @ c - 1 = 0x0_FFFF_FFFF
            bkpt #0
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!((m.cpu.r[1], m.cpu.r[0]), (1, 0));
    assert_eq!((m.cpu.r[5], m.cpu.r[4]), (0, 0xFFFF_FFFF));
}

#[test]
fn register_shifts_by_large_amounts() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x80000001
            movs r1, #32
            mov r2, r0
            lsls r2, r1           @ 0, C = bit 0
            mrs r3, apsr
            mov r4, r0
            asrs r4, r1           @ all ones, C = bit 31
            movs r5, #33
            mov r6, r0
            lsrs r6, r5           @ 0, C clear
            mrs r7, apsr
            bkpt #0
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[2], 0);
    assert_eq!(m.cpu.r[3] >> 28 & 0b0010, 0b0010);
    assert_eq!(m.cpu.r[4], u32::MAX);
    assert_eq!(m.cpu.r[6], 0);
    assert_eq!(m.cpu.r[7] >> 28 & 0b0010, 0);
}

#[test]
fn calls_returns_and_the_stack() {
    let mut m = machine(
        "
        reset:
            movs r0, #3
            bl triple
            mov r7, sp
            bkpt #0
        triple:
            push {r4, lr}
            movs r4, r0
            adds r0, r0, r4
            adds r0, r0, r4
            pop {r4, pc}
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[0], 9);
    assert_eq!(m.cpu.r[7], SRAM_BASE + SRAM_SIZE as u32, "the stack balanced");
}

#[test]
fn load_multiple_writes_back_only_without_its_base() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x20000000
            movs r1, #7
            movs r2, #9
            stm r0!, {r1, r2}     @ r0 += 8
            ldr r3, =0x20000000
            ldm r3!, {r4, r5}     @ r3 += 8
            ldr r6, =0x20000000
            ldm r6, {r1, r6}      @ base loaded: no writeback, r6 = 9
            bkpt #0
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[0], 0x2000_0008);
    assert_eq!(m.cpu.r[3], 0x2000_0008);
    assert_eq!((m.cpu.r[4], m.cpu.r[5]), (7, 9));
    assert_eq!(m.cpu.r[6], 9);
}

#[test]
fn unaligned_access_is_a_hard_fault() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x20000002
            ldr r1, [r0]
            bkpt #0
        hard_fault:
            bkpt #1
        .ltorg
        ",
    );
    ended(&mut m, 1);
    assert!(matches!(m.events[..], [Event::Fault { fault: Fault::Unaligned { addr: 0x2000_0002 }, .. }]));
}

/// The shape of a Hubris syscall: an unprivileged task on the process
/// stack traps, the handler reads and rewrites the stacked r0, and the
/// return lands back in Thread mode, unprivileged, on the process stack,
/// with the frame popped.
#[test]
fn svc_from_an_unprivileged_task() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x20001000
            msr psp, r0
            movs r0, #3           @ nPRIV | SPSEL
            msr control, r0
            isb
            movs r0, #20
            svc #7
            mrs r1, control
            mov r2, sp
            mrs r3, ipsr
            bkpt #0
        svc:
            mrs r0, psp
            ldr r1, [r0]          @ the stacked r0
            adds r1, r1, #22
            str r1, [r0]
            bx lr
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[0], 42);
    assert_eq!(m.cpu.r[1], 3, "still unprivileged, still on psp");
    assert_eq!(m.cpu.r[2], 0x2000_1000, "the frame was popped");
    assert_eq!(m.cpu.r[3], 0, "back in Thread mode");
    assert_eq!(m.taken[SVCALL as usize], 1);
}

/// A stack pointer that is 4 mod 8 is realigned to stack a frame, and the
/// realignment is undone on return.
#[test]
fn exception_frames_realign_the_stack() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x20001004
            mov sp, r0
            svc #0
            mov r1, sp
            bkpt #0
        svc:
            mov r4, sp            @ r0-r3 are unstacked on return
            bx lr
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[4], 0x2000_1004 - 0x24, "frame at an 8-byte boundary");
    assert_eq!(m.cpu.r[1], 0x2000_1004);
}

/// SysTick, in instructions: `wfi` sleeps to each tick rather than
/// spinning, and the ticks arrive every `rvr + 1` cycles.
#[test]
fn systick_wakes_wfi() {
    let mut m = machine(
        "
        reset:
            movs r7, #0
            ldr r0, =0xE000E010
            movs r1, #99
            str r1, [r0, #4]      @ RVR
            str r1, [r0, #8]      @ CVR: any write clears it
            movs r1, #7
            str r1, [r0]          @ CSR: enable, interrupt, core clock
        wait:
            wfi
            cmp r7, #5
            bne wait
            bkpt #0
        systick:
            adds r7, r7, #1
            bx lr
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.taken[SYSTICK as usize], 5);
    // Five periods of 100 cycles, plus a few dozen instructions of setup
    // and handlers.
    assert!((500..600).contains(&m.cycles), "{} cycles", m.cycles);
}

/// PendSV set from inside a handler of the same priority waits for that
/// handler to return, then tail-chains — the deferred context switch.
#[test]
fn pendsv_tail_chains_after_svc() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0xE000ED1C
            ldr r1, =0xC0000000
            str r1, [r0]          @ SVCall priority 3
            ldr r1, =0xC0C00000
            str r1, [r0, #4]      @ PendSV and SysTick priority 3
            movs r6, #0
            svc #0
            bkpt #0
        svc:
            ldr r0, =0xE000ED04
            ldr r1, =0x10000000
            str r1, [r0]          @ ICSR.PENDSVSET
            movs r6, #1           @ still in SVCall: PendSV has not run
            bx lr
        pendsv:
            lsls r6, r6, #1       @ runs after SVCall's body: 1 -> 2
            bx lr
        .ltorg
        ",
    );
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[6], 2);
    assert_eq!(m.taken[PENDSV as usize], 1);
}

/// What task isolation rests on: an unprivileged load from a region it
/// has no access to is refused, and is a HardFault, taken in Handler mode
/// where the MPU's default map lets the kernel see everything again.
#[test]
fn the_mpu_refuses_an_unprivileged_load() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0xE000ED9C
            ldr r1, =0x08000010   @ region 0: flash, VALID
            str r1, [r0]
            ldr r1, =0x0600001F   @ 64 KiB, read-only to everyone, enabled
            str r1, [r0, #4]
            ldr r1, =0x20001011   @ region 1: top 4 KiB of SRAM, VALID
            str r1, [r0]
            ldr r1, =0x13000017   @ 4 KiB, full access, XN, enabled
            str r1, [r0, #4]
            ldr r0, =0xE000ED94
            movs r1, #5           @ ENABLE | PRIVDEFENA
            str r1, [r0]
            ldr r0, =0x20002000
            msr psp, r0
            movs r0, #3
            msr control, r0
            isb
            ldr r0, =0x20001ff0
            ldr r1, [r0]          @ allowed: the task's own stack
            ldr r0, =0x20000000
            ldr r1, [r0]          @ refused: below its region
            bkpt #0
        hard_fault:
            bkpt #1
        .ltorg
        ",
    );
    ended(&mut m, 1);
    match m.events[..] {
        [Event::Fault { fault: Fault::MemManage { addr, access }, privileged, .. }] => {
            assert_eq!((addr, access, privileged), (0x2000_0000, Access::Read, false));
        }
        ref events => panic!("{events:?}"),
    }
}

/// A fault inside HardFault has nowhere to go.
#[test]
fn a_fault_in_hard_fault_locks_up() {
    let mut m = machine(
        "
        reset:
            udf #0
        hard_fault:
            udf #1
        ",
    );
    assert!(matches!(run(&mut m), Outcome::Lockup { fault: Fault::Undefined { .. }, .. }));
}

#[test]
fn gpio_set_and_reset_through_bsrr() {
    let mut m = machine(
        "
        reset:
            ldr r0, =0x50000800   @ GPIOC
            ldr r1, =0x00000040
            str r1, [r0, #0x18]   @ set PC6
            ldr r1, =0x00400000
            str r1, [r0, #0x18]   @ reset PC6
            bkpt #0
        .ltorg
        ",
    );
    ended(&mut m, 0);
    let edges: Vec<_> = m
        .events
        .iter()
        .map(|e| match e {
            Event::Pin { edge, .. } => (edge.port, edge.before, edge.after),
            e => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(edges, [(2, 0, 0x40), (2, 0x40, 0)]);
}
