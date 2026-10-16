//! A linear regulator's output capacitance against its card's stability
//! minimum (`cout_min_f`).
//!
//! An LDO's control loop is compensated by its output capacitor: below the
//! datasheet minimum the loop can ring or oscillate, and the rail it makes is
//! no longer the DC level the rest of the board is judged at. The binder
//! already records the shortfall as an operating-limit assumption (which alone
//! leaves the board not judged); this check states it as a finding on the part
//! and its output net, with the card's minimum, the capacitance found and the
//! capacitors counted, so the board is judged and the fix is named.
//!
//! What is counted is what the binder counts: fitted capacitors with one pad
//! on the regulator's output net and the other on its ground (its own `gnd`
//! net, or any ground net when the part has none). A capacitor behind a
//! ferrite or a resistor is not across the output and does not count.
//! Capacitor ESR is not on the board, so a card's ESR window stays the
//! binder's stated condition and is never judged here.

use std::fmt::Write as _;

use hauksbee_bind::binder::{is_ground, resolve};
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::value::parse_value;
use hauksbee_models::{ComponentKind, Confidence, ModelLibrary};

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

/// One finding per fitted regulator whose card states `cout_min_f` and whose
/// output carries less than that from output to ground.
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
        let Some(need) = model.params.get_f64("cout_min_f").filter(|f| *f > 0.0) else {
            continue;
        };
        let Some(out) = role_net_id(comp, &model, "out") else {
            continue;
        };
        let Some(out_net) = board.net(out) else {
            continue;
        };
        if is_ground(&out_net.name) {
            continue;
        }
        let gnd = role_net_id(comp, &model, "gnd");
        let is_return = |id: i64| {
            gnd.map_or_else(
                || board.net(id).is_some_and(|n| is_ground(&n.name)),
                |g| g == id,
            )
        };
        let caps: Vec<(&Component, f64)> = board
            .net_members(out)
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| AssemblyState::of(c).is_present() && is_capacitor(c))
            .filter(|c| {
                let nets: Vec<i64> = c.pins.iter().filter_map(|p| p.net).collect();
                nets.len() == 2
                    && nets.contains(&out)
                    && nets.iter().any(|n| *n != out && is_return(*n))
            })
            .filter_map(|c| {
                let f = parse_value(c.value.trim()).map(|v| v.si)?;
                (f.is_finite() && f > 0.0).then_some((c, f))
            })
            .fold(Vec::new(), |mut acc, (c, f)| {
                if !acc
                    .iter()
                    .any(|(seen, _): &(&Component, f64)| seen.reference == c.reference)
                {
                    acc.push((c, f));
                }
                acc
            });
        let found: f64 = caps.iter().map(|(_, f)| f).sum();
        // A part-per-billion slack, as in the binder: "10uF" parses one ulp
        // short of the card's 10e-6.
        if found >= need * (1.0 - 1e-9) {
            continue;
        }
        let mut counted = String::new();
        for (i, (c, f)) in caps.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            let _ = write!(counted, "{sep}{} {}", c.reference, farads(*f));
        }
        let (severity, what) = if caps.is_empty() {
            (
                Severity::High,
                "has no capacitor from output to ground".to_string(),
            )
        } else {
            (
                Severity::Medium,
                format!("has {} from output to ground ({counted})", farads(found)),
            )
        };
        report.findings.push(LintFinding {
            check: LintCheck::OperatingEnvelope,
            severity,
            message: format!(
                "{} ({}) output '{}' {what}, below the {} minimum output capacitance card '{}' \
                 states for loop stability: the regulator may ring or oscillate, so its rail is \
                 not the steady level the rest of the board is judged at. Add at least {} \
                 directly from '{}' to ground at the regulator (the type the card's source \
                 names; ceramic or tantalum per its ESR guidance).",
                comp.reference,
                comp.value.trim(),
                out_net.name,
                farads(need),
                model.id,
                farads(need - found),
                out_net.name,
            ),
            refs: std::iter::once(comp.reference.clone())
                .chain(caps.iter().map(|(c, _)| c.reference.clone()))
                .collect(),
            nets: vec![out_net.name.clone()],
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// An AMS1117-3.3 (SOT-223: 1 GND, 2 OUT, 3 IN; card cout_min_f 22 uF,
    /// datasheet Applications Hints) from +5V to +3V3 with `caps` across its
    /// output.
    fn ldo(caps: &[&str]) -> ExtractedBoard {
        let mut parts = vec![part("U1", "AMS1117-3.3", &[("1", 3), ("2", 2), ("3", 1)])];
        for (i, v) in caps.iter().enumerate() {
            parts.push(part(&format!("C{}", i + 1), v, &[("1", 2), ("2", 3)]));
        }
        board(&[(1, "+5V"), (2, "+3V3"), (3, "GND")], parts)
    }

    fn lint(caps: &[&str]) -> Vec<LintFinding> {
        regulator_cout_lint(&ldo(caps), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn output_capacitance_below_the_cards_minimum_is_flagged() {
        let short = lint(&["10u", "100n"]);
        assert_eq!(short.len(), 1, "{short:?}");
        assert_eq!(short[0].severity, Severity::Medium);
        assert_eq!(short[0].nets, ["+3V3"]);
        assert_eq!(short[0].refs, ["U1", "C1", "C2"]);
        for says in [
            "U1 (AMS1117-3.3) output '+3V3' has 10.1 µF",
            "C1 10 µF, C2 100 nF",
            "below the 22 µF minimum",
            "card 'ams1117",
            "Add at least 11.9 µF",
        ] {
            assert!(
                short[0].message.contains(says),
                "{says:?} in {}",
                short[0].message
            );
        }
        let none = lint(&[]);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].severity, Severity::High);
        assert!(none[0]
            .message
            .contains("has no capacitor from output to ground"));
    }

    #[test]
    fn output_capacitance_at_or_above_the_minimum_is_silent() {
        assert!(lint(&["22u"]).is_empty());
        assert!(lint(&["10u", "10u", "2.2u"]).is_empty());
        // A DNP capacitor is not fitted, so it does not make up the minimum.
        let mut b = ldo(&["10u", "22u"]);
        b.components[2].dnp = true;
        assert_eq!(
            regulator_cout_lint(&b, &ModelLibrary::builtin())
                .findings
                .len(),
            1
        );
    }
}
