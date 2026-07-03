//! Run diagnostics: which robustness-ladder strategies actually FIRED.
//!
//! The plan (dev-plan 02 section 2.6) demands that ladder behavior be
//! observable without env vars. This is the simplest honest mechanism: a
//! thread-local bitmask noted at the exact program points where a granted
//! strategy's code path engages (not where it is merely permitted), drained
//! by whoever owns the run window. Solves run on the caller's thread, so a
//! drain around a solve brackets that solve. The step census (debug estate)
//! prints the pending set in its march report; programmatic callers use
//! [`take_strategy_activations`]. A bit-OR per activation, no allocation, no
//! env var, no framework.

use crate::options::Strategy;
use std::cell::Cell;

thread_local! {
    static ACTIVATED: Cell<u16> = const { Cell::new(0) };
}

/// Note that a strategy's code path engaged on this thread.
pub(crate) fn note(s: Strategy) {
    ACTIVATED.with(|c| c.set(c.get() | s.bit()));
}

/// Strategies that fired on this thread since the last drain, draining them.
pub fn take_strategy_activations() -> Vec<Strategy> {
    let bits = ACTIVATED.with(|c| c.replace(0));
    Strategy::ALL
        .into_iter()
        .filter(|s| bits & s.bit() != 0)
        .collect()
}

/// Strategies that fired on this thread since the last drain, without
/// draining (the census report uses this so it never steals a caller's
/// window).
pub fn peek_strategy_activations() -> Vec<Strategy> {
    let bits = ACTIVATED.with(|c| c.get());
    Strategy::ALL
        .into_iter()
        .filter(|s| bits & s.bit() != 0)
        .collect()
}
