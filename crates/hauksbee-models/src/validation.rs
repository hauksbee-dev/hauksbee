//! Physical-range validation for model parameters. Used by the extraction
//! pipeline to reject LLM-generated model entries whose parameters are missing or
//! out of physical bounds before they are saved to the user database, so a
//! hallucinated part value cannot silently enter the model library. [`validate`]
//! collects every violation at once rather than stopping at the first.

use crate::schema::{ComponentKind, ModelEntry};
use thiserror::Error;

/// A validation error.
#[derive(Debug, Error)]
#[error("model '{id}': {message}")]
pub struct ValidationError {
    pub id: String,
    pub message: String,
}

/// Validate a [`ModelEntry`], checking that required params are present and
/// within physical bounds.
///
/// Returns `Ok(())` on success, or a list of violations.
pub fn validate(entry: &ModelEntry) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    macro_rules! require_f64 {
        ($key:expr) => {
            if entry.params.get_f64($key).is_none() {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("missing required param '{}'", $key),
                });
            }
        };
    }

    macro_rules! check_range {
        ($key:expr, $min:expr, $max:expr) => {
            if let Some(v) = entry.params.get_f64($key) {
                // A non-finite value (NaN / ±inf) slips through `v < min || v > max`
                // because every IEEE comparison against NaN is false, NaN is
                // neither below-min nor above-max, so it must be rejected up front
                // or a `nan`/`inf` TOML literal defeats the whole physical-bounds
                // gate and propagates into the solver.
                if !v.is_finite() || v < $min || v > $max {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "param '{}' = {} is outside physical range [{}, {}]",
                            $key, v, $min, $max
                        ),
                    });
                }
            }
        };
    }

    // Require `$lo` < `$hi` when both are present. A swapped/degenerate pair
    // (e.g. an opamp with rail_lo=5, rail_hi=0) is otherwise accepted as valid
    // and gives the solver an empty/inverted saturation band, silently pinning
    // the output. Only checked when both parse; the require_f64! calls report a
    // missing member on their own.
    macro_rules! check_order {
        ($lo:expr, $hi:expr) => {
            if let (Some(lo), Some(hi)) = (entry.params.get_f64($lo), entry.params.get_f64($hi)) {
                if lo >= hi {
                    errors.push(ValidationError {
                        id: entry.id.clone(),
                        message: format!(
                            "param '{}' = {} must be strictly less than '{}' = {}",
                            $lo, lo, $hi, hi
                        ),
                    });
                }
            }
        };
    }

    match entry.kind {
        ComponentKind::Diode => {
            require_f64!("is");
            require_f64!("n");
            require_f64!("rs");
            check_range!("is", 1e-20, 1e-3);
            check_range!("n", 0.5, 3.0);
            check_range!("rs", 0.0, 1000.0);
            check_range!("cjo", 0.0, 1e-6);
        }
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
            require_f64!("is");
            require_f64!("bf");
            require_f64!("nf");
            require_f64!("vaf");
            check_range!("is", 1e-20, 1e-3);
            check_range!("bf", 1.0, 2000.0);
            check_range!("nf", 0.5, 3.0);
            check_range!("vaf", 1.0, 500.0);
            check_range!("rb", 0.0, 1e6);
            check_range!("rc", 0.0, 1e6);
            check_range!("re", 0.0, 1e6);
        }
        ComponentKind::Nmos | ComponentKind::Pmos => {
            require_f64!("vto");
            require_f64!("kp");
            check_range!("vto", -10.0, 10.0);
            // kp = k'·(W/L) for the level-1 SPICE model. For a discrete POWER
            // MOSFET the effective W/L is enormous, so kp legitimately runs into
            // the tens or hundreds (the repo's own datasheet-cited db/mosfet.toml
            // has kp up to 200 for ipa045n10n3g). A 1.0 A/V² ceiling false-flagged
            // 6 of 8 shipped models and rejected any correctly-extracted power
            // FET. Bound generously, still catches a nonsense hallucination.
            check_range!("kp", 1e-6, 1000.0);
            check_range!("lambda", 0.0, 1.0);
        }
        ComponentKind::Vreg => {
            require_f64!("vout");
            require_f64!("dropout_v");
            require_f64!("iq_a");
            check_range!("vout", 0.5, 30.0);
            check_range!("dropout_v", 0.0, 10.0);
            check_range!("iq_a", 0.0, 1.0);
        }
        ComponentKind::Opamp => {
            require_f64!("gain");
            require_f64!("rail_lo");
            require_f64!("rail_hi");
            check_range!("gain", 1.0, 1e9);
            check_range!("rail_lo", -60.0, 60.0);
            check_range!("rail_hi", -60.0, 60.0);
            check_order!("rail_lo", "rail_hi");
        }
        ComponentKind::Comparator => {
            require_f64!("out_lo");
            require_f64!("out_hi");
            require_f64!("hysteresis");
            check_range!("hysteresis", 0.0, 5.0);
            check_range!("out_lo", -60.0, 60.0);
            check_range!("out_hi", -60.0, 60.0);
            check_order!("out_lo", "out_hi");
        }
        ComponentKind::AnalogSwitch => {
            require_f64!("ron");
            require_f64!("roff");
            check_range!("ron", 0.01, 10_000.0);
            check_range!("roff", 1e3, 1e12);
            // On-resistance must be far below off-resistance; the two ranges
            // overlap ([0.01,1e4] vs [1e3,1e12]), so a swapped/degenerate pair
            // (ron=5000, roff=2000) is representable and would model a switch that
            // conducts MORE when open, an inverted transmission gate the solver
            // routes the wrong way. Same hazard the R35 opamp/comparator order
            // checks close.
            check_order!("ron", "roff");
        }
        // Digital / MCU / connector / ignore: no mandatory numeric params
        _ => {}
    }

    check_required_pins(entry, &mut errors);

    // Absolute-maximum ratings gate the engine's stress/destruction faults. A
    // NaN, negative, or zero rating passes every kind-specific check above (which
    // only look at `params`, never `ratings`), then silently disables the fault:
    // the stress monitor computes `if limit > 0.0 { value/limit } else { 0.0 }`,
    // so a NaN (NaN>0 is false) or non-positive limit yields frac 0 and the
    // Overcurrent/Overvoltage/Overpower check never trips, an unprotected part
    // that validated clean. Reject any present rating that is not positive-finite.
    for (name, rating) in [
        ("max_current_a", entry.ratings.max_current_a),
        ("max_surge_current_a", entry.ratings.max_surge_current_a),
        ("max_power_w", entry.ratings.max_power_w),
        ("max_voltage_v", entry.ratings.max_voltage_v),
        ("max_pin_current_a", entry.ratings.max_pin_current_a),
        ("max_ripple_current_a", entry.ratings.max_ripple_current_a),
        ("max_junction_temp_c", entry.ratings.max_junction_temp_c),
        // The thermal resistances are solver-facing and UNFLOORED: thermal.rs
        // computes `Tj = ambient + power.max(0)*theta_ja`, so a negative/NaN
        // theta drives Tj at or below ambient and the Overtemperature fault never
        // trips (frac.max(0) = 0). Gate them like the other ratings (R52 missed
        // these two).
        ("theta_ja_c_per_w", entry.ratings.theta_ja_c_per_w),
        ("theta_jc_c_per_w", entry.ratings.theta_jc_c_per_w),
    ] {
        if let Some(v) = rating {
            if !v.is_finite() || v <= 0.0 {
                errors.push(ValidationError {
                    id: entry.id.clone(),
                    message: format!("rating '{name}' = {v} must be a positive finite number"),
                });
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// When an entry supplies an explicit `[models.pins]` map, verify it carries the
/// signal roles the binder needs for that kind. A role typo (`"1" = "anmode"`)
/// otherwise passes every check above, then binds the part OPEN at run time with
/// a misleading "pin not connected" message; the exact trap a first-time part
/// author hits. This checks only that each REQUIRED role is PRESENT (under any
/// binder-accepted alias / channel suffix); it never flags EXTRA pins, so a
/// legitimately-declared power/NC pin (an op-amp's `vcc`/`vee`) is fine. An
/// empty pins map is the footprint/pin-rules inference path and is left alone.
fn check_required_pins(entry: &ModelEntry, errors: &mut Vec<ValidationError>) {
    if entry.pins.is_empty() {
        return;
    }
    // A behavioral part (converter/FSM/DAC power IC) references its pins from the
    // [models.behavioral] block by arbitrary datasheet names (e.g. an LTC4020's
    // `bat`/`pvin`), NOT through the simple analog binder, so the canonical
    // anchor roles do not apply. Leave it alone.
    if !entry.behavioral.is_empty() {
        return;
    }
    // Each inner slice is one required role; the model satisfies it by mapping
    // some pin to ANY name in the slice, after normalization. Names are the
    // binder's accepted aliases (see bind_diode/bjt/mosfet/vreg/opamp/comparator
    // in hauksbee-engine). analog_switch is deliberately EXCLUDED: it binds
    // SPST (`in_out_a`/`in_out_b`) and SPDT (`com`/`s0`/`s1`) forms with too
    // varied a vocabulary to anchor-check without false positives.
    let required: &[&[&str]] = match entry.kind {
        ComponentKind::Diode => &[&["anode", "a", "p"], &["cathode", "k", "n"]],
        ComponentKind::BjtNpn | ComponentKind::BjtPnp => {
            &[&["collector", "c"], &["base", "b"], &["emitter", "e"]]
        }
        ComponentKind::Nmos | ComponentKind::Pmos => {
            &[&["drain", "d"], &["gate", "g"], &["source", "s"]]
        }
        ComponentKind::Vreg => &[&["out"]],
        ComponentKind::Opamp | ComponentKind::Comparator => &[
            &["out"],
            &["in_plus", "inp", "in+"],
            &["in_minus", "inn", "in-"],
        ],
        // Kinds whose pin vocabulary is open or handled elsewhere (analog_switch,
        // digital, mcu, dac, adc, shift_register, connector, passive, ignore).
        _ => return,
    };

    // Normalize each declared role: lowercase, strip a trailing channel suffix
    // (`_a`..`_d` or `_q<N>`), then a trailing digit run + underscore. This
    // folds the binder's channel variants onto the base role: `out_1`->`out`,
    // `d1`/`d2`->`d` (dual MOSFET), `collector_q2`->`collector`.
    let normalize = |role: &str| -> String {
        let mut r = role.to_ascii_lowercase();
        for sfx in ["_a", "_b", "_c", "_d"] {
            if let Some(base) = r.strip_suffix(sfx) {
                r = base.to_string();
                break;
            }
        }
        if let Some(idx) = r.rfind("_q") {
            if idx + 2 < r.len() && r[idx + 2..].chars().all(|c| c.is_ascii_digit()) {
                r = r[..idx].to_string();
            }
        }
        r = r.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
        r.trim_end_matches('_').to_string()
    };
    let declared: std::collections::HashSet<String> =
        entry.pins.values().map(|role| normalize(role)).collect();

    for role_family in required {
        if !role_family.iter().any(|name| declared.contains(*name)) {
            errors.push(ValidationError {
                id: entry.id.clone(),
                message: format!(
                    "[models.pins] declares no '{}' pin (a {:?} needs it); \
                     the part would bind OPEN. Accepted role names: {}",
                    role_family[0],
                    entry.kind,
                    role_family.join(" / ")
                ),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ComponentKind, ModelEntry, Params};
    use std::collections::BTreeMap;

    fn make_diode(is: f64, n: f64, rs: f64) -> ModelEntry {
        let mut p = Params::default();
        p.set_f64("is", is);
        p.set_f64("n", n);
        p.set_f64("rs", rs);
        ModelEntry {
            id: "test_diode".into(),
            kind: ComponentKind::Diode,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        }
    }

    #[test]
    fn valid_diode_passes() {
        let entry = make_diode(1e-14, 1.5, 1.0);
        assert!(validate(&entry).is_ok());
    }

    #[test]
    fn typoed_pin_role_is_caught_but_extra_pins_are_allowed() {
        // U3: a required signal role missing from an explicit [models.pins] map
        // (typically a typo) must fail lint. Passing lint clean, it binds the
        // part OPEN at run time with a misleading "not connected" message.
        let mut d = make_diode(1e-14, 1.5, 1.0);
        d.pins = BTreeMap::from([
            ("1".into(), "anmode".into()),
            ("2".into(), "cathode".into()),
        ]);
        let errs = validate(&d).unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("anode")),
            "a typo'd anode must be caught, got: {errs:?}"
        );

        // A correct map (any accepted alias) passes, and EXTRA pins never flag.
        let mut ok = make_diode(1e-14, 1.5, 1.0);
        ok.pins = BTreeMap::from([
            ("1".into(), "a".into()),
            ("2".into(), "k".into()),
            ("3".into(), "case".into()), // an extra thermal/NC pad is fine
        ]);
        assert!(
            validate(&ok).is_ok(),
            "aliases + an extra pin must pass: {:?}",
            validate(&ok)
        );

        // An empty pins map (the footprint/pin-rules inference path) is left alone.
        let inferred = make_diode(1e-14, 1.5, 1.0);
        assert!(inferred.pins.is_empty() && validate(&inferred).is_ok());

        // Channel suffixes fold onto the base role: a dual op-amp wired _a/_b,
        // and a numbered dual MOSFET (d1/g1/s1), both satisfy the anchors.
        let mut op = make_diode(1e-14, 1.5, 1.0);
        op.kind = ComponentKind::Opamp;
        op.params = Params::default();
        op.params.set_f64("gain", 1e5);
        op.params.set_f64("rail_lo", 0.0);
        op.params.set_f64("rail_hi", 12.0);
        op.pins = BTreeMap::from([
            ("1".into(), "out_a".into()),
            ("2".into(), "in_minus_a".into()),
            ("3".into(), "in_plus_a".into()),
        ]);
        assert!(
            validate(&op).is_ok(),
            "suffixed opamp channel must pass: {:?}",
            validate(&op)
        );
    }

    #[test]
    fn nonpositive_or_nonfinite_ratings_are_rejected() {
        // R52: absolute-maximum ratings gate the engine's stress faults, but a
        // NaN/negative/zero rating passed validation and then silently disabled
        // the fault (limit>0.0 is false → frac 0 → never trips). Reject them.
        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.max_current_a = Some(f64::NAN);
        assert!(
            validate(&entry)
                .unwrap_err()
                .iter()
                .any(|e| e.message.contains("max_current_a")),
            "a NaN max_current_a must be rejected"
        );

        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.max_voltage_v = Some(-75.0);
        assert!(
            validate(&entry)
                .unwrap_err()
                .iter()
                .any(|e| e.message.contains("max_voltage_v")),
            "a negative max_voltage_v must be rejected"
        );

        // A well-formed positive rating still passes.
        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.max_current_a = Some(1.0);
        entry.ratings.max_voltage_v = Some(75.0);
        assert!(validate(&entry).is_ok(), "valid ratings must pass");
    }

    #[test]
    fn nonpositive_or_nonfinite_thermal_resistances_are_rejected() {
        // R53: theta_ja_c_per_w / theta_jc_c_per_w are UNFLOORED solver inputs
        // (Tj = ambient + power*theta_ja), so a negative/NaN value drives Tj at or
        // below ambient and the Overtemperature fault never trips, a silent
        // safety-disable the R52 ratings gate missed for these two fields.
        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.theta_ja_c_per_w = Some(-50.0);
        assert!(
            validate(&entry)
                .unwrap_err()
                .iter()
                .any(|e| e.message.contains("theta_ja_c_per_w")),
            "a negative theta_ja must be rejected"
        );

        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.theta_jc_c_per_w = Some(f64::NAN);
        assert!(
            validate(&entry)
                .unwrap_err()
                .iter()
                .any(|e| e.message.contains("theta_jc_c_per_w")),
            "a NaN theta_jc must be rejected"
        );

        // A well-formed positive thermal resistance still passes.
        let mut entry = make_diode(1e-14, 1.5, 1.0);
        entry.ratings.theta_ja_c_per_w = Some(62.0);
        assert!(validate(&entry).is_ok(), "a valid theta_ja must pass");
    }

    #[test]
    fn out_of_range_is_fails() {
        // IS way too large, physically impossible
        let entry = make_diode(1.0, 1.5, 1.0);
        let errs = validate(&entry).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("is")));
    }

    #[test]
    fn missing_required_param_fails() {
        let mut p = Params::default();
        p.set_f64("is", 1e-14);
        // n and rs missing
        let entry = ModelEntry {
            id: "bad".into(),
            kind: ComponentKind::Diode,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("'n'")));
        assert!(errs.iter().any(|e| e.message.contains("'rs'")));
    }

    #[test]
    fn bjt_range_check() {
        let mut p = Params::default();
        p.set_f64("is", 1e-14);
        p.set_f64("bf", 9999.0); // way too high
        p.set_f64("nf", 1.0);
        p.set_f64("vaf", 80.0);
        let entry = ModelEntry {
            id: "bad_bjt".into(),
            kind: ComponentKind::BjtNpn,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("bf")));
    }

    #[test]
    fn opamp_with_inverted_rails_is_rejected() {
        // R35: rail_lo/rail_hi were required but never order-checked, so a
        // swapped pair (rail_lo=5, rail_hi=0) passed validation and handed the
        // solver an empty saturation band. It must now fail on the ordering.
        let mut p = Params::default();
        p.set_f64("gain", 1e5);
        p.set_f64("rail_lo", 5.0);
        p.set_f64("rail_hi", 0.0);
        let entry = ModelEntry {
            id: "inverted_opamp".into(),
            kind: ComponentKind::Opamp,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("rail_lo") && e.message.contains("less than")),
            "swapped opamp rails must be rejected: {errs:?}"
        );

        // A correctly-ordered opamp still validates.
        let mut ok = Params::default();
        ok.set_f64("gain", 1e5);
        ok.set_f64("rail_lo", 0.0);
        ok.set_f64("rail_hi", 5.0);
        let good = ModelEntry {
            id: "good_opamp".into(),
            kind: ComponentKind::Opamp,
            description: String::new(),
            r#match: Default::default(),
            params: ok,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        assert!(validate(&good).is_ok(), "a well-ordered opamp must pass");
    }

    #[test]
    fn power_mosfet_kp_above_one_is_accepted() {
        // R39: kp = k'·(W/L) legitimately reaches the tens/hundreds for discrete
        // power MOSFETs; the repo's own db/mosfet.toml has kp up to 200. The old
        // 1.0 A/V² ceiling false-flagged 6 of 8 shipped models and rejected any
        // correctly-extracted power FET.
        for kp in [4.5, 10.0, 15.0, 30.0, 200.0] {
            let mut p = Params::default();
            p.set_f64("vto", 2.0);
            p.set_f64("kp", kp);
            let entry = ModelEntry {
                id: "power_fet".into(),
                kind: ComponentKind::Nmos,
                description: String::new(),
                r#match: Default::default(),
                params: p,
                pins: BTreeMap::new(),
                ratings: Default::default(),
                straps: Vec::new(),
                behavioral: Default::default(),
                logic: Default::default(),
            };
            assert!(
                validate(&entry).is_ok(),
                "a power MOSFET with kp={kp} must validate: {:?}",
                validate(&entry)
            );
        }
        // An absurd kp is still rejected.
        let mut bad = Params::default();
        bad.set_f64("vto", 2.0);
        bad.set_f64("kp", 5000.0);
        let entry = ModelEntry {
            id: "absurd_fet".into(),
            kind: ComponentKind::Nmos,
            description: String::new(),
            r#match: Default::default(),
            params: bad,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        assert!(
            validate(&entry)
                .unwrap_err()
                .iter()
                .any(|e| e.message.contains("kp")),
            "kp=5000 is still out of range"
        );
    }

    #[test]
    fn nan_params_are_rejected_not_silently_accepted() {
        // R37: every IEEE comparison against NaN is false, so `v < min || v > max`
        // let a `nan` TOML literal slip through the physical-bounds gate and reach
        // the solver. A NaN param must be rejected.
        let mut p = Params::default();
        p.set_f64("vout", f64::NAN);
        p.set_f64("dropout_v", 0.3);
        p.set_f64("iq_a", 1e-3);
        let entry = ModelEntry {
            id: "nan_vreg".into(),
            kind: ComponentKind::Vreg,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(
            errs.iter().any(|e| e.message.contains("vout")),
            "a NaN vout must be rejected: {errs:?}"
        );
    }

    #[test]
    fn analog_switch_with_ron_above_roff_is_rejected() {
        // R37: on-resistance must be far below off-resistance, but the two ranges
        // overlap and there was no order check, so a swapped pair (ron=5000,
        // roff=2000) validated, an inverted transmission gate. Must be rejected.
        let mut p = Params::default();
        p.set_f64("ron", 5000.0);
        p.set_f64("roff", 2000.0);
        let entry = ModelEntry {
            id: "inverted_switch".into(),
            kind: ComponentKind::AnalogSwitch,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("ron") && e.message.contains("less than")),
            "a switch with ron >= roff must be rejected: {errs:?}"
        );

        // A well-ordered switch still validates.
        let mut ok = Params::default();
        ok.set_f64("ron", 5.0);
        ok.set_f64("roff", 1e9);
        let good = ModelEntry {
            id: "good_switch".into(),
            kind: ComponentKind::AnalogSwitch,
            description: String::new(),
            r#match: Default::default(),
            params: ok,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        assert!(
            validate(&good).is_ok(),
            "a well-ordered analog switch must pass"
        );
    }

    #[test]
    fn comparator_with_inverted_outputs_is_rejected() {
        // R35: same gap on the comparator out_lo/out_hi pair.
        let mut p = Params::default();
        p.set_f64("out_lo", 3.3);
        p.set_f64("out_hi", 0.0);
        p.set_f64("hysteresis", 0.05);
        let entry = ModelEntry {
            id: "inverted_comp".into(),
            kind: ComponentKind::Comparator,
            description: String::new(),
            r#match: Default::default(),
            params: p,
            pins: BTreeMap::new(),
            ratings: Default::default(),
            straps: Vec::new(),
            behavioral: Default::default(),
            logic: Default::default(),
        };
        let errs = validate(&entry).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("out_lo") && e.message.contains("less than")),
            "swapped comparator outputs must be rejected: {errs:?}"
        );
    }
}
