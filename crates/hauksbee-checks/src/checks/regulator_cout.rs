//! A linear regulator's output capacitance against what its card's datasheet
//! says about it: a stated minimum (`cout_min_f`) or a recommendation
//! (`cout_recommended_f`).
//!
//! An LDO's control loop is compensated by its output capacitor: below the
//! datasheet minimum the loop can ring or oscillate, and the rail it makes is
//! no longer the DC level the rest of the board is judged at. The binder
//! already records a shortfall against a minimum as an operating-limit
//! assumption (which alone leaves the board not judged); this check states it
//! as a finding on the part and its output net, with the card's value, the
//! capacitance found and the capacitors counted, so the board is judged and
//! the fix is named.
//!
//! A datasheet MINIMUM ("a minimum of 4.7 µF is required", a Recommended
//! Operating Conditions row) gates. A RECOMMENDATION ("22 µF tantalum will
//! ensure stability", "stable with 1 µF") is a sufficient value, not a floor,
//! so falling short of it is a note that says so; only a regulator with no
//! output capacitor at all is outside every condition such a sheet states.
//! Where stability also depends on the capacitor's type or ESR (the card's
//! `cout_esr_*` window, or the type the recommendation names), the message
//! says so: the board does not carry ESR, so that part is never judged.
//!
//! What is counted is what the binder counts: fitted capacitors with one pad
//! on the regulator's output net and the other on its ground (its own `gnd`
//! net, or any ground net when the part has none). A capacitor behind a
//! ferrite or a resistor is not across the output and does not count. A
//! capacitor whose value does not read as a capacitance makes the check
//! abstain, naming it: an unknown size is never counted as 0 F.

use std::fmt::Write as _;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
    ABSTENTION_MARKER, REMEDY_MARKER,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ComponentKind, Confidence, ModelEntry, ModelLibrary};

use super::envelope::role_net_id;

/// `f` farads in the unit a reader expects for an output capacitor.
fn farads(f: f64) -> String {
    if f >= 1e-6 {
        let uf = format!("{:.2}", f * 1e6);
        format!("{} µF", uf.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{:.0} nF", f * 1e9)
    }
}

/// A part-per-billion slack, as in the binder: "10uF" parses one ulp short of
/// a card's 10e-6, and a capacitor of exactly the stated value meets it.
fn short_of(found: f64, need: f64) -> bool {
    found < need * (1.0 - 1e-9)
}

/// What the card states about the output capacitor.
struct CoutSpec {
    /// The datasheet minimum, which gates.
    min: Option<f64>,
    /// The datasheet's recommended (sufficient, not minimum) value.
    recommended: Option<f64>,
    /// The capacitor type the recommendation names ("solid tantalum").
    recommended_type: Option<String>,
    /// The card's stated ESR window for stability.
    esr: Option<(f64, f64)>,
}

impl CoutSpec {
    fn of(model: &ModelEntry) -> Option<Self> {
        let positive = |name| model.params.get_f64(name).filter(|f| *f > 0.0);
        let spec = Self {
            min: positive("cout_min_f"),
            recommended: positive("cout_recommended_f"),
            recommended_type: model
                .params
                .get_str("cout_recommended_type")
                .map(str::to_string),
            esr: positive("cout_esr_min_ohm").zip(positive("cout_esr_max_ohm")),
        };
        (spec.min.is_some() || spec.recommended.is_some()).then_some(spec)
    }

    /// The largest value the card states: below it something is said.
    fn target(&self) -> f64 {
        self.min.unwrap_or(0.0).max(self.recommended.unwrap_or(0.0))
    }

    /// The sentence on what besides capacitance decides stability, if the card
    /// says anything.
    fn type_note(&self) -> String {
        let mut note = String::new();
        if let Some((lo, hi)) = self.esr {
            let _ = write!(
                note,
                " Stability also needs the capacitor's ESR inside {lo}..{hi} Ω, which the \
                 board does not record, so that part is not judged."
            );
        }
        if let (Some(rec), Some(kind)) = (self.recommended, &self.recommended_type) {
            let _ = write!(
                note,
                " The datasheet's {} is {kind}: stability depends on the capacitor's type and \
                 ESR, which the board does not record.",
                farads(rec)
            );
        }
        note
    }
}

/// The fitted capacitors from the regulator output `out` to its return (its
/// ground pin's net, or any ground-named net when it has none), each once.
fn caps_across_output(board: &ExtractedBoard, out: i64, gnd: Option<i64>) -> Vec<&Component> {
    let is_return = |id: i64| {
        gnd.map_or_else(
            || board.net(id).is_some_and(|n| is_ground(&n.name)),
            |g| g == id,
        )
    };
    let mut across: Vec<&Component> = Vec::new();
    for (c, _) in board.net_members(out) {
        let nets: Vec<i64> = c.pins.iter().filter_map(|p| p.net).collect();
        let spans = nets.len() == 2
            && nets.contains(&out)
            && nets.iter().any(|n| *n != out && is_return(*n));
        if spans
            && AssemblyState::of(c).is_present()
            && is_capacitor(c)
            && !across.iter().any(|seen| seen.reference == c.reference)
        {
            across.push(c);
        }
    }
    across
}

/// A capacitor's value as a capacitance: a positive magnitude in farads, or
/// unitless below 10 mF ("100n", "4u7"). "4.7uH" is not a capacitance, and a
/// unitless "10k" or bare "4.7" is not one either: read as farads it would be
/// thousands of them, and which unit a bare number meant is not known.
fn capacitance(c: &Component) -> Option<f64> {
    parse_value(c.value.trim())
        .filter(|v| v.unit.as_deref().map_or(v.si < 1e-2, |u| u == "F"))
        .map(|v| v.si)
        .filter(|f| f.is_finite() && *f > 0.0)
}

/// The note for capacitors that are across the output but have no readable
/// capacitance, so the card's value cannot be judged.
fn unvalued_note(
    comp: &Component,
    (model_id, need, what): (&str, f64, &str),
    (out_name, unvalued): (&str, &[&str]),
) -> LintFinding {
    let caps = unvalued.join(", ");
    let has = if unvalued.len() == 1 { "has" } else { "have" };
    LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::Low,
        message: format!(
            "{} ({}) output '{out_name}': the output-capacitance check{ABSTENTION_MARKER} against \
             the {} {what} card '{model_id}' states for loop stability, because {caps} across \
             the output {has} no readable capacitance value. {REMEDY_MARKER}Give {caps} a \
             capacitance value (\"4u7\", \"100n\", \"0.1uF\") so the requirement can be checked.",
            comp.reference,
            comp.value.trim(),
            farads(need),
        ),
        refs: std::iter::once(comp.reference.clone())
            .chain(unvalued.iter().map(|r| (*r).to_string()))
            .collect(),
        nets: vec![out_name.to_string()],
    }
}

/// The note for a card whose output-capacitor value is outside its physical range:
/// the requirement is not judged, and the card is named as the thing to fix.
fn suspect_card_note(
    comp: &Component,
    model_id: &str,
    out_name: &str,
    slip: &hauksbee_models::ranges::Implausible,
) -> LintFinding {
    LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::Low,
        message: format!(
            "{} ({}) output '{out_name}': the output-capacitance check{ABSTENTION_MARKER} against \
             card '{model_id}', because the card's value is outside its physical range and is \
             not trusted: {}. {REMEDY_MARKER}Correct the card from its datasheet (or re-extract \
             it); the board is not judged against this value.",
            comp.reference,
            comp.value.trim(),
            slip.message(),
        ),
        refs: vec![comp.reference.clone()],
        nets: vec![out_name.to_string()],
    }
}

/// One finding per fitted linear regulator whose card states an output
/// capacitance and whose output carries less than that from output to ground.
#[must_use]
pub fn regulator_cout_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = resolve(lib, part);
        if matches!(res.confidence, Confidence::Guessed | Confidence::Unresolved) {
            continue;
        }
        let Some(model) = res.model else { continue };
        if model.kind != ComponentKind::Vreg || model.behavioral.converter.is_some() {
            continue;
        }
        // A card value outside its physical range (`cout_min_f = 1.0` for 1 uF) is a
        // unit slip, never a requirement: the check reads the card without it and
        // says the card is suspect, rather than demanding a million microfarads.
        let suspect = hauksbee_models::ranges::quarantine(&model);
        let model = suspect.as_ref().map_or(model, |q| q.model.clone());
        let cout_slip = suspect.as_ref().and_then(|q| {
            q.found
                .iter()
                .find(|f| f.param && f.key.starts_with("cout_"))
        });
        let spec = CoutSpec::of(&model);
        if spec.is_none() && cout_slip.is_none() {
            continue;
        }
        // A card the binder refuses for this footprint names the wrong pads
        // as output and ground; the binder's package-mismatch note covers it.
        if !super::card_fits(board, comp, &model) {
            continue;
        }
        let Some(out) = role_net_id(comp, &model, "out") else {
            continue;
        };
        let Some(out_net) = board.net(out) else {
            continue;
        };
        let gnd = role_net_id(comp, &model, "gnd");
        if is_ground(&out_net.name) || gnd == Some(out) {
            continue;
        }
        // An output pad on a net nothing else touches (KiCad's `unconnected-`
        // placeholder) feeds no load: an unused alternate footprint, not a
        // regulator short of output capacitance.
        if super::is_unconnected_net(&out_net.name) {
            continue;
        }
        if let Some(slip) = cout_slip {
            report
                .findings
                .push(suspect_card_note(comp, &model.id, &out_net.name, slip));
        }
        let Some(spec) = spec else { continue };
        let across = caps_across_output(board, out, gnd);
        let caps: Vec<(&Component, f64)> = across
            .iter()
            .filter_map(|c| Some((*c, capacitance(c)?)))
            .collect();
        let unvalued: Vec<&str> = across
            .iter()
            .filter(|c| capacitance(c).is_none())
            .map(|c| c.reference.as_str())
            .collect();
        let found: f64 = caps.iter().map(|(_, f)| f).sum();
        if !short_of(found, spec.target()) {
            continue;
        }
        if !unvalued.is_empty() {
            // Capacitors are there but their size is not: the value cannot be
            // judged, so the check abstains naming them, never a fail.
            let (need, what) = match spec.min {
                Some(min) if short_of(found, min) => (min, "minimum"),
                _ => (spec.target(), "recommended"),
            };
            report.findings.push(unvalued_note(
                comp,
                (&model.id, need, what),
                (&out_net.name, &unvalued),
            ));
            continue;
        }
        if let Some(finding) = shortfall(comp, &model, &spec, (&out_net.name, &caps, found)) {
            report.findings.push(finding);
        }
    }
    report
}

/// The finding for an output carrying `found` farads (`caps`), short of what
/// the card states.
fn shortfall(
    comp: &Component,
    model: &ModelEntry,
    spec: &CoutSpec,
    (out_name, caps, found): (&str, &[(&Component, f64)], f64),
) -> Option<LintFinding> {
    let mut counted = String::new();
    for (i, (c, f)) in caps.iter().enumerate() {
        let sep = if i == 0 { "" } else { ", " };
        let _ = write!(counted, "{sep}{} {}", c.reference, farads(*f));
    }
    let has = if caps.is_empty() {
        "has no capacitor from output to ground".to_string()
    } else {
        format!("has {} from output to ground ({counted})", farads(found))
    };
    let head = format!(
        "{} ({}) output '{out_name}' {has}",
        comp.reference,
        comp.value.trim()
    );
    let type_note = spec.type_note();
    let at = format!("directly from '{out_name}' to ground at the regulator");
    let (severity, message) = match (spec.min, spec.recommended) {
        (Some(min), _) if short_of(found, min) => (
            if caps.is_empty() {
                Severity::High
            } else {
                Severity::Medium
            },
            format!(
                "{head}, below the {} minimum output capacitance card '{}' states for loop \
                 stability: the regulator may ring or oscillate, so its rail is not the steady \
                 level the rest of the board is judged at.{type_note} {REMEDY_MARKER}Add at \
                 least {} {at}.",
                farads(min),
                model.id,
                farads(min - found),
            ),
        ),
        (_, Some(rec)) if short_of(found, rec) => {
            let met = spec.min.map_or(String::new(), |m| {
                format!(" It meets the {} datasheet minimum.", farads(m))
            });
            if caps.is_empty() && spec.min.is_none() {
                (
                    Severity::Medium,
                    format!(
                        "{head}. Card '{}' records the datasheet's {} as the output capacitance \
                         that keeps the loop stable (a recommendation, not a stated minimum), \
                         and no output capacitor at all is outside every condition the \
                         datasheet states it stable for.{type_note} {REMEDY_MARKER}Add {} {at}.",
                        model.id,
                        farads(rec),
                        farads(rec),
                    ),
                )
            } else {
                (
                    Severity::Low,
                    format!(
                        "{head}, below the {} card '{}' records as the datasheet's \
                         recommendation for loop stability (a recommended value, not a stated \
                         minimum, so this is a note, not a fault).{met}{type_note} \
                         {REMEDY_MARKER}If the regulator rings at load steps, add {} {at}.",
                        farads(rec),
                        model.id,
                        farads(rec - found),
                    ),
                )
            }
        }
        _ => return None,
    };
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity,
        message,
        refs: std::iter::once(comp.reference.clone())
            .chain(caps.iter().map(|(c, _)| c.reference.clone()))
            .collect(),
        nets: vec![out_name.to_string()],
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// A three-terminal LDO (pads 1 GND, 2 OUT, 3 IN) from +5V to +3V3 with
    /// `caps` across its output. The default part is an MCP1700-3302 (card
    /// `cout_min_f` 1 µF; DS20001826F p.13 "A minimum output capacitance of
    /// 1.0 µF is required for small signal stability").
    fn ldo(value: &str, caps: &[&str]) -> ExtractedBoard {
        let mut parts = vec![part("U1", value, &[("1", 3), ("2", 2), ("3", 1)])];
        for (i, v) in caps.iter().enumerate() {
            parts.push(part(&format!("C{}", i + 1), v, &[("1", 2), ("2", 3)]));
        }
        board(&[(1, "+5V"), (2, "+3V3"), (3, "GND")], parts)
    }

    fn lint_on(value: &str, caps: &[&str]) -> Vec<LintFinding> {
        regulator_cout_lint(&ldo(value, caps), &ModelLibrary::builtin()).findings
    }

    fn lint(caps: &[&str]) -> Vec<LintFinding> {
        lint_on(MCP1700, caps)
    }

    const MCP1700: &str = "MCP1700T-3302E/TT";

    fn says(f: &LintFinding, all: &[&str]) {
        for s in all {
            assert!(f.message.contains(s), "{s:?} in {}", f.message);
        }
    }

    /// A regulator whose output pad is on no net (the unused alternate
    /// footprint of a pair) has no output to decouple; the same part with its
    /// output on a rail and no capacitor is still flagged.
    #[test]
    fn a_regulator_with_its_output_on_no_net_is_silent() {
        let unused = board(
            &[(1, "+5V"), (2, "unconnected-(U1-OUT-Pad2)"), (3, "GND")],
            vec![part("U1", MCP1700, &[("1", 3), ("2", 2), ("3", 1)])],
        );
        let found = regulator_cout_lint(&unused, &ModelLibrary::builtin()).findings;
        assert!(found.is_empty(), "{found:?}");
        let wired = lint(&[]);
        assert_eq!(wired.len(), 1, "{wired:?}");
    }

    #[test]
    fn output_capacitance_below_the_cards_minimum_is_flagged() {
        let short = lint(&["470n", "100n"]);
        assert_eq!(short.len(), 1, "{short:?}");
        assert_eq!(short[0].severity, Severity::Medium);
        assert_eq!(short[0].nets, ["+3V3"]);
        assert_eq!(short[0].refs, ["U1", "C1", "C2"]);
        says(
            &short[0],
            &[
                "U1 (MCP1700T-3302E/TT) output '+3V3' has 570 nF",
                "C1 470 nF, C2 100 nF",
                "below the 1 µF minimum",
                "card 'mcp1700_3v3'",
            ],
        );
        // The rule states its own fix, matching the mechanism.
        assert_eq!(
            short[0].own_remedy(),
            Some("Add at least 430 nF directly from '+3V3' to ground at the regulator.")
        );
        let none = lint(&[]);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].severity, Severity::High);
        says(&none[0], &["has no capacitor from output to ground"]);
    }

    /// Capacitors whose value does not read as a capacitance are present,
    /// size unknown: the check abstains naming them, never "no capacitor".
    #[test]
    fn unvalued_output_capacitors_make_the_check_abstain() {
        for odd in ["", "?", "TBD"] {
            let f = lint(&[odd, "100n"]);
            assert_eq!(f.len(), 1, "{odd:?}: {f:?}");
            assert_eq!(f[0].severity, Severity::Low);
            assert!(f[0].is_abstention(), "{}", f[0].message);
            says(
                &f[0],
                &["because C1 across the output has no readable capacitance value"],
            );
            assert!(f[0].own_remedy().is_some_and(|r| r.starts_with("Give C1")));
        }
    }

    /// RKM capacitor markings ("4u7", "u1") are values, not unknowns.
    #[test]
    fn rkm_capacitor_values_are_counted() {
        assert!(lint(&["4u7"]).is_empty());
        assert!(lint(&["1u0"]).is_empty());
        let short = lint(&["u1", "0u1", "u47"]);
        assert_eq!(short.len(), 1, "{short:?}");
        says(
            &short[0],
            &["has 670 nF", "C1 100 nF, C2 100 nF, C3 470 nF"],
        );
    }

    #[test]
    fn output_capacitance_at_or_above_the_minimum_is_silent() {
        assert!(lint(&["1u"]).is_empty());
        assert!(lint(&["470n", "470n", "0.06u"]).is_empty());
        // A DNP capacitor is not fitted, so it does not make up the minimum.
        let mut b = ldo(MCP1700, &["470n", "1u"]);
        b.components[2].dnp = true;
        assert_eq!(
            regulator_cout_lint(&b, &ModelLibrary::builtin())
                .findings
                .len(),
            1
        );
    }

    /// AMS1117: "The addition of 22µF solid tantalum on the output will ensure
    /// stability" and "smaller capacitors can be used with equally good
    /// results" (AMS1117 datasheet p.4): a recommendation, so a shortfall is a
    /// note that says why, never a gating finding; no capacitor at all is.
    #[test]
    fn a_datasheet_recommendation_is_a_note_not_a_fault() {
        let short = lint_on("AMS1117-3.3", &["10u", "2.2u", "100n"]);
        assert_eq!(short.len(), 1, "{short:?}");
        assert_eq!(short[0].severity, Severity::Low);
        assert!(!short[0].is_abstention());
        says(
            &short[0],
            &[
                "has 12.3 µF",
                "below the 22 µF card 'ams1117_3v3' records as the datasheet's recommendation",
                "not a stated minimum",
                "solid tantalum",
            ],
        );
        assert!(lint_on("AMS1117-3.3", &["22u"]).is_empty());
        let none = lint_on("AMS1117-3.3", &[]);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].severity, Severity::Medium);
        says(
            &none[0],
            &["outside every condition the datasheet states it stable for"],
        );
    }

    /// TPS709: the stated minimum is 1.5 µF effective (§8.1.1) and the pin
    /// table's 2.2 µF is the recommendation. Between the two is a note.
    #[test]
    fn between_the_minimum_and_the_recommendation_is_a_note() {
        let fits = |caps: &[&str]| {
            let mut parts = vec![part(
                "U1",
                "TPS70918DBVR",
                &[("1", 1), ("2", 3), ("3", 1), ("4", 1), ("5", 2)],
            )];
            for (i, v) in caps.iter().enumerate() {
                parts.push(part(&format!("C{}", i + 1), v, &[("1", 2), ("2", 3)]));
            }
            let b = board(&[(1, "+5V"), (2, "+1V8"), (3, "GND")], parts);
            regulator_cout_lint(&b, &ModelLibrary::builtin()).findings
        };
        let between = fits(&["1.5u"]);
        assert_eq!(between.len(), 1, "{between:?}");
        assert_eq!(between[0].severity, Severity::Low);
        says(&between[0], &["It meets the 1.5 µF datasheet minimum"]);
        let below = fits(&["1u"]);
        assert_eq!(below[0].severity, Severity::Medium);
        assert!(fits(&["2.2u"]).is_empty());
    }

    /// A card whose pin map the binder refuses for this footprint (its ground
    /// role on the output net) is not judged here: the output and ground it
    /// names are the wrong pads.
    #[test]
    fn a_card_that_does_not_fit_the_footprint_is_not_judged() {
        // TLV75533P DBV order code (DBV: 1 IN, 2 GND, 3 EN, 4 NC, 5 OUT) on a
        // 6-pad DRV layout whose pad 2 (NC on DRV) sits on the output and whose
        // pad 3 and exposed pad are ground.
        let b = tlv_wson_board("TLV75533PDBVR", "470n");
        assert!(regulator_cout_lint(&b, &ModelLibrary::builtin())
            .findings
            .is_empty());
    }

    /// The same WSON-6 layout with a DRV order code binds the DRV card (TLV755P
    /// SBVS320D Table 4-1: OUT 1, NC 2, GND 3, EN 4, NC 5, IN 6), so the
    /// regulator is judged on its own pads: 1 µF is the stated minimum (p.4).
    #[test]
    fn a_drv_order_code_is_judged_with_the_drv_pin_map() {
        let short = regulator_cout_lint(
            &tlv_wson_board("TLV75533PDRVR", "470n"),
            &ModelLibrary::builtin(),
        )
        .findings;
        assert_eq!(short.len(), 1, "{short:?}");
        assert_eq!(short[0].severity, Severity::Medium);
        says(
            &short[0],
            &[
                "has 470 nF",
                "below the 1 µF minimum",
                "card 'tlv75533p_drv'",
            ],
        );
        let fine = regulator_cout_lint(
            &tlv_wson_board("TLV75533PDRVR", "4u7"),
            &ModelLibrary::builtin(),
        )
        .findings;
        assert!(fine.is_empty(), "{fine:?}");
    }

    /// A 6-pad WSON layout: pads 1 and 2 on the 3V3 net, 3 and the exposed pad
    /// 7 on ground, 4 to 6 on 5 V, with one output capacitor of `cap`.
    fn tlv_wson_board(value: &str, cap: &str) -> ExtractedBoard {
        let mut u = part(
            "U1",
            value,
            &[
                ("1", 2),
                ("2", 2),
                ("3", 3),
                ("4", 1),
                ("5", 1),
                ("6", 1),
                ("7", 3),
            ],
        );
        u.footprint = "Package_SON:WSON-6-1EP_2x2mm_P0.65mm_EP1x1.6mm".into();
        u.position = Some((0.0, 0.0, 0.0));
        board(
            &[(1, "+5V"), (2, "+3V3"), (3, "GND")],
            vec![u, part("C1", cap, &[("1", 2), ("2", 3)])],
        )
    }

    /// TI LM1117 (SNOS412Q p.1): "A minimum of 10-µF tantalum capacitor is
    /// required at the output to improve the transient response and
    /// stability." A minimum, so a shortfall gates; AMS1117's 22 µF is only a
    /// recommendation, so the same 4.7 µF there is a note, not a fault.
    #[test]
    fn the_lm1117_ten_microfarad_minimum_gates() {
        let short = lint_on("LM1117-3.3", &["4u7"]);
        assert_eq!(short.len(), 1, "{short:?}");
        assert_eq!(short[0].severity, Severity::Medium);
        says(
            &short[0],
            &[
                "has 4.7 µF",
                "below the 10 µF minimum",
                "card 'lm1117_3v3'",
                "Add at least 5.3 µF",
            ],
        );
        // Legal controls: the minimum met exactly, and more than it.
        assert!(lint_on("LM1117-3.3", &["10u"]).is_empty());
        assert!(lint_on("LM1117MPX-3.3_NOPB", &["22u"]).is_empty());
        let none = lint_on("LM1117-5.0", &[]);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].severity, Severity::High);
        // The AMS1117 with the same 4.7 µF is a note under its recommendation.
        let ams = lint_on("AMS1117-3.3", &["4u7"]);
        assert_eq!(ams.len(), 1, "{ams:?}");
        assert_eq!(ams[0].severity, Severity::Low);
    }
}
