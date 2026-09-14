//! Load virtual flags only when an operation consumes them, not on every PC.
use crate::{
    flags::{Flags, Rule},
    state::Width,
};

pub(super) struct FlagState<const SPECIALIZE: bool>(Flags);

impl<const SPECIALIZE: bool> FlagState<SPECIALIZE> {
    #[inline(always)]
    pub(super) fn new(flags: Flags) -> Self {
        let mut state = Self(flags);
        state.replace(flags);
        state
    }

    #[inline(always)]
    pub(super) fn snapshot(&self) -> Flags {
        #[cfg(target_arch = "wasm32")]
        if SPECIALIZE {
            return Flags::load_virtual();
        }
        self.0
    }

    #[inline(always)]
    pub(super) fn replace(&mut self, flags: Flags) {
        #[cfg(target_arch = "wasm32")]
        if SPECIALIZE {
            flags.save_virtual();
            return;
        }
        self.0 = flags;
    }

    #[inline(always)]
    pub(super) fn record(&mut self, rule: Rule, width: Width, left: u64, right: u64, result: u64) {
        let mut flags = self.snapshot();
        flags.record(rule, width, left, right, result);
        self.replace(flags);
    }

    #[inline(always)]
    pub(super) fn record_with_carry(
        &mut self,
        rule: Rule,
        width: Width,
        left: u64,
        right: u64,
        result: u64,
        carry: bool,
    ) {
        let mut flags = self.snapshot();
        flags.record_with_carry(rule, width, left, right, result, carry);
        self.replace(flags);
    }

    #[inline(always)]
    pub(super) fn carry(&self) -> bool {
        self.snapshot().carry()
    }
}
