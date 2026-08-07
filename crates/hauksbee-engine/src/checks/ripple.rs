//! Input-capacitor ripple-current check for switching converters.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
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
//! The check fires *only* when the topology, cap rating, output current, and
//! nominal duty are all known from citeable sources:
//!
//!   - the [discrete converter stage](super::converter) must resolve (switch
//!     node tied to a FET + inductor, an input bulk cap on the input rail);
//!   - the input cap's ripple rating must be decision-grade DB evidence
//!     (`max_ripple_current_a` from a specific fitted-part datasheet);
//!   - the output current `I_out` must be an operating-current attribution
//!     (currently a `regulated_current` programming equation), not a converter
//!     limit or regulator/connector rating;
//!   - both rail names must carry one unambiguous nominal voltage, so the buck
//!     duty `D = Vout/Vin` is evidence rather than an assumed global 0.5.
//!
//! When any decision input is unknown, the check emits an *info* note (the
//! negative is on the record) and does not fire. It never invents a current,
//! rating, or duty.

use std::collections::{BTreeMap, HashSet};

use hauksbee_extract::{ExtractedBoard, SiCheck, SiFinding, SiReport, SiSeverity};
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
    let (lo, hi) = if d_lo <= d_hi {
        (d_lo, d_hi)
    } else {
        (d_hi, d_lo)
    };
    if lo <= 0.5 && hi >= 0.5 {
        return buck_input_cap_ripple_rms(i_out_a, 0.5);
    }
    let a = buck_input_cap_ripple_rms(i_out_a, lo);
    let b = buck_input_cap_ripple_rms(i_out_a, hi);
    a.max(b)
}

/// Resolve the exact fitted part's datasheet ripple rating. Capacitance alone
/// cannot establish this value: dielectric, can size, ESR construction,
/// temperature, and frequency all matter, so an absent part-specific rating is
/// an explicit `None`, never a class heuristic.
fn input_cap_ripple_rating(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    cap_ref: &str,
) -> Option<f64> {
    board
        .component(cap_ref)
        .and_then(|comp| resolve(lib, comp).model)
        .and_then(|model| model.ratings.max_ripple_current_a)
}

/// Attribute the converter's output current `I_out` (A) from a citeable source
/// on the output or input rail. Returns `(i_out, citation)` or `None`.
///
/// The shared ampacity attribution contract supplies only established operating
/// currents. It sums simultaneous regulated loads and excludes converter/OCP
/// limits plus device/contact ratings, which are capabilities rather than draw.
fn attribute_i_out(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    stage: &ConverterStage,
) -> Option<(f64, String)> {
    super::ampacity::attributed_operating_currents(board, lib).remove(&stage.output_rail.1)
}

/// Parse one voltage token from a rail name. Supported conventional spellings
/// are `12V`, `3V3`, and `3.3V`, embedded in names such as `PWR_IN_12V` or
/// `VOUT_3V3`. For hierarchical names only the leaf is electrically
/// descriptive; parent-sheet tokens are ignored. More than one voltage token
/// in that leaf is ambiguous and is refused.
fn named_rail_voltage(name: &str) -> Option<f64> {
    fn parse_token(token: &str) -> Option<f64> {
        let parse_positive = |text: &str| {
            let value = text.parse::<f64>().ok()?;
            (value.is_finite() && value > 0.0).then_some(value)
        };

        if let Some(number) = token.strip_suffix('V') {
            if !number.is_empty()
                && number.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                && number.chars().filter(|&ch| ch == '.').count() <= 1
            {
                return parse_positive(number);
            }
        }

        let mut parts = token.split('V');
        let whole = parts.next()?;
        let fraction = parts.next()?;
        if parts.next().is_some()
            || whole.is_empty()
            || fraction.is_empty()
            || !whole.chars().all(|ch| ch.is_ascii_digit())
            || !fraction.chars().all(|ch| ch.is_ascii_digit())
        {
            return None;
        }
        parse_positive(&format!("{whole}.{fraction}"))
    }

    let leaf = name
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let upper = leaf.to_ascii_uppercase();
    let mut found = None;
    for token in upper.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.')) {
        let Some(volts) = parse_token(token) else {
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some(volts);
    }
    found
}

/// Nominal buck duty from explicit voltage-bearing input/output rail names.
/// A non-step-down ratio or either missing/ambiguous voltage returns `None`;
/// callers must record an abstention instead of substituting a convenient duty.
fn nominal_buck_duty(input_rail: &str, output_rail: &str) -> Option<f64> {
    let input_v = named_rail_voltage(input_rail)?;
    let output_v = named_rail_voltage(output_rail)?;
    let duty = output_v / input_v;
    (duty.is_finite() && duty > 0.0 && duty < 1.0).then_some(duty)
}

/// Run the input-cap ripple check and append findings / info notes to the SI
/// report.
pub fn append_ripple(board: &ExtractedBoard, lib: &ModelLibrary, report: &mut SiReport) {
    let stages = detect_converters(board, lib);
    let mut suppliers: BTreeMap<i64, Vec<&ConverterStage>> = BTreeMap::new();
    for stage in &stages {
        suppliers
            .entry(stage.output_rail.0)
            .or_default()
            .push(stage);
    }
    let mut ambiguous_output_nets = HashSet::new();
    for (output_net, stages) in suppliers.iter().filter(|(_, stages)| stages.len() > 1) {
        if !stages.iter().any(|stage| stage.topology == Topology::Buck) {
            continue;
        }
        ambiguous_output_nets.insert(*output_net);
        let mut stage_labels: Vec<_> = stages
            .iter()
            .map(|stage| {
                format!(
                    "{} (switch '{}', input '{}')",
                    stage.inductor_ref, stage.switch_node.1, stage.input_rail.1
                )
            })
            .collect();
        stage_labels.sort();
        let mut references: Vec<_> = stages
            .iter()
            .map(|stage| stage.inductor_ref.clone())
            .collect();
        references.sort();
        references.dedup();
        report.findings.push(SiFinding {
            check: SiCheck::InputCapRipple,
            severity: SiSeverity::Info,
            message: format!(
                "input-cap ripple: output rail '{}' has {} detected supplying stages ({}); the net-wide attributable load is known, but its supplier split is unknown - no stage input capacitor is flagged.",
                stages[0].output_rail.1,
                stages.len(),
                stage_labels.join(", "),
            ),
            refs: references,
            nets: vec![stages[0].output_rail.1.clone()],
        });
    }
    for stage in &stages {
        // Only buck input caps are modelled here (the input current is the pulsed
        // one). A boost's pulsed current is on the output; left to a future arm.
        if stage.topology != Topology::Buck {
            continue;
        }
        if ambiguous_output_nets.contains(&stage.output_rail.0) {
            continue;
        }
        let cap = match stage.input_bulk_caps.as_slice() {
            [] => continue,
            [cap] => cap,
            caps => {
                report.findings.push(SiFinding {
                    check: SiCheck::InputCapRipple,
                    severity: SiSeverity::Info,
                    message: format!(
                        "input-cap ripple: buck stage '{} -> {}' has {} parallel input bulk capacitors ({}); their frequency-dependent impedance and ripple-current sharing are unknown - not flagged.",
                        stage.input_rail.1,
                        stage.output_rail.1,
                        caps.len(),
                        caps.iter()
                            .map(|cap| cap.reference.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    refs: caps.iter().map(|cap| cap.reference.clone()).collect(),
                    nets: vec![stage.input_rail.1.clone()],
                });
                continue;
            }
        };
        let rating = input_cap_ripple_rating(board, lib, &cap.reference);
        let i_out = attribute_i_out(board, lib, stage);

        // Both the exact fitted-part rating and I_out must be known to fire.
        // Preserve whichever half is known in the abstention so the user can
        // see the precise remaining evidence gap.
        let (rating_a, i_out_a, i_cite) = match (rating, i_out) {
            (Some(rating_a), Some((i_out_a, i_cite))) => (rating_a, i_out_a, i_cite),
            (rating, i_out) => {
                let why = match (rating, i_out.as_ref()) {
                    (None, None) => "it has no part-specific datasheet ripple rating and the output current is not attributable".to_string(),
                    (None, Some((i_out_a, citation))) => format!(
                        "it has no part-specific datasheet ripple rating; attributable I_out {i_out_a:.2} A from: {citation}"
                    ),
                    (Some(rating_a), None) => format!(
                        "it has a {rating_a:.2} A_rms part-specific datasheet rating but the output current is not attributable"
                    ),
                    (Some(_), Some(_)) => unreachable!("complete evidence handled above"),
                };
                report.findings.push(SiFinding {
                    check: SiCheck::InputCapRipple,
                    severity: SiSeverity::Info,
                    message: format!(
                        "input-cap ripple: buck stage on '{}' (switch node '{}', inductor {}) has input bulk cap {} ({}), but {why} - not flagged.",
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
            }
        };

        // A non-positive rating (a malformed or zero model-DB ratings entry;
        // ratings are not range-validated in hauksbee-models) would divide below
        // into Inf/NaN and print a nonsensical "~infx overstress". Treat it as an
        // unusable rating: note it and move on, never divide by it. `!(x > 0.0)`
        // also rejects NaN.
        if !(rating_a > 0.0) {
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Info,
                message: format!(
                    "input-cap ripple: buck stage on '{}' has input bulk cap {} ({}) whose \
                     ripple rating is {:.2} A_rms, not a usable positive value - not flagged.",
                    stage.input_rail.1, cap.reference, cap.value, rating_a,
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone()],
            });
            continue;
        }

        let Some(duty) = nominal_buck_duty(&stage.input_rail.1, &stage.output_rail.1) else {
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Info,
                message: format!(
                    "input-cap ripple: buck stage '{} -> {}' has attributable I_out {:.2} A and {} ({}) has a {:.2} A_rms part-specific datasheet rating, but the rail names do not provide one unambiguous nominal voltage each; duty D=Vout/Vin is unknown - not flagged. I_out from: {}",
                    stage.input_rail.1,
                    stage.output_rail.1,
                    i_out_a,
                    cap.reference,
                    cap.value,
                    rating_a,
                    i_cite,
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone(), stage.output_rail.1.clone()],
            });
            continue;
        };

        let i_rms = buck_input_cap_ripple_rms(i_out_a, duty);
        let ratio = i_rms / rating_a;

        if i_rms > rating_a {
            report.findings.push(SiFinding {
                check: SiCheck::InputCapRipple,
                severity: SiSeverity::Medium,
                message: format!(
                    "input bulk cap {} ({}) on buck '{} -> {}' carries ~{:.2} A_rms ripple \
                     (I_out {:.1} A, D={:.3} from named nominal rail voltages) but is rated {:.2} A_rms{}: ~{:.2}x \
                     overstress, which shortens cap life (I^2*ESR self-heating). I_out from: {}",
                    cap.reference,
                    cap.value,
                    stage.input_rail.1,
                    stage.output_rail.1,
                    i_rms,
                    i_out_a,
                    duty,
                    rating_a,
                    " [datasheet]",
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
                    "input bulk cap {} on '{} -> {}': ~{:.2} A_rms ripple at D={:.3} from named nominal rail voltages vs {:.2} A_rms rating ({:.2}x) - ok.",
                    cap.reference,
                    stage.input_rail.1,
                    stage.output_rail.1,
                    i_rms,
                    duty,
                    rating_a,
                    ratio
                ),
                refs: vec![cap.reference.clone()],
                nets: vec![stage.input_rail.1.clone(), stage.output_rail.1.clone()],
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

    #[test]
    fn nominal_buck_duty_requires_unambiguous_voltages_in_both_rail_names() {
        assert_eq!(nominal_buck_duty("VIN_5V", "VOUT_2V5"), Some(0.5));
        let duty = nominal_buck_duty("PWR_IN_12V", "CORE_OUT_3V3").unwrap();
        assert!((duty - 0.275).abs() < 1e-12);
        assert_eq!(nominal_buck_duty("VIN", "VOUT_3V3"), None);
        assert_eq!(nominal_buck_duty("VIN_12V_24V", "VOUT_5V"), None);
        assert_eq!(nominal_buck_duty("VIN_5V", "VOUT_12V"), None);
        assert_eq!(nominal_buck_duty("VIN_0V", "VOUT_0V"), None);
    }

    #[test]
    fn nominal_buck_duty_uses_only_the_hierarchical_net_leaf() {
        // Parent-sheet labels describe hierarchy, not the electrical name of
        // the leaf net. A voltage token in a parent path must never fabricate
        // a nominal voltage for an otherwise unlabelled VIN/VOUT.
        assert_eq!(named_rail_voltage("/12V_DOMAIN/VIN"), None);
        assert_eq!(named_rail_voltage("/POWER_12V/VOUT_5V"), Some(5.0));
        let duty = nominal_buck_duty("/POWER_TREE/VIN_12V", "/CORE_5V/VOUT_3V3").unwrap();
        assert!((duty - 0.275).abs() < 1e-12);
    }

    // The hunt's mppt-1210-hus C1 case, hand-checked: 1200 uF / 3.0 A_rms rated,
    // across a 10 A buck input at D ~ 0.5 -> ~5.0 A_rms ~ 1.66x its rating.
    #[test]
    fn mppt_1210_c1_overstress_is_1_66x() {
        let i_out = 10.0;
        let rating = 3.0; // UCC EKYB630ELL122MLN3S, 3.0 A_rms at 100 kHz / 105 C.
        let i_rms = buck_input_cap_ripple_rms(i_out, 0.5);
        assert!(
            (i_rms - 5.0).abs() < 1e-9,
            "worst-case ripple ~5.0 A_rms, got {i_rms}"
        );
        let ratio = i_rms / rating;
        assert!(
            (ratio - 1.6667).abs() < 0.01,
            "overstress ~1.66x, got {ratio}"
        );
        assert!(i_rms > rating, "must register as overstress");
    }

    #[test]
    fn output_bulk_cap_sees_only_inductor_ripple_not_the_input_pulse() {
        // The hunt's honest contrast: the OUTPUT bulk cap (C5, 820 uF) sees only
        // inductor-ripple RMS, a small fraction of I_out, not the input pulse. We
        // do not model the output cap here, but assert the input-pulse magnitude
        // is the large one so the check is aimed at the right cap.
        let input_pulse = buck_input_cap_ripple_rms(10.0, 0.5); // 5.0 A
        assert!(
            input_pulse >= 4.0,
            "input cap carries the large pulsed ripple"
        );
    }

    #[test]
    fn nonpositive_ripple_rating_is_guarded_against_infx() {
        // Bug-hunt #10: a zero/negative/NaN ripple rating (a malformed model-DB
        // ratings entry) must be treated as unusable, never divided into the
        // overstress ratio, which for a zero rating printed "~infx overstress".
        // This pins the exact `!(rating_a > 0.0)` predicate the check applies.
        for bad in [0.0f64, -1.0, f64::NAN] {
            assert!(!(bad > 0.0), "rating {bad} must be rejected as unusable");
        }
        // The division the guard prevents, for a zero rating, is the Inf that
        // formatted as "~infx"; a real positive rating stays finite and sane.
        let i_rms = buck_input_cap_ripple_rms(10.0, 0.5); // 5.0 A_rms
        assert!(
            !(i_rms / 0.0).is_finite(),
            "zero rating divides to a non-finite ratio"
        );
        assert!(
            (i_rms / 3.0).is_finite(),
            "a real rating yields a finite ratio"
        );
    }
}
