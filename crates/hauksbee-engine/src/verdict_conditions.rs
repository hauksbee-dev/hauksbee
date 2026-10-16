//! What a pass rests on beyond the conditions its surface states outright.
//!
//! A pass says the checks found nothing that blocks. It must not say more
//! than was judged, so the gaps the report already lists become conditions
//! beside the verdict (the `verdict_conditions` of `--json`, the "this rests
//! on:" list of `--plain`, the closing `VERDICT:` line, the web headline and
//! `hauksbee-ci`'s report all read the same list):
//!
//! - an abstention: a check that declined to judge one part or net
//!   ([`LintFinding::is_abstention`]: a rating with no working model on its
//!   current path, an operating envelope with no known level, a strap-bearing
//!   MCU with no strap table, unfilled zones). The rest of the board was
//!   judged, so the run stays a pass; the gap is named.
//! - on a model-dependent surface, what the run's assumptions leave
//!   unjudged, each kind in its own words:
//!   - parts with no model (open parts): a verdict-critical one already makes
//!     the run PARTLY JUDGED, so the ones left on a pass are named here;
//!   - model-fidelity assumptions (a stand-in or reduced-fidelity model, a
//!     part modelled only in part, a pin role inferred from the package, a
//!     documented default value, a model that does not state its scope): one
//!     condition groups them by glossary label and names the parts;
//!   - checks that did not run, or parts of the board the run never
//!     exercised;
//!   - inputs read past a known reader gap;
//!   - every placed part treated as fitted for want of a BOM.
//!
//! Every list names a few subjects and counts the rest.
//!
//! What stays out, and why:
//!
//! - a swept assumed corner: its surface already states it as a condition;
//! - a waived finding or assumption: the waiver section lists it with its
//!   reason and date;
//! - a violated operating limit: the check that measured it reports it as a
//!   finding (`regulator_cout` for the binder's output-capacitor limit, the
//!   board-limit failure in `hauksbee-ci`), and it is not an assumption;
//! - an analysis that did not run to a result, or a limit judged against a
//!   stand-in card's rating: those make the run PARTLY JUDGED, not a pass
//!   (`JsonReport::stated_condition`).

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::{LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_ir::evidence::{Assumption, EntityKind, Remedy, Scope};
use hauksbee_ir::glossary::Term;
use hauksbee_ir::vocabulary::{
    open_parts_condition, FITTED_BY_DEFAULT_CONDITION, MODELLING_ASSUMPTIONS_LEAD,
    MORE_ABSTENTIONS, NOT_JUDGED_BY_RUN_LEAD, READER_LIMITATION_LEAD,
};

use crate::result::JsonFinding;

/// How many abstentions a condition list names one by one. More than this
/// many plus one are named up to this count and the rest counted, so a board
/// with a dozen unchecked ratings still reads in one line.
const NAMED_ABSTENTIONS: usize = 2;

/// How many subjects a condition names per label before it counts the rest.
const NAMED_PER_LABEL: usize = 4;

/// The conditions a pass rests on that the run's findings and assumptions imply.
///
/// In a stable order: abstentions first (each a specific unchecked part), then
/// what the assumptions leave unjudged (only when `model_dependent`, the
/// surfaces whose claims rest on the models: copper-only and descriptive
/// surfaces do not).
#[must_use]
pub fn derived(
    findings: &[JsonFinding],
    assumptions: &[Assumption],
    model_dependent: bool,
) -> Vec<String> {
    let mut out = abstention_conditions(findings);
    if model_dependent {
        out.extend(assumption_conditions(assumptions));
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
///
/// Abstentions the plain report words identically (a generic sentence on
/// several parts) are said once, naming every part they cover.
// False positive: "{subjects}" is the vocabulary template's placeholder, filled by replace.
#[allow(clippy::literal_string_with_formatting_args)]
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
    // One entry per distinct sentence, in first-seen order, with every
    // subject it covers and how many abstentions it stands for.
    let mut groups: Vec<(String, Vec<String>, usize)> = Vec::new();
    let plain = crate::plain::plain_netlint(&report);
    for (what, subject) in plain
        .findings
        .iter()
        .map(|f| sentence(&f.what))
        .zip(subjects)
    {
        if let Some(group) = groups.iter_mut().find(|(w, _, _)| *w == what) {
            if !group.1.contains(&subject) {
                group.1.push(subject);
            }
            group.2 += 1;
        } else {
            groups.push((what, vec![subject], 1));
        }
    }
    let named = |(what, subjects, _): &(String, Vec<String>, usize)| {
        // A sentence that already names its one subject stands as it is.
        if subjects.len() <= 1 {
            what.clone()
        } else {
            format!("{} ({}).", what.trim_end_matches('.'), name_list(subjects))
        }
    };
    if groups.len() <= NAMED_ABSTENTIONS + 1 {
        return groups.iter().map(named).collect();
    }
    let rest: usize = groups[NAMED_ABSTENTIONS..].iter().map(|g| g.2).sum();
    // The counted ones still name what they cover, so the headline says which
    // parts the pass did not judge even when it does not say why.
    let mut covered: Vec<String> = Vec::new();
    for subject in groups[NAMED_ABSTENTIONS..].iter().flat_map(|g| &g.1) {
        if !covered.contains(subject) {
            covered.push(subject.clone());
        }
    }
    let mut out: Vec<String> = groups.iter().take(NAMED_ABSTENTIONS).map(named).collect();
    out.push(
        MORE_ABSTENTIONS
            .replace("{n}", &rest.to_string())
            .replace("{subjects}", &name_list(&covered)),
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
    counted_list(names, 0)
}

/// [`name_list`] plus `unnamed` entries that have no name to show, counted
/// with the rest ("A, B and 2 more", or "2 entries" when none has a name).
fn counted_list(names: &[String], unnamed: usize) -> String {
    let mut shown: Vec<String> = names.iter().take(NAMED_PER_LABEL).cloned().collect();
    let more = names.len().saturating_sub(NAMED_PER_LABEL) + unnamed;
    if more > 0 {
        let noun = match (shown.is_empty(), more) {
            (false, _) => "more",
            (true, 1) => "entry",
            (true, _) => "entries",
        };
        shown.push(format!("{more} {noun}"));
    }
    match shown.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The reader-facing name an assumption is about: the name its constructor
/// recorded (a designator, a check, a net, an input), else the part its
/// remedy acts on, else its first part subject, else the part a parameter
/// belongs to. `None` when it names nothing a reader would recognise: a
/// board-scoped entry's id key is a producer key (`U7-tj-max`,
/// `reader-note/<digest>`), never printed.
fn assumption_part(a: &Assumption) -> Option<String> {
    if let Some(name) = a.subject_name() {
        return Some(name.to_string());
    }
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
        _ => None,
    })
}

/// Which condition an assumption's term belongs to, or `None` when it is not
/// a condition of the pass (see the module docs for why).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// A part with no model.
    Open,
    /// A model-fidelity assumption.
    Modelling,
    /// A check that did not run, or a part of the board never exercised.
    NotJudged,
    /// An input read past a known reader gap.
    Reader,
    /// Every placed part treated as fitted.
    Fitted,
}

const fn condition_kind(term: Term) -> Option<Kind> {
    match term {
        Term::OpenPart | Term::IdentityOnly => Some(Kind::Open),
        Term::SubstituteModel
        | Term::ReducedFidelity
        | Term::ExecutablePartial
        | Term::InferredPinRole
        | Term::DefaultParameter
        | Term::ScopeUnspecified => Some(Kind::Modelling),
        Term::NotChecked | Term::NotExercised => Some(Kind::NotJudged),
        Term::ParserLimitation => Some(Kind::Reader),
        Term::FittedByDefault => Some(Kind::Fitted),
        // A swept corner is stated by its own surface; a waiver is listed
        // with its reason; a violated limit is a finding, not an assumption;
        // the rest are not assumption terms.
        Term::AssumedCorner
        | Term::Waived
        | Term::OperatingLimit
        | Term::Pass
        | Term::Fail
        | Term::Invalid
        | Term::ConditionalPass
        | Term::DefaultRules
        | Term::ReportOnlyExit
        | Term::McuHalted
        | Term::UartActivity => None,
    }
}

/// How many parts an open-part claim covers: one claim can stand for several
/// parts the run cannot tell apart, each a subject of its scope.
fn open_part_count(a: &Assumption) -> usize {
    match a.scope() {
        Scope::Subjects(set) => set.as_slice().len().max(1),
        _ => 1,
    }
}

/// The conditions a pass rests on that `assumptions` imply, one per kind of
/// gap, in a stable order: parts with no model, the modelling assumptions,
/// what the run did not judge, reader gaps, then parts assumed fitted. Empty
/// when the assumptions leave nothing unjudged. `hauksbee-ci` reads this for
/// the assumptions on its assertions' paths, so every surface words them the
/// same.
#[must_use]
pub fn assumption_conditions(assumptions: &[Assumption]) -> Vec<String> {
    // A part swept across its assumed corners is stated by its own condition,
    // which names the part as assumed and says how to lift it (bind its part
    // number). The class models behind those corners (a stand-in, a fallback,
    // one modelled only in part) are that same assumption, so the part is not
    // named again under a model-fidelity label. Its other assumptions (a pin
    // role inferred from the package, a default value) are not what the
    // corners sweep, so they stay named.
    let swept: BTreeSet<String> = assumptions
        .iter()
        .filter(|a| a.term() == Term::AssumedCorner)
        .filter_map(assumption_part)
        .collect();
    // Per kind, per term: the named subjects and how many have no name.
    let mut by_kind: BTreeMap<Kind, BTreeMap<Term, (BTreeSet<String>, usize)>> = BTreeMap::new();
    let mut open_parts = 0usize;
    for a in assumptions {
        let term = a.term();
        let Some(kind) = condition_kind(term) else {
            continue;
        };
        let part = assumption_part(a);
        let corner_model = matches!(
            term,
            Term::ReducedFidelity | Term::ExecutablePartial | Term::SubstituteModel
        );
        if corner_model && part.as_ref().is_some_and(|p| swept.contains(p)) {
            continue;
        }
        if kind == Kind::Open {
            open_parts += open_part_count(a);
        }
        let entry = by_kind.entry(kind).or_default().entry(term).or_default();
        match part {
            Some(p) => {
                entry.0.insert(p);
            }
            None => entry.1 += 1,
        }
    }
    by_kind
        .into_iter()
        .map(|(kind, terms)| {
            // Every term of the kind as "Label (A, B and 2 more)".
            let labelled = || {
                terms
                    .iter()
                    .map(|(term, (names, unnamed))| {
                        let names: Vec<String> = names.iter().cloned().collect();
                        format!(
                            "{} ({})",
                            term.entry().label,
                            counted_list(&names, *unnamed)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            // Every subject of the kind, the terms merged.
            let merged = || {
                let mut names: BTreeSet<String> = BTreeSet::new();
                let mut unnamed = 0;
                for (set, n) in terms.values() {
                    names.extend(set.iter().cloned());
                    unnamed += n;
                }
                let names: Vec<String> = names.into_iter().collect();
                counted_list(&names, unnamed)
            };
            match kind {
                Kind::Open => {
                    let names: BTreeSet<String> = terms
                        .values()
                        .flat_map(|(set, _)| set.iter().cloned())
                        .collect();
                    let names: Vec<String> = names.into_iter().collect();
                    let shown = counted_list(&names, open_parts.saturating_sub(names.len()));
                    open_parts_condition(open_parts.max(names.len()), &shown)
                }
                Kind::Modelling => format!("{MODELLING_ASSUMPTIONS_LEAD}: {}.", labelled()),
                Kind::NotJudged => format!("{NOT_JUDGED_BY_RUN_LEAD}: {}.", labelled()),
                Kind::Reader => format!("{READER_LIMITATION_LEAD}: {}.", merged()),
                Kind::Fitted => FITTED_BY_DEFAULT_CONDITION.replace("{subjects}", &merged()),
            }
        })
        .collect()
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
        assert_eq!(said.len(), 2, "{said:?}");
        // A part with no model is not a modelling assumption: it has its own
        // condition, which says it was not judged.
        assert!(
            said[0].starts_with("1 part has no model and was not judged (J1)"),
            "{said:?}"
        );
        let c = &said[1];
        assert!(c.starts_with(MODELLING_ASSUMPTIONS_LEAD), "{c}");
        assert!(c.contains("Inferred pin role (Q5)"), "{c}");
        assert!(c.contains("Partly modelled (U1)"), "{c}");
        assert!(!c.contains("J1"), "{c}");
        // Control: copper-only surfaces do not rest on the models.
        assert!(derived(&[], &assumptions, false).is_empty());
        // Control: a fully modelled board with no assumptions has no condition.
        assert!(derived(&[], &[], true).is_empty());
    }

    #[test]
    fn open_parts_are_named_up_to_a_bound_and_counted() {
        let open: Vec<Assumption> = (1..=7)
            .map(|i| Assumption::open_part(&format!("U{i}"), "XYZ", "no model matched"))
            .collect();
        let said = derived(&[], &open, true);
        assert_eq!(
            said,
            [open_parts_condition(7, "U1, U2, U3, U4 and 3 more")],
            "{said:?}"
        );
        // Control: a pass with no open part names none.
        let modelled = [Assumption::inferred_pin_role_for_component(
            "Q1", "Q1", "1", "gate",
        )];
        assert!(derived(&[], &modelled, true)
            .iter()
            .all(|c| !c.contains("no model")));
    }

    #[test]
    fn a_check_that_did_not_run_is_not_called_a_modelling_assumption() {
        use hauksbee_ir::evidence::{AssumptionSource, Subject};
        let drc = Assumption::not_checked(AssumptionSource::Reader, "drc", None, "", "");
        let note = Assumption::parser_limitation(
            AssumptionSource::Reader,
            Subject::new(
                "reader-cross-check/0123abcd",
                "the reader's disagreeing sources",
            ),
            Scope::Board,
            "two netlists disagree",
            "",
        );
        let said = derived(&[], &[drc, note], true);
        assert_eq!(said.len(), 2, "{said:?}");
        assert_eq!(
            said[0],
            format!("{NOT_JUDGED_BY_RUN_LEAD}: Not checked (drc).")
        );
        // A reader gap names the input by its prose name, never its id key.
        assert_eq!(
            said[1],
            format!("{READER_LIMITATION_LEAD}: the reader's disagreeing sources.")
        );
        assert!(said
            .iter()
            .all(|c| !c.starts_with(MODELLING_ASSUMPTIONS_LEAD)));
        assert!(said.iter().all(|c| !c.contains("0123abcd")), "{said:?}");
    }

    #[test]
    fn a_violated_operating_limit_is_a_finding_not_a_condition() {
        // The regulator_cout check reports the shortfall as a finding; the
        // assumption is not something the pass rests on.
        let limit = Assumption::operating_limit_violated(
            "U8",
            "U8",
            "U8 requires at least 1 uF output capacitance for stability",
            "",
        );
        assert!(derived(&[], &[limit], true).is_empty());
        // Control: a part modelled only in part IS a modelling assumption.
        let partial = Assumption::partial_model_for_component("U8", "U8", "", "the enable", "");
        assert_eq!(derived(&[], &[partial], true).len(), 1);
    }

    #[test]
    fn a_board_scoped_entry_is_named_by_its_part_not_its_key() {
        let tj = Assumption::partial_model_for_component(
            "U7-tj-max",
            "U7",
            "",
            "U7's limits past their hottest row fall to zero at an assumed junction limit",
            "",
        );
        let said = derived(&[], &[tj], true);
        assert!(said[0].contains("Partly modelled (U7)"), "{said:?}");
        assert!(!said[0].contains("tj-max"), "{said:?}");
    }

    #[test]
    fn every_part_assumed_fitted_is_its_own_condition() {
        use hauksbee_ir::evidence::{AssumptionSource, Subject};
        let fitted = Assumption::fitted_by_default(
            AssumptionSource::Reader,
            Subject::same("the exchange input"),
            Scope::Board,
        );
        let said = derived(&[], &[fitted], true);
        assert_eq!(
            said,
            [FITTED_BY_DEFAULT_CONDITION.replace("{subjects}", "the exchange input")]
        );
    }

    #[test]
    fn abstentions_worded_alike_name_every_part_they_cover() {
        // The same generic sentence on two parts is said once, naming both:
        // deduplicating by the words alone would drop the second part.
        let message = "zones not filled: connectivity through the pours was not checked";
        let findings: Vec<JsonFinding> = ["D1", "D2"]
            .iter()
            .map(|r| lint_json(LintCheck::UnroutedPad, Severity::Low, message, &[r]))
            .collect();
        let said = derived(&findings, &[], true);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].ends_with("(D1 and D2)."), "{said:?}");
        // Control: one part keeps the sentence as it is.
        let said = derived(&findings[..1], &[], true);
        assert_eq!(said, [format!("{message}.")], "{said:?}");
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
        let said = derived(&[], &[swept.clone(), partial.clone(), unswept], true);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("(D5)") && !said[0].contains("D4"),
            "{said:?}"
        );
        // The corners sweep the part's parameters, not its pinout: a pin role
        // inferred from the package on the swept part is still named.
        let pin = Assumption::inferred_pin_role_for_component("D4", "D4", "1", "anode");
        let said = derived(&[], &[swept, partial, pin], true);
        assert!(said[0].contains("Inferred pin role (D4)"), "{said:?}");
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
