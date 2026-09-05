//! The shared vocabulary of the spec validators (`validation`, `behavioral`,
//! `sensor_spec`, `logic_spec`): the numeric gates every solver-facing float
//! passes through, the identifier rule expression names obey, and a collector
//! so a validation walk reads as one line per rule instead of five.
//!
//! NaN and the infinities fail every ordinary comparison (`nan <= 0.0` is
//! false), so a bare `v <= 0.0` gate lets a `nan` TOML literal straight into
//! the solver. Every gate here checks finiteness first, once, for all of them.

use std::fmt::Display;

/// Finite and strictly positive: resistances, setpoints, limits, ratings.
pub(crate) fn positive_finite(v: f64) -> bool {
    v.is_finite() && v > 0.0
}

/// Finite and non-negative: currents, times, thresholds that may be zero.
pub(crate) fn nonneg_finite(v: f64) -> bool {
    v.is_finite() && v >= 0.0
}

/// An expression identifier: `[A-Za-z_][A-Za-z0-9_]*`.
pub(crate) fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The bare identifiers in an `evalexpr` expression: every maximal run of
/// `[A-Za-z0-9_]` that starts with a letter or underscore, minus the sandbox's
/// builtin function names. Conservative on purpose: numbers and operators are
/// ignored, only undeclared *names* are worth flagging.
pub(crate) fn expr_identifiers(expr: &str) -> impl Iterator<Item = &str> {
    const BUILTINS: &[&str] = &["math", "min", "max", "abs", "round", "floor", "ceil", "if"];
    expr.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|tok| {
            !tok.is_empty()
                && tok.starts_with(|c: char| c.is_alphabetic() || c == '_')
                && !BUILTINS.contains(tok)
        })
}

/// The problems one validation walk found, in the order they were found.
#[derive(Debug, Default)]
pub(crate) struct Problems(pub Vec<String>);

impl Problems {
    pub fn push(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }

    /// Record `msg()` unless `ok`.
    pub fn require(&mut self, ok: bool, msg: impl FnOnce() -> String) {
        if !ok {
            self.0.push(msg());
        }
    }

    /// `what` must be a positive finite number.
    pub fn positive(&mut self, what: impl Display, v: f64) {
        self.require(positive_finite(v), || {
            format!("{what} must be a positive finite number, got {v}")
        });
    }

    /// `what` must be a non-negative finite number.
    pub fn nonneg(&mut self, what: impl Display, v: f64) {
        self.require(nonneg_finite(v), || {
            format!("{what} must be a non-negative finite number, got {v}")
        });
    }

    /// `what` must be finite (either sign is legal).
    pub fn finite(&mut self, what: impl Display, v: f64) {
        self.require(v.is_finite(), || format!("{what} must be finite, got {v}"));
    }

    /// `what` must be a non-empty string.
    pub fn non_empty(&mut self, what: impl Display, s: &str) {
        self.require(!s.trim().is_empty(), || format!("{what} is empty"));
    }
}
