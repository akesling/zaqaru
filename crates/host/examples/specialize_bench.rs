//! Runs the manual weval experiment, comparing complete state before timing.
use std::time::Instant;
use wasmtime::{Engine, ExternType, Linker, Module, Store, TypedFunc};

struct Harness {
    store: Store<()>,
    memory: wasmtime::Memory,
    run: TypedFunc<(u32, u32, u64, u64, u32), u32>,
    ptr: TypedFunc<(), u32>,
    len: TypedFunc<(), u32>,
}
impl Harness {
    fn new(path: &str) -> wasmtime::Result<Self> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path)?;
        let mut linker = Linker::new(&engine);
        for import in module.imports() {
            if let ExternType::Func(ty) = import.ty() {
                let name = format!("{}::{}", import.module(), import.name());
                linker.func_new(import.module(), import.name(), ty, move |_, _, _| {
                    wasmtime::bail!("specialization left a reachable intrinsic: {name}")
                })?;
            }
        }
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &module)?;
        Ok(Self {
            memory: instance.get_memory(&mut store, "memory").unwrap(),
            run: instance.get_typed_func(&mut store, "run")?,
            ptr: instance.get_typed_func(&mut store, "state_ptr")?,
            len: instance.get_typed_func(&mut store, "state_len")?,
            store,
        })
    }
    fn execute(
        &mut self,
        mode: u32,
        fixture: u32,
        iterations: u64,
        budget: u64,
        scenario: u32,
    ) -> wasmtime::Result<(f64, u32, Vec<u8>)> {
        let start = Instant::now();
        let result = self.run.call(
            &mut self.store,
            (mode, fixture, iterations, budget, scenario),
        )?;
        let seconds = start.elapsed().as_secs_f64();
        let ptr = self.ptr.call(&mut self.store, ())? as usize;
        let len = self.len.call(&mut self.store, ())? as usize;
        Ok((
            seconds,
            result,
            self.memory.data(&self.store)[ptr..ptr + len].to_vec(),
        ))
    }
}

fn main() -> wasmtime::Result<()> {
    let path = std::env::args().nth(1).expect("specialized wasm path");
    let baseline = std::env::args().nth(2).expect("reference wasm path");
    let iterations: u64 = std::env::args()
        .nth(3)
        .map(|s| s.parse().unwrap())
        .unwrap_or(2_000_000);
    let mut h = Harness::new(&path)?;
    let mut original = Harness::new(&baseline)?;
    let names = [
        "arithmetic",
        "checked_memory",
        "flags_widths",
        "multiply_shift",
    ];
    for (fixture, name) in names.iter().enumerate() {
        for scenario in 0..5 {
            for iterations in [1, 2, 71, 1024] {
                for budget in [0, 1, 2, 5, 17, 100_000, u64::MAX] {
                    let (_, kind_a, a) =
                        original.execute(0, fixture as u32, iterations, budget, scenario)?;
                    let (_, kind_b, b) =
                        h.execute(1, fixture as u32, iterations, budget, scenario)?;
                    assert_eq!(
                        (kind_a, a),
                        (kind_b, b),
                        "{name}, scenario {scenario}, budget {budget}"
                    );
                }
            }
        }
    }
    eprintln!(
        "560 state comparisons passed (registers, flags, RIP, retirement, memory, faults, dirty code)."
    );
    println!("fixture,repeat,baseline_seconds,specialized_seconds,speedup");
    for (fixture, name) in names.iter().enumerate() {
        for repeat in 0..5 {
            let mut results = [0.; 2];
            let mut reference = None;
            for mode in if repeat % 2 == 0 { [0, 1] } else { [1, 0] } {
                let (seconds, kind, state) = if mode == 0 {
                    original.execute(0, fixture as u32, iterations, u64::MAX, 0)?
                } else {
                    h.execute(1, fixture as u32, iterations, u64::MAX, 0)?
                };
                assert_eq!(kind, 2, "loop must finish at syscall deferral");
                let counter = u64::from_le_bytes(state[8..16].try_into().unwrap());
                let retired = u64::from_le_bytes(state[136..144].try_into().unwrap());
                assert_eq!(counter, 0, "loop exited early");
                assert_eq!(
                    retired,
                    iterations * [5, 8, 7, 6][fixture],
                    "incorrect amount of work"
                );
                if let Some(ref previous) = reference {
                    assert_eq!(previous, &(kind, state));
                } else {
                    reference = Some((kind, state));
                }
                results[mode as usize] = seconds;
            }
            println!(
                "{name},{repeat},{:.6},{:.6},{:.3}",
                results[0],
                results[1],
                results[0] / results[1]
            );
        }
    }
    Ok(())
}
