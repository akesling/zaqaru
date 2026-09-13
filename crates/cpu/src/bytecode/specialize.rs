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
    pub(super) fn read_reg(index: u64) -> u64;
    #[link_name = "write.reg"]
    pub(super) fn write_reg(index: u64, value: u64);
}

/// Specialize one trace while retaining the existing interpreter semantics.
#[inline(always)]
pub fn run(code: &[u64], ip: &[u64], tcb: &mut Tcb, space: &mut Space, budget: u64) -> Leave {
    let empty = Trace {
        entry: 0,
        code: Vec::new(),
        ip: Vec::new(),
    };
    run_inner::<true>(&empty, 0, tcb, space, budget, Resolver::Runloop, code, ip)
}
