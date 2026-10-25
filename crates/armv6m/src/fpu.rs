//! The floating-point unit: coprocessors 10 and 11.
//!
//! Not yet modelled. Every coprocessor instruction is the UsageFault a
//! part without an FPU raises for it.

use crate::{Fault, Machine};

/// Executes a coprocessor instruction; false if it is not one this FPU
/// has.
pub(crate) fn execute(machine: &mut Machine, hw1: u32, hw2: u32) -> Result<bool, Fault> {
    let _ = machine;
    Err(Fault::NoCoprocessor { instr: hw1 << 16 | hw2 })
}
