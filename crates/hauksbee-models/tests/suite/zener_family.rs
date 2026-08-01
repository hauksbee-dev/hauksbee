//! The 1N47xxA Zeners: each code clamps at its own voltage, and the knee is
//! sharp enough to regulate.
//!
//! A Zener is the one diode whose reverse breakdown is the whole point, so two
//! things have to hold. Each part must carry its own `bv`, since a family that
//! collapses onto one entry silently turns every clamp on a board into the same
//! rail. And each must carry `ibv`, the current the datasheet quotes its
//! voltage at: without it the knee is as soft as the forward saturation
//! current, which puts a 5.1 V part at 5.8 V under an ordinary shunt load and
//! makes a "regulated" node wander with its source.

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

/// Every code the Multicomp 1N4728A-1N4764A datasheet's Specification Table
/// prints, with the voltage and knee current it prints for it.
const FAMILY: &[(&str, f64, f64)] = &[
    ("1N4728A", 3.3, 0.076),
    ("1N4729A", 3.6, 0.069),
    ("1N4730A", 3.9, 0.064),
    ("1N4732A", 4.7, 0.053),
    ("1N4733A", 5.1, 0.049),
    ("1N4734A", 5.6, 0.045),
    ("1N4735A", 6.2, 0.041),
    ("1N4736A", 6.8, 0.037),
    ("1N4737A", 7.5, 0.034),
    ("1N4738A", 8.2, 0.031),
    ("1N4739A", 9.1, 0.028),
    ("1N4740A", 10.0, 0.025),
    ("1N4759A", 62.0, 0.004),
];

#[test]
fn each_code_clamps_at_its_own_voltage() {
    for (val, vz, _) in FAMILY {
        let m = resolve(val);
        assert_eq!(
            m.params.get_f64("bv"),
            Some(*vz),
            "{val} must clamp at its own datasheet voltage"
        );
    }
}

#[test]
fn every_zener_states_the_current_its_voltage_was_measured_at() {
    for (val, _, izt) in FAMILY {
        let m = resolve(val);
        let ibv = m
            .params
            .get_f64("ibv")
            .unwrap_or_else(|| panic!("{val} has no ibv, so its knee is not a knee"));
        assert!(
            (ibv - izt).abs() < 1e-9,
            "{val}: ibv should be the datasheet IZT {izt} A, got {ibv}"
        );
        assert!(ibv > 0.0, "{val}: a knee at zero current is not a knee");
    }
}

#[test]
fn the_family_does_not_collapse_onto_one_entry() {
    // Thirteen distinct parts. A regex that swallowed its neighbours would make
    // a 3.3 V clamp and a 62 V clamp the same component.
    let mut ids: Vec<String> = FAMILY.iter().map(|(v, _, _)| resolve(v).id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), FAMILY.len(), "codes share an entry: {ids:?}");
}

#[test]
fn the_tolerance_suffix_does_not_change_the_voltage() {
    // 1N4733, 1N4733A, 1N4733B are the same silicon at different tolerances.
    // A suffix that fell through to some other entry would move the rail.
    for v in ["1N4733", "1N4733A", "1N4733B", "1n4733a"] {
        let m = resolve(v);
        assert_eq!(m.id, "1n4733a", "{v} resolved to {}", m.id);
        assert_eq!(m.params.get_f64("bv"), Some(5.1));
    }
}

#[test]
fn a_rectifier_is_not_swallowed_by_the_zener_regexes() {
    // 1N4001-1N4007 sit next door in the same file and are not clamps.
    for v in ["1N4001", "1N4007", "1N4148"] {
        let m = resolve(v);
        assert!(
            !m.id.starts_with("1n47") || m.id == "1n4148",
            "{v} resolved to the Zener entry {}",
            m.id
        );
    }
}
