//! Which bytecode ops a run dispatches, and which follow which.
//!
//! The accelerator's switch loop is most of the engine's time on a covered
//! workload, and the switch is already the cheapest dispatch wasm offers
//! (`docs/performance.md`, 6d). What is left is fewer dispatches — two ops
//! that always run together made one — and cheaper work per op. Both are
//! chosen by counting: the op mix says where per-op work goes, and the most
//! frequent adjacent pairs say which fusions would remove the most
//! dispatches.
//!
//! **Off unless asked for**, like [`crate::histogram`], and for the same
//! reason: it runs once per dispatched op.

#[cfg(feature = "opcount")]
use std::cell::RefCell;

#[cfg(feature = "opcount")]
thread_local! {
    static COUNTS: RefCell<(Vec<u64>, Vec<u64>, u8)> = RefCell::new((vec![0; 64], vec![0; 64 * 64], 63));
}

/// Counts one dispatched op.
#[cfg(feature = "opcount")]
pub fn record(op: u8) {
    COUNTS.with_borrow_mut(|(ops, pairs, previous)| {
        ops[op as usize & 63] += 1;
        pairs[(*previous as usize) * 64 + (op as usize & 63)] += 1;
        *previous = op & 63;
    });
}

#[cfg(not(feature = "opcount"))]
#[inline(always)]
pub fn record(_op: u8) {}

/// A trace was entered: the next op follows nothing.
#[cfg(feature = "opcount")]
pub fn entered() {
    COUNTS.with_borrow_mut(|(_, _, previous)| *previous = 63);
}

#[cfg(not(feature = "opcount"))]
#[inline(always)]
pub fn entered() {}

#[cfg(not(feature = "opcount"))]
pub fn report(_name: impl Fn(u8) -> String) -> Option<String> {
    None
}

/// The op mix and the commonest pairs.
#[cfg(feature = "opcount")]
pub fn report(name: impl Fn(u8) -> String) -> Option<String> {
    use core::fmt::Write;
    let (ops, pairs) = COUNTS.with_borrow(|(ops, pairs, _)| (ops.clone(), pairs.clone()));
    let total: u64 = ops.iter().sum();
    if total == 0 {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "\n{total} bytecode ops dispatched\n\n  {:<14} {:>12} {:>7}", "op", "count", "share");
    let mut ranked: Vec<(usize, u64)> = ops.iter().copied().enumerate().filter(|(_, c)| *c > 0).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    for (op, count) in ranked.iter().take(30) {
        let _ = writeln!(out, "  {:<14} {:>12} {:>6.2}%", name(*op as u8), count, *count as f64 / total as f64 * 100.0);
    }
    let mut ranked: Vec<(usize, u64)> = pairs.iter().copied().enumerate().filter(|(i, c)| *c > 0 && i / 64 != 63).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let _ = writeln!(out, "\n  {:<28} {:>12} {:>7}", "pair", "count", "share");
    for (pair, count) in ranked.iter().take(40) {
        let label = format!("{} → {}", name((pair / 64) as u8), name((pair % 64) as u8));
        let _ = writeln!(out, "  {:<28} {:>12} {:>6.2}%", label, count, *count as f64 / total as f64 * 100.0);
    }
    Some(out)
}
