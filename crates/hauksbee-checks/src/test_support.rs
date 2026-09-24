//! Shared test-only fixture builders for `hauksbee-checks`'s check modules.
//!
//! Every `checks::*` module tests against synthetic `ExtractedBoard`s built
//! from the same handful of primitives (a numbered `Pin`, a named
//! `Component`, a board of `(net id, net name)` pairs). Centralising them
//! here keeps each check's own test module focused on the scenario under
//! test rather than re-deriving `Pin`/`Component`/`ExtractedBoard`
//! boilerplate.

use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};

/// A pin with the given pad `number` on `net`, with no schematic name/type
/// and no board position.
pub(crate) fn pin(number: &str, net: i64) -> Pin {
    Pin {
        number: number.into(),
        net: Some(net),
        ..Default::default()
    }
}

/// A pin like [`pin`], but with a schematic pin function name set (e.g. a
/// named CC/D+/D- pad).
pub(crate) fn pin_fn(number: &str, net: i64, function: &str) -> Pin {
    Pin {
        function: function.into(),
        ..pin(number, net)
    }
}

/// A two-terminal `Device:R` resistor in an `R_0603` footprint: pad 1 on net
/// 1, pad 2 on net 2, populated (not DNP), no extra properties.
pub(crate) fn two_pin_resistor(reference: &str, value: &str) -> Component {
    Component {
        reference: reference.into(),
        value: value.into(),
        lib_id: "Device:R".into(),
        footprint: "R_0603".into(),
        layer: "F.Cu".into(),
        pins: vec![pin("1", 1), pin("2", 2)],
        ..Default::default()
    }
}

/// A board named `name`, with `nets` as `(id, name)` pairs and the given
/// components.
pub(crate) fn board(
    name: &str,
    nets: &[(i64, &str)],
    components: Vec<Component>,
) -> ExtractedBoard {
    ExtractedBoard {
        name: name.into(),
        nets: nets.iter().map(|(id, n)| Net::new(*id, *n)).collect(),
        components,
    }
}
