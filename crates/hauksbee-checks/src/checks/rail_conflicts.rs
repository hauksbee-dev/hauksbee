//! One rail, more than one source: the binder's rail-ownership conflicts as
//! lint findings.
//!
//! The binder groups nets a series conductor joins into rails and records
//! every rail that two regulators drive, or that joins named rails of
//! different voltages (`hauksbee_bind::rail_owner`). A simulation of such a
//! board runs, but the sources fight, so this static surface says so before
//! any number from that rail is trusted.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use hauksbee_bind::bind_report::BindReport;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

/// Every rail more than one source drives.
#[must_use]
pub fn rail_conflict_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    rail_conflict_findings(&hauksbee_bind::binder::bind_board(board, lib).report)
}

/// [`rail_conflict_lint`] over a board already bound into `report`.
#[must_use]
pub fn rail_conflict_findings(report: &BindReport) -> NetLintReport {
    let findings = report.rail_conflicts.iter().map(|c| LintFinding {
        check: LintCheck::RailConflict,
        severity: Severity::High,
        message: c.message.clone(),
        refs: c.parts.clone(),
        nets: c.nets.clone(),
    });
    NetLintReport {
        findings: findings.collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_extract::{Component, Net, Pin};

    fn ldo(reference: &str, out: i64) -> Component {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        Component {
            reference: reference.into(),
            value: "AMS1117-3.3".into(),
            footprint: "Package_TO_SOT_SMD:SOT-223-3_TabPin2".into(),
            pins: vec![pin("1", 1), pin("2", out), pin("3", 2)],
            ..Default::default()
        }
    }

    #[test]
    fn two_regulators_on_one_rail_are_a_finding_and_one_is_not() {
        let board = |outs: [i64; 2]| ExtractedBoard {
            name: "tied-ldos".into(),
            nets: vec![
                Net::new(1, "GND"),
                Net::new(2, "+5V"),
                Net::new(3, "+3V3"),
                Net::new(4, "+3V3_B"),
            ],
            components: vec![ldo("U1", outs[0]), ldo("U2", outs[1])],
        };
        let lib = ModelLibrary::builtin();
        let tied = rail_conflict_lint(&board([3, 3]), &lib);
        let found: Vec<_> = tied.of_check(LintCheck::RailConflict).collect();
        assert_eq!(found.len(), 1, "{:?}", tied.findings);
        assert_eq!(found[0].refs, ["U1", "U2"]);
        assert!(found[0].message.contains("fight"), "{}", found[0].message);
        let apart = rail_conflict_lint(&board([3, 4]), &lib);
        assert_eq!(apart.of_check(LintCheck::RailConflict).count(), 0);
    }
}
