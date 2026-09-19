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

use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

/// A pin at `number` wired to `net`.
pub(crate) fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.into(),
        net: Some(net),
        function: String::new(),
        kind: String::new(),
        position: None,
    }
}

/// A board named `"t"` built from `(net id, net name)` pairs plus components.
pub(crate) fn board(nets: &[(i64, &str)], components: Vec<Component>) -> ExtractedBoard {
    ExtractedBoard {
        name: "t".into(),
        nets: nets
            .iter()
            .map(|(id, name)| Net {
                id: *id,
                name: (*name).into(),
            })
            .collect(),
        components,
    }
}
