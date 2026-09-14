//! Virtual storage only: all flag semantics remain in the existing Flags methods.
//! Slots 0..31 belong to GPRs/scratches; 32..38 hold the lazy flags record.
use super::{Flags, Rule, Width};
use crate::bytecode::specialize::{read_reg, write_reg};

impl Flags {
    #[inline(always)]
    pub(crate) fn save_virtual(&self) {
        unsafe {
            write_reg(32, self.rule as u64);
            write_reg(33, self.width as u64);
            write_reg(34, self.left);
            write_reg(35, self.right);
            write_reg(36, self.result);
            write_reg(37, self.carry_in);
            write_reg(38, self.bits);
        }
    }

    #[inline(always)]
    pub(crate) fn load_virtual() -> Self {
        unsafe {
            // These private slots are initialized from valid Flags at every
            // entry and written only by save_virtual. Bytecode operands are
            // masked to 0..31, so guest register writes cannot name them.
            // Preserve the enum invariants without a validation switch at
            // every bytecode PC, which needlessly multiplies evaluator state.
            let rule = core::mem::transmute::<u8, Rule>(read_reg(32) as u8);
            let width = core::mem::transmute::<u8, Width>(read_reg(33) as u8);
            Self {
                rule,
                width,
                left: read_reg(34),
                right: read_reg(35),
                result: read_reg(36),
                carry_in: read_reg(37),
                bits: read_reg(38),
            }
        }
    }
}
