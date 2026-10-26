//! The trace (`armv6m::trace`): what crosses an H7's pins and buses, as a
//! probe on them sees it.

use armv6m::devices::Wiring;
use armv6m::stm32h7::Stm32h7;
use armv6m::trace::{I2cEvent, Signal};

const I2C2: u32 = 0x4000_5800;

#[test]
fn a_pin_driven_from_outside_is_recorded() {
    let mut soc = Stm32h7::default();
    soc.trace.on = true;
    soc.drive(2, 13, Some(true), 1000);
    soc.drive(2, 13, Some(false), 2000);
    let pins: Vec<_> = soc.trace.records.iter().filter(|r| r.signal.channel() == "PC13").collect();
    assert_eq!(pins.len(), 2);
    assert_eq!((pins[0].time, &pins[0].signal), (1000, &Signal::Pin { port: 2, pin: 13, high: true }));
}

#[test]
fn an_i2c_register_read_is_recorded_as_the_wire_carries_it() {
    let mut soc = Stm32h7::default();
    let mut flash = vec![0xFF; 2 << 20];
    soc.attach(Wiring::parse("i2c I2C2 0x48 tmp117").unwrap()).unwrap();
    soc.trace.on = true;
    let mut now = 0;
    let mut step = |soc: &mut Stm32h7, offset: u32, value: u32, now: &mut u64| {
        soc.write(I2C2 + offset, value, 4, *now, &mut flash);
        // Let the bus finish whatever it started.
        while let Some(due) = soc.due() {
            *now = due;
            soc.advance(due);
        }
    };
    // PE; a write of the pointer, 0x0F; then a read of two bytes, AUTOEND.
    step(&mut soc, 0x00, 1, &mut now);
    step(&mut soc, 0x04, 0x48 << 1 | 1 << 16 | 1 << 13, &mut now);
    step(&mut soc, 0x28, 0x0F, &mut now);
    step(&mut soc, 0x04, 0x48 << 1 | 1 << 10 | 2 << 16 | 1 << 25 | 1 << 13, &mut now);
    for _ in 0..2 {
        soc.read(I2C2 + 0x24, 4, now);
        while let Some(due) = soc.due() {
            now = due;
            soc.advance(due);
        }
    }
    let events: Vec<I2cEvent> = soc
        .trace
        .records
        .iter()
        .filter_map(|r| match r.signal {
            Signal::I2c { controller: 1, event } => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        [
            I2cEvent::Start { address: 0x48, read: false, ack: true },
            I2cEvent::Byte { byte: 0x0F },
            I2cEvent::Start { address: 0x48, read: true, ack: true },
            I2cEvent::Byte { byte: 0x01 },
            I2cEvent::Byte { byte: 0x17 },
            I2cEvent::Stop,
        ]
    );
    let times: Vec<u64> = soc.trace.records.iter().map(|r| r.time).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "in order: {times:?}");
}

#[test]
fn nothing_is_recorded_until_the_trace_is_on() {
    let mut soc = Stm32h7::default();
    soc.drive(2, 13, Some(true), 1000);
    assert!(soc.trace.records.is_empty());
}
