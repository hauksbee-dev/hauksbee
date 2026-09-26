//! Shared fixture builders for the `checks::*` unit-test modules.
//!
//! Several `checks/*.rs` files each hand-rolled an identical `Pin`/`ExtractedBoard`
//! builder inside their own `#[cfg(test)] mod tests`. This module holds the
//! byte-for-byte-identical ones so they are defined once; a file whose
//! builder differs in some real way (e.g. `boot.rs`'s single-argument `pin`,
//! or `ampacity.rs`'s named-board `board`) keeps its own local version.
//!
//! Gated by `#[cfg(test)]` on the `mod test_support;` declaration in
//! `checks/mod.rs`, not here, to avoid a duplicated-attribute lint.

use hauksbee_extract::{Component, ExtractedBoard};

/// A pin at `number` wired to `net`.
pub use crate::test_support::pin;

/// A component at `reference`/`value` whose pads are `(pad number, net id)`.
pub fn part(reference: &str, value: &str, pads: &[(&str, i64)]) -> Component {
    Component {
        reference: reference.into(),
        value: value.into(),
        pins: pads.iter().map(|(n, net)| pin(n, *net)).collect(),
        ..Default::default()
    }
}

/// A board named `"t"` built from `(net id, net name)` pairs plus components.
pub fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
    crate::test_support::board("t", nets, components)
}
