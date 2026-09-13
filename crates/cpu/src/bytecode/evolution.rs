//! Opt-in runtime trace discovery. Compilation produces a successor module;
//! nothing installs code into the executing instance.
//!
//! All pointers here belong to one wasm32 linear memory and its function table.
//! The compiler must preserve that ABI when carrying a frozen continuation into
//! a successor. They are never transferable handles between arbitrary Blocks.

use super::{Leave, Trace};
use crate::{space::Space, state::Tcb};
use std::collections::BTreeMap;

type Runner =
    unsafe extern "C" fn(*mut Leave, *const u64, u32, *const u64, *mut Tcb, *mut Space, u64);

struct Binding {
    verified_identity: u64,
    code: Vec<u64>,
    ip: Vec<u64>,
    visits: u64,
    retired: u64,
    compiled: Option<Runner>,
}

#[repr(C)]
pub struct Request {
    next: *mut Request,
    prev: *mut Request,
    id: u32,
    globals: u32,
    function: Runner,
    arguments: *const u8,
    length: u32,
    destination: *mut Option<Runner>,
}

static mut BINDINGS: BTreeMap<u64, Box<Binding>> = BTreeMap::new();
static mut HEAD: *mut Request = core::ptr::null_mut();
static mut WEVALED: u8 = 0;
static mut PREPARED: bool = false;
static mut COMPILED_RETIRED: u64 = 0;
static mut NEXT_IDENTITY: u64 = 1;
static mut GENERATION: u64 = 1;
#[derive(Clone, Copy)]
struct Cached {
    identity: u64,
    generation: u64,
    binding: *mut Binding,
}
static mut FAST: [Cached; 64] = [Cached {
    identity: 0,
    generation: 0,
    binding: core::ptr::null_mut(),
}; 64];

pub(super) fn next_identity() -> u64 {
    unsafe {
        let id = NEXT_IDENTITY;
        NEXT_IDENTITY = id.checked_add(1).expect("trace identity exhausted");
        id
    }
}

#[unsafe(export_name = "weval.pending.head")]
pub extern "C" fn head() -> *mut *mut Request {
    &raw mut HEAD
}
#[unsafe(export_name = "weval.is.wevaled")]
pub extern "C" fn specialized() -> *mut u8 {
    &raw mut WEVALED
}
#[unsafe(export_name = "weval.func.0")]
pub extern "C" fn target() -> Runner {
    generic
}
#[unsafe(no_mangle)]
pub extern "C" fn zaqaru_compiled_retired() -> u64 {
    unsafe { COMPILED_RETIRED }
}

unsafe extern "C" fn generic(
    out: *mut Leave,
    code: *const u64,
    len: u32,
    ip: *const u64,
    tcb: *mut Tcb,
    space: *mut Space,
    budget: u64,
) {
    unsafe {
        out.write(super::specialize::run(
            core::slice::from_raw_parts(code, len as usize),
            core::slice::from_raw_parts(ip, len as usize),
            &mut *tcb,
            &mut *space,
            budget,
        ));
    }
}

/// Single-threaded guest entry, just like the engine's process table.
fn lookup(trace: &Trace) -> *mut Binding {
    let cached = unsafe { (&*(&raw const FAST))[trace.identity as usize & 63] };
    if cached.identity == trace.identity && cached.generation == unsafe { GENERATION } {
        return cached.binding;
    }
    let bindings = unsafe { &mut *(&raw mut BINDINGS) };
    // Bound discovery storage. This is a first generation, not an unbounded JIT
    // cache. Unselected traces retain the ordinary interpreter path.
    if !bindings.contains_key(&trace.entry) {
        if trace.code.len() > 8192 || unsafe { PREPARED } {
            return core::ptr::null_mut();
        }
        if bindings.len() >= 256 {
            // Startup must not permanently crowd application traces out of
            // the profile. Evict the least observed candidate before freeze.
            let cold = *bindings.iter().min_by_key(|(_, b)| (b.retired, b.visits)).unwrap().0;
            bindings.remove(&cold);
        }
        unsafe {
            GENERATION = GENERATION
                .checked_add(1)
                .expect("binding generation exhausted");
        }
        bindings.insert(
            trace.entry,
            Box::new(Binding {
                verified_identity: trace.identity,
                code: trace.code.clone(),
                ip: trace.ip.clone(),
                visits: 0,
                retired: 0,
                compiled: None,
            }),
        );
    }
    let binding = bindings.get_mut(&trace.entry).unwrap();
    // A virtual address is not a code identity: different processes and code
    // writes can reuse it. Compare both instruction words and fault/resume IPs.
    if binding.verified_identity != trace.identity {
        if binding.code != trace.code || binding.ip != trace.ip {
            return core::ptr::null_mut();
        }
        // The engine's cached traces are immutable. A newly decoded trace gets
        // a fresh identity even when its allocation/address has been reused.
        binding.verified_identity = trace.identity;
    }
    let binding = &mut **binding as *mut Binding;
    unsafe {
        (&mut *(&raw mut FAST))[trace.identity as usize & 63] = Cached {
            identity: trace.identity,
            generation: GENERATION,
            binding,
        };
    }
    binding
}

pub(super) fn observe(trace: &Trace) -> bool {
    let Some(binding) = (unsafe { lookup(trace).as_mut() }) else {
        return false;
    };
    binding.visits = binding.visits.saturating_add(1);
    binding.compiled.is_some()
}

pub(super) fn record_retired(trace: &Trace, retired: u64) {
    if unsafe { PREPARED } { return; }
    if let Some(binding) = unsafe { lookup(trace).as_mut() } {
        binding.retired = binding.retired.saturating_add(retired);
    }
}

pub(super) fn dispatch(
    trace: &Trace,
    tcb: &mut Tcb,
    space: &mut Space,
    budget: u64,
) -> Option<Leave> {
    let binding = unsafe { lookup(trace).as_mut() }?;
    binding.visits = binding.visits.saturating_add(1);
    let runner = binding.compiled?;
    let before = tcb.retired;
    let mut result = core::mem::MaybeUninit::uninit();
    unsafe {
        runner(
            result.as_mut_ptr(),
            binding.code.as_ptr(),
            binding.code.len() as u32,
            binding.ip.as_ptr(),
            tcb,
            space,
            budget,
        );
        COMPILED_RETIRED += tcb.retired - before;
        Some(result.assume_init())
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

/// Called only while frozen, once per lineage in this experiment. Requests and
/// argument buffers deliberately live as long as the module's continuation.
pub fn prepare() -> u32 {
    if unsafe { PREPARED } {
        return 0;
    }
    unsafe {
        PREPARED = true;
    }
    let bindings = unsafe { &mut *(&raw mut BINDINGS) };
    let mut hot: Vec<_> = bindings.values_mut().filter(|b| b.visits >= 4).collect();
    hot.sort_by_key(|b| core::cmp::Reverse((b.retired, b.visits)));
    let mut count = 0;
    for binding in hot.into_iter().take(32) {
        let mut args = Vec::new();
        arg(&mut args, 0, 255, 0); // dynamic result destination
        buffer(&mut args, &binding.code);
        arg(&mut args, 1, 0, binding.code.len() as u64);
        buffer(&mut args, &binding.ip);
        for _ in 0..3 {
            arg(&mut args, 0, 255, 0);
        }
        let length = args.len() as u32;
        let arguments = Box::leak(args.into_boxed_slice()).as_ptr();
        unsafe {
            let request = Box::into_raw(Box::new(Request {
                next: HEAD,
                prev: core::ptr::null_mut(),
                id: count,
                globals: 0,
                function: generic,
                arguments,
                length,
                destination: &mut binding.compiled,
            }));
            if !HEAD.is_null() {
                (*HEAD).prev = request;
            }
            HEAD = request;
        }
        count += 1;
    }
    count
}
