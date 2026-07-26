//! The internal-diagnostics channel.
//!
//! Solver/engine internals sometimes want to say something that is true and
//! useful *to a hauksbee developer* but is noise, or worse, a trust wound, in
//! a user's CI log: dev-plan references, "not stamped yet" caveats, emulator
//! stack dumps. The persona-validation panel caught two of these reaching
//! user-facing CI output (`[effects] ... (dev-plan 04 §3.2)` from the diode
//! stamp, and simavr's `avr_sadly_crashed` crash dump).
//!
//! This module is the single boundary those notes must pass through. By default
//! they are *silent*. They print to stderr only when `HAUKSBEE_DEBUG` is set in
//! the environment (any non-empty value). This is a channel split, not a
//! deletion: the information still exists for whoever is debugging the engine,
//! it just no longer bleeds into the default user surface.
//!
//! Anything a *user* needs to act on (a chip substitution, an unresolved part, a
//! refuse-rather-than-fake INVALID) is NOT an internal note and must keep going
//! to its normal stderr path; this channel is only for engine-internal chatter.

use std::sync::OnceLock;

/// Whether the internal-diagnostics channel is open (i.e. `HAUKSBEE_DEBUG` is
/// set to a non-empty value). Read once and cached: the env does not change
/// mid-process and this is hit from hot stamp paths.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("HAUKSBEE_DEBUG")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    })
}

/// Emit an engine-internal note to the debug channel. A no-op unless
/// [`enabled`]. `tag` is a short channel label (e.g. `"effects"`), rendered as
/// `[tag] msg`, matching the pre-existing `[effects]` prefix.
pub fn note(tag: &str, msg: &str) {
    if enabled() {
        eprintln!("[{tag}] {msg}");
    }
}
