//! `timerfd`: a deadline you can wait on as a descriptor.
//!
//! nusb's event loop is why it is here: every USB device it opens comes with
//! one, armed for transfer timeouts and watched by the same `epoll` set as
//! the device, so a device without a timer does not open at all.
//!
//! The shape is the eventfd's. A timer is arena state — a deadline, an
//! interval, and the expirations counted since the last read — so whether
//! one is readable is a question a scheduling decision can ask. What moves
//! it is the clock, and the clock is read only where the kernel already
//! reads it: [`Timers::observe`] is told the time by the system's idle pass
//! and by every monotonic reading a syscall makes, and that is when a
//! deadline passing becomes an expiration. A parked `epoll_wait` watching
//! the timer is then ready, the next time the scheduler looks.
//!
//! One divergence, stated: a *blocking* read of a timer with nothing
//! counted is refused with `ENOSYS` rather than parked. Every reader found
//! so far opens its timers `TFD_NONBLOCK` and waits in `epoll`.

use crate::errno::Errno;
use crate::syscall::{Arguments, Outcome};

/// `TFD_NONBLOCK` and `TFD_CLOEXEC`, which are `O_NONBLOCK` and `O_CLOEXEC`.
pub const NONBLOCK: i32 = 0o4000;
pub const CLOEXEC: i32 = 0o2000000;
/// `TFD_TIMER_ABSTIME`: the new value is a time, not a delay.
pub const ABSOLUTE: i32 = 1;
/// `TFD_TIMER_CANCEL_ON_SET`, which only means something for a realtime
/// clock that is set — and nothing here sets one.
pub const CANCEL_ON_SET: i32 = 2;

const CLOCK_REALTIME: i64 = 0;
const CLOCK_MONOTONIC: i64 = 1;
const CLOCK_BOOTTIME: i64 = 7;

/// `struct itimerspec`: the interval, then the value, each a `timespec`.
const ITIMERSPEC: u64 = 32;

#[derive(Clone, Copy, Debug)]
struct Timer {
    realtime: bool,
    /// When it next expires, on the monotonic clock.
    deadline: Option<u64>,
    interval: u64,
    expirations: u64,
    references: u32,
}

/// Every timer in the container, and the latest time any of them was told.
#[derive(Clone, Default, Debug)]
pub struct Timers {
    entries: Vec<Option<Timer>>,
    now: u64,
}

impl Timers {
    fn create(&mut self, realtime: bool) -> u32 {
        let timer = Timer { realtime, deadline: None, interval: 0, expirations: 0, references: 1 };
        if let Some(free) = self.entries.iter().position(Option::is_none) {
            self.entries[free] = Some(timer);
            return free as u32;
        }
        self.entries.push(Some(timer));
        (self.entries.len() - 1) as u32
    }

    pub fn acquire(&mut self, id: u32) {
        if let Some(Some(timer)) = self.entries.get_mut(id as usize) {
            timer.references += 1;
        }
    }

    pub fn release(&mut self, id: u32) {
        let Some(Some(timer)) = self.entries.get_mut(id as usize) else { return };
        timer.references = timer.references.saturating_sub(1);
        if timer.references == 0 {
            self.entries[id as usize] = None;
        }
    }

    /// Brings every timer up to `now`: a deadline passed is an expiration,
    /// and a periodic timer counts every period that went by. Answers
    /// whether any expired.
    pub fn observe(&mut self, now: u64) -> bool {
        self.now = self.now.max(now);
        let now = self.now;
        let mut expired = false;
        for timer in self.entries.iter_mut().flatten() {
            let Some(deadline) = timer.deadline else { continue };
            if now < deadline {
                continue;
            }
            expired = true;
            match timer.interval {
                0 => {
                    timer.expirations += 1;
                    timer.deadline = None;
                }
                interval => {
                    let periods = (now - deadline) / interval + 1;
                    timer.expirations += periods;
                    timer.deadline = Some(deadline + periods * interval);
                }
            }
        }
        expired
    }

    pub fn readable(&self, id: u32) -> bool {
        self.timer(id).is_some_and(|timer| timer.expirations > 0)
    }

    /// The soonest deadline of any timer: a moment the container has to be
    /// awake for, whether or not anything is parked on it yet.
    pub fn earliest(&self) -> Option<u64> {
        self.entries.iter().flatten().filter_map(|timer| timer.deadline).min()
    }

    fn timer(&self, id: u32) -> Option<&Timer> {
        self.entries.get(id as usize).and_then(Option::as_ref)
    }

    fn timer_mut(&mut self, id: u32) -> Option<&mut Timer> {
        self.entries.get_mut(id as usize).and_then(Option::as_mut)
    }
}

/// See [`crate::ring::Shared`].
pub type Shared = std::rc::Rc<std::cell::RefCell<Timers>>;

/// A `timespec` as nanoseconds, or `EINVAL` for one Linux refuses.
fn nanoseconds(bytes: &[u8]) -> Result<u64, Errno> {
    let seconds = i64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes"));
    let nanos = i64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(Errno::Invalid);
    }
    Ok((seconds as u64).saturating_mul(1_000_000_000).saturating_add(nanos as u64))
}

fn timespec(nanoseconds: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&((nanoseconds / 1_000_000_000) as i64).to_le_bytes());
    bytes[8..16].copy_from_slice(&((nanoseconds % 1_000_000_000) as i64).to_le_bytes());
    bytes
}

impl<S: crate::abi::Store, M: crate::machine::Machine> crate::syscall::Kernel<'_, S, M> {
    /// `timerfd_create(2)`.
    pub(crate) fn timerfd_create(&mut self, arguments: Arguments) -> Outcome {
        use crate::file::open_flags;
        let clock = arguments.get(0);
        let flags = arguments.get(1) as i32;
        if flags & !(NONBLOCK | CLOEXEC) != 0 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        let realtime = match clock {
            CLOCK_REALTIME => true,
            CLOCK_MONOTONIC | CLOCK_BOOTTIME => false,
            _ => return Outcome::Done(Errno::Invalid.as_result()),
        };
        let id = self.timers.borrow_mut().create(realtime);
        let open = open_flags::READ_ONLY | if flags & NONBLOCK != 0 { open_flags::NONBLOCK } else { 0 };
        match self.files.open(crate::fd::Backing::Timer(id), open, flags & CLOEXEC != 0) {
            Ok(fd) => Outcome::Done(i64::from(fd)),
            Err(errno) => {
                self.timers.borrow_mut().release(id);
                Outcome::Done(errno.as_result())
            }
        }
    }

    fn timer_of(&self, fd: i32) -> Result<u32, Errno> {
        match self.files.description(fd)?.backing {
            crate::fd::Backing::Timer(id) => Ok(id),
            _ => Err(Errno::Invalid),
        }
    }

    /// `timerfd_settime(2)`.
    pub(crate) fn timerfd_settime(&mut self, arguments: Arguments) -> Outcome {
        Outcome::Done(match self.set_timer(arguments) {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    fn set_timer(&mut self, arguments: Arguments) -> Result<(), Errno> {
        let id = self.timer_of(arguments.get(0) as i32)?;
        let flags = arguments.get(1) as i32;
        if flags & !(ABSOLUTE | CANCEL_ON_SET) != 0 {
            return Err(Errno::Invalid);
        }
        let mut new = [0u8; ITIMERSPEC as usize];
        self.pages.read(arguments.get(2) as u64, &mut new).map_err(|_| Errno::Fault)?;
        let interval = nanoseconds(&new[0..16])?;
        let value = nanoseconds(&new[16..32])?;
        // The old setting first, read before anything changes.
        let old = arguments.get(3) as u64;
        if old != 0 {
            let current = self.current_setting(id)?;
            self.pages.write(old, &current).map_err(|_| Errno::Fault)?;
        }
        let now = self.monotonic().ok_or(Errno::NoSys)?;
        let realtime = self.timers.borrow().timer(id).ok_or(Errno::BadFile)?.realtime;
        let deadline = match (value, flags & ABSOLUTE != 0) {
            (0, _) => None,
            (value, false) => Some(now.saturating_add(value)),
            // An absolute realtime value is a wall-clock time; this keeps
            // deadlines on the monotonic clock, so it is moved across by
            // the clocks' present difference.
            (value, true) if realtime => {
                let wall = self.realtime().ok_or(Errno::NoSys)?;
                Some(now.saturating_add(value.saturating_sub(wall)))
            }
            (value, true) => Some(value),
        };
        let mut timers = self.timers.borrow_mut();
        let timer = timers.timer_mut(id).ok_or(Errno::BadFile)?;
        timer.deadline = deadline;
        timer.interval = interval;
        timer.expirations = 0;
        timers.observe(now);
        Ok(())
    }

    /// `timerfd_gettime(2)`.
    pub(crate) fn timerfd_gettime(&mut self, arguments: Arguments) -> Outcome {
        let result = self.timer_of(arguments.get(0) as i32).and_then(|id| self.current_setting(id)).and_then(
            |setting| self.pages.write(arguments.get(1) as u64, &setting).map_err(|_| Errno::Fault),
        );
        Outcome::Done(match result {
            Ok(()) => 0,
            Err(errno) => errno.as_result(),
        })
    }

    /// The setting as `itimerspec` gives it back: the interval, and how long
    /// until the next expiration.
    fn current_setting(&mut self, id: u32) -> Result<[u8; ITIMERSPEC as usize], Errno> {
        let now = self.monotonic().unwrap_or(0);
        let timers = self.timers.borrow();
        let timer = timers.timer(id).ok_or(Errno::BadFile)?;
        let remaining = timer.deadline.map_or(0, |deadline| deadline.saturating_sub(now).max(1));
        let mut setting = [0u8; ITIMERSPEC as usize];
        setting[0..16].copy_from_slice(&timespec(timer.interval));
        setting[16..32].copy_from_slice(&timespec(remaining));
        Ok(setting)
    }

    /// `read` on a timer: the expirations since the last read, as eight
    /// bytes, and the count starts again.
    pub(crate) fn read_timer(&mut self, id: u32, flags: i32, buffer: u64, count: u64) -> Outcome {
        if count < 8 {
            return Outcome::Done(Errno::Invalid.as_result());
        }
        if let Some(now) = self.monotonic() {
            self.timers.borrow_mut().observe(now);
        }
        let taken = {
            let mut timers = self.timers.borrow_mut();
            let Some(timer) = timers.timer_mut(id) else {
                return Outcome::Done(Errno::BadFile.as_result());
            };
            std::mem::take(&mut timer.expirations)
        };
        if taken == 0 {
            return Outcome::Done(match flags & crate::file::open_flags::NONBLOCK != 0 {
                true => Errno::TryAgain.as_result(),
                // See the module's comment.
                false => Errno::NoSys.as_result(),
            });
        }
        Outcome::Done(match self.pages.write(buffer, &taken.to_le_bytes()) {
            Ok(()) => 8,
            Err(_) => Errno::Fault.as_result(),
        })
    }
}
