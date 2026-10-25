//! The ARMv7-M system model, as programs: faults with exceptions of their
//! own and what they record, escalation, BASEPRI and FAULTMASK, priority
//! grouping, derived faults on the way in and out of a handler, and the
//! extended frame.
//!
//! As in `isa.rs`, each test is assembly — for a Cortex-M7 this time — with
//! its handlers found by label, ending at a `bkpt` whose immediate says how
//! it ended. The instruction set itself is `thumb2.rs`'s business.

use std::collections::HashMap;
use std::process::Command;

use armv6m::bus::{FLASH_BASE, SRAM_BASE};
use armv6m::part::STM32G070;
use armv6m::scs::cfsr;
use armv6m::scs::exception::*;
use armv6m::state::Core;
use armv6m::{Machine, Outcome};
use object::{Object, ObjectSection, ObjectSymbol};

const CODE: u32 = 0x400;
const STACK_TOP: u32 = SRAM_BASE + 0x9000;

fn assemble(source: &str) -> (Vec<u8>, HashMap<String, u32>) {
    let dir = std::env::temp_dir().join(format!("armv7m-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    let asm = dir.join("test.s");
    let obj = dir.join("test.o");
    std::fs::write(&asm, format!(".syntax unified\n.thumb\n.text\n{source}\n")).unwrap();
    let status = Command::new("clang")
        .args(["--target=thumbv7em-none-eabihf", "-mcpu=cortex-m7", "-mfpu=fpv5-d16", "-mfloat-abi=hard", "-c"])
        .arg(&asm)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("clang, to assemble the test programs");
    assert!(status.success(), "assembly failed");
    let data = std::fs::read(&obj).unwrap();
    let file = object::File::parse(&*data).unwrap();
    let text = file.section_by_name(".text").unwrap();
    assert!(text.relocations().next().is_none(), "test programs must assemble without relocations");
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
    vector(0, STACK_TOP);
    for (n, label) in [
        (1, "reset"),
        (HARD_FAULT, "hard_fault"),
        (MEM_MANAGE, "mem_manage"),
        (BUS_FAULT, "bus_fault"),
        (USAGE_FAULT, "usage_fault"),
        (SVCALL, "svc"),
        (PENDSV, "pendsv"),
        (SYSTICK, "systick"),
        (IRQ0, "irq0"),
        (IRQ0 + 1, "irq1"),
    ] {
        if let Some(offset) = symbols.get(label) {
            vector(n, (FLASH_BASE + CODE + offset) | 1);
        }
    }
    let mut m = Machine::stm32g0(STM32G070, &image).unwrap();
    m.set_core(Core::CortexM7).unwrap();
    m
}

fn ended(m: &mut Machine, imm: u8) {
    match m.run(10_000_000) {
        Outcome::Breakpoint { imm: got, .. } if got == imm => {}
        other => panic!("expected bkpt #{imm}, got {other:?}; events {:?}", m.events),
    }
}

/// The SCB and MPU addresses the programs use.
const PRELUDE: &str = "
    .equ SHCSR, 0xE000ED24
    .equ CFSR, 0xE000ED28
    .equ HFSR, 0xE000ED2C
    .equ MMFAR, 0xE000ED34
    .equ CCR, 0xE000ED14
    .equ AIRCR, 0xE000ED0C
    .equ SHPR1, 0xE000ED18
    .equ SHPR2, 0xE000ED1C
    .equ SHPR3, 0xE000ED20
    .equ ISER, 0xE000E100
    .equ ISPR, 0xE000E200
    .equ IPR, 0xE000E400
    .equ MPU_CTRL, 0xE000ED94
    .equ MPU_RNR, 0xE000ED98
    .equ MPU_RBAR, 0xE000ED9C
    .equ MPU_RASR, 0xE000EDA0
";

fn program(body: &str) -> String {
    format!("{PRELUDE}\n{body}\n.ltorg\n")
}

#[test]
fn an_undefined_instruction_is_a_usage_fault_when_enabled_and_forced_when_not() {
    let source = program(
        "
        reset:
            ldr r0, =SHCSR
            ldr r1, =(1 << 18)      @ USGFAULTENA
            str r1, [r0]
            udf #1
        usage_fault:
            ldr r0, =CFSR
            ldr r4, [r0]
            str r4, [r0]            @ write-one-to-clear
            ldr r5, [r0]
            bkpt #1
        hard_fault:
            bkpt #2
        ",
    );
    let mut m = machine(&source);
    ended(&mut m, 1);
    assert_eq!(m.cpu.r[4], cfsr::UNDEFINSTR);
    assert_eq!(m.cpu.r[5], 0);

    // The same with the enable left clear: HardFault, FORCED.
    let source = source.replace("ldr r1, =(1 << 18)", "movs r1, #0");
    let mut m = machine(&source.replace("bkpt #2", "ldr r0, =HFSR\nldr r6, [r0]\nldr r0, =CFSR\nldr r4, [r0]\nbkpt #2"));
    ended(&mut m, 2);
    assert_eq!(m.cpu.r[6], 1 << 30);
    assert_eq!(m.cpu.r[4], cfsr::UNDEFINSTR, "the syndrome is recorded either way");
}

#[test]
fn a_divide_by_zero_answers_zero_unless_ccr_traps_it() {
    let mut m = machine(&program(
        "
        reset:
            movs r0, #7
            movs r1, #0
            sdiv r2, r0, r1
            ldr r3, =SHCSR
            ldr r4, =(1 << 18)
            str r4, [r3]
            ldr r3, =CCR
            ldr r4, [r3]
            orr r4, r4, #(1 << 4)   @ DIV_0_TRP
            str r4, [r3]
            udiv r2, r0, r1
            bkpt #0
        usage_fault:
            ldr r0, =CFSR
            ldr r5, [r0]
            bkpt #1
        ",
    ));
    ended(&mut m, 1);
    assert_eq!(m.cpu.r[2], 0);
    assert_eq!(m.cpu.r[5], cfsr::DIVBYZERO);
}

#[test]
fn an_unprivileged_access_the_mpu_refuses_is_a_mem_manage_fault_with_its_address() {
    let mut m = machine(&program(
        "
        reset:
            ldr r0, =SHCSR
            ldr r1, =(1 << 16)      @ MEMFAULTENA
            str r1, [r0]
            @ Region 0: all of SRAM, read-write for both.
            ldr r0, =MPU_RNR
            movs r1, #0
            str r1, [r0]
            ldr r0, =MPU_RBAR
            ldr r1, =0x20000000
            str r1, [r0]
            ldr r0, =MPU_RASR
            ldr r1, =(3 << 24) | (15 << 1) | 1     @ AP=RW/RW, 64 KiB
            str r1, [r0]
            @ Region 1: 256 bytes of it, privileged only.
            ldr r0, =MPU_RNR
            movs r1, #1
            str r1, [r0]
            ldr r0, =MPU_RBAR
            ldr r1, =0x20004000
            str r1, [r0]
            ldr r0, =MPU_RASR
            ldr r1, =(1 << 24) | (7 << 1) | 1      @ AP=RW/none, 256 bytes
            str r1, [r0]
            @ Region 2: flash, read-only and executable for both.
            ldr r0, =MPU_RNR
            movs r1, #2
            str r1, [r0]
            ldr r0, =MPU_RBAR
            ldr r1, =0x08000000
            str r1, [r0]
            ldr r0, =MPU_RASR
            ldr r1, =(6 << 24) | (16 << 1) | 1     @ AP=RO/RO, 128 KiB
            str r1, [r0]
            ldr r0, =MPU_CTRL
            movs r1, #0b101         @ ENABLE, PRIVDEFENA
            str r1, [r0]
            movs r0, #1
            msr CONTROL, r0         @ drop privilege
            isb
            ldr r2, =0x20004010
            ldr r3, [r2]
            bkpt #0
        mem_manage:
            ldr r0, =CFSR
            ldr r5, [r0]
            ldr r0, =MMFAR
            ldr r6, [r0]
            bkpt #1
        ",
    ));
    ended(&mut m, 1);
    assert_eq!(m.cpu.r[5], cfsr::DACCVIOL | cfsr::MMARVALID);
    assert_eq!(m.cpu.r[6], 0x2000_4010);
}

#[test]
fn basepri_holds_off_what_it_masks_and_faultmask_holds_off_everything() {
    let mut m = machine(&program(
        "
        reset:
            ldr r0, =IPR
            ldr r1, =0x00008040     @ IRQ0 at 0x40, IRQ1 at 0x80
            str r1, [r0]
            ldr r0, =ISER
            movs r1, #3
            str r1, [r0]
            movs r1, #0x80
            msr BASEPRI, r1         @ masks 0x80 and below
            ldr r0, =ISPR
            movs r1, #3
            str r1, [r0]            @ both pending: IRQ0 runs, IRQ1 waits
            movs r7, #1             @ IRQ0's handler records r7
            movs r1, #0x40
            msr BASEPRI_MAX, r1     @ raises it: nothing more preempts
            mrs r2, BASEPRI
            movs r1, #0x80
            msr BASEPRI_MAX, r1     @ cannot lower it
            mrs r3, BASEPRI
            movs r1, #0
            msr BASEPRI, r1
            cpsid f                 @ FAULTMASK: still nothing
            mrs r4, FAULTMASK
            cpsie f
            bkpt #0
        irq0:
            mov r6, r7
            bx lr
        irq1:
            add r8, r7, #100
            bx lr
        ",
    ));
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[6], 0, "IRQ0 ran before r7 was set: BASEPRI 0x80 does not mask 0x40");
    assert_eq!(m.cpu.r[2], 0x40);
    assert_eq!(m.cpu.r[3], 0x40);
    assert_eq!(m.cpu.r[4], 1);
    // IRQ1 ran when BASEPRI and FAULTMASK came off, by which time r7 was 1.
    assert_eq!(m.cpu.r[8], 101);
}

#[test]
fn a_subpriority_orders_but_does_not_preempt() {
    // PRIGROUP 4: bits 7:5 are the group, 4:0 subpriority. IRQ0 at 0x20 and
    // IRQ1 at 0x30 are the same group, so IRQ1's handler, pending IRQ0,
    // runs to completion first.
    let mut m = machine(&program(
        "
        reset:
            ldr r0, =AIRCR
            ldr r1, =0x05FA0400
            str r1, [r0]
            ldr r0, =IPR
            ldr r1, =0x00003020
            str r1, [r0]
            ldr r0, =ISER
            movs r1, #3
            str r1, [r0]
            ldr r0, =ISPR
            movs r1, #2
            str r1, [r0]            @ IRQ1
            bkpt #0
        irq1:
            movs r1, #1
            str r1, [r0]            @ pend IRQ0 from inside IRQ1
            movs r5, #1
            bx lr
        irq0:
            mov r6, r5              @ 1 if IRQ1 finished first
            bx lr
        ",
    ));
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[6], 1);
}

#[test]
fn a_stacking_fault_is_mstkerr_and_leaves_the_exception_pending() {
    let mut m = machine(&program(
        "
        reset:
            ldr r0, =SHCSR
            ldr r1, =(1 << 16)
            str r1, [r0]
            ldr r0, =SHPR2
            ldr r1, =0x80000000     @ SVCall below MemManage
            str r1, [r0]
            @ The MPU with flash and the privileged default map: an
            @ unprivileged stack is nowhere.
            @ Region 2: flash, read-only and executable for both.
            ldr r0, =MPU_RNR
            movs r1, #2
            str r1, [r0]
            ldr r0, =MPU_RBAR
            ldr r1, =0x08000000
            str r1, [r0]
            ldr r0, =MPU_RASR
            ldr r1, =(6 << 24) | (16 << 1) | 1     @ AP=RO/RO, 128 KiB
            str r1, [r0]
            ldr r0, =MPU_CTRL
            movs r1, #0b101
            str r1, [r0]
            ldr r0, =0x20004000
            msr PSP, r0
            movs r0, #0b11          @ unprivileged, on PSP
            msr CONTROL, r0
            isb
            svc #0
        svc:
            bkpt #9
        mem_manage:
            ldr r0, =CFSR
            ldr r5, [r0]
            ldr r0, =SHCSR
            ldr r6, [r0]
            mov r7, lr
            bkpt #1
        ",
    ));
    ended(&mut m, 1);
    assert_eq!(m.cpu.r[5], cfsr::MSTKERR);
    assert_ne!(m.cpu.r[6] & 1 << 15, 0, "SVCALLPENDED");
    assert_eq!(m.cpu.r[7], 0xFFFF_FFFD, "the frame was for Thread mode on PSP");
}

#[test]
fn an_unstacking_fault_is_tail_chained_with_the_same_exc_return() {
    let mut m = machine(&program(
        "
        reset:
            ldr r0, =SHCSR
            ldr r1, =(1 << 16)
            str r1, [r0]
            ldr r0, =SHPR2
            ldr r1, =0x80000000
            str r1, [r0]
            ldr r0, =0x20004000
            msr PSP, r0
            movs r0, #0b10          @ privileged, on PSP
            msr CONTROL, r0
            isb
            svc #0
            bkpt #9
        svc:
            @ Point PSP somewhere the unprivileged return cannot read, and
            @ return to unprivileged Thread mode through it.
            ldr r0, =MPU_CTRL
            movs r1, #0b101
            str r1, [r0]
            movs r0, #0b11
            msr CONTROL, r0
            ldr lr, =0xFFFFFFFD
            bx lr
        mem_manage:
            ldr r0, =CFSR
            ldr r5, [r0]
            mov r7, lr
            mrs r6, IPSR
            bkpt #1
        ",
    ));
    ended(&mut m, 1);
    assert_eq!(m.cpu.r[5], cfsr::MUNSTKERR);
    assert_eq!(m.cpu.r[7], 0xFFFF_FFFD);
    assert_eq!(m.cpu.r[6], MEM_MANAGE);
    assert!(!m.bus.scs.nvic.active.contains(SVCALL), "the return deactivated SVCall");
}

#[test]
fn a_context_that_used_the_fpu_is_stacked_with_it() {
    let mut m = machine(&program(
        "
        reset:
            movs r0, #0b100         @ FPCA
            msr CONTROL, r0
            svc #0
            mrs r6, CONTROL
            bkpt #0
        svc:
            mov r4, lr
            ldr r5, [sp, #0x20]     @ s0, as stacked
            mrs r7, CONTROL
            bx lr
        ",
    ));
    m.cpu.s[0] = 0x3F80_0000;
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[4], 0xFFFF_FFE9, "Thread, MSP, extended frame");
    assert_eq!(m.cpu.r[5], 0x3F80_0000);
    assert_eq!(m.cpu.r[7] & 4, 0, "FPCA clear in the handler");
    assert_eq!(m.cpu.r[6] & 4, 4, "and set again on return");
    assert_eq!(m.cpu.msp, STACK_TOP);
}

#[test]
fn the_fpu_is_absent_until_cpacr_enables_it() {
    let mut m = machine(&program(
        "
        .equ CPACR, 0xE000ED88
        reset:
            ldr r0, =SHCSR
            ldr r1, =(1 << 18)
            str r1, [r0]
            vadd.f32 s0, s1, s2
            bkpt #0
        usage_fault:
            ldr r0, =CFSR
            ldr r5, [r0]
            str r5, [r0]
            @ Enable CP10 and CP11 and go back to the instruction.
            ldr r0, =CPACR
            ldr r1, =(0xF << 20)
            str r1, [r0]
            dsb
            isb
            bx lr
        ",
    ));
    m.cpu.s[1] = 1.5f32.to_bits();
    m.cpu.s[2] = 2.25f32.to_bits();
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[5], cfsr::NOCP);
    assert_eq!(f32::from_bits(m.cpu.s[0]), 3.75);
}

#[test]
fn an_fp_instruction_marks_the_context_and_the_frame_carries_it() {
    let mut m = machine(&program(
        "
        .equ CPACR, 0xE000ED88
        reset:
            ldr r0, =CPACR
            ldr r1, =(0xF << 20)
            str r1, [r0]
            isb
            vmov.f32 s0, #1.0
            mrs r6, CONTROL         @ FPCA, from ASPEN
            svc #0
            vmov r7, s0             @ restored by the return
            bkpt #0
        svc:
            mov r4, lr
            vmov.f32 s0, #2.0       @ the handler's own use
            bx lr
        ",
    ));
    ended(&mut m, 0);
    assert_eq!(m.cpu.r[6] & 4, 4);
    assert_eq!(m.cpu.r[4], 0xFFFF_FFE9);
    assert_eq!(f32::from_bits(m.cpu.r[7]), 1.0);
}
