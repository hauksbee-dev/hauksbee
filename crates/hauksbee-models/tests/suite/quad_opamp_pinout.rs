//! The LM324's pin map, which is the field worth testing on a quad op-amp.
//!
//! Its supplies are not where a reader expects them. A 741 or an LM358 puts
//! power at the corners; the LM324 puts VCC+ at pin 4 and VCC- at pin 11, in the
//! middle of each side. A map copied from the dual would bind both supplies to
//! signal pins and simulate a different circuit while binding cleanly.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolve(value: &str) -> hauksbee_models::ModelEntry {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery {
        value: Some(value.into()),
        ..Default::default()
    };
    lib.resolve(&q)
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve to any model"))
}

#[test]
fn the_supplies_sit_mid_side_not_at_the_corners() {
    // TI SLOS066AE, Table 5-1 "Pin Functions", 14-pin packages.
    let m = resolve("LM324");
    assert_eq!(m.pins.get("4").map(String::as_str), Some("vcc"));
    assert_eq!(m.pins.get("11").map(String::as_str), Some("vss"));
    // And nothing else claims to be a supply.
    let supplies: Vec<&String> = m
        .pins
        .iter()
        .filter(|(_, r)| *r == "vcc" || *r == "vss")
        .map(|(p, _)| p)
        .collect();
    assert_eq!(supplies.len(), 2, "exactly two supply pins: {supplies:?}");
}

#[test]
fn all_four_amplifiers_are_mapped() {
    let m = resolve("LM324");
    for unit in ["a", "b", "c", "d"] {
        for role in ["out", "in_minus", "in_plus"] {
            let want = format!("{role}_{unit}");
            assert!(
                m.pins.values().any(|r| *r == want),
                "no pin carries {want}; a quad needs all four units"
            );
        }
    }
    assert_eq!(m.pins.len(), 14, "a 14-pin part maps 14 pins");
}

#[test]
fn the_quad_does_not_swallow_the_dual() {
    // LM358 is the two-amplifier sibling with a different map. If the quad's
    // regex reached it, every LM358 on a board would bind to the wrong pins.
    assert_eq!(resolve("LM358").id, "lm358");
    assert_eq!(resolve("LM324").id, "lm324");
    assert_eq!(resolve("LM324N").id, "lm324");
    assert_eq!(resolve("LM224").id, "lm324");
}
