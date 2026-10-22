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

    fn diode(reference: &str, anode: i64, cathode: i64) -> Component {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        Component {
            reference: reference.into(),
            value: "SS14".into(),
            footprint: "Diode_SMD:D_SMA".into(),
            pins: vec![pin("A", anode), pin("K", cathode)],
            ..Default::default()
        }
    }

    /// Two LDOs, each behind its own diode onto one rail, are a diode-OR: one
    /// source at a time carries the rail, they do not fight. Without the diodes
    /// the same two outputs on one net do fight.
    #[test]
    fn two_regulators_each_behind_a_diode_are_a_diode_or_not_a_fight() {
        let nets = vec![
            Net::new(1, "GND"),
            Net::new(2, "+5V"),
            Net::new(3, "+3V3"),
            Net::new(4, "OUT_A"),
            Net::new(5, "OUT_B"),
        ];
        let lib = ModelLibrary::builtin();
        let ored = ExtractedBoard {
            name: "diode-or".into(),
            nets: nets.clone(),
            components: vec![
                ldo("U1", 4),
                ldo("U2", 5),
                diode("D1", 4, 3),
                diode("D2", 5, 3),
            ],
        };
        let found = rail_conflict_lint(&ored, &lib);
        assert_eq!(
            found.of_check(LintCheck::RailConflict).count(),
            0,
            "{:?}",
            found.findings
        );
        let tied = ExtractedBoard {
            name: "tied".into(),
            nets,
            components: vec![ldo("U1", 3), ldo("U2", 3)],
        };
        let found = rail_conflict_lint(&tied, &lib);
        assert_eq!(
            found.of_check(LintCheck::RailConflict).count(),
            1,
            "{:?}",
            found.findings
        );
    }

    fn two_pin(reference: &str, value: &str, footprint: &str, nets: [i64; 2]) -> Component {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        Component {
            reference: reference.into(),
            value: value.into(),
            footprint: footprint.into(),
            pins: vec![pin("1", nets[0]), pin("2", nets[1])],
            ..Default::default()
        }
    }

    /// A buck from +5V to +3V3: switch pin, inductor, output rail. `switch` names
    /// the switch-node net. The node between the switch pin and the inductor is
    /// pulsed by the converter, not a rail, so a voltage in its name (a
    /// `+5V_SW` that echoes the input) does not make the inductor join two
    /// rails of different voltage.
    fn buck(switch: &str) -> ExtractedBoard {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        // SY8089AAAC, SOT23-5: 1 en, 2 gnd, 3 sw, 4 in, 5 fb.
        let u1 = Component {
            reference: "U1".into(),
            value: "SY8089AAAC".into(),
            footprint: "Package_TO_SOT_SMD:SOT-23-5".into(),
            pins: vec![pin("1", 2), pin("2", 1), pin("3", 4), pin("4", 2), pin("5", 3)],
            ..Default::default()
        };
        ExtractedBoard {
            name: "buck".into(),
            nets: vec![
                Net::new(1, "GND"),
                Net::new(2, "+5V"),
                Net::new(3, "+3V3"),
                Net::new(4, switch),
            ],
            components: vec![
                u1,
                two_pin("L1", "2.2uH", "Inductor_SMD:L_0805_2012Metric", [4, 3]),
                two_pin("C1", "22u", "Capacitor_SMD:C_0805_2012Metric", [3, 1]),
            ],
        }
    }

    #[test]
    fn a_converter_switch_node_named_for_a_voltage_is_not_a_rail() {
        let lib = ModelLibrary::builtin();
        for switch in ["SW", "+5V_SW"] {
            let found = rail_conflict_lint(&buck(switch), &lib);
            assert_eq!(
                found.of_check(LintCheck::RailConflict).count(),
                0,
                "{switch}: {:?}",
                found.findings
            );
        }
    }

    /// A TLV61070A boost from +3V3 to +5V: the inductor runs from the input
    /// rail to the switch pin, whose net is named for the output voltage.
    fn boost(switch: &str) -> ExtractedBoard {
        let pin = |number: &str, net| Pin {
            number: number.into(),
            net: Some(net),
            ..Default::default()
        };
        // SOT23-6: 1 sw, 2 gnd, 3 en, 4 fb, 5 out, 6 in.
        let u1 = Component {
            reference: "U1".into(),
            value: "TLV61070A".into(),
            footprint: "Package_TO_SOT_SMD:SOT-23-6".into(),
            pins: vec![
                pin("1", 4),
                pin("2", 1),
                pin("3", 2),
                pin("4", 5),
                pin("5", 3),
                pin("6", 2),
            ],
            ..Default::default()
        };
        ExtractedBoard {
            name: "boost".into(),
            nets: vec![
                Net::new(1, "GND"),
                Net::new(2, "+3V3"),
                Net::new(3, "+5V"),
                Net::new(4, switch),
                Net::new(5, "FB"),
            ],
            components: vec![
                u1,
                two_pin("L1", "2.2uH", "Inductor_SMD:L_0805_2012Metric", [2, 4]),
                two_pin("C1", "22u", "Capacitor_SMD:C_0805_2012Metric", [3, 1]),
            ],
        }
    }

    #[test]
    fn a_boost_switch_node_named_for_the_output_voltage_is_not_a_rail_conflict() {
        let lib = ModelLibrary::builtin();
        for switch in ["SW", "+5V_SW"] {
            let found = rail_conflict_lint(&boost(switch), &lib);
            assert_eq!(
                found.of_check(LintCheck::RailConflict).count(),
                0,
                "{switch}: {:?}",
                found.findings
            );
        }
    }

    /// The same inductor-shaped link between two rails of different voltage with
    /// no converter between them is a real conflict: nothing sets the node.
    #[test]
    fn a_zero_ohm_link_between_two_named_rails_of_different_voltage_is_a_conflict() {
        let lib = ModelLibrary::builtin();
        let board = ExtractedBoard {
            name: "linked".into(),
            nets: vec![Net::new(1, "GND"), Net::new(2, "+5V"), Net::new(3, "+3V3")],
            components: vec![two_pin(
                "R1",
                "0R",
                "Resistor_SMD:R_0603_1608Metric",
                [2, 3],
            )],
        };
        let found = rail_conflict_lint(&board, &lib);
        assert_eq!(
            found.of_check(LintCheck::RailConflict).count(),
            1,
            "{:?}",
            found.findings
        );
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
