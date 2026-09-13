//! Register virtualization for the opt-in specialization experiment.

pub(super) struct Registers<const SPECIALIZE: bool>([u64; 32]);

// Explicit expansion keeps register numbers constant before weval encounters
// the dispatch loop. A Rust loop would merge its index into a runtime value.
macro_rules! each {
    ($i:ident, $body:block) => {
        each!(@expand $i, $body; 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    };
    (@expand $i:ident, $body:block; $($n:literal),*) => {
        $({ let $i = $n; $body })*
    };
}

impl<const SPECIALIZE: bool> Registers<SPECIALIZE> {
    #[inline(always)]
    pub fn new(architectural: &[u64; 16]) -> Self {
        let mut result = Self([0; 32]);
        if !SPECIALIZE {
            result.0[..16].copy_from_slice(architectural);
            return result;
        }
        each!(i, {
            result.set(i, architectural[i]);
            result.set(i + 16, 0);
        });
        result
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> u64 {
        #[cfg(all(feature = "specialize", target_arch = "wasm32"))]
        if SPECIALIZE {
            return unsafe { super::specialize::read_reg(index as u64) };
        }
        self.0[index]
    }

    #[inline(always)]
    pub fn set(&mut self, index: usize, value: u64) {
        #[cfg(all(feature = "specialize", target_arch = "wasm32"))]
        if SPECIALIZE {
            unsafe { super::specialize::write_reg(index as u64, value) };
            return;
        }
        self.0[index] = value;
    }

    #[inline(always)]
    pub fn flush(&self, architectural: &mut [u64; 16]) {
        if !SPECIALIZE {
            architectural.copy_from_slice(&self.0[..16]);
            return;
        }
        each!(i, {
            architectural[i] = self.get(i);
        });
    }
}

#[inline(always)]
pub(super) fn enter<const SPECIALIZE: bool>() {
    #[cfg(all(feature = "specialize", target_arch = "wasm32"))]
    if SPECIALIZE {
        unsafe { super::specialize::push_context(0) };
    }
}

#[inline(always)]
pub(super) fn context<const SPECIALIZE: bool>(_pc: usize) {
    #[cfg(all(feature = "specialize", target_arch = "wasm32"))]
    if SPECIALIZE {
        unsafe { super::specialize::update_context(_pc as u32) };
    }
}

#[inline(always)]
pub(super) fn leave<const SPECIALIZE: bool>() {
    #[cfg(all(feature = "specialize", target_arch = "wasm32"))]
    if SPECIALIZE {
        unsafe { super::specialize::pop_context() };
    }
}
