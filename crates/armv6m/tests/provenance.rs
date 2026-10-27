//! Where values came from (`armv6m::provenance`): a program reads a
//! TMP117's device ID off I2C2, byte by byte from RXDR, and moves it about —
//! a buffer, a merge of the two bytes, the stack, another buffer — and the
//! tag on the last copy leads back, step by step, to the reads of RXDR.

use std::process::Command;

use armv6m::bus::FLASH_BASE;
use armv6m::devices::Wiring;
use armv6m::part::Part;
use armv6m::provenance::What;
use armv6m::{Machine, Outcome};
use object::{Object, ObjectSection};

const CODE: u32 = 0x400;

fn assemble(source: &str) -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("provenance-{}-{:?}", std::process::id(), std::thread::current().id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (asm, obj) = (dir.join("test.s"), dir.join("test.o"));
    std::fs::write(&asm, format!(".syntax unified\n.thumb\n.text\n{source}\n")).unwrap();
    let status = Command::new("clang")
        .args(["--target=thumbv7em-none-eabihf", "-mcpu=cortex-m7", "-c"])
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
    text.data().unwrap().to_vec()
}

/// An H753 with a TMP117 on I2C2, running `source` from reset.
fn h753(source: &str) -> Machine {
    let code = assemble(source);
    let mut image = vec![0u8; CODE as usize + code.len()];
    image[CODE as usize..].copy_from_slice(&code);
    image[0..4].copy_from_slice(&0x2000_2000u32.to_le_bytes());
    image[4..8].copy_from_slice(&((FLASH_BASE + CODE) | 1).to_le_bytes());
    let mut m = Machine::new(Part::named("stm32h753").unwrap(), &image).unwrap();
    m.attach(Wiring::parse("i2c I2C2 0x48 tmp117").unwrap()).unwrap();
    m
}

const READ_THE_ID: &str = "
    ldr r0, =0x40005800      @ I2C2
    movs r1, #1
    str r1, [r0]             @ CR1: PE
    ldr r1, =0x00012090      @ CR2: a byte to 0x48, START
    str r1, [r0, #4]
1:  ldr r2, [r0, #0x18]      @ ISR: TXIS
    tst r2, #2
    beq 1b
    movs r1, #0x0F
    str r1, [r0, #0x28]      @ TXDR: the pointer, the device ID's
2:  ldr r2, [r0, #0x18]      @ TC
    tst r2, #0x40
    beq 2b
    ldr r1, =0x02022490      @ CR2: two bytes from 0x48, AUTOEND, START
    str r1, [r0, #4]
    ldr r3, =0x24000100      @ a buffer
3:  ldr r2, [r0, #0x18]      @ RXNE
    tst r2, #4
    beq 3b
    ldr r4, [r0, #0x24]      @ RXDR: 0x01
    strb r4, [r3]
4:  ldr r2, [r0, #0x18]
    tst r2, #4
    beq 4b
    ldr r5, [r0, #0x24]      @ RXDR: 0x17
    strb r5, [r3, #1]
    ldrb r6, [r3]
    ldrb r7, [r3, #1]
    orr r6, r7, r6, lsl #8   @ 0x0117: the two bytes merged
    push {r6}
    pop.w {r8}
    ldr r9, =0x24000200      @ another buffer
    strh r8, [r9]
    movs r6, #0
    str r6, [r3, #12]        @ nothing that came from anywhere
    bkpt #1
    .ltorg
";

fn run(m: &mut Machine) {
    match m.run_until(u64::MAX, 10_000_000) {
        Outcome::Breakpoint { imm: 1, .. } => {}
        other => panic!("expected bkpt #1, got {other:?}"),
    }
}

#[test]
fn a_copy_leads_back_through_every_step_to_the_bytes_off_the_bus() {
    let mut m = h753(&format!(".thumb_func\nreset:\n{READ_THE_ID}"));
    m.follow_values(true);
    run(&mut m);
    assert_eq!(m.bus.peek(0x2400_0200, 2).ok(), Some(0x0117));
    let p = m.provenance.as_ref().unwrap();
    let tag = *p.memory.get(&0x2400_0200).expect("the copy is tagged");
    let kinds: Vec<What> = p.history(tag, 16).iter().map(|(_, node)| node.what).collect();
    // The copy, from the stack; the stack slot, from the merge; the merge's
    // first part from the buffer; and the buffer from RXDR.
    assert!(matches!(kinds[0], What::Store { addr: 0x2400_0200, size: 2, value: 0x0117 }), "{kinds:?}");
    assert!(matches!(kinds[1], What::Store { size: 4, value: 0x0117, .. }), "the push: {kinds:?}");
    assert_eq!(kinds[2], What::Merge, "{kinds:?}");
    assert!(matches!(kinds[3], What::Store { addr: 0x2400_0100, size: 1, value: 0x01 }), "{kinds:?}");
    assert_eq!(kinds[4], What::Read { addr: 0x4000_5824, value: 0x01 }, "{kinds:?}");
    assert_eq!(kinds.len(), 5);
    // The merge's other part: the second byte, from RXDR too.
    let merge = p.history(tag, 16)[2].1.clone();
    let other: Vec<What> = p.history(merge.parents[1], 16).iter().map(|(_, node)| node.what).collect();
    assert_eq!(other, [What::Store { addr: 0x2400_0101, size: 1, value: 0x17 }, What::Read { addr: 0x4000_5824, value: 0x17 }]);
    // A zero stored from an immediate came from nowhere.
    assert!(p.memory.get(&0x2400_010c).is_none());
}

#[test]
fn following_nothing_changes_nothing() {
    let mut followed = h753(&format!(".thumb_func\nreset:\n{READ_THE_ID}"));
    followed.follow_values(true);
    run(&mut followed);
    let mut plain = h753(&format!(".thumb_func\nreset:\n{READ_THE_ID}"));
    run(&mut plain);
    assert_eq!((followed.retired, followed.time, &followed.cpu), (plain.retired, plain.time, &plain.cpu));
    assert!(plain.provenance.is_none());
}

#[test]
fn a_busy_wait_for_the_next_byte_waits_for_it() {
    // Reading RXDR starts the next byte; polling for it must not be
    // skipped past it to whatever was due before (nothing, here).
    let mut m = h753(&format!(".thumb_func\nreset:\n{READ_THE_ID}"));
    run(&mut m);
    assert!(m.time < 10_000_000_000, "done in a few bus bit-times, not {} ps", m.time);
}
