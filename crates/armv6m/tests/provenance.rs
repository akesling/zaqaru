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
    assert!(matches!(kinds[2], What::Merge { value: Some(0x0117), .. }), "{kinds:?}");
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
        What::Load { addr, .. } => Some((addr, node.parents[1])),
        _ => None,
    });
    let (addr, address) = load.expect("the lookup is a step of the story");
    assert_eq!(addr, 0x2400_0102);
    assert!(matches!(story.last().unwrap().1.what, What::Held { addr: 0x2400_0102, value: 0x5A, .. }), "the entry, held since before");
    let origins: Vec<What> = p.origins(address, 8).iter().map(|(_, node)| node.what).collect();
    assert!(origins.iter().any(|w| matches!(w, What::Held { addr: 0x2400_0180, value: 2, .. })), "the index: {origins:?}");
    // As a derivation: the entry, chosen by the index — the table's own
    // address saying nothing of which.
    let says_nothing = |node: &armv6m::provenance::Node| match node.what {
        What::Held { addr, value, .. } => addr.wrapping_sub(node.pc) < 4096 || value >= 0x2000_0000,
        What::Register { value, .. } => value >= 0x2000_0000,
        _ => false,
    };
    let tree = p.derivation(p.memory[&0x2400_0200], 16, 100, &says_nothing);
    let mut text = String::new();
    derivation_text(p, &tree, "", 0, &mut text);
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    assert_eq!(lines[..2], ["store 90 to 0x24000200", "load 90 from 0x24000102"], "{text}");
    // The address the entry was read at, the table's own left out, of the
    // index.
    assert_eq!(lines[3..5], ["chose: merge 603980034", "held 2 at 0x24000180"], "{text}");
    assert!(tree.operands[0].1.operands[1].1.pointer, "the table's address, left out");
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

#[test]
fn a_value_depends_on_what_it_was_made_of_and_what_chose_it() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100      @ a table
    movs r1, #0x50
    strb r1, [r3, #2]
    ldr r4, =0x24000180      @ an index
    movs r1, #2
    strb r1, [r4]
    ldr r2, =0x24000190      @ something to add
    movs r1, #7
    strb r1, [r2]
begin:
    ldrb r5, [r4]
    ldrb r6, [r3, r5]        @ the entry the index chooses
    ldrb r7, [r2]
    add r6, r7               @ and what is added to it
    ldr r0, =0x24000200
    strb r6, [r0]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 9), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let held: Vec<(u32, bool)> = p
        .influences(0x2400_0200, 16, &|_| false)
        .iter()
        .filter_map(|&(_, node, _, via)| match node.what {
            What::Held { addr, .. } => Some((addr, via)),
            _ => None,
        })
        .collect();
    assert!(held.contains(&(0x2400_0102, false)), "the entry: {held:?}");
    assert!(held.contains(&(0x2400_0190, false)), "what was added: {held:?}");
    assert!(held.contains(&(0x2400_0180, true)), "the index, which chose: {held:?}");
}

/// A derivation as text, a node a line, indented by depth: what each is,
/// its value, its role, how often it repeated.
fn derivation_text(p: &armv6m::provenance::Provenance, d: &armv6m::provenance::Derived, role: &str, depth: usize, out: &mut String) {
    let node = p.node(d.tag).unwrap();
    let what = match node.what {
        What::Store { addr, value, .. } => format!("store {value} to {addr:#x}"),
        What::Merge { value: Some(value), .. } => format!("merge {value}"),
        What::Merge { value: None, .. } => "merge".to_string(),
        What::Load { addr, value } => format!("load {value} from {addr:#x}"),
        What::Bytes { value, .. } => format!("bytes {value}"),
        What::Held { addr, value, .. } => format!("held {value} at {addr:#x}"),
        What::Register { n, value } => format!("r{n} {value}"),
        What::Read { value, .. } => format!("read {value}"),
    };
    let copies = if d.copies.is_empty() { String::new() } else { format!(" copied {}", d.copies.len()) };
    let repeats = if d.repeats > 0 { format!(" repeated {}", d.repeats) } else { String::new() };
    out.push_str(&format!("{}{role}{what}{copies}{repeats}\n", "  ".repeat(depth)));
    for (role, operand) in &d.operands {
        let role = match role {
            armv6m::provenance::Role::Value => "",
            armv6m::provenance::Role::Chose => "chose: ",
            armv6m::provenance::Role::Again => "before: ",
        };
        derivation_text(p, operand, role, depth + 1, out);
    }
}

#[test]
fn a_value_s_derivation_says_what_it_was_made_of_and_folds_a_count() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100      @ x, y and a scale
    movs r1, #5
    str r1, [r3]
    movs r1, #3
    str r1, [r3, #4]
    movs r1, #2
    str r1, [r3, #8]
    ldr r4, =0x24000180      @ a count
    movs r1, #0
    str r1, [r4]
begin:
    movs r2, #4
1:  ldr r1, [r4]             @ counted four times
    adds r1, #1
    str r1, [r4]
    subs r2, #1
    bne 1b
    ldr r5, [r3]
    ldr r6, [r3, #4]
    ldr r7, [r3, #8]
    mla r0, r6, r7, r5       @ x + y * scale: 11
    ldr r1, [r4]
    add r0, r1               @ and the count: 15
    ldr r6, =0x24000200
    str r0, [r6]
    ldr r1, [r6]
    ldr r7, =0x24000204
    str r1, [r7]             @ a copy
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 10), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    // Pointers, and constants beside the code, say nothing of which.
    let says_nothing = |node: &armv6m::provenance::Node| match node.what {
        What::Held { addr, value, .. } => addr.wrapping_sub(node.pc) < 4096 || value >= 0x2000_0000,
        What::Register { value, .. } => value >= 0x2000_0000,
        _ => false,
    };
    let tree = p.derivation(p.memory[&0x2400_0204], 16, 100, &says_nothing);
    let mut text = String::new();
    derivation_text(p, &tree, "", 0, &mut text);
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
    // The copy, of what the add made of the multiply-add and the count.
    assert_eq!(lines[0], "store 15 to 0x24000204 copied 1", "{text}");
    assert_eq!(lines[1], "merge 15", "{text}");
    assert_eq!(lines[2], "merge 11", "{text}");
    // The multiply-add's three, each held since before.
    for value in [3, 2, 5] {
        assert!(text.contains(&format!("held {value} at")), "{value}: {text}");
    }
    // The count: its store, repeated, from what the first began from.
    assert!(text.contains("store 4 to 0x24000180 repeated 3"), "{text}");
    assert!(text.contains("before: store 1 to 0x24000180"), "{text}");
    assert!(text.contains("held 0 at 0x24000180"), "{text}");
}

#[test]
fn an_address_made_of_immediates_is_made_of_nothing() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    movs r1, #5
    str r1, [r3]
    nop
begin:
    ldr r0, [r3]             @ r0 holds something followed
    ldr r1, [r3]
    cmp r0, #0
    movw r0, #0x0200         @ and then an address, of nothing
    movt r0, #0x2400
    str r1, [r0]
    str r0, [r0, #4]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let tag = p.memory[&0x2400_0204];
    let node = p.node(tag).unwrap();
    assert!(matches!(node.what, What::Store { value: 0x2400_0200, .. }), "{node:?}");
    assert_eq!(node.parents[0], 0, "the address is a constant: {:?}", p.node(node.parents[0]));
}

#[test]
fn a_pop_is_chosen_by_nothing() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    movs r1, #5
    str r1, [r3]
    ldr r0, =0x24000200
    movs r2, #1
    movs r7, #9
    adr r4, after
    adds r4, #1
    push {r4}
    push {r7}
    nop
begin:
    ldr r1, [r3]             @ followed values in registers
    ldr r2, [r3]
    str.w r1, [r0, r2, lsl #2]
    pop {r7, pc}
    .align 2
after:
    ldr r6, =0x24000300
    str r7, [r6]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 11), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let store = p.node(p.memory[&0x2400_0300]).unwrap();
    let popped = p.node(store.parents[0]).unwrap();
    assert!(matches!(popped.what, What::Held { value: 9, .. }), "{popped:?}");
}

#[test]
fn a_value_masked_by_a_constant_is_a_step_of_its_own() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x12345678
    str r1, [r3]
    nop
begin:
    ldr r0, [r3]
    mov.w r2, #0xfc0
    and.w r2, r2, r0, lsr #4
    ldr r6, =0x24000200
    str r2, [r6]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let store = p.node(p.memory[&0x2400_0200]).unwrap();
    let made = p.node(store.parents[0]).unwrap();
    assert!(matches!(made.what, What::Merge { value: Some(0x540), constant: Some((2, 0xfc0)) }), "the mask made in the window, a constant: {made:?}");
}

#[test]
fn a_value_masked_in_a_loop_begun_before_the_window_is_a_step_of_its_own() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x00010001
    str r1, [r3]
    mov.w r2, #0xfc0
    ldr r0, =0x12345678
    nop
begin:
    ldr r1, [r3]
    add r0, r1               @ the position, stepped
    and.w r4, r2, r0, lsr #4
    ldr r6, =0x24000200
    str r4, [r6]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 6), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let store = p.node(p.memory[&0x2400_0200]).unwrap();
    let made = p.node(store.parents[0]).unwrap();
    let parents: Vec<_> = made.parents.iter().map(|&t| p.node(t).map(|n| n.what)).collect();
    assert!(matches!(made.what, What::Merge { value: Some(0x540), .. }), "{made:?} {parents:?}");
}

#[test]
fn a_span_s_texture_index_is_its_position_shifted_and_masked() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x00010001
    str r1, [r3]
    ldr r1, =0x24001000      @ the flat
    str r1, [r3, #4]
    ldr r1, =0x24002000      @ the colour map
    str r1, [r3, #8]
    ldr r0, =0x12345678
    nop
begin:
    ldr r1, [r3]             @ the step
    ldr r9, =0x24000104
    ldr r8, =0x24000108
    mov.w r2, #0xfc0
    ldr r6, =0x24000200
    movs r5, #4
1:  ldr.w r4, [r9]
    and.w r12, r2, r0, lsr #4
    add.w r4, r4, r0, lsr #26
    ldrb.w r12, [r4, r12]
    ldr.w r7, [r8]
    ldrb.w r12, [r7, r12]
    strb r12, [r6], #1
    add r0, r1
    subs r5, #1
    bne 1b
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 9), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let says_nothing = |node: &armv6m::provenance::Node| match node.what {
        What::Held { addr, value, .. } => addr.wrapping_sub(node.pc) < 4096 || value >= 0x2000_0000,
        What::Register { value, .. } => value >= 0x2000_0000,
        What::Store { size: 4, value, .. } => value >= 0x2000_0000,
        _ => false,
    };
    let tree = p.derivation(p.memory[&0x2400_0202], 24, 200, &says_nothing);
    let mut text = String::new();
    derivation_text(p, &tree, "", 0, &mut text);
        // The entry chosen by the position, shifted and masked: a step of its
    // own, not the position itself.
    assert!(text.contains(&format!("merge {}", (0x12345678u32 >> 4) & 0xfc0)), "the mask's own step:\n{text}");
}

#[test]
fn a_count_s_word_says_since_when_the_same_code_has_stepped_it() {
    let source = ".thumb_func\nreset:
    ldr r0, =0x24000100
    movs r1, #0
    str r1, [r0]             @ set: another writer
    movs r3, #5
1:  ldr r1, [r0]
    adds r1, #1
    str r1, [r0]             @ stepped
    ldr r2, =2000
2:  subs r2, #1
    bne 2b
    subs r3, #1
    bne 1b
    bkpt #1
    .ltorg
";
    let mut m = h753(source);
    run(&mut m);
    let mark = m.bus.last_write(0x2400_0100).unwrap();
    assert_eq!(m.bus.peek(0x2400_0100, 4).ok(), Some(5));
    // Four gaps of the delay loop's microseconds between five steps: the
    // first step's stamp is `run` before the last's.
    assert!(mark.run > 4, "{mark:?}");
    let first = mark.stamp - u64::from(mark.run);
    assert!(first >= 1 && first < mark.stamp, "{mark:?}");
    // Set again by other code: a run of its own, from nothing.
    assert!(m.debug_write(0x2400_0100, &[0; 4]));
    assert_eq!(m.bus.last_write(0x2400_0100).unwrap().run, 0);
}

#[test]
fn a_multiply_s_two_halves_are_each_what_they_came_to() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x12345678
    str r1, [r3]
    ldr r1, =0x00abcdef
    str r1, [r3, #4]
    nop
begin:
    ldr r0, [r3]
    ldr r1, [r3, #4]
    smull r4, r5, r0, r1
    ldr r6, =0x24000200
    str r4, [r6]
    str r5, [r6, #4]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 5), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let product = 0x1234_5678i64 * 0x00ab_cdef;
    let made = |at: u32| p.node(p.node(p.memory[&at]).unwrap().parents[0]).unwrap().what;
    assert!(matches!(made(0x2400_0200), What::Merge { value: Some(v), .. } if v == product as u32), "{:?}", made(0x2400_0200));
    assert!(matches!(made(0x2400_0204), What::Merge { value: Some(v), .. } if v == (product >> 32) as u32), "{:?}", made(0x2400_0204));
}

/// Nothing a pointer: what says nothing of which, as a test has it.
fn pointers(node: &armv6m::provenance::Node) -> bool {
    match node.what {
        What::Held { addr, value, .. } => addr.wrapping_sub(node.pc) < 4096 || value >= 0x2000_0000,
        What::Register { n: 13 | 15, .. } => true,
        What::Register { value, .. } | What::Store { size: 4, value, .. } => value >= 0x2000_0000,
        _ => false,
    }
}

#[test]
fn an_index_stepped_longer_than_the_walk_looks_still_chose_its_entry() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000400      @ a table
    movs r1, #0              @ the index, from before
    ldr r5, =5000
    nop
begin:
1:  adds r1, #1
    subs r5, #1
    bne 1b
    ldr r2, [r3, r1, lsl #2]
    ldr r6, =0x24000200
    str r2, [r6]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let tree = p.derivation(p.memory[&0x2400_0200], 8, 64, &pointers);
    let mut text = String::new();
    derivation_text(p, &tree, "", 0, &mut text);
    assert!(text.contains("chose: "), "the index chose it, however long its way back:\n{text}");
}

#[test]
fn a_constant_followed_only_off_the_bus_is_what_its_register_held() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x12345678
    str r1, [r3]
    movs r1, #0xff           @ the mask, before following began
    nop
begin:
    ldr r0, [r3]
    ands r0, r1
    ldr r6, =0x24000200
    str r0, [r6]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 5), Outcome::Budget));
    let mut m = h753(source);
    m.follow_forward_from(probe.time, 0x2400_0100, 4);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let store = p.node(p.memory[&0x2400_0200]).unwrap();
    let made = p.node(store.parents[0]).unwrap();
    assert!(matches!(made.what, What::Merge { value: Some(0x78), constant: Some((1, 0xff)) }), "{made:?}");
}

#[test]
fn a_count_kept_in_a_register_and_spilled_is_folded_through_its_copies() {
    // t1++ round a loop, the count stored where it is kept and copied to
    // another place each time — a spill, an argument — before the next.
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    movs r1, #0
    str r1, [r3]
    nop
begin:
    movs r5, #6
1:  ldr r1, [r3, #4]         @ from the copy
    adds r1, #1
    str r1, [r3]             @ the count
    ldr r2, [r3]
    str r2, [r3, #4]         @ copied
    subs r5, #1
    bne 1b
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let tree = p.derivation(p.memory[&0x2400_0100], 24, 200, &pointers);
    let mut text = String::new();
    derivation_text(p, &tree, "", 0, &mut text);
    assert!(text.lines().next().unwrap().contains("repeated 5"), "folded once, at the top:\n{text}");
    assert_eq!(text.matches("repeated").count(), 1, "not again at each copy:\n{text}");
}

#[test]
fn bytes_put_together_say_which_bytes_each_part_was() {
    let source = ".thumb_func\nreset:
    ldr r3, =0x24000100
    ldr r1, =0x11223344
    str r1, [r3]
    nop
begin:
    ldr r6, =0x24000200
    movs r2, #0xaa
    strb r2, [r6, #1]        @ one byte of its own
    ldr r1, [r3]
    strh r1, [r6, #2]        @ two of another
    ldr r0, [r6]
    str r0, [r3, #8]
    bkpt #1
    .ltorg
";
    let mut probe = h753(source);
    assert!(matches!(probe.run_until(u64::MAX, 4), Outcome::Budget));
    let mut m = h753(source);
    m.follow_everything_from(probe.time);
    run(&mut m);
    let p = m.provenance.as_ref().unwrap();
    let tree = p.derivation(p.memory[&0x2400_0108], 8, 64, &pointers);
    let bytes = &tree.operands[0].1;
    let lanes: Vec<u8> = bytes.operands.iter().map(|(_, o)| o.lanes).collect();
    assert!(lanes.contains(&0b0010) && lanes.contains(&0b1100), "byte 1 one's, bytes 2 and 3 another's: {lanes:?}");
}
