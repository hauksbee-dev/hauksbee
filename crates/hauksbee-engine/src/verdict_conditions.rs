//! What a pass rests on beyond the conditions its surface states outright.
//!
//! A pass says the checks found nothing that blocks. It must not say more
//! than was judged, so two kinds of gap the report already lists become
//! conditions beside the verdict (the `verdict_conditions` of `--json`, the
//! "this rests on:" list of `--plain`, the closing `VERDICT:` line and the web
//! headline all read the same list):
//!
//! - an abstention: a check that declined to judge one part or net
//!   ([`LintFinding::is_abstention`]: a rating with no working model on its
//!   current path, an operating envelope with no known level, a strap-bearing
//!   MCU with no strap table, unfilled zones). The rest of the board was
//!   judged, so the run stays a pass; the gap is named.
//! - a modelling assumption on a model-dependent surface: a part modelled only
//!   in part, a pin role inferred from the package, a reduced-fidelity
//!   (fallback) model, a documented default value. One condition groups them by
//!   their glossary label and names the parts.
//!
//! What stays out, and why:
//!
//! - a part with no model at all (an open part): the `NOT COVERED` line names
//!   it, a verdict-critical one already makes the run PARTLY JUDGED, and a
//!   check whose result it could change abstains, which names it here;
//! - a swept assumed corner: its surface already states it as a condition;
//! - a waived finding: the waiver section lists it with its reason and date;
//! - an analysis that did not run to a result, or a limit judged against a
//!   stand-in card's rating: those make the run PARTLY JUDGED, not a pass
//!   (`JsonReport::stated_condition`).

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::{LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_ir::evidence::{Assumption, EntityKind, Remedy, Scope};
use hauksbee_ir::glossary::Term;
use hauksbee_ir::vocabulary::{MODELLING_ASSUMPTIONS_LEAD, MORE_ABSTENTIONS};

use crate::result::JsonFinding;

/// How many abstentions a condition list names one by one. More than this
/// many plus one are named up to this count and the rest counted, so a board
/// with a dozen unchecked ratings still reads in one line.
const NAMED_ABSTENTIONS: usize = 2;

/// How many parts the modelling-assumption condition names per label before
/// it counts the rest.
const NAMED_PER_LABEL: usize = 4;

/// The conditions a pass rests on that the run's findings and assumptions imply.
///
/// In a stable order: abstentions first (each a specific unchecked part), then
/// the modelling assumptions (only when `model_dependent`, the surfaces whose
/// claims rest on the models: copper-only and descriptive surfaces do not).
#[must_use]
pub fn derived(
    findings: &[JsonFinding],
    assumptions: &[Assumption],
    model_dependent: bool,
) -> Vec<String> {
    let mut out = abstention_conditions(findings);
    if model_dependent {
        out.extend(modelling_condition(assumptions));
    }
    out
}

/// The lint finding a JSON finding was converted from, when it was one
/// (`crate::result::lint_findings_json` is the inverse).
fn as_lint(f: &JsonFinding) -> Option<LintFinding> {
    if f.check != "lint" {
        return None;
    }
    let check = LintCheck::ALL.into_iter().find(|c| c.as_str() == f.kind)?;
    let severity = match f.severity.as_str() {
        "serious" => Severity::High,
        "warning" => Severity::Medium,
        "note" => Severity::Low,
        _ => return None,
    };
    Some(LintFinding {
        check,
        severity,
        message: f.message.clone(),
        refs: f.refs.clone(),
        nets: f.nets.clone(),
    })
}

/// One sentence, single-spaced, ending in a full stop: conditions are joined
/// with a space, so each must stand on its own.
fn sentence(text: &str) -> String {
    let mut s = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    s
}

/// Every abstention among `findings`, in the words the plain report gives it
/// (so the triage can drop one it already lists as an item to inspect). An
/// abstention that makes the run PARTLY JUDGED (an analysis that judged
/// nothing, a stand-in's limit) is not a condition: it keeps the run from
/// passing at all.
fn abstention_conditions(findings: &[JsonFinding]) -> Vec<String> {
    use hauksbee_models::rating_basis::{is_condition, is_not_judged};
    let abstained: Vec<LintFinding> = findings
        .iter()
        .filter_map(as_lint)
        .filter(|f| f.is_abstention() && !is_not_judged(&f.message) && !is_condition(&f.message))
        .collect();
    if abstained.is_empty() {
        return Vec::new();
    }
    let report = NetLintReport {
        findings: abstained,
    };
    let subjects: Vec<String> = report.findings.iter().map(abstention_subject).collect();
    let mut said = BTreeSet::new();
    let whats: Vec<(String, String)> = crate::plain::plain_netlint(&report)
        .findings
        .iter()
        .map(|f| sentence(&f.what))
        .zip(subjects)
        .filter(|(w, _)| said.insert(w.clone()))
        .collect();
    if whats.len() <= NAMED_ABSTENTIONS + 1 {
        return whats.into_iter().map(|(w, _)| w).collect();
    }
    let rest = whats.len() - NAMED_ABSTENTIONS;
    // The counted ones still name what they cover, so the headline says which
    // parts the pass did not judge even when it does not say why.
    let mut covered = BTreeSet::new();
    let counted: Vec<String> = whats[NAMED_ABSTENTIONS..]
        .iter()
        .map(|(_, s)| s.clone())
        .filter(|s| covered.insert(s.clone()))
        .collect();
    let mut out: Vec<String> = whats
        .into_iter()
        .take(NAMED_ABSTENTIONS)
        .map(|(w, _)| w)
        .collect();
    out.push(
        MORE_ABSTENTIONS
            .replace("{n}", &rest.to_string())
            .replace("{subjects}", &name_list(&counted)),
    );
    out
}

/// What an abstention covers: its first part, else its first net.
fn abstention_subject(f: &LintFinding) -> String {
    f.refs.first().cloned().unwrap_or_else(|| {
        f.nets.first().map_or_else(
            || "a board-level check".to_string(),
            |n| format!("net \"{n}\""),
        )
    })
}

/// Up to [`NAMED_PER_LABEL`] names joined as prose, the rest counted:
/// "A, B, C, D and 3 more".
fn name_list(names: &[String]) -> String {
    let mut shown: Vec<String> = names.iter().take(NAMED_PER_LABEL).cloned().collect();
    let more = names.len().saturating_sub(NAMED_PER_LABEL);
    if more > 0 {
        shown.push(format!("{more} more"));
    }
    match shown.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The part an assumption is about, when it names one: the part its remedy
/// acts on, else its first part subject, else the part a parameter belongs
/// to, else (a board-scoped entry such as a partly modelled part) the part its
/// id is keyed by.
fn assumption_part(a: &Assumption) -> Option<String> {
    let from_remedy = match a.remedy() {
        Remedy::AddModel { reference }
        | Remedy::SetPartNumber { reference }
        | Remedy::MarkDnp { reference }
        | Remedy::RenameDuplicate { reference, .. } => Some(reference.clone()),
        _ => None,
    };
    from_remedy.or_else(|| match a.scope() {
        Scope::Subjects(set) => set
            .as_slice()
            .iter()
            .find(|e| matches!(e.kind(), EntityKind::Part | EntityKind::Pin))
            .map(|e| e.id().split('/').next().unwrap_or_default().to_string()),
        Scope::Parameter(p) => Some(p.subject().id().to_string()),
        Scope::Board => a
            .id()
            .as_str()
            .split_once(':')
            .map(|(_, key)| key)
            .filter(|key| {
                !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
            })
            .map(str::to_string),
        _ => None,
    })
}

/// One condition naming every modelling assumption the checks were judged on,
/// grouped by glossary label, or `None` when the models carry none.
fn modelling_condition(assumptions: &[Assumption]) -> Option<String> {
    // A part swept across its assumed corners is stated by its own condition,
    // which names the part as assumed and says how to lift it (bind its part
    // number). The class models behind those corners (a stand-in, a fallback,
    // one modelled only in part) are that same assumption, so the part is not
    // named again under any label.
    let swept: BTreeSet<String> = assumptions
        .iter()
        .filter(|a| a.term() == Term::AssumedCorner)
        .filter_map(assumption_part)
        .collect();
    let mut by_term: BTreeMap<Term, (BTreeSet<String>, usize)> = BTreeMap::new();
    for a in assumptions {
        let term = a.term();
        if matches!(term, Term::OpenPart | Term::AssumedCorner | Term::Waived) {
            continue;
        }
        let part = assumption_part(a);
        if part.as_ref().is_some_and(|p| swept.contains(p)) {
            continue;
        }
        let entry = by_term.entry(term).or_default();
        match part {
            Some(p) => {
                entry.0.insert(p);
            }
            None => entry.1 += 1,
        }
    }
    if by_term.is_empty() {
        return None;
    }
    let groups: Vec<String> = by_term
        .into_iter()
        .map(|(term, (parts, unnamed))| {
            let label = term.entry().label;
            let mut names: Vec<String> = parts.iter().take(NAMED_PER_LABEL).cloned().collect();
            let more = parts.len().saturating_sub(NAMED_PER_LABEL) + unnamed;
            if more > 0 {
                let noun = if names.is_empty() {
                    if more == 1 {
                        "entry"
                    } else {
                        "entries"
                    }
                } else {
                    "more"
                };
                names.push(format!("{more} {noun}"));
            }
            let list = match names.as_slice() {
                [] => String::new(),
                [one] => one.clone(),
                [init @ .., last] => format!("{} and {last}", init.join(", ")),
            };
            format!("{label} ({list})")
        })
        .collect();
    Some(format!(
        "{MODELLING_ASSUMPTIONS_LEAD}: {}.",
        groups.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::LintFinding;

    fn lint_json(
        check: LintCheck,
        severity: Severity,
        message: &str,
        refs: &[&str],
    ) -> JsonFinding {
        let report = NetLintReport {
            findings: vec![LintFinding {
                check,
                severity,
                message: message.to_string(),
                refs: refs.iter().map(ToString::to_string).collect(),
                nets: vec!["N$7".to_string()],
            }],
        };
        crate::result::lint_findings_json(&report).remove(0)
    }

    #[test]
    fn an_abstained_rating_qualifies_the_pass_and_a_judged_one_does_not() {
        // Faulty: the rating check abstained (current runs into an open part).
        let abstained = lint_json(
            LintCheck::DcOverstress,
            Severity::Low,
            "D3's current and power ratings check abstained: its current path runs into BAT \
             (no model, left open), so the rating is unverified, not met",
            &["D3"],
        );
        let said = derived(&[abstained], &[], true);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("D3") && said[0].contains("unverified"),
            "{said:?}"
        );
        // Control: a judged rating note (no abstention) adds nothing.
        let judged = lint_json(
            LintCheck::DcOverstress,
            Severity::Low,
            "D3 sees 0.10 W of its 0.50 W rating",
            &["D3"],
        );
        assert!(derived(&[judged], &[], true).is_empty());
    }

    #[test]
    fn an_unchecked_strap_mcu_qualifies_the_pass_on_every_surface() {
        let mcu = lint_json(
            LintCheck::UncheckedMcu,
            Severity::Low,
            "U1 (STM32F405) looks like a strap-bearing MCU but has no boot-strap table, so its \
             boot strap-pins were NOT checked",
            &["U1"],
        );
        // Abstentions qualify even a surface whose claims do not rest on models.
        let said = derived(std::slice::from_ref(&mcu), &[], false);
        assert_eq!(said.len(), 1);
        assert!(
            said[0].starts_with("U1 (STM32F405)") && said[0].ends_with('.'),
            "{said:?}"
        );
        // Control: a medium-severity strap finding is a real finding, not an abstention.
        let wrong = lint_json(
            LintCheck::StrapPin,
            Severity::Medium,
            "BOOT0 on U1 floats at reset",
            &["U1"],
        );
        assert!(derived(&[wrong], &[], false).is_empty());
    }

    #[test]
    fn a_partly_judged_abstention_is_not_a_pass_condition() {
        // The verdict refuses a pass for an analysis that judged nothing, so
        // naming it as a pass condition would contradict the verdict.
        let not_judged = hauksbee_models::rating_basis::not_judged("the DC solve failed");
        let f = lint_json(
            LintCheck::DcOverstress,
            Severity::Low,
            &format!("ratings check abstained: {not_judged}"),
            &[],
        );
        assert!(derived(&[f], &[], true).is_empty());
    }

    #[test]
    fn many_abstentions_are_named_up_to_a_bound_and_the_rest_counted() {
        let findings: Vec<JsonFinding> = (1..=5)
            .map(|i| {
                lint_json(
                    LintCheck::DcOverstress,
                    Severity::Low,
                    &format!("D{i}'s ratings check abstained: open part on its path"),
                    &[&format!("D{i}")],
                )
            })
            .collect();
        let said = derived(&findings, &[], true);
        assert_eq!(said.len(), NAMED_ABSTENTIONS + 1, "{said:?}");
        assert!(
            said[NAMED_ABSTENTIONS].starts_with("3 more checks"),
            "{said:?}"
        );
        // The counted ones still name the parts they cover; the named ones
        // are not repeated in the count.
        assert!(
            said[NAMED_ABSTENTIONS].contains("(D3, D4 and D5)"),
            "{said:?}"
        );
        // Control: three are named one by one (no "1 more" stub).
        let said = derived(&findings[..3], &[], true);
        assert_eq!(said.len(), 3);
        assert!(said.iter().all(|s| !s.contains("more checks")), "{said:?}");
    }

    #[test]
    fn modelling_assumptions_are_named_by_label_on_model_dependent_surfaces() {
        let assumptions = vec![
            Assumption::partial_model_for_component("U1", "U1", "BQ76200", "the charge pump", ""),
            Assumption::inferred_pin_role_for_component("Q5", "Q5", "1", "gate"),
            Assumption::inferred_pin_role_for_component("Q5", "Q5", "2", "source"),
            Assumption::open_part("J1", "CONN_01X02", "no model"),
        ];
        let said = derived(&[], &assumptions, true);
        assert_eq!(said.len(), 1, "{said:?}");
        let c = &said[0];
        assert!(c.starts_with(MODELLING_ASSUMPTIONS_LEAD), "{c}");
        assert!(c.contains("Inferred pin role (Q5)"), "{c}");
        assert!(c.contains("Partly modelled (U1)"), "{c}");
        // An open part is named by NOT COVERED and its own abstentions, not here.
        assert!(!c.contains("J1"), "{c}");
        // Control: copper-only surfaces do not rest on the models.
        assert!(derived(&[], &assumptions, false).is_empty());
        // Control: a fully modelled board has no modelling condition.
        assert!(derived(
            &[],
            &[Assumption::open_part("J1", "CONN_01X02", "no model")],
            true
        )
        .is_empty());
    }

    #[test]
    fn a_swept_part_is_not_named_again_for_its_stand_in_model() {
        let swept = Assumption::swept_corner("corners/D4", "D4", "led_hi, led_lo", false);
        // The class models behind the corners: one a fallback, one modelled
        // only in part. Both are the swept assumption the corner condition
        // already states.
        let partial = Assumption::partial_model(
            "D4",
            "LED_GENERIC",
            "Simulated at assumed corners, not any datasheet.",
            "",
        );
        let said = derived(&[], &[swept.clone(), partial.clone()], true);
        assert!(said.is_empty(), "{said:?}");
        // Faulty control: the same partial model on a part that was NOT swept
        // is a condition the pass rests on.
        let unswept = Assumption::partial_model(
            "D5",
            "LED_GENERIC",
            "Simulated at assumed corners, not any datasheet.",
            "",
        );
        let said = derived(&[], &[swept, partial, unswept], true);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("(D5)") && !said[0].contains("D4"),
            "{said:?}"
        );
    }

    #[test]
    fn long_label_lists_are_counted() {
        let assumptions: Vec<Assumption> = (1..=7)
            .map(|i| {
                let r = format!("Q{i}");
                Assumption::inferred_pin_role_for_component(&r, &r, "1", "gate")
            })
            .collect();
        let said = derived(&[], &assumptions, true);
        assert!(
            said[0].contains("Inferred pin role (Q1, Q2, Q3, Q4 and 3 more)"),
            "{said:?}"
        );
    }
}
