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
                // because every IEEE comparison against NaN is false — NaN is
                // neither below-min nor above-max — so it must be rejected up front
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
    // the output. Only checked when both parse — the require_f64! calls report a
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
            check_range!("kp", 1e-6, 1.0);
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
            // conducts MORE when open — an inverted transmission gate the solver
            // routes the wrong way. Same hazard the R35 opamp/comparator order
            // checks close.
            check_order!("ron", "roff");
        }
        // Digital / MCU / connector / ignore: no mandatory numeric params
        _ => {}
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
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
    fn out_of_range_is_fails() {
        // IS way too large — physically impossible
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
            errs.iter().any(|e| e.message.contains("rail_lo") && e.message.contains("less than")),
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
        // roff=2000) validated — an inverted transmission gate. Must be rejected.
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
            errs.iter().any(|e| e.message.contains("ron") && e.message.contains("less than")),
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
        assert!(validate(&good).is_ok(), "a well-ordered analog switch must pass");
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
            errs.iter().any(|e| e.message.contains("out_lo") && e.message.contains("less than")),
            "swapped comparator outputs must be rejected: {errs:?}"
        );
    }
}
