//! Quiescent continuation handoff for the opt-in Assembly experiment.
//! A frozen source cannot execute; a retired source can never resume.

static mut STATE: u32 = 0; // running, frozen, retired
static mut TOKEN: u64 = 0;

pub fn runnable() -> bool {
    unsafe { STATE == 0 }
}

/// Freeze between turns, giving the controller a lineage-local handoff token.
#[unsafe(no_mangle)]
pub extern "C" fn zaqaru_freeze(token: u64) -> i32 {
    unsafe {
        if STATE != 0 || token == 0 {
            return -1;
        }
        STATE = 1;
        TOKEN = token;
    }
    cpu::bytecode::evolution::prepare() as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn zaqaru_retire(token: u64) -> i32 {
    unsafe {
        if STATE != 1 || TOKEN != token {
            return -1;
        }
        STATE = 2;
    }
    0
}

/// The controller owns exclusive activation authority. Copying a continuation
/// does not confer that authority; it must retire the source before this call.
#[unsafe(no_mangle)]
pub extern "C" fn zaqaru_resume(token: u64) -> i32 {
    unsafe {
        if STATE != 1 || TOKEN != token {
            return -1;
        }
        STATE = 0;
        TOKEN = 0;
    }
    0
}
