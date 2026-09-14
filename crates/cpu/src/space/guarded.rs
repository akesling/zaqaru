//! A short-lived proof for stack-adjacent data pages. The exclusive borrow and
//! restricted API prevent permission changes or newly decoded code while live.
use super::{Fault, PAGE_SHIFT, PAGE_SIZE, Space, Width};
#[cfg(feature = "stack-forwarding")]
mod forwarding;

#[cfg(target_arch = "wasm32")]
static mut ACCEPTED_WINDOWS: u64 = 0;
#[cfg(target_arch = "wasm32")]
pub(crate) fn accepted_windows() -> u64 {
    unsafe { ACCEPTED_WINDOWS }
}

pub(crate) struct GuardedSpace<'a, const ENABLED: bool> {
    space: &'a mut Space,
    base: u64,
    length: u64,
    #[cfg(feature = "stack-forwarding")]
    forwarding: forwarding::Forwarding<ENABLED>,
}

impl<'a, const ENABLED: bool> GuardedSpace<'a, ENABLED> {
    #[inline(always)]
    pub(crate) fn new(space: &'a mut Space, stack: u64) -> Self {
        let (base, length) = if ENABLED {
            window(space, stack)
        } else {
            (0, 0)
        };
        Self {
            space,
            base,
            length,
            #[cfg(feature = "stack-forwarding")]
            forwarding: forwarding::Forwarding::new(),
        }
    }

    #[inline(always)]
    fn contains(&self, address: u64, width: Width) -> bool {
        let offset = address.wrapping_sub(self.base);
        ENABLED && offset < self.length && u64::from(width.bytes()) <= self.length - offset
    }

    #[inline(always)]
    pub(crate) fn load(&self, address: u64, width: Width) -> Result<u64, Fault> {
        if self.contains(address, width) {
            #[cfg(feature = "stack-forwarding")]
            if width == Width::Qword {
                if let Some(value) = self.forwarding.load(address) {
                    return Ok(value);
                }
            }
            // No method on this wrapper can revoke the entry proof.
            let value = unsafe { Space::load_permitted(address, width) };
            #[cfg(feature = "stack-forwarding")]
            if width == Width::Qword {
                self.forwarding.remember(address, value);
            }
            return Ok(value);
        }
        self.space.load(address, width)
    }

    #[inline(always)]
    pub(crate) fn store(&mut self, address: u64, width: Width, value: u64) -> Result<(), Fault> {
        #[cfg(feature = "stack-forwarding")]
        self.forwarding.invalidate();
        if self.contains(address, width) {
            // Proven writable and not marked as cached code. Decoding, map
            // changes and syscalls require dropping this exclusive wrapper.
            unsafe { Space::store_permitted(address, width, value) };
            #[cfg(feature = "stack-forwarding")]
            if width == Width::Qword {
                self.forwarding.remember(address, value);
            }
            return Ok(());
        }
        self.space.store(address, width, value)
    }

    pub(crate) fn read(&self, address: u64, bytes: &mut [u8]) -> Result<(), Fault> {
        self.space.read(address, bytes)
    }
    pub(crate) fn write(&mut self, address: u64, bytes: &[u8]) -> Result<(), Fault> {
        #[cfg(feature = "stack-forwarding")]
        self.forwarding.invalidate();
        self.space.write(address, bytes)
    }
    #[inline(always)]
    pub(crate) fn has_dirty_code(&self) -> bool {
        self.space.has_dirty_code()
    }
}

#[inline(never)]
fn window(space: &Space, stack: u64) -> (u64, u64) {
    let Some(last) = stack.checked_sub(1) else {
        return (0, 0);
    };
    let top = (last & !(PAGE_SIZE - 1)).checked_add(PAGE_SIZE);
    let Some(top) = top.filter(|&top| top <= space.limit && top >= 2 * PAGE_SIZE) else {
        return (0, 0);
    };
    let base = top - 2 * PAGE_SIZE;
    for page in (base >> PAGE_SHIFT)..(top >> PAGE_SHIFT) {
        if !space.readable.get(page as usize)
            || !space.writable.get(page as usize)
            || space.code.get(page as usize)
        {
            // A failed speculative proof is not a guest fault. The original
            // access path determines whether and where execution actually faults.
            return (0, 0);
        }
    }
    #[cfg(target_arch = "wasm32")]
    unsafe {
        ACCEPTED_WINDOWS = ACCEPTED_WINDOWS.wrapping_add(1);
    }
    (base, 2 * PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        arena::Arena,
        space::{Access, Protection},
    };

    fn mapped() -> (Arena, Space) {
        let arena = Arena::new(4 * PAGE_SIZE);
        let mut space = Space::new(arena.limit());
        space.protect(arena.base(), arena.length(), Protection::READ_WRITE);
        (arena, space)
    }

    #[test]
    fn guard_covers_whole_accesses_and_preserves_aliasing() {
        let (arena, mut space) = mapped();
        let mut guarded = GuardedSpace::<true>::new(&mut space, arena.limit());
        let base = arena.limit() - 2 * PAGE_SIZE;
        assert_eq!(guarded.base, base);
        assert_eq!(guarded.length, 2 * PAGE_SIZE);
        for address in [base, base + PAGE_SIZE - 4, arena.limit() - 8] {
            assert!(guarded.contains(address, Width::Qword));
            guarded.store(address, Width::Qword, u64::MAX).unwrap();
            guarded.store(address + 1, Width::Byte, 0).unwrap();
            assert_eq!(
                guarded.load(address, Width::Qword).unwrap(),
                u64::MAX ^ 0xff00
            );
        }
        for address in [base - 1, arena.limit() - 7, arena.limit(), u64::MAX] {
            assert!(!guarded.contains(address, Width::Qword));
        }
        // Outside the proof, an otherwise valid access still succeeds.
        guarded.store(base - 8, Width::Qword, 17).unwrap();
        assert_eq!(guarded.load(base - 8, Width::Qword).unwrap(), 17);
    }

    #[test]
    fn failed_proof_does_not_fault_or_bypass_code_invalidation() {
        let (arena, mut space) = mapped();
        let page = arena.limit() - PAGE_SIZE;
        space.mark_code(page >> PAGE_SHIFT);
        let mut guarded = GuardedSpace::<true>::new(&mut space, arena.limit());
        assert_eq!(guarded.length, 0);
        assert!(!guarded.contains(0, Width::Byte));
        guarded.store(page, Width::Qword, 23).unwrap();
        assert!(guarded.has_dirty_code());
        drop(guarded);
        assert_eq!(space.take_dirty_code(), vec![page >> PAGE_SHIFT]);
    }

    #[test]
    fn permission_changes_and_straddling_faults_use_the_original_path() {
        let (arena, mut space) = mapped();
        let page = arena.limit() - PAGE_SIZE;
        {
            let guard = GuardedSpace::<true>::new(&mut space, arena.limit());
            assert!(guard.length > 0);
        }
        space.protect(page, PAGE_SIZE, Protection::READ);
        let mut guard = GuardedSpace::<true>::new(&mut space, arena.limit());
        assert_eq!(guard.length, 0);
        guard.load(page, Width::Qword).unwrap();
        assert_eq!(
            guard.store(page - 4, Width::Qword, 99),
            Err(Fault {
                address: page,
                access: Access::Write
            })
        );
        drop(guard);
        space.unmap(page, PAGE_SIZE);
        let guard = GuardedSpace::<true>::new(&mut space, arena.limit());
        assert_eq!(guard.length, 0);
        assert_eq!(
            guard.load(page - 4, Width::Qword),
            Err(Fault {
                address: page,
                access: Access::Read
            })
        );
    }

    #[test]
    fn invalid_stack_pointers_and_disabled_guards_prove_nothing() {
        let (arena, mut space) = mapped();
        for stack in [0, 1, u64::MAX, arena.limit() + PAGE_SIZE] {
            let guard = GuardedSpace::<true>::new(&mut space, stack);
            assert_eq!(guard.length, 0);
            assert!(!guard.contains(arena.base(), Width::Byte));
        }
        let guard = GuardedSpace::<false>::new(&mut space, arena.limit());
        assert_eq!(guard.length, 0);
    }

    #[cfg(feature = "stack-forwarding")]
    #[test]
    fn forwarded_word_is_written_through_and_invalidated_by_aliasing_writes() {
        let (arena, mut space) = mapped();
        let at = arena.limit() - 16;
        let mut guard = GuardedSpace::<true>::new(&mut space, arena.limit());
        guard.store(at, Width::Qword, 0x1122334455667788).unwrap();
        assert_eq!(guard.forwarding.load(at), Some(0x1122334455667788));
        // Bypass forwarding through the byte-read API: memory is already current.
        let mut bytes = [0; 8];
        guard.read(at, &mut bytes).unwrap();
        assert_eq!(u64::from_le_bytes(bytes), 0x1122334455667788);
        guard.store(at + 1, Width::Byte, 0xff).unwrap();
        assert_eq!(guard.forwarding.load(at), None);
        assert_eq!(guard.load(at, Width::Qword).unwrap(), 0x112233445566ff88);
        assert_eq!(guard.forwarding.load(at), Some(0x112233445566ff88));
        // Vector/bulk writes invalidate too, even with a partial overlap.
        guard.write(at + 4, &[0; 4]).unwrap();
        assert_eq!(guard.forwarding.load(at), None);
        assert_eq!(guard.load(at, Width::Qword).unwrap(), 0x5566ff88);
        // An outside-window write must also invalidate conservatively.
        guard.store(arena.base(), Width::Qword, 1).unwrap();
        assert_eq!(guard.forwarding.load(at), None);
    }
}
