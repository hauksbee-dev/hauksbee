//! The measurement probe: run a spec once and report ONE assertion's reading with no
//! bound on it, so an editor can pre-fill a threshold from the board's own answer.
//!
//! A reading is not a verdict. The answer carries the value, its range over the
//! window and the uncertainty the run itself admits, and never a pass or fail; an
//! assertion the run could not honestly read (a net never sampled, a rate with too
//! few edges, a failed solve) is an error that says why, never a number.

use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value};

use crate::assertions::{edge_rate, evaluate, peak_for_ref};
use crate::runner::{run_spec, RunOutcome};
use crate::spec::Spec;
use hauksbee_ir::units::format_series;

/// Kinds that reduce a run to one number.
const MEASURABLE: &[&str] = &[
    "voltage",
    "toggle",
    "max_current",
    "phase_margin",
    "ac_gain",
    "pulse_width",
];

/// The reading itself: `(value, min, max, unit, uncertainty)`; `None` where the kind
/// has no single number to give beyond its detail line.
type Reading = (
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<&'static str>,
    Option<String>,
);

fn read(spec: &Spec, a: &crate::spec::Assertion, out: &RunOutcome) -> Reading {
    match a.kind.as_str() {
        "voltage" => {
            let net = a.net.clone().unwrap_or_default();
            let thr = a.after_ms.unwrap_or(0.0);
            out.windows.get(&(net, thr.to_bits())).map_or(
                (None, None, None, Some("V"), None),
                |w| {
                    (
                        Some(w.last_v),
                        Some(w.min_v),
                        Some(w.max_v),
                        Some("V"),
                        Some(format!(
                            "the value is where the net settled; it ranged {} \
                             over {} samples",
                            format_series(&[w.min_v, w.max_v], " to ", "V"),
                            w.samples
                        )),
                    )
                },
            )
        }
        "max_current" => {
            let reference = a.reference.clone().unwrap_or_default();
            (
                peak_for_ref(&out.peak_current, &reference).map(|(_, peak)| peak),
                None,
                None,
                Some("A"),
                Some("the highest current the component carried at any sample".into()),
            )
        }
        "toggle" => {
            let net = a.net.clone().unwrap_or_default();
            let dt_s = spec.frame_ms.max(1e-3) / 1000.0;
            let series = out.net_series.get(&net).map_or(&[][..], Vec::as_slice);
            match edge_rate(series, dt_s) {
                Ok((hz, phrase)) => (Some(hz), None, None, Some("Hz"), Some(phrase)),
                Err(_) => (None, None, None, Some("Hz"), None),
            }
        }
        _ => (None, None, None, None, None),
    }
}

/// Measure assertion `index` (counting `[[assert]]` blocks from 0) of the spec at `path`.
///
/// # Errors
///
/// A message when the spec does not load, `index` is out of range, the kind judges a
/// condition rather than reading a number, the run fails, or the run cannot honestly
/// read the quantity.
pub fn measure(path: &Path, index: usize) -> Result<Value, String> {
    let spec = Spec::load(path).map_err(|e| e.to_string())?;
    let total = spec.asserts.len();
    let chosen = spec
        .asserts
        .get(index)
        .cloned()
        .ok_or_else(|| format!("assertion {index} does not exist; the spec has {total}"))?;
    measure_assertion(spec, chosen)
}

/// Read `chosen` off one nominal run of `spec`, whether or not `spec` lists it.
///
/// This is [`measure`] for a caller that holds the assertion in hand (the
/// `hauksbee-ci add --observe` flow measures an assertion it has not written yet).
///
/// # Errors
///
/// As [`measure`], minus the spec-loading and index errors.
pub fn measure_assertion(
    mut spec: Spec,
    mut chosen: crate::spec::Assertion,
) -> Result<Value, String> {
    let started = Instant::now();
    if !MEASURABLE.contains(&chosen.kind.as_str()) {
        return Err(format!(
            "a `{}` assertion judges a condition; there is no single reading to take. \
             Measurable kinds: {}",
            chosen.kind,
            MEASURABLE.join(", ")
        ));
    }
    // Strip every bound: what is left is the question, "what is it?", not "is it in range?".
    chosen.min = None;
    chosen.max = None;
    chosen.amps = None;
    chosen.freq_hz = None;
    chosen.min_toggles = None;
    chosen.tolerance = None;
    // One run, nominal parts: a reading is not an ensemble claim.
    spec.asserts = vec![chosen.clone()];
    spec.tolerances.clear();
    spec.ensemble = None;
    spec.fuzz = None;
    for o in &mut spec.overrides {
        o.tolerance = None;
    }

    let outcomes = run_spec(&spec).map_err(|e| e.to_string())?;
    let outcome = outcomes.first().ok_or("the run produced no outcome")?;
    let result = evaluate(&spec, &outcomes)
        .into_iter()
        .next()
        .ok_or("the run produced no result")?;
    // A reading the run could not honestly take comes back as the reason, never a number.
    if result.invalid || !result.passed {
        return Err(result.detail);
    }
    let (value, min, max, unit, uncertainty) = read(&spec, &chosen, outcome);
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(json!({
        "ok": true,
        "kind": chosen.kind,
        "value": value, "min": min, "max": max, "unit": unit,
        "uncertainty": uncertainty,
        "window_ms": chosen.after_ms.map_or(spec.duration_ms, |t| spec.duration_ms - t),
        "t_ms": wall_ms,
        "detail": result.detail,
        "error_budget": outcome.error_budget.as_ref().map(hauksbee_ir::evidence::ErrorBudget::plain_summary),
        "judged": false,
        "cached": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("spec.toml");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn an_assertion_that_judges_a_condition_is_not_measurable() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "board = \"b.kicad_pcb\"\n[[assert]]\nkind = \"no_faults\"\n",
        );
        let err = measure(&p, 0).unwrap_err();
        assert!(err.contains("judges a condition"), "{err}");
    }

    #[test]
    fn an_index_past_the_end_names_how_many_there_are() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "board = \"b.kicad_pcb\"\n[[assert]]\nkind = \"no_faults\"\n",
        );
        let err = measure(&p, 3).unwrap_err();
        assert!(err.contains("has 1"), "{err}");
    }
}
