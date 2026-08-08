//! A regulator that regulates below ground must validate.
//!
//! `vout` is stamped as a DC source against ground, so its sign carries the
//! whole meaning of a 79xx or any dual-supply rail. Bounding it positive-only
//! is worse than refusing the part: the only way to satisfy such a bound is to
//! drop the sign, and a -5 V regulator recorded as +5 V validates, binds
//! cleanly, and turns every op-amp's negative supply into a second positive
//! one. Nothing downstream can tell the difference.

use std::collections::BTreeMap;

use hauksbee_models::schema::{ComponentKind, ModelEntry, Params};
use hauksbee_models::validation::validate;

fn vreg(vout: f64) -> ModelEntry {
    let mut params = Params::default();
    params.set_f64("vout", vout);
    params.set_f64("dropout_v", 1.1);
    params.set_f64("iq_a", 0.001);
    let mut pins = BTreeMap::new();
    pins.insert("1".to_string(), "gnd".to_string());
    pins.insert("2".to_string(), "in".to_string());
    pins.insert("3".to_string(), "out".to_string());
    ModelEntry {
        id: "test_vreg".into(),
        kind: ComponentKind::Vreg,
        description: String::new(),
        r#match: Default::default(),
        params,
        pins,
        ratings: Default::default(),
        straps: Vec::new(),
        behavioral: Default::default(),
        logic: Default::default(),
        current_program: None,
        passive_class: None,
    }
}

#[test]
fn a_negative_rail_is_valid() {
    for v in [-5.0, -12.0, -15.0, -3.3] {
        assert!(
            validate(&vreg(v)).is_ok(),
            "{v} V is a real regulator output, not a typo"
        );
    }
}

#[test]
fn a_positive_rail_is_still_valid() {
    for v in [3.3, 5.0, 12.0, 24.0] {
        assert!(validate(&vreg(v)).is_ok(), "{v} V must still pass");
    }
}

#[test]
fn a_magnitude_out_of_range_is_still_refused_on_both_signs() {
    // The bound still has to do its job: a hallucinated 400 V LDO is caught
    // whichever way its sign points, and so is a rail too small to be one.
    for v in [400.0, -400.0, 0.1, -0.1, 0.0] {
        assert!(
            validate(&vreg(v)).is_err(),
            "{v} V is not a plausible regulator output"
        );
    }
}

#[test]
fn a_non_finite_rail_is_refused() {
    // NaN defeats every naive comparison, and inf would stamp an infinite
    // source into the solver.
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(validate(&vreg(v)).is_err(), "{v} must never validate");
    }
}
