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

/// A byte written before following begins, then copied, beside a
/// constant, a register's value from before, and a byte of flash.
const BEFORE_AND_AFTER: &str = "
    ldr r3, =0x24000100
    movs r1, #0x5A
    strb r1, [r3]            @ before: what the copy is of
    movs r2, #0x33           @ before: held in a register
    nop
begin:
    ldrb r4, [r3]            @ held since before, written by the strb
    ldr r6, =0x24000200
    strb r4, [r6]
    movs r5, #7
    strb r5, [r6, #1]        @ a constant
    strb r2, [r6, #2]        @ a register's, from before
    adr r7, table
    ldrb r7, [r7]
    strb r7, [r6, #3]        @ flash's
    bkpt #1
    .align 2
table:
    .byte 0xC4, 0, 0, 0
    .ltorg
";

#[test]
fn following_everything_ends_every_history_where_it_came_in() {
    let source = format!(".thumb_func\nreset:\n{BEFORE_AND_AFTER}");
    // Where `begin` is in time: five instructions in.
    let mut probe = h753(&source);
    assert!(matches!(probe.run_until(u64::MAX, 5), Outcome::Budget));
    let from = probe.time;
    let mut m = h753(&source);
    m.follow_everything_from(from);
    run(&mut m);
    let p = m.provenance.as_ref().expect("following began");
    assert!(p.everything && p.since >= from);
    // The value's own way, past the loads' addresses.
    let what = |addr: u32| -> Vec<What> {
        let tag = *p.memory.get(&addr).unwrap_or_else(|| panic!("{addr:#x} is tagged"));
        p.history(tag, 8).iter().map(|(_, node)| node.what).filter(|w| !matches!(w, What::Load { .. })).collect()
    };
    // The copy: from the byte held since before, with the write that put it there.
    let copy = what(0x2400_0200);
    assert!(matches!(copy[0], What::Store { addr: 0x2400_0200, size: 1, value: 0x5A }), "{copy:?}");
    match copy[1] {
        What::Held { addr: 0x2400_0100, size: 1, value: 0x5A, written: Some(mark) } => {
            assert!(mark.stamp != 0 && mark.writer >= armv6m::bus::FLASH_BASE, "{mark:?}")
        }
        other => panic!("held since before: {other:?}"),
    }
    assert_eq!(copy.len(), 2);
    // The constant: a store of nothing.
    assert_eq!(what(0x2400_0201), [What::Store { addr: 0x2400_0201, size: 1, value: 7 }]);
    // The register's, from before.
    let register = what(0x2400_0202);
    assert!(matches!(register[1], What::Register { n: 2, value: 0x33 }), "{register:?}");
    // Flash's: held, and never written.
    let flash = what(0x2400_0203);
    assert!(matches!(flash[1], What::Held { value: 0xC4, written: None, .. }), "{flash:?}");
}

#[test]
fn following_everything_from_an_instant_changes_nothing() {
    let source = format!(".thumb_func\nreset:\n{BEFORE_AND_AFTER}");
    let mut followed = h753(&source);
    followed.follow_everything_from(1);
    run(&mut followed);
    let mut plain = h753(&source);
    run(&mut plain);
    assert_eq!((followed.retired, followed.time, &followed.cpu), (plain.retired, plain.time, &plain.cpu));
}

#[test]
fn a_bytes_story_follows_its_lane_through_a_word_copy() {
    let source = ".thumb_func\nreset:
    movs r2, #0x33
    ldr r3, =0x24000100
    movs r1, #7
    strb r1, [r3]            @ byte 0: a constant
    strb r2, [r3, #1]        @ byte 1: a register's, from before
    adr r7, table
    ldrb r7, [r7]
    strb r7, [r3, #2]        @ byte 2: flash's
    strb r1, [r3, #3]
    ldr r4, [r3]             @ the four, as a word
    ldr r6, =0x24000200
    str r4, [r6]             @ copied
    bkpt #1
    .align 2
table:
    .byte 0xC4, 0, 0, 0
    .ltorg
";
    let mut m = h753(source);
    m.follow_everything_from(m.time + 1);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let story = |address: u32| -> Vec<(What, u32)> {
        p.story(address, 8).iter().map(|&(_, node, byte)| (node.what, byte)).filter(|(w, _)| !matches!(w, What::Load { .. })).collect()
    };
    let flash = story(0x2400_0202);
    assert!(matches!(flash[0], (What::Store { addr: 0x2400_0200, size: 4, .. }, 0x2400_0202)), "{flash:?}");
    assert!(matches!(flash[1], (What::Store { addr: 0x2400_0102, size: 1, value: 0xC4 }, 0x2400_0102)), "{flash:?}");
    assert!(matches!(flash[2], (What::Held { value: 0xC4, written: None, .. }, _)), "{flash:?}");
    let register = story(0x2400_0201);
    assert!(matches!(register[1], (What::Store { addr: 0x2400_0101, value: 0x33, .. }, _)), "{register:?}");
    assert!(matches!(register[2], (What::Register { n: 2, value: 0x33 }, _)), "{register:?}");
    let constant = story(0x2400_0203);
    assert!(matches!(constant[..], [_, (What::Store { addr: 0x2400_0103, value: 7, .. }, _)]), "{constant:?}");
}

#[test]
fn a_value_looked_up_in_a_table_leads_to_the_index_that_chose_it() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100      @ a table
    movs r1, #0x5A
    strb r1, [r3, #2]        @ its third entry
    ldr r4, =0x24000180      @ where the index is kept
    movs r1, #2
    strb r1, [r4]
begin:
    ldrb r5, [r4]            @ the index, held since before
    ldrb r6, [r3, r5]        @ the entry it chooses
    ldr r7, =0x24000200
    strb r6, [r7]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 6), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let story = p.story(0x2400_0200, 8);
    let load = story.iter().find_map(|&(_, node, _)| match node.what {
        What::Load { addr } => Some((addr, node.parents[1])),
        _ => None,
    });
    let (addr, address) = load.expect("the lookup is a step of the story");
    assert_eq!(addr, 0x2400_0102);
    assert!(matches!(story.last().unwrap().1.what, What::Held { addr: 0x2400_0102, value: 0x5A, .. }), "the entry, held since before");
    let origins: Vec<What> = p.origins(address, 8).iter().map(|(_, node)| node.what).collect();
    assert!(origins.iter().any(|w| matches!(w, What::Held { addr: 0x2400_0180, value: 2, .. })), "the index: {origins:?}");
}

#[test]
fn bytes_followed_forward_tag_whatever_is_made_of_them() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    movs r1, #0x21
    strb r1, [r3]            @ the byte to follow
    nop
begin:
    ldrb r4, [r3]
    adds r4, #1              @ made into something else
    ldr r6, =0x24000200
    strb r4, [r6]            @ where it went
    movs r5, #9
    strb r5, [r6, #1]        @ beside it, from nowhere it was
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_forward_from(probe.time, 0x2400_0100, 1);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    assert!(p.seeded);
    assert!(p.memory.contains_key(&0x2400_0200), "what was made of it is tagged");
    assert!(!p.memory.contains_key(&0x2400_0201), "what was not is not");
}
