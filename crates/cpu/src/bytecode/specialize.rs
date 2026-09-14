//! Experimental entry point for weval. The normal engine never calls it.
//! Code and IP buffers are specialization inputs; all machine state is dynamic.

use super::{Leave, Resolver, Trace, run_inner};
use crate::{space::Space, state::Tcb};

#[link(wasm_import_module = "weval")]
unsafe extern "C" {
    #[link_name = "push.context"]
    pub(super) fn push_context(pc: u32);
    #[link_name = "update.context"]
    pub(super) fn update_context(pc: u32);
    #[link_name = "pop.context"]
    pub(super) fn pop_context();
    #[link_name = "read.reg"]
    pub(crate) fn read_reg(index: u64) -> u64;
    #[link_name = "write.reg"]
    pub(crate) fn write_reg(index: u64, value: u64);
}

/// Specialize one trace while retaining the existing interpreter semantics.
#[inline(always)]
pub fn run(code: &[u64], ip: &[u64], tcb: &mut Tcb, space: &mut Space, budget: u64) -> Leave {
    execute(code, ip, 0, Resolver::Runloop, tcb, space, budget)
}

#[inline(always)]
pub fn run_region(
    code: &[u64],
    ip: &[u64],
    tcb: &mut Tcb,
    space: &mut Space,
    budget: u64,
) -> Leave {
    let start = code[0] as usize;
    execute(
        code,
        ip,
        start,
        Resolver::Region(&code[1..start]),
        tcb,
        space,
        budget,
    )
}

#[inline(always)]
fn execute(
    code: &[u64],
    ip: &[u64],
    start: usize,
    resolver: Resolver<'_>,
    tcb: &mut Tcb,
    space: &mut Space,
    budget: u64,
) -> Leave {
    let empty = Trace {
        #[cfg(feature = "evolution")]
        identity: 0,
        entry: 0,
        code: Vec::new(),
        ip: Vec::new(),
    };
    run_inner::<true>(&empty, start, tcb, space, budget, resolver, code, ip)
}
