//! An STM32 general-purpose timer (RM0433 38–41) as a time base: the
//! counter, its prescaler and auto-reload, one-pulse mode, and the update
//! event and its interrupt. Capture, compare, PWM and the slave modes are
//! registers that read back and do nothing.
//!
//! The counter is not clocked. It is worked out from the machine's cycle
//! count when read, from where it was when it last started or was written,
//! and the timer says when its next update is due (see [`Timer::due`]) so
//! that the machine's run loop — and a `wfi` it skips over — stops there.

// CR1.
const CEN: u32 = 1 << 0;
const UDIS: u32 = 1 << 1;
const URS: u32 = 1 << 2;
const OPM: u32 = 1 << 3;
// DIER, SR, EGR.
const UIE: u32 = 1 << 0;
const UIF: u32 = 1 << 0;
const UG: u32 = 1 << 0;

#[derive(Clone, Debug)]
pub struct Timer {
    cr1: u32,
    dier: u32,
    sr: u32,
    psc: u32,
    arr: u32,
    /// The counter's value at `since`, the cycle it last started counting
    /// from or was set at.
    cnt: u32,
    since: u64,
    /// Whether the counter is 32 bits wide (TIM2 and TIM5) or 16.
    wide: bool,
    /// Core cycles per tick of the timer's kernel clock.
    divider: u64,
    other: [u32; 0x20],
}

impl Timer {
    pub fn new(wide: bool, divider: u64) -> Timer {
        Timer {
            cr1: 0,
            dier: 0,
            sr: 0,
            psc: 0,
            arr: if wide { u32::MAX } else { 0xFFFF },
            cnt: 0,
            since: 0,
            wide,
            divider,
            other: [0; 0x20],
        }
    }

    pub fn reset(&mut self) {
        *self = Timer::new(self.wide, self.divider);
    }

    /// Core cycles per count.
    fn period(&self) -> u64 {
        self.divider * (u64::from(self.psc) + 1)
    }

    /// The counter, at cycle `now`.
    fn count(&self, now: u64) -> u32 {
        if self.cr1 & CEN == 0 {
            return self.cnt;
        }
        let counts = (now.saturating_sub(self.since)) / self.period();
        let modulus = u64::from(self.arr) + 1;
        ((u64::from(self.cnt) + counts) % modulus) as u32
    }

    /// The cycle the next update event happens at, if the timer is
    /// counting toward one.
    pub fn due(&self) -> Option<u64> {
        if self.cr1 & CEN == 0 || self.arr == 0 {
            return None;
        }
        let to_go = u64::from(self.arr - self.cnt.min(self.arr)) + 1;
        Some(self.since + to_go * self.period())
    }

    /// Brings the timer up to cycle `now`: the updates since, UIF for
    /// them, and in one-pulse mode the counter stopped at the first.
    pub fn advance(&mut self, now: u64) {
        let Some(due) = self.due().filter(|&due| due <= now) else { return };
        if self.cr1 & UDIS == 0 {
            self.sr |= UIF;
        }
        if self.cr1 & OPM != 0 {
            self.cr1 &= !CEN;
            self.cnt = 0;
            self.since = due;
            return;
        }
        let modulus = u64::from(self.arr) + 1;
        let periods = (now - due) / (modulus * self.period());
        self.since = due + periods * modulus * self.period();
        self.cnt = 0;
    }

    pub fn read(&self, offset: u32, now: u64) -> u32 {
        match offset {
            0x00 => self.cr1,
            0x0C => self.dier,
            0x10 => self.sr,
            0x24 => self.count(now),
            0x28 => self.psc,
            0x2C => self.arr,
            0x14 => 0,
            offset => self.other[(offset / 4 % 0x20) as usize],
        }
    }

    pub fn write(&mut self, offset: u32, value: u32, now: u64) {
        // Whatever is changed, the counter is settled where it is first.
        self.advance(now);
        self.cnt = self.count(now);
        self.since = now;
        let mask = if self.wide { u32::MAX } else { 0xFFFF };
        match offset {
            0x00 => self.cr1 = value & 0x3FF,
            0x0C => self.dier = value,
            // SR: a zero clears a flag, a one leaves it.
            0x10 => self.sr &= value,
            0x14 => {
                if value & UG != 0 {
                    self.cnt = 0;
                    if self.cr1 & URS == 0 {
                        self.sr |= UIF;
                    }
                }
            }
            0x24 => self.cnt = value & mask,
            0x28 => self.psc = value & 0xFFFF,
            0x2C => self.arr = value & mask,
            offset => self.other[(offset / 4 % 0x20) as usize] = value,
        }
    }

    pub fn interrupt(&self) -> bool {
        self.sr & self.dier & UIE != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_pulse_timer_updates_once_and_stops() {
        let mut t = Timer::new(false, 2);
        t.write(0x00, OPM, 0);
        t.write(0x0C, UIE, 0);
        t.write(0x28, 9, 0);
        t.write(0x2C, 100, 0);
        t.write(0x00, OPM | CEN, 1000);
        assert_eq!(t.read(0x24, 1000 + 2 * 10 * 50), 50);
        assert_eq!(t.due(), Some(1000 + 2 * 10 * 101));
        t.advance(1000 + 2 * 10 * 101);
        assert!(t.interrupt());
        assert_eq!(t.read(0x00, 0) & CEN, 0, "OPM stopped it");
        assert_eq!(t.due(), None);
        t.write(0x10, !UIF, 5000);
        assert!(!t.interrupt());
    }

    #[test]
    fn a_free_running_timer_wraps_at_its_reload() {
        let mut t = Timer::new(true, 1);
        t.write(0x2C, 9, 0);
        t.write(0x00, CEN, 0);
        assert_eq!(t.read(0x24, 25), 5);
        t.advance(25);
        assert_eq!(t.read(0x10, 25), UIF);
        assert_eq!(t.due(), Some(30));
        assert_eq!(t.read(0x24, 33), 3);
    }
}
