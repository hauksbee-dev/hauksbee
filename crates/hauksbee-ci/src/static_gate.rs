//! The board's own static verdict as part of a CI run.
//!
//! `hauksbee check` and `hauksbee-ci run` judge the same board. A simulation
//! run that only asserts `no_faults` would otherwise read GREEN over a board
//! the static check fails (a pin-function conflict, a serious short), so every
//! finding that fails the check becomes a failing row of the run, named with
//! its parts, nets and fix on every surface the run writes (human, JSON,
//! `JUnit`, GitHub annotations).
//!
//! A spec with an `asbuilt` overlay is judged on the reworked board: the
//! overlay is applied to the static board before the check runs
//! ([`hauksbee_engine::reports::check::static_gate_as_built`]), so the static
//! rows agree with the board the simulation ran. A finding on a part or net
//! an overlay entry reworks but the static board could not carry is the
//! design's: its row is NOT JUDGED (invalid), never a pass.

use hauksbee_engine::reports::check::StaticGate;
use hauksbee_engine::result::JsonFinding;
use hauksbee_ir::vocabulary::{
    STATIC_CHECK_FAILS_LEAD, STATIC_CHECK_KIND, STATIC_CHECK_NOT_JUDGED_LEAD,
};

use crate::assertions::AssertResult;

/// One failing row per gating finding, in the check's report order. A board
/// the static check passes adds no row, so a clean board's run is unchanged.
#[must_use]
pub fn static_check_results(findings: &[JsonFinding], seeds_total: usize) -> Vec<AssertResult> {
    gate_rows(
        &StaticGate {
            gating: findings.to_vec(),
            ..StaticGate::default()
        },
        seeds_total,
    )
}

/// [`static_check_results`] for a board with a waiver file.
///
/// Each gate-grade finding a waiver overrules still gets its row, failed but
/// marked waived with the waiver's reason, so the run shows what it excused
/// instead of hiding it.
#[must_use]
pub fn gate_rows(gate: &StaticGate, seeds_total: usize) -> Vec<AssertResult> {
    let mut rows: Vec<AssertResult> = Vec::new();
    let gating = gate.gating.iter().filter(|f| f.gates()).map(|f| (f, None));
    let waived = gate
        .waived
        .iter()
        .filter(|(f, _)| f.gates())
        .map(|(f, why)| (f, Some(why.clone())));
    for (finding, waiver) in gating.chain(waived) {
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
        // A finding on parts an unapplied rework touches is the design's (a
        // waived one stays what its waiver says).
        let unapplied = (gate.as_built.as_ref())
            .filter(|_| waiver.is_none())
            .and_then(|report| {
                (report.unapplied.iter()).find(|u| u.touches(&finding.refs, &finding.nets))
            });
        let detail = unapplied.map_or_else(
            || format!("{STATIC_CHECK_FAILS_LEAD}: [{}] {said}", finding.severity),
            |u| {
                format!(
                    "{STATIC_CHECK_NOT_JUDGED_LEAD} ({}: {}): [{}] {said}",
                    u.entry, u.why, finding.severity
                )
            },
        );
        rows.push(AssertResult {
            detail,
            invalid: unapplied.is_some(),
            why: Some(
                finding
                    .fix
                    .clone()
                    .unwrap_or_else(|| finding.message.clone()),
            ),
            subject_nets: finding.nets.clone(),
            subject_refs: finding.refs.clone(),
            waived: waiver,
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
    fn a_waived_gating_finding_keeps_its_row_marked_waived() {
        let gate = StaticGate {
            as_built: None,
            gating: vec![finding("drc", "serious")],
            waived: vec![
                (
                    finding("lint", "serious"),
                    "placement only (until 2030-01-01)".into(),
                ),
                // A waived note never gated, so it has no row to keep.
                (finding("lint", "info"), "noise (until 2030-01-01)".into()),
            ],
        };
        let rows = gate_rows(&gate, 1);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].waived.is_none() && !rows[0].passed);
        assert!(
            !rows[1].passed,
            "a waived row still reads as the failure it is"
        );
        assert_eq!(
            rows[1].waived.as_deref(),
            Some("placement only (until 2030-01-01)")
        );
        assert_eq!(
            rows[1].label,
            "static_check: lint pin_function_conflict on U1, U2, GPIO43"
        );
    }

    #[test]
    fn a_finding_the_check_does_not_gate_on_adds_no_row() {
        // An informational lint note passes `hauksbee check`, so it cannot fail the run.
        assert!(static_check_results(&[finding("lint", "info")], 1).is_empty());
        assert!(static_check_results(&[], 1).is_empty());
    }

    #[test]
    fn a_finding_on_a_rework_the_static_board_could_not_carry_is_not_judged() {
        use hauksbee_engine::asbuilt::{AsBuiltReport, UnappliedEntry};
        let report = AsBuiltReport {
            lines: Vec::new(),
            unapplied: vec![UnappliedEntry {
                entry: "replace U2".into(),
                why: "von=3.6 is a switch trip point".into(),
                refs: vec!["U2".into()],
                nets: Vec::new(),
            }],
        };
        let mut other = finding("lint", "serious");
        other.refs = vec!["U9".into()];
        other.nets = vec!["GPIO7".into()];
        let gate = StaticGate {
            gating: vec![finding("lint", "serious"), other],
            waived: Vec::new(),
            as_built: Some(report),
        };
        let rows = gate_rows(&gate, 1);
        assert_eq!(rows.len(), 2, "{rows:?}");
        // On U2: the design's finding, not judged on the reworked board.
        assert!(rows[0].invalid && !rows[0].passed, "{:?}", rows[0]);
        assert!(
            rows[0].detail.starts_with(STATIC_CHECK_NOT_JUDGED_LEAD)
                && rows[0].detail.contains("replace U2"),
            "{}",
            rows[0].detail
        );
        // Untouched by the rework: an ordinary failure.
        assert!(!rows[1].invalid && !rows[1].passed);
        assert!(rows[1].detail.starts_with(STATIC_CHECK_FAILS_LEAD));
    }
}
