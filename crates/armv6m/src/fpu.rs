//! The floating-point unit: coprocessors 10 and 11, as a Cortex-M7 with
//! FPv5-D16 has them — single and double precision, thirty-two single
//! registers that pair into sixteen doubles.
//!
//! The arithmetic is IEEE 754's, exactly: every rounding mode FPSCR can
//! select, and every cumulative exception flag. It is done in software
//! (LLVM's APFloat, as `rustc_apfloat`) rather than by the host's floats,
//! for two reasons. The host can only round to nearest and keeps no flags;
//! and in the browser the host is WebAssembly, whose NaNs have unspecified
//! bits — where a guest's NaN payloads would make a run stop being a pure
//! function of its input. What ARM adds on top of IEEE is here: which NaN
//! an operation propagates and in what order, the default NaN, FPSCR.FZ
//! flushing denormals, and conversions to integer that saturate.
//!
//! Where APFloat and ARM disagree about IEEE 754 — ARM detects underflow
//! before rounding, and APFloat does not count rounding down to the
//! largest finite number as an overflow — the exact result decides, worked
//! out again in quad precision. Half precision is IEEE's: FPSCR.AHP's
//! alternative format is accepted and ignored.
//!
//! The FPU must be enabled in CPACR, as on the part: until it is, every
//! instruction here is a NOCP UsageFault.

use rustc_apfloat::ieee::{Double, Half, Quad, Single};
use rustc_apfloat::{Float, FloatConvert, Round, Status, StatusAnd};

use crate::state::Core;
use crate::{Fault, Machine};

/// FPSCR's cumulative exception flags.
const IOC: u32 = 1 << 0;
const DZC: u32 = 1 << 1;
const OFC: u32 = 1 << 2;
const UFC: u32 = 1 << 3;
const IXC: u32 = 1 << 4;
const IDC: u32 = 1 << 7;
/// FPSCR's controls.
const FZ: u32 = 1 << 24;
const DN: u32 = 1 << 25;
/// The bits of FPSCR M-profile implements.
const FPSCR_MASK: u32 = 0xF7C0_009F;

/// FPCCR.ASPEN: an FP instruction marks the context as having used the
/// FPU (CONTROL.FPCA), so that exceptions stack it.
pub const FPCCR_ASPEN: u32 = 1 << 31;
/// FPCCR.LSPEN: stacking is lazy. The model stacks eagerly whatever this
/// says; see [`crate::exception`].
pub const FPCCR_LSPEN: u32 = 1 << 30;

/// A floating-point format, as bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fmt {
    H,
    S,
    D,
}

impl Fmt {
    fn frac(self) -> u32 {
        match self {
            Fmt::H => 10,
            Fmt::S => 23,
            Fmt::D => 52,
        }
    }

    fn width(self) -> u32 {
        match self {
            Fmt::H => 16,
            Fmt::S => 32,
            Fmt::D => 64,
        }
    }

    fn exp_mask(self) -> u64 {
        ((1u64 << (self.width() - 1 - self.frac())) - 1) << self.frac()
    }

    fn frac_mask(self) -> u64 {
        (1u64 << self.frac()) - 1
    }

    fn sign(self) -> u64 {
        1 << (self.width() - 1)
    }

    fn quiet_bit(self) -> u64 {
        1 << (self.frac() - 1)
    }

    fn is_nan(self, x: u64) -> bool {
        x & self.exp_mask() == self.exp_mask() && x & self.frac_mask() != 0
    }

    fn is_snan(self, x: u64) -> bool {
        self.is_nan(x) && x & self.quiet_bit() == 0
    }

    fn is_denormal(self, x: u64) -> bool {
        x & self.exp_mask() == 0 && x & self.frac_mask() != 0
    }

    fn is_zero(self, x: u64) -> bool {
        x & !self.sign() == 0
    }

    fn is_infinite(self, x: u64) -> bool {
        x & !self.sign() == self.exp_mask()
    }

    fn default_nan(self) -> u64 {
        self.exp_mask() | self.quiet_bit()
    }

    fn neg(self, x: u64) -> u64 {
        x ^ self.sign()
    }

    fn abs(self, x: u64) -> u64 {
        x & !self.sign()
    }

    fn zero(self, negative: bool) -> u64 {
        if negative { self.sign() } else { 0 }
    }

    fn infinity(self, negative: bool) -> u64 {
        self.zero(negative) | self.exp_mask()
    }

    /// The largest exponent a finite number has.
    fn emax(self) -> i32 {
        (1 << (self.width() - 2 - self.frac())) - 1
    }

    /// `2^n`, for the fixed-point scalings.
    fn power_of_two(self, n: u32) -> u64 {
        let bias = (1u64 << (self.width() - 2 - self.frac())) - 1;
        (bias + n as u64) << self.frac()
    }
}

/// An operation's FPSCR: its controls in, its flags out.
struct Env {
    fpscr: u32,
}

/// Runs `$body` with `$T` the APFloat type for `$fmt`.
macro_rules! with_format {
    ($fmt:expr, $T:ident => $body:expr) => {
        match $fmt {
            Fmt::H => {
                type $T = Half;
                $body
            }
            Fmt::S => {
                type $T = Single;
                $body
            }
            Fmt::D => {
                type $T = Double;
                $body
            }
        }
    };
}

#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

impl Env {
    fn round(&self) -> Round {
        match self.fpscr >> 22 & 3 {
            0 => Round::NearestTiesToEven,
            1 => Round::TowardPositive,
            2 => Round::TowardNegative,
            _ => Round::TowardZero,
        }
    }

    fn raise(&mut self, flags: u32) {
        self.fpscr |= flags;
    }

    /// `FPUnpack`'s flush: a denormal operand is a zero when FZ is set,
    /// and says so in IDC.
    fn input(&mut self, fmt: Fmt, x: u64) -> u64 {
        if self.fpscr & FZ != 0 && fmt != Fmt::H && fmt.is_denormal(x) {
            self.raise(IDC);
            return x & fmt.sign();
        }
        x
    }

    /// A NaN made quiet, or the default NaN if DN asks for it.
    fn quiet(&self, fmt: Fmt, x: u64) -> u64 {
        if self.fpscr & DN != 0 { fmt.default_nan() } else { x | fmt.quiet_bit() }
    }

    /// `FPProcessNaNs`: the first signalling NaN, quietened, raising
    /// Invalid Operation; failing that, the first quiet one.
    fn nans(&mut self, fmt: Fmt, ops: &[u64]) -> Option<u64> {
        if let Some(&x) = ops.iter().find(|&&x| fmt.is_snan(x)) {
            self.raise(IOC);
            return Some(self.quiet(fmt, x));
        }
        ops.iter().find(|&&x| fmt.is_nan(x)).map(|&x| self.quiet(fmt, x))
    }

    /// The flags of an APFloat result, and its bits: the default NaN for
    /// an invalid operation, and zero for a tiny result under FZ. `tiny`
    /// says the exact result was below the smallest normal number, which
    /// is ARM's test for underflow (see [`Env::exact`]).
    fn finish<F: Float>(&mut self, fmt: Fmt, r: StatusAnd<F>, tiny: bool) -> u64 {
        let mut flags = 0;
        if r.status.contains(Status::INVALID_OP) {
            flags |= IOC;
        }
        if r.status.contains(Status::DIV_BY_ZERO) {
            flags |= DZC;
        }
        if r.status.contains(Status::OVERFLOW) {
            flags |= OFC;
        }
        if r.status.contains(Status::INEXACT) {
            flags |= IXC;
            if tiny {
                flags |= UFC;
            }
        }
        let mut bits = r.value.to_bits() as u64;
        if fmt.is_nan(bits) {
            bits = fmt.default_nan();
        }
        if self.fpscr & FZ != 0 && fmt != Fmt::H && (tiny || fmt.is_denormal(bits)) {
            // Flushed: an underflow, and not an inexact one.
            bits &= fmt.sign();
            flags = flags & !IXC | UFC;
        }
        self.raise(flags);
        bits
    }

    /// What the exact result says that the rounded one cannot, for an
    /// inexact result at either end of the range. Rounding toward zero in
    /// magnitude, a result too big for the format is the largest finite
    /// number, and IEEE 754 calls that an overflow whenever the exact
    /// result reaches 2^(emax+1) — which APFloat does not report. And ARM
    /// detects underflow before rounding: a result is tiny if the exact one
    /// is below 2^emin, even if it rounds up to the smallest normal number.
    /// `exact` is the result again in quad precision, rounded toward zero,
    /// which is on the same side of either threshold. Answers whether the
    /// result is tiny.
    fn exact<T: Float>(&mut self, fmt: Fmt, r: &StatusAnd<T>, exact: impl FnOnce() -> Quad) -> bool {
        if !r.status.contains(Status::INEXACT) || r.value.is_nan() {
            return false;
        }
        let magnitude = r.value.abs();
        let largest = magnitude.bitwise_eq(T::largest()) && !r.status.contains(Status::OVERFLOW);
        let small = magnitude <= T::smallest_normalized();
        if !(largest || small) {
            return false;
        }
        let exact = exact().abs();
        let one = Quad::from_u128(1).value;
        if largest && exact >= one.scalbn(fmt.emax() + 1) {
            self.raise(OFC);
        }
        small && exact < one.scalbn(1 - fmt.emax())
    }

    fn binary(&mut self, fmt: Fmt, op: Op, a: u64, b: u64) -> u64 {
        let (a, b) = (self.input(fmt, a), self.input(fmt, b));
        if let Some(nan) = self.nans(fmt, &[a, b]) {
            return nan;
        }
        let round = self.round();
        let apply = |x: Quad, y: Quad, round: Round| match op {
            Op::Add => x.add_r(y, round),
            Op::Sub => x.sub_r(y, round),
            Op::Mul => x.mul_r(y, round),
            Op::Div => x.div_r(y, round),
        };
        with_format!(fmt, T => {
            let (x, y) = (T::from_bits(a as u128), T::from_bits(b as u128));
            let r = match op {
                Op::Add => x.add_r(y, round),
                Op::Sub => x.sub_r(y, round),
                Op::Mul => x.mul_r(y, round),
                Op::Div => x.div_r(y, round),
            };
            let tiny = self.exact(fmt, &r, || apply(quad(x), quad(y), Round::TowardZero).value);
            self.finish(fmt, r, tiny)
        })
    }

    /// `FPMulAdd`: `addend + a * b`, rounded once.
    fn fused(&mut self, fmt: Fmt, addend: u64, a: u64, b: u64) -> u64 {
        let (addend, a, b) = (self.input(fmt, addend), self.input(fmt, a), self.input(fmt, b));
        // A quiet NaN addend does not hide an invalid product.
        let invalid_product = fmt.is_infinite(a) && fmt.is_zero(b) || fmt.is_zero(a) && fmt.is_infinite(b);
        if fmt.is_nan(addend) && !fmt.is_snan(addend) && invalid_product {
            self.raise(IOC);
            return fmt.default_nan();
        }
        if let Some(nan) = self.nans(fmt, &[addend, a, b]) {
            return nan;
        }
        let round = self.round();
        with_format!(fmt, T => {
            let (x, y, z) = (T::from_bits(a as u128), T::from_bits(b as u128), T::from_bits(addend as u128));
            let r = x.mul_add_r(y, z, round);
            let tiny = self.exact(fmt, &r, || quad(x).mul_add_r(quad(y), quad(z), Round::TowardZero).value);
            self.finish(fmt, r, tiny)
        })
    }

    /// `FPSqrt`. APFloat has no square root. The host's is correctly
    /// rounded to nearest, and deterministic everywhere, the browser
    /// included; the remainder `x - r*r`, exact in a fused multiply-add,
    /// says which side of `r` the true root is on — which is all a
    /// directed rounding, and Inexact, need.
    fn sqrt(&mut self, fmt: Fmt, x: u64) -> u64 {
        let x = self.input(fmt, x);
        if let Some(nan) = self.nans(fmt, &[x]) {
            return nan;
        }
        if fmt.is_zero(x) || fmt.is_infinite(x) && x & fmt.sign() == 0 {
            return x;
        }
        if x & fmt.sign() != 0 {
            self.raise(IOC);
            return fmt.default_nan();
        }
        let round = self.round();
        let nearest: u64 = match fmt {
            Fmt::S => (f64::from(f32::from_bits(x as u32)).sqrt() as f32).to_bits() as u64,
            _ => f64::from_bits(x).sqrt().to_bits(),
        };
        with_format!(fmt, T => {
            let value = T::from_bits(x as u128);
            let mut r = T::from_bits(nearest as u128);
            // In quad precision, where r*r is exact and nothing underflows.
            let remainder = (-quad(r)).mul_add_r(quad(r), quad(value), Round::NearestTiesToEven).value;
            if !remainder.is_zero() {
                let above = !remainder.is_negative();
                match round {
                    Round::TowardPositive if above => r = r.next_up().value,
                    Round::TowardNegative | Round::TowardZero if !above => r = r.next_down().value,
                    _ => {}
                }
                self.raise(IXC);
            }
            r.to_bits() as u64
        })
    }

    /// `FPCompare`: the NZCV a VCMP sets. `signal_quiet` is VCMPE's, which
    /// raises Invalid Operation for any NaN rather than a signalling one.
    fn compare(&mut self, fmt: Fmt, a: u64, b: u64, signal_quiet: bool) -> u32 {
        let (a, b) = (self.input(fmt, a), self.input(fmt, b));
        if fmt.is_nan(a) || fmt.is_nan(b) {
            if signal_quiet || fmt.is_snan(a) || fmt.is_snan(b) {
                self.raise(IOC);
            }
            return 0b0011;
        }
        with_format!(fmt, T => {
            match T::from_bits(a as u128).partial_cmp(&T::from_bits(b as u128)) {
                Some(std::cmp::Ordering::Equal) => 0b0110,
                Some(std::cmp::Ordering::Less) => 0b1000,
                _ => 0b0010,
            }
        })
    }

    /// `FPConvert`, between formats.
    fn convert(&mut self, from: Fmt, to: Fmt, x: u64) -> u64 {
        let x = self.input(from, x);
        if from.is_nan(x) {
            if from.is_snan(x) {
                self.raise(IOC);
            }
            if self.fpscr & DN != 0 {
                return to.default_nan();
            }
            // The payload's top bits carry over, the quiet bit set.
            let sign = if x & from.sign() != 0 { to.sign() } else { 0 };
            let payload = x & from.frac_mask();
            let moved = if to.frac() >= from.frac() {
                payload << (to.frac() - from.frac())
            } else {
                payload >> (from.frac() - to.frac())
            };
            return sign | to.exp_mask() | to.quiet_bit() | moved & to.frac_mask();
        }
        let round = self.round();
        let mut lost = false;
        macro_rules! go {
            ($F:ty, $G:ty) => {{
                let value = <$F>::from_bits(x as u128);
                let r: StatusAnd<$G> = value.convert_r(round, &mut lost);
                let tiny = self.exact(to, &r, || quad(value));
                self.finish(to, r, tiny)
            }};
        }
        match (from, to) {
            (Fmt::S, Fmt::D) => go!(Single, Double),
            (Fmt::D, Fmt::S) => go!(Double, Single),
            (Fmt::S, Fmt::H) => go!(Single, Half),
            (Fmt::H, Fmt::S) => go!(Half, Single),
            (Fmt::D, Fmt::H) => go!(Double, Half),
            (Fmt::H, Fmt::D) => go!(Half, Double),
            _ => x,
        }
    }

    /// `FPToFixed`: to a `width`-bit integer with `frac` fraction bits,
    /// rounded as asked, saturating — NaN to zero — and raising Invalid
    /// Operation when it saturates.
    fn to_fixed(&mut self, fmt: Fmt, x: u64, signed: bool, width: u32, frac: u32, round: Round) -> u32 {
        let x = self.input(fmt, x);
        let (min, max): (i128, i128) =
            if signed { (-(1i128 << (width - 1)), (1i128 << (width - 1)) - 1) } else { (0, (1i128 << width) - 1) };
        let mask = if width == 32 { u32::MAX } else { (1 << width) - 1 };
        if fmt.is_nan(x) {
            self.raise(IOC);
            return 0;
        }
        if fmt.is_infinite(x) {
            self.raise(IOC);
            return (if x & fmt.sign() != 0 { min } else { max }) as u32 & mask;
        }
        with_format!(fmt, T => {
            let mut value = T::from_bits(x as u128);
            if frac > 0 {
                // Exact: a power of two, far from overflow for any value
                // that fits.
                value = value.mul_r(T::from_bits(fmt.power_of_two(frac) as u128), Round::NearestTiesToEven).value;
            }
            let rounded = value.round_to_integral(round).value;
            let mut exact = false;
            let whole = rounded.to_i128_r(100, Round::TowardZero, &mut exact).value;
            if whole < min || whole > max {
                self.raise(IOC);
                return whole.clamp(min, max) as u32 & mask;
            }
            if rounded.to_bits() != value.to_bits() && !(rounded.is_zero() && value.is_zero()) {
                self.raise(IXC);
            }
            whole as u32 & mask
        })
    }

    /// `FixedToFP`: from a `width`-bit integer with `frac` fraction bits,
    /// rounded as FPSCR says.
    fn from_fixed(&mut self, fmt: Fmt, x: u32, signed: bool, width: u32, frac: u32) -> u64 {
        let value: i128 = if signed {
            ((x << (32 - width)) as i32 >> (32 - width)) as i128
        } else {
            (if width == 32 { x } else { x & ((1 << width) - 1) }) as i128
        };
        let round = self.round();
        with_format!(fmt, T => {
            let r: StatusAnd<T> = if frac == 0 {
                T::from_i128_r(value, round)
            } else {
                // Exact in a double, scaled exactly, then rounded once.
                let wide = Double::from_i128_r(value, Round::NearestTiesToEven).value;
                let scaled = wide.div_r(Double::from_bits(Fmt::D.power_of_two(frac) as u128), Round::NearestTiesToEven).value;
                let mut lost = false;
                scaled.convert_r(round, &mut lost)
            };
            self.finish(fmt, r, false)
        })
    }

    /// `FPRoundInt`: to an integral value in the same format. `exact` is
    /// VRINTX's, the one that raises Inexact.
    fn round_int(&mut self, fmt: Fmt, x: u64, round: Round, exact: bool) -> u64 {
        let x = self.input(fmt, x);
        if let Some(nan) = self.nans(fmt, &[x]) {
            return nan;
        }
        with_format!(fmt, T => {
            let r = T::from_bits(x as u128).round_to_integral(round);
            if exact && r.status.contains(Status::INEXACT) {
                self.raise(IXC);
            }
            r.value.to_bits() as u64
        })
    }

    /// `FPMaxNum` and `FPMinNum`: IEEE 754-2008's, where a quiet NaN
    /// loses to a number.
    fn max_min(&mut self, fmt: Fmt, a: u64, b: u64, max: bool) -> u64 {
        let (mut a, mut b) = (self.input(fmt, a), self.input(fmt, b));
        let quiet = |x: u64| fmt.is_nan(x) && !fmt.is_snan(x);
        if quiet(a) && !fmt.is_nan(b) {
            a = fmt.infinity(max);
        } else if quiet(b) && !fmt.is_nan(a) {
            b = fmt.infinity(max);
        }
        if let Some(nan) = self.nans(fmt, &[a, b]) {
            return nan;
        }
        if fmt.is_zero(a) && fmt.is_zero(b) {
            let (na, nb) = (a & fmt.sign() != 0, b & fmt.sign() != 0);
            return fmt.zero(if max { na && nb } else { na || nb });
        }
        with_format!(fmt, T => {
            let ordering = T::from_bits(a as u128).partial_cmp(&T::from_bits(b as u128));
            let a_wins = match ordering {
                Some(std::cmp::Ordering::Greater) => max,
                Some(std::cmp::Ordering::Less) => !max,
                _ => false,
            };
            if a_wins { a } else { b }
        })
    }
}

/// A value in quad precision, exactly.
fn quad<T: FloatConvert<Quad>>(x: T) -> Quad {
    let mut lost = false;
    x.convert_r(Round::NearestTiesToEven, &mut lost).value
}

/// `VFPExpandImm`: an eight-bit immediate as a float.
fn expand_imm(fmt: Fmt, imm8: u32) -> u64 {
    let sign = (imm8 >> 7) as u64;
    let b6 = (imm8 >> 6 & 1) as u64;
    let exp_bits = fmt.width() - 1 - fmt.frac();
    // NOT(b6) : Replicate(b6, E-3) : imm8<5:4>
    let replicated = if b6 == 1 { ((1u64 << (exp_bits - 3)) - 1) << 2 } else { 0 };
    let exp = (b6 ^ 1) << (exp_bits - 1) | replicated | (imm8 >> 4 & 3) as u64;
    let frac = ((imm8 & 0xF) as u64) << (fmt.frac() - 4);
    sign << (fmt.width() - 1) | exp << fmt.frac() | frac
}

fn bits(x: u32, hi: u32, lo: u32) -> u32 {
    x >> lo & ((1u64 << (hi - lo + 1)) - 1) as u32
}

fn bit(x: u32, n: u32) -> u32 {
    x >> n & 1
}

impl Machine {
    /// A register of format `fmt`: single `n`, or double `n`, which is
    /// singles `2n` and `2n+1`. A double past d15, which an FPv5-D16 does
    /// not have, reads as zero; every instruction that names one is
    /// refused before it would matter.
    fn fp(&self, fmt: Fmt, n: u32) -> u64 {
        let n = n as usize;
        let s = |i: usize| self.cpu.s.get(i).copied().unwrap_or(0) as u64;
        match fmt {
            Fmt::D => s(2 * n + 1) << 32 | s(2 * n),
            _ => s(n),
        }
    }

    fn set_fp(&mut self, fmt: Fmt, n: u32, value: u64) {
        let n = n as usize;
        match fmt {
            Fmt::D => {
                self.cpu.s[2 * n] = value as u32;
                self.cpu.s[2 * n + 1] = (value >> 32) as u32;
            }
            _ => self.cpu.s[n] = value as u32,
        }
    }
}

/// The register an instruction names: `Vx:X` for a single, `X:Vx` for a
/// double.
fn reg(fmt: Fmt, v: u32, extra: u32) -> u32 {
    if fmt == Fmt::D { extra << 4 | v } else { v << 1 | extra }
}

/// Executes a coprocessor instruction; false if it is not one this FPU
/// has, which the caller makes UNDEFINED. A coprocessor other than the
/// FPU's is absent, which is a NOCP fault, as is the FPU while CPACR
/// leaves it off.
pub(crate) fn execute(m: &mut Machine, hw1: u32, hw2: u32) -> Result<bool, Fault> {
    let instr = hw1 << 16 | hw2;
    if m.core == Core::CortexM0Plus || bits(hw2, 11, 9) != 0b101 {
        return Err(Fault::NoCoprocessor { instr });
    }
    let unconditional = hw1 >> 12 == 0xF;
    let load_store = hw1 & 0xEE00 == 0xEC00 && !unconditional;
    let data = hw1 & 0xEF00 == 0xEE00;
    if !(load_store || data) {
        return Ok(false);
    }
    let access = m.bus.scs.cpacr >> 20 & 3;
    if access == 0 || access == 2 || access == 1 && !m.cpu.privileged() {
        return Err(Fault::NoCoprocessor { instr });
    }
    if unconditional && m.it_active {
        return Ok(false);
    }
    // `ExecuteFPCheck`: the first FP instruction in a context takes up
    // FPDSCR's defaults, and marks the context as using the FPU.
    if m.bus.scs.fpccr & FPCCR_ASPEN != 0 && !m.cpu.fpca {
        m.cpu.fpscr = m.cpu.fpscr & !0x07C0_0000 | m.bus.scs.fpdscr & 0x07C0_0000;
        m.cpu.fpca = true;
    }
    if load_store {
        return if hw1 & 0xFFE0 == 0xEC40 { Ok(transfer64(m, hw1, hw2)) } else { load_store_multiple(m, hw1, hw2) };
    }
    Ok(if unconditional {
        fpv5(m, hw1, hw2)
    } else if bit(hw2, 4) == 1 {
        transfer32(m, hw1, hw2)
    } else {
        data_processing(m, hw1, hw2)
    })
}

/// VMOV between two core registers and two singles or a double.
fn transfer64(m: &mut Machine, hw1: u32, hw2: u32) -> bool {
    let to_core = bit(hw1, 4) == 1;
    let t = bits(hw2, 15, 12);
    let t2 = bits(hw1, 3, 0);
    let vm = bits(hw2, 3, 0);
    let mbit = bit(hw2, 5);
    if bits(hw2, 7, 6) != 0 || bit(hw2, 4) != 1 || to_core && (t == 15 || t2 == 15) {
        return false;
    }
    let (lo, hi) = if bit(hw2, 8) == 1 {
        let d = mbit << 4 | vm;
        if d >= 16 {
            return false;
        }
        (2 * d, 2 * d + 1)
    } else {
        let s = vm << 1 | mbit;
        if s == 31 {
            return false;
        }
        (s, s + 1)
    };
    if to_core {
        let (a, b) = (m.cpu.s[lo as usize], m.cpu.s[hi as usize]);
        m.cpu.set_reg(t as usize, a);
        m.cpu.set_reg(t2 as usize, b);
    } else {
        m.cpu.s[lo as usize] = m.cpu.reg(t as usize);
        m.cpu.s[hi as usize] = m.cpu.reg(t2 as usize);
    }
    true
}

/// VLDR, VSTR, VLDM, VSTM, VPUSH and VPOP.
fn load_store_multiple(m: &mut Machine, hw1: u32, hw2: u32) -> Result<bool, Fault> {
    let p = bit(hw1, 8) == 1;
    let u = bit(hw1, 7) == 1;
    let w = bit(hw1, 5) == 1;
    let load = bit(hw1, 4) == 1;
    let n = bits(hw1, 3, 0);
    let double = bit(hw2, 8) == 1;
    let dbit = bit(hw1, 6);
    let vd = bits(hw2, 15, 12);
    let imm8 = hw2 & 0xFF;
    let first = if double { dbit << 4 | vd } else { vd << 1 | dbit };
    let base = if n == 15 { m.cpu.pc.wrapping_add(4) & !3 } else { m.cpu.reg(n as usize) };

    if p && !w {
        // VLDR and VSTR: one register, an offset either way.
        if double && first >= 16 {
            return Ok(false);
        }
        let addr = if u { base.wrapping_add(imm8 << 2) } else { base.wrapping_sub(imm8 << 2) };
        transfer_registers(m, load, double, first, 1, addr)?;
        return Ok(true);
    }
    // The multiples: increment after, or decrement before writing back.
    // Based on the pc they are UNPREDICTABLE, and QEMU allows them unless
    // they would write it back.
    if p == u || n == 15 && w {
        return Ok(false);
    }
    // An odd count of words with doubles is FLDMX and FSTMX, deprecated:
    // the last word is skipped, but counted in the writeback.
    let count = if double { imm8 / 2 } else { imm8 };
    let limit = if double { 16 } else { 32 };
    if count == 0 || first + count > limit {
        return Ok(false);
    }
    let start = if u { base } else { base.wrapping_sub(imm8 << 2) };
    transfer_registers(m, load, double, first, count, start)?;
    if w {
        let after = if u { base.wrapping_add(imm8 << 2) } else { start };
        m.cpu.set_reg(n as usize, after);
    }
    Ok(true)
}

/// Loads or stores `count` registers from `first` at `addr`, which must be
/// word-aligned. A double is two words, low first.
fn transfer_registers(m: &mut Machine, load: bool, double: bool, first: u32, count: u32, addr: u32) -> Result<(), Fault> {
    if addr & 3 != 0 {
        return Err(Fault::Unaligned { addr });
    }
    let (start, words) = if double { (2 * first, 2 * count) } else { (first, count) };
    for i in 0..words {
        let at = addr.wrapping_add(4 * i);
        let s = (start + i) as usize;
        if load {
            m.cpu.s[s] = m.load(at, 4)?;
        } else {
            m.store(at, 4, m.cpu.s[s])?;
        }
    }
    Ok(())
}

/// VMOV between a core register and a single or a word of a double, and
/// VMRS and VMSR (A6.5).
fn transfer32(m: &mut Machine, hw1: u32, hw2: u32) -> bool {
    let l = bit(hw1, 4) == 1;
    let c = bit(hw2, 8) == 1;
    let a = bits(hw1, 7, 5);
    let t = bits(hw2, 15, 12);
    if bits(hw2, 3, 0) != 0 {
        return false;
    }
    match (l, c, a) {
        (_, false, 0b000) => {
            if bits(hw2, 6, 5) != 0 || l && t == 15 {
                return false;
            }
            let s = (bits(hw1, 3, 0) << 1 | bit(hw2, 7)) as usize;
            if l {
                m.cpu.set_reg(t as usize, m.cpu.s[s]);
            } else {
                m.cpu.s[s] = m.cpu.reg(t as usize);
            }
            true
        }
        (_, false, 0b111) => {
            // VMSR and VMRS: FPSCR is the only register M-profile has.
            if bits(hw1, 3, 0) != 1 || bits(hw2, 7, 5) != 0 {
                return false;
            }
            match (l, t) {
                (false, _) => m.cpu.fpscr = m.cpu.reg(t as usize) & FPSCR_MASK,
                // VMRS APSR_nzcv, FPSCR.
                (true, 15) => {
                    let apsr = m.cpu.apsr() & !0xF000_0000 | m.cpu.fpscr & 0xF000_0000;
                    m.cpu.set_apsr(apsr);
                }
                (true, _) => m.cpu.set_reg(t as usize, m.cpu.fpscr),
            }
            true
        }
        (_, true, a) if a & 0b110 == 0 && bits(hw2, 6, 5) == 0 => {
            // A word of a double.
            let d = bit(hw2, 7) << 4 | bits(hw1, 3, 0);
            if d >= 16 || l && t == 15 {
                return false;
            }
            let s = (2 * d + bit(hw1, 5)) as usize;
            if l {
                m.cpu.set_reg(t as usize, m.cpu.s[s]);
            } else {
                m.cpu.s[s] = m.cpu.reg(t as usize);
            }
            true
        }
        _ => false,
    }
}

/// Floating-point data processing (A6.4): the arithmetic.
fn data_processing(m: &mut Machine, hw1: u32, hw2: u32) -> bool {
    let opc1 = bits(hw1, 7, 4) & 0b1011;
    let fmt = if bit(hw2, 8) == 1 { Fmt::D } else { Fmt::S };
    if opc1 == 0b1011 {
        return other(m, hw1, hw2, fmt);
    }
    let d = reg(fmt, bits(hw2, 15, 12), bit(hw1, 6));
    let n = reg(fmt, bits(hw1, 3, 0), bit(hw2, 7));
    let mm = reg(fmt, bits(hw2, 3, 0), bit(hw2, 5));
    if fmt == Fmt::D && (d >= 16 || n >= 16 || mm >= 16) {
        return false;
    }
    let mut env = Env { fpscr: m.cpu.fpscr };
    let op = bit(hw2, 6) == 1;
    let (x, y, acc) = (m.fp(fmt, n), m.fp(fmt, mm), m.fp(fmt, d));
    let result = match opc1 {
        // VMLA, VMLS: unfused, the product rounded on its own.
        0b0000 => {
            let product = env.binary(fmt, Op::Mul, x, y);
            env.binary(fmt, Op::Add, acc, if op { fmt.neg(product) } else { product })
        }
        // VNMLS, VNMLA.
        0b0001 => {
            let product = env.binary(fmt, Op::Mul, x, y);
            env.binary(fmt, Op::Add, fmt.neg(acc), if op { fmt.neg(product) } else { product })
        }
        // VMUL, VNMUL.
        0b0010 => {
            let product = env.binary(fmt, Op::Mul, x, y);
            if op { fmt.neg(product) } else { product }
        }
        0b0011 => env.binary(fmt, if op { Op::Sub } else { Op::Add }, x, y),
        0b1000 if !op => env.binary(fmt, Op::Div, x, y),
        // VFNMS, VFNMA.
        0b1001 => env.fused(fmt, fmt.neg(acc), if op { fmt.neg(x) } else { x }, y),
        // VFMA, VFMS.
        0b1010 => env.fused(fmt, acc, if op { fmt.neg(x) } else { x }, y),
        _ => return false,
    };
    m.set_fp(fmt, d, result);
    m.cpu.fpscr = env.fpscr;
    true
}

/// The `opc1 = 1x11` group: immediates, moves, the unary operations, the
/// comparisons and the conversions.
fn other(m: &mut Machine, hw1: u32, hw2: u32, fmt: Fmt) -> bool {
    let mut env = Env { fpscr: m.cpu.fpscr };
    let opc2 = bits(hw1, 3, 0);
    let opc3 = bits(hw2, 7, 6);
    let dbit = bit(hw1, 6);
    let vd = bits(hw2, 15, 12);
    let vm = bits(hw2, 3, 0);
    let mbit = bit(hw2, 5);
    let d = reg(fmt, vd, dbit);
    let mm = reg(fmt, vm, mbit);
    let single_d = vd << 1 | dbit;
    let single_m = vm << 1 | mbit;
    let fits = |r: u32, f: Fmt| f != Fmt::D || r < 16;
    if opc3 & 1 == 0 {
        // VMOV immediate.
        if bit(hw2, 7) != 0 || bits(hw2, 5, 4) != 0 || !fits(d, fmt) {
            return false;
        }
        m.set_fp(fmt, d, expand_imm(fmt, opc2 << 4 | vm));
        return true;
    }
    if bit(hw2, 4) != 0 {
        return false;
    }
    let round = env.round();
    let value = m.fp(fmt, mm);
    match (opc2, opc3) {
        // VMOV, VABS, VNEG, VSQRT.
        (0b0000 | 0b0001, _) => {
            if !fits(d, fmt) || !fits(mm, fmt) {
                return false;
            }
            let result = match (opc2, opc3) {
                (0b0000, 0b01) => value,
                (0b0000, _) => fmt.abs(value),
                (_, 0b01) => fmt.neg(value),
                _ => env.sqrt(fmt, value),
            };
            m.set_fp(fmt, d, result);
        }
        // VCVTB and VCVTT: half precision in the bottom or top half of a
        // single register.
        (0b0010 | 0b0011, _) => {
            let top = opc3 & 0b10 != 0;
            if opc2 & 1 == 1 {
                if !fits(mm, fmt) {
                    return false;
                }
                let half = env.convert(fmt, Fmt::H, value) as u32;
                let old = m.cpu.s[single_d as usize];
                m.cpu.s[single_d as usize] = if top { old & 0xFFFF | half << 16 } else { old & 0xFFFF_0000 | half };
            } else {
                if !fits(d, fmt) {
                    return false;
                }
                let source = m.cpu.s[single_m as usize];
                let half = if top { source >> 16 } else { source & 0xFFFF };
                let result = env.convert(Fmt::H, fmt, half as u64);
                m.set_fp(fmt, d, result);
            }
        }
        // VCMP and VCMPE, against a register or against zero.
        (0b0100 | 0b0101, _) => {
            if !fits(d, fmt) || opc2 == 0b0100 && !fits(mm, fmt) || opc2 == 0b0101 && (vm != 0 || mbit != 0) {
                return false;
            }
            let other = if opc2 == 0b0101 { 0 } else { value };
            let nzcv = env.compare(fmt, m.fp(fmt, d), other, opc3 & 0b10 != 0);
            env.fpscr = env.fpscr & 0x0FFF_FFFF | nzcv << 28;
        }
        // VRINTR, VRINTZ, VRINTX.
        (0b0110, _) | (0b0111, 0b01) => {
            if !fits(d, fmt) || !fits(mm, fmt) {
                return false;
            }
            let (mode, exact) = match (opc2, opc3) {
                (0b0110, 0b01) => (round, false),
                (0b0110, _) => (Round::TowardZero, false),
                _ => (round, true),
            };
            let result = env.round_int(fmt, value, mode, exact);
            m.set_fp(fmt, d, result);
        }
        // VCVT between single and double.
        (0b0111, _) => {
            let (from, to, dest, src) =
                if fmt == Fmt::S { (Fmt::S, Fmt::D, dbit << 4 | vd, single_m) } else { (Fmt::D, Fmt::S, single_d, mbit << 4 | vm) };
            if to == Fmt::D && dest >= 16 || from == Fmt::D && src >= 16 {
                return false;
            }
            let result = env.convert(from, to, m.fp(from, src));
            m.set_fp(to, dest, result);
        }
        // VCVT from a 32-bit integer in a single register.
        (0b1000, _) => {
            if !fits(d, fmt) {
                return false;
            }
            let result = env.from_fixed(fmt, m.cpu.s[single_m as usize], opc3 & 0b10 != 0, 32, 0);
            m.set_fp(fmt, d, result);
        }
        // VCVT and VCVTR to a 32-bit integer in a single register.
        (0b1100 | 0b1101, _) => {
            if !fits(mm, fmt) {
                return false;
            }
            let mode = if opc3 & 0b10 != 0 { Round::TowardZero } else { round };
            m.cpu.s[single_d as usize] = env.to_fixed(fmt, value, opc2 & 1 == 1, 32, 0, mode);
        }
        // VCVT between floating and fixed point, in place.
        (0b1010 | 0b1011 | 0b1110 | 0b1111, _) => {
            if !fits(d, fmt) {
                return false;
            }
            let to_fixed = opc2 & 0b0100 != 0;
            let signed = opc2 & 1 == 0;
            let width = if opc3 & 0b10 != 0 { 32 } else { 16 };
            let imm5 = vm << 1 | mbit;
            if width < imm5 {
                return false;
            }
            let frac = width - imm5;
            if to_fixed {
                let result = env.to_fixed(fmt, m.fp(fmt, d), signed, width, frac, Round::TowardZero);
                // Extended to the whole register.
                let wide = match (width, signed) {
                    (16, true) => result as u16 as i16 as i64 as u64,
                    (_, true) => result as i32 as i64 as u64,
                    _ => result as u64,
                };
                m.set_fp(fmt, d, if fmt == Fmt::D { wide } else { wide & 0xFFFF_FFFF });
            } else {
                let result = env.from_fixed(fmt, m.fp(fmt, d) as u32, signed, width, frac);
                m.set_fp(fmt, d, result);
            }
        }
        _ => return false,
    }
    m.cpu.fpscr = env.fpscr;
    true
}

/// FPv5's unconditional instructions: VSEL, VMAXNM and VMINNM, and VRINT
/// and VCVT with a rounding mode of their own.
fn fpv5(m: &mut Machine, hw1: u32, hw2: u32) -> bool {
    if bits(hw1, 11, 8) != 0b1110 || bit(hw2, 4) != 0 {
        return false;
    }
    let fmt = if bit(hw2, 8) == 1 { Fmt::D } else { Fmt::S };
    let d = reg(fmt, bits(hw2, 15, 12), bit(hw1, 6));
    let n = reg(fmt, bits(hw1, 3, 0), bit(hw2, 7));
    let mm = reg(fmt, bits(hw2, 3, 0), bit(hw2, 5));
    let single_d = bits(hw2, 15, 12) << 1 | bit(hw1, 6);
    let fits = |r: u32| fmt != Fmt::D || r < 16;
    let mut env = Env { fpscr: m.cpu.fpscr };
    let mode = |rm: u32| match rm {
        0 => Round::NearestTiesToAway,
        1 => Round::NearestTiesToEven,
        2 => Round::TowardPositive,
        _ => Round::TowardNegative,
    };
    if bit(hw1, 7) == 0 {
        // VSEL: EQ, VS, GE, GT.
        if bit(hw2, 6) != 0 || !fits(d) || !fits(n) || !fits(mm) {
            return false;
        }
        let cond = [0x0, 0x6, 0xA, 0xC][bits(hw1, 5, 4) as usize];
        let chosen = if m.condition(cond) { n } else { mm };
        let value = m.fp(fmt, chosen);
        m.set_fp(fmt, d, value);
        return true;
    }
    match bits(hw1, 5, 4) {
        0b00 => {
            if !fits(d) || !fits(n) || !fits(mm) {
                return false;
            }
            let result = env.max_min(fmt, m.fp(fmt, n), m.fp(fmt, mm), bit(hw2, 6) == 0);
            m.set_fp(fmt, d, result);
        }
        0b11 if bits(hw1, 3, 2) == 0b10 && bits(hw2, 7, 6) == 0b01 => {
            if !fits(d) || !fits(mm) {
                return false;
            }
            let result = env.round_int(fmt, m.fp(fmt, mm), mode(bits(hw1, 1, 0)), false);
            m.set_fp(fmt, d, result);
        }
        0b11 if bits(hw1, 3, 2) == 0b11 && bit(hw2, 6) == 1 => {
            if !fits(mm) {
                return false;
            }
            let signed = bit(hw2, 7) == 1;
            m.cpu.s[single_d as usize] = env.to_fixed(fmt, m.fp(fmt, mm), signed, 32, 0, mode(bits(hw1, 1, 0)));
        }
        _ => return false,
    }
    m.cpu.fpscr = env.fpscr;
    true
}
