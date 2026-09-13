//! Manual, single-trace partial-evaluation experiment. Not a container tier.
#![cfg(target_arch = "wasm32")]

use cpu::bytecode::{self, Leave, Trace};
use cpu::space::{Protection, Space};
use cpu::state::{Tcb, Width};
use iced_x86::code_asm::*;

type Runner = unsafe extern "C" fn(*const u64, u32, *const u64, *mut Tcb, *mut Space, u64) -> u32;

#[repr(C)]
pub struct Request {
    next: *mut Request,
    prev: *mut Request,
    id: u32,
    globals: u32,
    function: Runner,
    arguments: *const u8,
    length: u32,
    destination: *mut Runner,
}

struct Fixture {
    trace: Trace,
    compiled: Runner,
}
static mut FIXTURES: Vec<Fixture> = Vec::new();
static mut HEAD: *mut Request = core::ptr::null_mut();
static mut SPECIALIZED: u8 = 0;
static mut STATE: Vec<u8> = Vec::new();
static mut FAULT_ADDRESS: u64 = 0;
static mut FAULT_ACCESS: u32 = 0;
static mut RESUME: usize = 0;

#[unsafe(export_name = "weval.pending.head")]
pub extern "C" fn head() -> *mut *mut Request {
    &raw mut HEAD
}
#[unsafe(export_name = "weval.is.wevaled")]
pub extern "C" fn specialized() -> *mut u8 {
    &raw mut SPECIALIZED
}
#[unsafe(export_name = "weval.func.0")]
pub extern "C" fn target() -> Runner {
    generic
}

unsafe extern "C" fn generic(
    code: *const u64,
    len: u32,
    ip: *const u64,
    tcb: *mut Tcb,
    space: *mut Space,
    budget: u64,
) -> u32 {
    let leave = unsafe {
        bytecode::specialize::run(
            core::slice::from_raw_parts(code, len as usize),
            core::slice::from_raw_parts(ip, len as usize),
            &mut *tcb,
            &mut *space,
            budget,
        )
    };
    leave_code(leave)
}

fn leave_code(leave: Leave) -> u32 {
    match leave {
        Leave::Exit => 0,
        Leave::Preempted => 1,
        Leave::Defer { resume } => {
            unsafe {
                RESUME = resume;
            }
            2
        }
        Leave::Fault(fault) => {
            unsafe {
                FAULT_ADDRESS = fault.address;
                FAULT_ACCESS = fault.access as u32;
            }
            3
        }
    }
}

fn arg(args: &mut Vec<u8>, known: u32, ty: u32, raw: u64) {
    args.extend(known.to_le_bytes());
    args.extend(ty.to_le_bytes());
    args.extend(raw.to_le_bytes());
}
fn buffer(args: &mut Vec<u8>, words: &[u64]) {
    let len = words.len() as u64 * 8;
    arg(args, 1, 4, len | (len << 32));
    for word in words {
        args.extend(word.to_le_bytes());
    }
}

#[unsafe(export_name = "wizer-initialize")]
pub extern "C" fn initialize() {
    let mut fixtures = Vec::new();
    for kind in 0..4 {
        let mut a = CodeAssembler::new(64).unwrap();
        let mut top = a.create_label();
        a.set_label(&mut top).unwrap();
        match kind {
            0 => {
                a.add(rax, r8).unwrap();
                a.rol(rax, 13).unwrap();
                a.xor(rax, r9).unwrap();
            }
            1 => {
                a.mov(r8, rcx).unwrap();
                a.and(r8d, 0x3ff8u32).unwrap();
                a.mov(r9, qword_ptr(rsi + r8)).unwrap();
                a.xor(rax, r9).unwrap();
                a.add(r9, rax).unwrap();
                a.mov(qword_ptr(rsi + r8), r9).unwrap();
            }
            2 => {
                a.cmp(rax, r8).unwrap();
                a.cmova(rax, r9).unwrap();
                a.add(ax, 17).unwrap();
                a.adc(r8, r9).unwrap();
                a.xor(al, 31).unwrap();
            }
            _ => {
                a.imul_3(rax, rax, 31).unwrap();
                a.xor(rax, r8).unwrap();
                a.shr(r8, 3).unwrap();
                a.add(r8, rax).unwrap();
            }
        }
        a.dec(rcx).unwrap();
        a.jnz(top).unwrap();
        a.syscall().unwrap();
        let entry = 0x1000;
        let bytes = a.assemble(entry).unwrap();
        let instructions: Vec<_> =
            iced_x86::Decoder::with_ip(64, &bytes, entry, iced_x86::DecoderOptions::NONE)
                .into_iter()
                .collect();
        let block = cpu::block::Block {
            entry,
            end: entry + bytes.len() as u64,
            simple: false,
            quick: instructions.iter().map(cpu::quick::Quick::lower).collect(),
            instructions,
            trace: None,
        };
        fixtures.push(Fixture {
            trace: bytecode::transpile(&block).unwrap(),
            compiled: generic,
        });
    }
    unsafe {
        FIXTURES = fixtures;
        for (id, fixture) in (&mut *(&raw mut FIXTURES)).iter_mut().enumerate() {
            let mut args = Vec::new();
            buffer(&mut args, &fixture.trace.code);
            arg(&mut args, 1, 0, fixture.trace.code.len() as u64);
            buffer(&mut args, &fixture.trace.ip);
            for _ in 0..3 {
                arg(&mut args, 0, 255, 0);
            }
            let length = args.len() as u32;
            let arguments = Box::leak(args.into_boxed_slice()).as_ptr();
            let request = Box::into_raw(Box::new(Request {
                next: HEAD,
                prev: core::ptr::null_mut(),
                id: id as u32,
                globals: 0,
                function: generic,
                arguments,
                length,
                destination: &mut fixture.compiled,
            }));
            if !HEAD.is_null() {
                (*HEAD).prev = request;
            }
            HEAD = request;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run(mode: u32, fixture: u32, iterations: u64, budget: u64, scenario: u32) -> u32 {
    unsafe {
        FAULT_ADDRESS = 0;
        FAULT_ACCESS = 0;
        RESUME = 0;
    }
    // Separate page-aligned guest storage, with actual Space permission checks.
    let mut memory = vec![0u64; 4096];
    let base = (memory.as_mut_ptr() as u64 + 4095) & !4095;
    let mut space = Space::new(base + 16384);
    space.protect(base, 16384, Protection::READ_WRITE);
    for i in 0..2048 {
        space
            .store(
                base + i * 8,
                Width::Qword,
                i.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(1),
            )
            .unwrap();
    }
    let mut tcb = Tcb::new();
    tcb.rip = 0x1000;
    tcb.registers[0] = 7;
    tcb.registers[1] = iterations;
    tcb.registers[6] = base;
    tcb.registers[8] = 37;
    tcb.registers[9] = 71;
    tcb.flags.set_all(iterations & 0x8d5);
    if scenario == 1 {
        space.protect(base, 16384, Protection::READ);
    }
    if scenario == 2 {
        tcb.registers[6] = base + 16380;
    }
    if scenario == 3 {
        for page in base / 4096..base / 4096 + 4 {
            space.mark_code(page);
        }
    }
    if scenario == 4 {
        tcb.registers[6] = base + 4092 - (iterations & 0x3ff8);
        space.protect(base + 4096, 4096, Protection::READ);
    }
    let fixture = unsafe { &(&*(&raw const FIXTURES))[fixture as usize] };
    let result = if mode == 0 {
        leave_code(bytecode::run(
            &fixture.trace,
            0,
            &mut tcb,
            &mut space,
            budget,
            bytecode::Resolver::Runloop,
        ))
    } else {
        assert_ne!(unsafe { SPECIALIZED }, 0, "run weval before measuring");
        unsafe {
            (fixture.compiled)(
                fixture.trace.code.as_ptr(),
                fixture.trace.code.len() as u32,
                fixture.trace.ip.as_ptr(),
                &mut tcb,
                &mut space,
                budget,
            )
        }
    };
    // Serialize every architectural GPR, flags, RIP, retirement and guest byte.
    // Normalize the dynamically allocated buffer pointer for cross-run comparison.
    tcb.registers[6] -= base;
    let mut state = Vec::new();
    for reg in tcb.registers {
        state.extend(reg.to_le_bytes());
    }
    for value in [tcb.rip, tcb.retired, tcb.flags.status()] {
        state.extend(value.to_le_bytes());
    }
    unsafe {
        state.extend(
            if result == 3 {
                FAULT_ADDRESS.wrapping_sub(base)
            } else {
                0
            }
            .to_le_bytes(),
        );
        state.extend(FAULT_ACCESS.to_le_bytes());
        state.extend((RESUME as u64).to_le_bytes());
    }
    let dirty = space.take_dirty_code();
    state.extend((dirty.len() as u32).to_le_bytes());
    for page in dirty {
        state.extend((page - base / 4096).to_le_bytes());
    }
    let mut bytes = vec![0; 16384];
    space.read(base, &mut bytes).unwrap();
    state.extend(bytes);
    unsafe {
        STATE = state;
    }
    result
}

#[unsafe(no_mangle)]
pub extern "C" fn state_ptr() -> *const u8 {
    unsafe { (&*(&raw const STATE)).as_ptr() }
}
#[unsafe(no_mangle)]
pub extern "C" fn state_len() -> usize {
    unsafe { (&*(&raw const STATE)).len() }
}
