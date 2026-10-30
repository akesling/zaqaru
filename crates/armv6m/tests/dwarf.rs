//! The DWARF in a Hubris archive's ELFs, read (`armv6m::dwarf`): types,
//! statics, source lines, and every task's stack stepped back from what the
//! kernel saved of it, against the bench's own H753 build when it has been
//! built (`demo/hubris/build.sh`); skipped when it has not.

use armv6m::dwarf::{Debug, Type};
use armv6m::hubris::{Image, TaskTable};
use armv6m::part::Part;
use armv6m::{Machine, zip};

fn archive() -> Option<Vec<u8>> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/demo/hubris-build/bench/archives/h753.zip")).ok()
}

fn debug(archive: &[u8], owner: &str) -> Debug {
    let name = if owner == "kernel" { "elf/kernel".to_string() } else { format!("elf/task/{owner}") };
    Debug::read(&zip::entry(archive, &name).expect("the ELF")).expect("its DWARF")
}

#[test]
fn the_kernel_s_task_table_is_typed_down_to_each_task_s_saved_registers() {
    let Some(archive) = archive() else { return };
    let kernel = debug(&archive, "kernel");
    let space = kernel.static_named("HUBRIS_TASK_TABLE_SPACE").expect("the task table");
    let Some(Type::Array { element, count, stride, name }) = kernel.ty(space.ty) else { panic!("an array") };
    assert_eq!((count, name.as_str()), (24, "[MaybeUninit<kern::task::Task>; 24]"));
    assert_eq!(space.size, count * stride);
    assert_eq!(kernel.ty(element).map(|t| t.name().to_string()).as_deref(), Some("MaybeUninit<kern::task::Task>"));
    // A Rust enum with a niche: `Healthy` says so with a value in its
    // discriminant's bytes, and `Faulted` is what any other value means.
    let table = TaskTable::of(&kernel).expect("found by type");
    assert_eq!((table.base, table.stride, table.count), (space.address, stride, count));
    // A static the compiler put inside a larger allocation:
    // `DW_OP_addr; DW_OP_plus_uconst`.
    assert!(kernel.static_named("CURRENT_TASK_PTR").is_some());
}

#[test]
fn an_enum_s_variants_are_told_apart_by_its_discriminant() {
    let Some(archive) = archive() else { return };
    let kernel = debug(&archive, "kernel");
    let space = kernel.static_named("HUBRIS_TASK_TABLE_SPACE").unwrap();
    let Some(Type::Array { element, .. }) = kernel.ty(space.ty) else { panic!() };
    // Through MaybeUninit, ManuallyDrop and MaybeDangling to the Task.
    let mut task = element;
    while kernel.field(task, "state").is_none() {
        let Some(Type::Struct { fields, .. }) = kernel.ty(kernel.resolve(task)) else { panic!("a wrapper") };
        task = fields.iter().max_by_key(|f| kernel.size(f.ty)).unwrap().ty;
    }
    let (_, state) = kernel.field(task, "state").unwrap();
    let Some(Type::Struct { name, variants: Some(variants), .. }) = kernel.ty(state) else { panic!("an enum") };
    assert_eq!(name, "TaskState");
    let names: Vec<(Option<u64>, &str)> = variants.variants.iter().map(|v| (v.value, v.name.as_str())).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&(None, "Faulted")), "{names:?}");
    assert!(names.iter().any(|&(value, name)| name == "Healthy" && value.is_some()), "{names:?}");
    assert!(variants.discriminant.is_some());
}

#[test]
fn every_blocked_task_steps_back_through_its_system_call_into_its_main() {
    let Some(archive) = archive() else { return };
    let image = Image::read(&archive).unwrap();
    let mut m = Machine::new(Part::named("stm32h753").unwrap(), &zip::entry(&archive, "img/final.bin").unwrap()).unwrap();
    // 200 ms: every task started and waiting.
    m.run_until(200_000_000_000, u64::MAX);
    let kernel = debug(&archive, "kernel");
    let table = TaskTable::of(&kernel).unwrap();
    let word = |at: u32| m.bus.peek(at, 4).ok();
    let byte = |at: u32| m.bus.peek(at, 1).ok().map(|b| b as u8);
    let current = table.current(&word).expect("a task running");
    assert_eq!(image.tasks[current].name, "idle", "nothing else to do");
    let mut seen = 0;
    for task in image.tasks.iter().filter(|t| t.index != Some(current)) {
        let Some(regs) = table.registers(task.index.unwrap(), &word) else { continue };
        let d = debug(&archive, &task.name);
        let frames = d.unwind(regs, &word, 32);
        // The stub the system call went through has no call frame
        // information; its pushes say how to step past it.
        assert!(frames.len() >= 2, "{}: {frames:?}", task.name);
        let caller = d.lines(frames[1].pc - 1);
        assert!(caller.iter().any(|l| l.function.as_deref().is_some_and(|f| f.contains("sys_recv"))), "{}: {caller:?}", task.name);
        // On, a frame or two, to its main.
        let outermost: Vec<_> = frames[1..].iter().filter_map(|f| d.lines(f.pc - 1).last()?.function.clone()).collect();
        assert!(outermost.iter().any(|f| f == "main"), "{}: {outermost:?}", task.name);
        // And the frame's variables, some of them in memory and read.
        let variables = d.variables(&frames[1], true, &byte);
        assert!(variables.iter().any(|v| v.address.is_some() && v.bytes.iter().all(Option::is_some)), "{}", task.name);
        seen += 1;
    }
    assert!(seen > 15, "{seen} tasks");
}

#[test]
fn an_instruction_s_line_names_its_file_and_function() {
    let Some(archive) = archive() else { return };
    let image = Image::read(&archive).unwrap();
    let i2c = image.owner("i2c_driver").unwrap();
    let main = i2c.symbols.iter().find(|s| s.function && s.name.ends_with("main")).unwrap();
    let d = debug(&archive, "i2c_driver");
    let lines = d.lines(main.base);
    let outer = lines.last().expect("a line");
    assert_eq!(outer.function.as_deref(), Some("main"));
    assert_eq!(outer.file.as_deref(), Some("/hubris/drv/stm32xx-i2c-server/src/main.rs"));
    assert!(d.files().iter().any(|f| f == "/hubris/drv/stm32xx-i2c-server/src/main.rs"));
}

#[test]
fn an_address_is_named_down_its_static_s_type() {
    let Some(archive) = archive() else { return };
    let kernel = debug(&archive, "kernel");
    let table = TaskTable::of(&kernel).expect("the task table");
    let name = kernel.place_of(table.base + 3 * table.stride + 8).expect("inside the table");
    assert!(name.starts_with("HUBRIS_TASK_TABLE_SPACE[3]."), "a field of the fourth task, not MaybeUninit's: {name}");
    assert!(!name.contains("value"), "{name}");
    let space = kernel.static_named("HUBRIS_TASK_TABLE_SPACE").unwrap();
    assert_eq!(kernel.place_of(space.address + 1).as_deref().map(|n| n.starts_with("HUBRIS_TASK_TABLE_SPACE[0]")), Some(true));
    assert!(kernel.place_of(0x1000).is_none(), "nothing there");
}

#[test]
fn every_function_static_and_type_is_found_where_it_was_declared() {
    use armv6m::dwarf::Kind;
    let Some(archive) = archive() else { return };
    let kernel = debug(&archive, "kernel");
    let all = kernel.definitions();
    let find = |name: &str, kind: Kind| all.iter().find(|d| d.name == name && d.kind == kind && !d.declaration).unwrap_or_else(|| panic!("{name}"));
    // Generated by the build: where it was generated to.
    let table = find("HUBRIS_TASK_TABLE_SPACE", Kind::Static);
    assert!(table.file.starts_with("/hubris/build/kern-") && table.file.ends_with("/out/kconfig.rs"), "{table:?}");
    let ticks = find("TICKS", Kind::Static);
    assert_eq!((ticks.file.as_str(), ticks.path.as_str()), ("/hubris/sys/kern/src/arch/arm_m.rs", "kern::arch::arm_m::TICKS"));
    // Rust's types rustc gives no place: the kernel's index finds them in
    // the source.
    assert!(!all.iter().any(|d| d.name == "Task" && d.kind == Kind::Type));
    // A method, in its type.
    let recv = find("as_recv_args", Kind::Function);
    assert_eq!(recv.path, "kern::task::ArchState::as_recv_args");
    let safe_copy = find("safe_copy", Kind::Function);
    assert!(safe_copy.file.ends_with("kern/src/umem.rs") && safe_copy.line > 0, "{safe_copy:?}");
    // Each once, however many instances of it there are.
    assert_eq!(all.iter().filter(|d| d.name == "safe_copy" && d.kind == Kind::Function).count(), 1);
}
