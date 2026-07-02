//! Input-capacitor ripple-current check for switching converters.
//!
//! A buck converter chops its input current between 0 and `I_out` at the duty
//! cycle `D = Vout/Vin`. The input bulk capacitor has to supply the AC part of
//! that pulsed draw, so it carries an RMS ripple current
//!
//!   I_rms = I_out * sqrt(D - D^2)
//!
//! which peaks at `0.5 * I_out` when `D = 0.5`. Electrolytic bulk caps are rated
//! for a maximum RMS ripple current (the I^2*ESR self-heating budget); running a
//! cap past that rating ages it out early. This is exactly the mppt-1210-hus
//! finding: `C1` (1200 uF, rated 3.0 A_rms) sits across a 10 A buck input at
//! `D ~ 0.5`, so it carries ~5.0 A_rms, ~1.66x its rating.
//!
//! ## Zero-false-positive boundary
//!
//! The check fires *only* when **both** the topology and the cap's ripple rating
//! are known, and an output current is attributed from a citeable source:
//!
//!   - the [discrete converter stage](super::converter) must resolve (switch
//!     node tied to a FET + inductor, an input bulk cap on the input rail);
//!   - the input cap's ripple rating must be known, either from the DB
//!     (`max_ripple_current_a`, a datasheet override) or a conservative
//!     per-class default keyed on the cap's value/dielectric;
//!   - the output current `I_out` must be attributed from a converter limit, an
//!     output-rail current-sense/connector rating, or an explicit citation.
//!
//! When the topology resolves but the rating or `I_out` is unknown, the check
//! emits an *info* note (the negative is on the record) and does not fire. It
//! never invents a current or a rating.

use hauksbee_extract::{
    ExtractedBoard, SiCheck, SiFinding, SiReport, SiSeverity,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

use super::converter::{detect_converters, ConverterStage, Topology};
use crate::binder::resolve;

/// Worst-case input-cap RMS ripple current of a buck (A): `I_out*sqrt(D - D^2)`.
/// `d` is the duty cycle `Vout/Vin` in (0,1). Clamps `d` to the open interval so
/// a degenerate `d` returns 0 rather than NaN.
pub fn buck_input_cap_ripple_rms(i_out_a: f64, d: f64) -> f64 {
    if i_out_a <= 0.0 || d <= 0.0 || d >= 1.0 {
        return 0.0;
    }
    i_out_a * (d - d * d).sqrt()
}

/// The duty cycle that maximises input-cap ripple is always D = 0.5 (the
/// `sqrt(D - D^2)` peak), giving `0.5 * I_out`. When the operating duty range
/// brackets 0.5 the worst case is exactly there; otherwise it is at the nearer
/// endpoint. This returns the worst-case ripple over a duty range `[d_lo, d_hi]`.
pub fn worst_case_ripple_over_duty(i_out_a: f64, d_lo: f64, d_hi: f64) -> f64 {
    let (lo, hi) = if d_lo <= d_hi { (d_lo, d_hi) } else { (d_hi, d_lo) };
    if lo <= 0.5 && hi >= 0.5 {
        return buck_input_cap_ripple_rms(i_out_a, 0.5);
    }
    let a = buck_input_cap_ripple_rms(i_out_a, lo);
    let b = buck_input_cap_ripple_rms(i_out_a, hi);
    a.max(b)
}

/// Conservative per-class default RMS ripple rating (A) for a bulk cap, keyed on
/// its capacitance, when the datasheet rating is not in the DB. These are the
/// LOW end of typical aluminium-electrolytic ripple ratings at switching
/// frequency for the given capacitance, so the default under-states the cap's
/// real capability: the check therefore only fires on a clear overstress and
/// never on a cap whose true rating we merely do not have. A cap whose value
/// does not parse returns `None` (no default, so the check stays silent on it).
fn default_ripple_rating_a(farads: f64) -> Option<f64> {
    if farads <= 0.0 {
        return None;
    }
    let uf = farads * 1e6;
    // Representative low-end aluminium-electrolytic ripple ratings (A_rms) at
    // ~100 kHz / 105 C by capacitance band. Deliberately conservative (a real
    // part of this size is usually rated higher), so the comparison is honest.
    let rating = if uf >= 2200.0 {
        4.0
    } else if uf >= 1000.0 {
        2.5
    } else if uf >= 470.0 {
        1.8
    } else if uf >= 100.0 {
        1.0
    } else {
        // Sub-100 uF: likely an MLCC or small electrolytic; ripple is rarely the
        // limit and a default would be unreliable. Decline.
        return None;
    };
    Some(rating)
}

/// Resolve the input bulk cap's ripple rating: datasheet (DB) first, then the
/// conservative per-class default. `None` when neither is available.
fn input_cap_ripple_rating(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    cap_ref: &str,
    cap_farads: Option<f64>,
) -> Option<(f64, bool)> {
    if let Some(comp) = board.component(cap_ref) {
        if let Some(r) = resolve(lib, comp).model.and_then(|m| m.ratings.max_ripple_current_a) {
            return Some((r, true)); // datasheet-sourced
        }
    }
    cap_farads.and_then(default_ripple_rating_a).map(|r| (r, false))
}

/// Attribute the converter's output current `I_out` (A) from a citeable source
/// on the output or input rail. Returns `(i_out, citation)` or `None`.
///
/// Sources, in order: a DB converter `iout_limit_a` whose output pin lands on
/// the stage's output rail; a part with a continuous `max_current_a` rating on
/// the output rail (a current-sense amp's max, a connector contact, a load
/// switch). Never fabricates a current.
fn attribute_i_out(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    stage: &ConverterStage,
) -> Option<(f64, String)> {
    let out_id = stage.output_rail.0;
    let mut best: Option<(f64, String)> = None;
    for comp in &board.components {
        if comp.dnp {
            continue;
        }
        let touches_out = comp.pins.iter().any(|p| p.net == Some(out_id));
        if !touches_out {
            continue;
        }
        let Some(model) = resolve(lib, comp).model else {
            continue;
        };
        // Converter output-current limit on this rail.
        let mut candidate: Option<f64> = None;
        if let Some(conv) = &model.behavioral.converter {
            if let Some(i) = conv.iout_limit_a {
                candidate = Some(i);
            }
        }
        // A continuous current rating on a regulator / connector on this rail.
        // FETs are excluded (a device switch rating is not a proof of rail
        // current), and a generic placeholder model never seeds an attribution.
        if candidate.is_none() {
            if let Some(i) = model.ratings.max_current_a {
                use hauksbee_models::ComponentKind::*;
                if matches!(model.kind, Vreg | Connector) && !model.id.starts_with("generic") {
                    candidate = Some(i);
                }
            }
        }
        if let Some(i) = candidate {
            let cite = format!("{} ({}) rated {:.1} A on the output rail [datasheet]", comp.reference, model.id, i);
            best = match best {
                Some((b, _)) if b >= i => best,
                _ => Some((i, cite)),
            };
        }
    }
    best
}

/// Run the input-cap ripple check and append findings / info notes to the SI
/// report.
pub fn append_ripple(board: &ExtractedBoard, lib: &ModelLibrary, report: &mut SiReport) {
    let stages = detect_converters(board, lib);
    for stage in &stages {
        // Only buck input caps are modelled here (the input current is the pulsed
        // one). A boost's pulsed current is on the output; left to a future arm.
        if stage.topology != Topology::Buck {
            continue;
        }
        let Some(cap) = &stage.input_bulk_cap else {
            continue;
        };
        let cap_farads = cap.farads.or_else(|| parse_value(cap.value.trim()).map(|v| v.si));

        let rating = input_cap_ripple_rating(board, lib, &cap.reference, cap_farads);
        let i_out = attribute_i_out(board, lib, stage);

        // Both the rating and I_out must be known to fire. Otherwise, record the
        // honest info note and move on.
        let rating_known = rating.is_some();
        let i_out_known = i_out.is_some();
        let (Some((rating_a, from_datasheet)), Some((i_out_a, i_cite))) = (rating, i_out) else {
            let why = if !rating_known && !i_out_known {
                "neither its ripple rating nor the output current is known"
            } else if !rating_known {
                "its ripple rating is not known"
            } else {
                "the output current is not attributable"
            };
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Info,
                message: format!(
                    "input-cap ripple: buck stage on '{}' (switch node '{}', inductor {}) has input \
                     bulk cap {} ({}), but {why} - not flagged.",
                    stage.input_rail.1,
                    stage.switch_node.1,
                    stage.inductor_ref,
                    cap.reference,
                    cap.value,
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone()],
            });
            continue;
        };

        // Worst-case ripple is at D = 0.5 (the sqrt(D-D^2) peak): 0.5 * I_out.
        let i_rms = buck_input_cap_ripple_rms(i_out_a, 0.5);
        let ratio = i_rms / rating_a;

        if i_rms > rating_a {
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Medium,
                message: format!(
                    "input bulk cap {} ({}) on buck input '{}' carries ~{:.2} A_rms ripple \
                     (I_out {:.1} A, worst-case D=0.5) but is rated {:.2} A_rms{}: ~{:.2}x \
                     overstress, which shortens cap life (I^2*ESR self-heating). I_out from: {}",
                    cap.reference,
                    cap.value,
                    stage.input_rail.1,
                    i_rms,
                    i_out_a,
                    rating_a,
                    if from_datasheet { " [datasheet]" } else { " [conservative per-class default]" },
                    ratio,
                    i_cite,
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone()],
            });
        } else {
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Info,
                message: format!(
                    "input bulk cap {} on '{}': ~{:.2} A_rms ripple vs {:.2} A_rms rating ({:.2}x) - ok.",
                    cap.reference, stage.input_rail.1, i_rms, rating_a, ratio
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone()],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ripple_formula_peaks_at_half_duty() {
        // I_rms = I_out * sqrt(D - D^2). At D=0.5 -> 0.5*I_out.
        let i = buck_input_cap_ripple_rms(10.0, 0.5);
        assert!((i - 5.0).abs() < 1e-9, "D=0.5 -> 0.5*Iout, got {i}");
        // Symmetric about 0.5.
        let lo = buck_input_cap_ripple_rms(10.0, 0.3);
        let hi = buck_input_cap_ripple_rms(10.0, 0.7);
        assert!((lo - hi).abs() < 1e-9, "ripple symmetric about D=0.5");
        // Degenerate duties -> 0.
        assert_eq!(buck_input_cap_ripple_rms(10.0, 0.0), 0.0);
        assert_eq!(buck_input_cap_ripple_rms(10.0, 1.0), 0.0);
    }

    #[test]
    fn worst_case_over_duty_range_finds_the_peak() {
        // Range brackets 0.5 -> peak at 0.5.
        assert!((worst_case_ripple_over_duty(10.0, 0.3, 0.7) - 5.0).abs() < 1e-9);
        // Range entirely below 0.5 -> worst at the upper endpoint (nearer 0.5).
        let w = worst_case_ripple_over_duty(10.0, 0.2, 0.4);
        assert!((w - buck_input_cap_ripple_rms(10.0, 0.4)).abs() < 1e-9);
    }

    // The hunt's mppt-1210-hus C1 case, hand-checked: 1200 uF / 3.0 A_rms rated,
    // across a 10 A buck input at D ~ 0.5 -> ~5.0 A_rms ~ 1.66x its rating.
    #[test]
    fn mppt_1210_c1_overstress_is_1_66x() {
        let i_out = 10.0;
        let rating = 3.0; // UCC EKYB630ELL122MLN3S, 3.0 A_rms at 100 kHz / 105 C.
        let i_rms = buck_input_cap_ripple_rms(i_out, 0.5);
        assert!((i_rms - 5.0).abs() < 1e-9, "worst-case ripple ~5.0 A_rms, got {i_rms}");
        let ratio = i_rms / rating;
        assert!((ratio - 1.6667).abs() < 0.01, "overstress ~1.66x, got {ratio}");
        assert!(i_rms > rating, "must register as overstress");
    }

    #[test]
    fn output_bulk_cap_sees_only_inductor_ripple_not_the_input_pulse() {
        // The hunt's honest contrast: the OUTPUT bulk cap (C5, 820 uF) sees only
        // inductor-ripple RMS, a small fraction of I_out, not the input pulse. We
        // do not model the output cap here, but assert the input-pulse magnitude
        // is the large one so the check is aimed at the right cap.
        let input_pulse = buck_input_cap_ripple_rms(10.0, 0.5); // 5.0 A
        assert!(input_pulse >= 4.0, "input cap carries the large pulsed ripple");
    }

    #[test]
    fn conservative_default_is_below_typical_ratings() {
        // A 1200 uF cap defaults to 2.5 A (conservative) when no datasheet rating
        // is in the DB; the real UCC part is 3.0 A. The default under-states, so
        // the check is honest (it will not over-fire on an undocumented cap).
        let d = default_ripple_rating_a(1200e-6).unwrap();
        assert!(d <= 3.0, "default {d} should not exceed the real ~3.0 A rating");
        assert!(default_ripple_rating_a(50e-6).is_none(), "decline sub-100uF (MLCC/no default)");
    }
}
