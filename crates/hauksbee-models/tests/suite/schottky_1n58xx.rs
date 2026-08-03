//! The 1N581x Schottkys, and the two-point fit that needs a stamped `rs`.
//!
//! A rectifier's datasheet publishes its forward voltage at two currents, and
//! the rise between them is bulk resistance rather than the exponential. Any
//! model can be made to sit on one point. Sitting on both is what says the part
//! will behave at the current a board actually runs it at.

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

/// The Shockley forward voltage the entry's own parameters imply at `i`.
fn vf_at(m: &hauksbee_models::ModelEntry, i: f64) -> f64 {
    let vt = 0.02585;
    let is = m.params.get_f64("is").expect("is");
    let n = m.params.get_f64("n").expect("n");
    let rs = m.params.get_f64("rs").expect("rs");
    n * vt * (i / is).ln() + i * rs
}

#[test]
fn each_part_sits_on_both_published_forward_points() {
    // Vishay 88525, ELECTRICAL CHARACTERISTICS: VF at 1.0 A and at 3.1 A.
    for (part, vf1, vf31) in [
        ("1N5817", 0.450, 0.750),
        ("1N5818", 0.550, 0.875),
        ("1N5819", 0.600, 0.900),
    ] {
        let m = resolve(part);
        for (i, want) in [(1.0, vf1), (3.1, vf31)] {
            let got = vf_at(&m, i);
            assert!(
                (got - want).abs() < 2e-3,
                "{part} at {i} A: model says {got:.4} V, datasheet says {want} V"
            );
        }
    }
}

#[test]
fn the_series_resistance_is_what_carries_the_rise() {
    // The point of the whole exercise. Delete rs and the two published points
    // become unreachable together: the exponential alone cannot span them
    // without an unphysical ideality factor.
    for part in ["1N5817", "1N5818", "1N5819"] {
        let m = resolve(part);
        let rs = m.params.get_f64("rs").expect("rs");
        assert!(
            rs > 0.05,
            "{part} needs a real bulk resistance, got {rs} ohm"
        );
        let n = m.params.get_f64("n").expect("n");
        assert!(
            (0.9..=2.0).contains(&n),
            "{part} ideality {n} should stay physical; a fit without rs drives it past 10"
        );
    }
}

#[test]
fn the_reverse_rating_matches_the_part_number() {
    // 17, 18 and 19 differ in blocking voltage and nothing else structural, so a
    // shared regex or a copied entry would show up right here.
    for (part, vrrm) in [("1N5817", 20.0), ("1N5818", 30.0), ("1N5819", 40.0)] {
        let m = resolve(part);
        assert_eq!(m.params.get_f64("bv"), Some(vrrm), "{part} VRRM");
        assert_eq!(m.ratings.max_voltage_v, Some(vrrm), "{part} rating");
    }
}

#[test]
fn a_schottky_is_not_the_generic_fallback() {
    for part in ["1N5817", "1N5818", "1N5819"] {
        let m = resolve(part);
        assert_eq!(m.id, part.to_lowercase(), "{part} fell through to {}", m.id);
    }
}
