//! A Hubris archive read for what the board's memory is (`armv6m::hubris`),
//! against the bench's own H753 build when it has been built
//! (`demo/hubris/build.sh`); skipped when it has not.

use armv6m::hubris::Image;

fn h753() -> Option<Image> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/demo/hubris-build/bench/archives/h753.zip");
    let archive = std::fs::read(path).ok()?;
    Some(Image::read(&archive).expect("the bench's archive reads"))
}

#[test]
fn a_task_s_stack_is_below_its_data_and_its_symbols_are_named() {
    let Some(image) = h753() else { return };
    assert_eq!(image.name, "demo-stm32h753-nucleo");
    let i2c = image.owner("i2c_driver").expect("an I2C driver task");
    // `stacksize = 1048` in app.toml: 0x418 bytes, ending where its .bss begins.
    let (base, size) = i2c.stack.expect("its stack");
    assert_eq!(size, 1048);
    let bss = i2c.sections.iter().find(|s| s.name == ".bss").expect("its .bss");
    assert_eq!(base + size, bss.base);
    let place = image.whatis(base + 16).expect("in its stack");
    assert_eq!((place.owner.name.as_str(), place.part.as_str()), ("i2c_driver", "stack"));
    // Its entry point is code, and named.
    let main = i2c.symbols.iter().find(|s| s.function && s.name.ends_with("main")).expect("a main");
    let place = image.whatis(main.base + 2).expect("in its code");
    assert_eq!(place.part, ".text");
    assert_eq!(place.symbol.map(|(s, offset)| (s.name.as_str(), offset)), Some((main.name.as_str(), 2)));
}

#[test]
fn every_task_is_listed_in_the_kernel_s_order() {
    let Some(image) = h753() else { return };
    assert_eq!(image.tasks.first().map(|t| t.name.as_str()), Some("jefe"));
    assert!(image.tasks.iter().enumerate().all(|(n, t)| t.index == Some(n)));
    assert!(image.kernel.sections.iter().any(|s| s.name == ".text"));
    // The kernel's linker script says both ends of its stack.
    assert_eq!(image.kernel.stack, Some((0x2400_0000, 0x400)));
    assert_eq!(image.whatis(0x2400_0010).map(|p| (p.owner.name.clone(), p.part)), Some(("kernel".to_string(), "stack".to_string())));
}
