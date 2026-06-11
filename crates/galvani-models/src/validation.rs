//! Physical-range validation for model parameters.
//!
//! Used by the extraction pipeline to reject LLM-generated model entries
//! that have out-of-range parameters before they are saved to the user DB.

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
                if v < $min || v > $max {
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
        }
        ComponentKind::Comparator => {
            require_f64!("out_lo");
            require_f64!("out_hi");
            require_f64!("hysteresis");
            check_range!("hysteresis", 0.0, 5.0);
        }
        ComponentKind::AnalogSwitch => {
            require_f64!("ron");
            require_f64!("roff");
            check_range!("ron", 0.01, 10_000.0);
            check_range!("roff", 1e3, 1e12);
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
        };
        let errs = validate(&entry).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("bf")));
    }
}
