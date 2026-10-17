//! The board's own static verdict as part of a CI run.
//!
//! `hauksbee check` and `hauksbee-ci run` judge the same board. A simulation
//! run that only asserts `no_faults` would otherwise read GREEN over a board
//! the static check fails (a pin-function conflict, a serious short), so every
//! finding that fails the check becomes a failing row of the run, named with
//! its parts, nets and fix on every surface the run writes (human, JSON,
//! `JUnit`, GitHub annotations).

use hauksbee_engine::result::JsonFinding;
use hauksbee_ir::vocabulary::{STATIC_CHECK_FAILS_LEAD, STATIC_CHECK_KIND};

use crate::assertions::AssertResult;

/// One failing row per gating finding, in the check's report order. A board
/// the static check passes adds no row, so a clean board's run is unchanged.
#[must_use]
pub fn static_check_results(findings: &[JsonFinding], seeds_total: usize) -> Vec<AssertResult> {
    let mut rows: Vec<AssertResult> = Vec::new();
    for finding in findings.iter().filter(|f| f.gates()) {
        let mut subjects: Vec<&str> = finding.refs.iter().map(String::as_str).collect();
        subjects.extend(finding.nets.iter().map(String::as_str));
        let on = if subjects.is_empty() {
            String::new()
        } else {
            format!(" on {}", subjects.join(", "))
        };
        let base = format!(
            "{STATIC_CHECK_KIND}: {} {}{on}",
            finding.check, finding.kind
        );
        // Two findings of one kind on the same parts stay two rows.
        let same = rows
            .iter()
            .filter(|r| r.label == base || r.label.starts_with(&format!("{base} #")))
            .count();
        let label = if same == 0 {
            base
        } else {
            format!("{base} #{}", same + 1)
        };
        let said = if finding.plain.trim().is_empty() {
            &finding.message
        } else {
            &finding.plain
        };
        rows.push(AssertResult {
            detail: format!("{STATIC_CHECK_FAILS_LEAD}: [{}] {said}", finding.severity),
            why: Some(
                finding
                    .fix
                    .clone()
                    .unwrap_or_else(|| finding.message.clone()),
            ),
            subject_nets: finding.nets.clone(),
            subject_refs: finding.refs.clone(),
            ..AssertResult::shell(label, STATIC_CHECK_KIND, seeds_total)
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(check: &str, severity: &str) -> JsonFinding {
        JsonFinding {
            check: check.into(),
            kind: "pin_function_conflict".into(),
            severity: severity.into(),
            nets: vec!["GPIO43".into()],
            location_mm: None,
            layer: None,
            refs: vec!["U1".into(), "U2".into()],
            actionable: true,
            message: "3 UART TX pins drive GPIO43 and nothing receives".into(),
            plain: String::new(),
            fix: Some("Cross TX to RX".into()),
        }
    }

    #[test]
    fn a_gating_static_finding_fails_the_run_and_names_its_parts_and_fix() {
        let rows = static_check_results(&[finding("lint", "serious")], 2);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(!row.passed && !row.invalid && row.waived.is_none());
        assert_eq!(row.kind, STATIC_CHECK_KIND);
        assert_eq!(
            row.label,
            "static_check: lint pin_function_conflict on U1, U2, GPIO43"
        );
        assert!(
            row.detail.starts_with(STATIC_CHECK_FAILS_LEAD),
            "{}",
            row.detail
        );
        assert!(row.detail.contains("nothing receives"), "{}", row.detail);
        assert_eq!(row.why.as_deref(), Some("Cross TX to RX"));
        assert_eq!(row.seeds_total, 2);
        // A second finding of the same kind on the same parts keeps its own row.
        let rows =
            static_check_results(&[finding("lint", "serious"), finding("lint", "serious")], 1);
        assert_eq!(rows[1].label, format!("{} #2", rows[0].label));
    }

    #[test]
    fn a_finding_the_check_does_not_gate_on_adds_no_row() {
        // An informational lint note passes `hauksbee check`, so it cannot fail the run.
        assert!(static_check_results(&[finding("lint", "info")], 1).is_empty());
        assert!(static_check_results(&[], 1).is_empty());
    }
}
