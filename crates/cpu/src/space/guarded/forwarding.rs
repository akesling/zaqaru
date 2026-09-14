//! One write-through word: memory is always current at faults and exits.
//! Any store invalidates it; an exact guarded qword access can repopulate it.
use core::cell::Cell;

pub(super) struct Forwarding<const ENABLED: bool>([Cell<u64>; 3]);

impl<const ENABLED: bool> Forwarding<ENABLED> {
    #[inline(always)]
    pub(super) fn new() -> Self {
        let value = Self([Cell::new(0), Cell::new(0), Cell::new(0)]);
        value.set(0, 0);
        value.set(1, 0);
        value.set(2, 0);
        value
    }
    #[inline(always)]
    fn get(&self, index: usize) -> u64 {
        #[cfg(target_arch = "wasm32")]
        if ENABLED {
            return unsafe { crate::bytecode::specialize::read_reg(40 + index as u64) };
        }
        self.0[index].get()
    }
    #[inline(always)]
    fn set(&self, index: usize, value: u64) {
        #[cfg(target_arch = "wasm32")]
        if ENABLED {
            unsafe { crate::bytecode::specialize::write_reg(40 + index as u64, value) };
            return;
        }
        self.0[index].set(value);
    }
    #[inline(always)]
    pub(super) fn invalidate(&self) {
        self.set(0, 0);
    }
    #[inline(always)]
    pub(super) fn remember(&self, address: u64, value: u64) {
        self.set(0, 1);
        self.set(1, address);
        self.set(2, value);
    }
    #[inline(always)]
    pub(super) fn load(&self, address: u64) -> Option<u64> {
        if self.get(0) != 0 && self.get(1) == address {
            Some(self.get(2))
        } else {
            None
        }
    }
}
