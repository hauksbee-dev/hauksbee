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
        // An unfitted alternate footprint is an assumption the report states,
        // not two sources the board fights with.
        severity: if c.unfitted_alternate.is_some() {
            Severity::Low
        } else {
            Severity::High
        },
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
        regulator(reference, "AMS1117-3.3", out)
    }

    fn regulator(reference: &str, value: &str, out: i64) -> Component {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        Component {
            reference: reference.into(),
            value: value.into(),
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

    /// A 3.3 V and a 5 V regulator from one input onto a rail named 3.3 V: the
    /// 5 V part cannot be what holds that rail, so it reads as an unfitted
    /// alternate footprint (a stated note). The same pair on a rail whose name
    /// states no voltage, or matching neither part, still fights.
    #[test]
    fn a_regulator_that_contradicts_its_rail_name_is_an_unfitted_alternate() {
        let board = |rail: &str| ExtractedBoard {
            name: "alternate-ldo".into(),
            nets: vec![Net::new(1, "GND"), Net::new(2, "+5V"), Net::new(3, rail)],
            components: vec![
                regulator("U1", "AMS1117-3.3", 3),
                regulator("U2", "AMS1117-5.0", 3),
            ],
        };
        let lib = ModelLibrary::builtin();
        let named = rail_conflict_lint(&board("+3V3"), &lib);
        let found: Vec<_> = named.of_check(LintCheck::RailConflict).collect();
        assert_eq!(found.len(), 1, "{:?}", named.findings);
        assert_eq!(found[0].severity, Severity::Low);
        assert!(
            found[0].message.contains("U2 (5 V)"),
            "{}",
            found[0].message
        );
        assert!(
            found[0].message.contains("not fitted"),
            "{}",
            found[0].message
        );
        for rail in ["VOUT", "+12V"] {
            let fight = rail_conflict_lint(&board(rail), &lib);
            let found: Vec<_> = fight.of_check(LintCheck::RailConflict).collect();
            assert_eq!(found.len(), 1, "{rail}: {:?}", fight.findings);
            assert_eq!(found[0].severity, Severity::High, "{rail}");
        }
    }
}
