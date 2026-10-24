//! A backup-domain part with its main and battery supplies the wrong way round.
//!
//! A real-time clock, or a microcontroller with a battery-backed domain, has
//! two supply inputs: a main supply (`VCC`, `VDD`) that powers it while the
//! board is up, and a backup input (`VBAT`, `VBACKUP`) that keeps the clock
//! and its RAM alive from a cell when the main supply is gone. The part
//! switches to the backup input on its own when the main supply falls below
//! it. Wired the other way round, with the cell on `VCC` and the main rail on
//! `VBAT`, the chip runs from the coin cell all the time (it flattens in weeks,
//! and may sit below the part's minimum supply), and the main rail only ever
//! feeds the backup input, which is rated for a cell (the DS3231's `VBAT` is
//! 2.3 V to 5.5 V and the DS1307's 2.0 V to 3.5 V).
//!
//! Read from the card's roles and the supplies the nets reach, not from names:
//! the net on the primary supply pads reaches a battery part and no regulator
//! output, and the net on the backup pad reaches a regulator output (or other
//! power output) and no battery. A cell reached through a series resistor,
//! diode or ferrite counts as on the net (the usual backup path has a
//! Schottky or a protective resistor). A backup pad on ground or on nothing,
//! a board with no cell, both supplies from the same source and a supply that
//! shows neither a cell nor a regulator are all silent.

use hauksbee_extract::{
    is_ground, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::pin_roles::{power_pin, PowerKind};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelLibrary};

use super::fet_orientation::{is_battery_part, is_series_element};
use super::node_drive::Index;

/// Card roles that name a backup supply input.
const BACKUP_ROLES: &[&str] = &["vbat", "vbackup", "vback", "vbkp", "vbckp", "vbatt"];
/// Series hops walked from a supply net to find its source.
const HOPS: usize = 2;

fn same(a: &Component, b: &Component) -> bool {
    a.reference == b.reference
}

/// Two-pin parts a backup path runs through: any resistor (a protective
/// resistor is a kilohm), a diode, a ferrite, an inductor or a fuse.
fn is_backup_path_part(c: &Component) -> bool {
    c.pins.len() == 2
        && (is_series_element(c)
            || matches!(
                RefClass::of(&c.reference),
                RefClass::Resistor | RefClass::Diode
            ))
}

/// `net` and every net reached from it through up to [`HOPS`] such parts.
fn reach(idx: &Index<'_>, net: i64, part: &Component) -> Vec<i64> {
    let mut seen = vec![net];
    let mut frontier = vec![net];
    for _ in 0..HOPS {
        let mut onward = Vec::new();
        for n in frontier {
            if is_ground(idx.name(n)) {
                continue;
            }
            for (o, _) in idx.members(n) {
                if same(o, part) || !is_backup_path_part(o) {
                    continue;
                }
                for far in o.pins.iter().filter_map(|p| p.net).filter(|f| *f != 0) {
                    if !seen.contains(&far) {
                        seen.push(far);
                        onward.push(far);
                    }
                }
            }
        }
        frontier = onward;
    }
    seen
}

/// What a supply net reaches: the cell part, if any, and the regulator or
/// power-output part, if any.
#[derive(Default)]
struct Sources<'a> {
    cell: Option<&'a Component>,
    main: Option<&'a Component>,
}

fn sources<'a>(idx: &Index<'a>, nets: &[i64], part: &Component) -> Sources<'a> {
    let mut out = Sources::default();
    for &n in nets.iter().filter(|n| !is_ground(idx.name(**n))) {
        for (o, p) in idx.members(n) {
            if same(o, part) {
                continue;
            }
            let regulator_out = idx.model(o).zip(idx.role(o, p)).is_some_and(|(m, r)| {
                m.kind == ComponentKind::Vreg && matches!(r, "out" | "vout" | "sw")
            });
            if regulator_out || p.kind.eq_ignore_ascii_case("power_out") {
                out.main.get_or_insert(o);
            } else if is_battery_part(o) {
                out.cell.get_or_insert(o);
            }
        }
    }
    out
}

/// Run the check.
#[must_use]
pub fn backup_supply_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    let mut report = NetLintReport::default();
    for comp in &board.components {
        let Some(model) = idx.model(comp) else {
            continue;
        };
        let mut backup = Vec::new();
        let mut supply = Vec::new();
        for pin in &comp.pins {
            let (Some(role), Some(net)) = (idx.role(comp, pin), pin.net.filter(|n| *n != 0)) else {
                continue;
            };
            if BACKUP_ROLES.contains(&role) {
                backup.push((pin, net));
            } else if power_pin(role, model.kind)
                .is_some_and(|p| p.kind == PowerKind::Supply && p.base)
            {
                supply.push((pin, net));
            }
        }
        let (Some(&(backup_pin, backup_net)), Some(&(supply_pin, _))) =
            (backup.first(), supply.first())
        else {
            continue;
        };
        if is_ground(idx.name(backup_net)) || supply.iter().any(|(_, n)| *n == backup_net) {
            continue;
        }
        let backup_src = sources(&idx, &reach(&idx, backup_net, comp), comp);
        let mut supply_cell = None;
        let mut every_supply_is_a_cell = true;
        for (_, net) in &supply {
            let s = sources(&idx, &reach(&idx, *net, comp), comp);
            match (s.cell, s.main) {
                (Some(cell), None) => {
                    supply_cell.get_or_insert(cell);
                }
                _ => every_supply_is_a_cell = false,
            }
        }
        let (Some(cell), Some(main), true, None) = (
            supply_cell,
            backup_src.main,
            every_supply_is_a_cell,
            backup_src.cell,
        ) else {
            continue;
        };
        let name = |n: i64| idx.name(n).to_string();
        let supply_net = supply[0].1;
        report.findings.push(LintFinding {
            check: LintCheck::PinFunction,
            severity: Severity::High,
            message: format!(
                "{} ({}) has its supplies swapped: the cell {} is on its main supply pin {} ('{}') and the board's own supply from {} is on its backup pin {} ('{}'). The part then runs from the cell all the time, and the main rail only feeds the backup input, so the cell runs flat and the clock stops (and the part may sit below its minimum supply on a cell). {}put the main rail on {} and the cell on {}",
                comp.reference,
                comp.value,
                cell.reference,
                supply_pin.number,
                name(supply_net),
                main.reference,
                backup_pin.number,
                name(backup_net),
                hauksbee_extract::REMEDY_MARKER,
                supply_pin.number,
                backup_pin.number,
            ),
            refs: vec![comp.reference.clone()],
            nets: vec![name(supply_net), name(backup_net)],
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const CELL: i64 = 3;
    const SDA: i64 = 4;
    const SCL: i64 = 5;

    /// A DS3231 (SO-16W) with its main supply (pad 2) and backup (pad 14) on
    /// the given nets.
    fn rtc(vcc: i64, vbat: i64) -> Component {
        let mut pins: Vec<_> = (1..=16).map(|n| pin_fn(&n.to_string(), 0, "")).collect();
        for (pad, net) in [(2, vcc), (13, GND), (14, vbat), (15, SDA), (16, SCL)] {
            pins[pad - 1].net = Some(net);
        }
        Component {
            reference: "U1".into(),
            value: "DS3231SN".into(),
            lib_id: "Timer_RTC:DS3231SN".into(),
            footprint: "Package_SO:SOIC-16W_7.5x10.3mm_P1.27mm".into(),
            pins,
            ..Default::default()
        }
    }

    fn coin_cell() -> Component {
        Component {
            reference: "BT1".into(),
            value: "CR2032".into(),
            lib_id: "Device:Battery_Cell".into(),
            footprint: "Battery:BatteryHolder_Keystone_3034_1x20mm".into(),
            pins: vec![pin_fn("1", CELL, "+"), pin_fn("2", GND, "-")],
            ..Default::default()
        }
    }

    /// A regulator whose output pin is on the 3V3 rail.
    fn regulator() -> Component {
        let mut out = pin_fn("2", V33, "OUT");
        out.kind = "power_out".into();
        Component {
            reference: "U2".into(),
            value: "AP2112K-3.3".into(),
            lib_id: "Regulator_Linear:AP2112K-3.3".into(),
            footprint: "Package_TO_SOT_SMD:SOT-23-5".into(),
            pins: vec![out],
            ..Default::default()
        }
    }

    fn lint(parts: Vec<Component>) -> Vec<LintFinding> {
        let b = board(
            "t",
            &[
                (GND, "GND"),
                (V33, "+3V3"),
                (CELL, "Net-(BT1-+)"),
                (SDA, "SDA"),
                (SCL, "SCL"),
            ],
            parts,
        );
        backup_supply_lint(&b, &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_cell_on_vcc_and_the_main_rail_on_vbat_is_a_swap() {
        let found = lint(vec![rtc(CELL, V33), coin_cell(), regulator()]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["U1"]);
        assert!(
            found[0].message.contains("cell BT1") && found[0].message.contains("main supply pin 2"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_swap_through_a_series_resistor_on_the_cell_is_still_found() {
        const CELL_R: i64 = 6;
        let mut parts = vec![rtc(CELL_R, V33), coin_cell(), regulator()];
        parts.push(Component {
            reference: "R1".into(),
            value: "1k".into(),
            lib_id: "Device:R".into(),
            footprint: "R_0603".into(),
            pins: vec![pin_fn("1", CELL, ""), pin_fn("2", CELL_R, "")],
            ..Default::default()
        });
        let b = board(
            "t",
            &[
                (GND, "GND"),
                (V33, "+3V3"),
                (CELL, "Net-(BT1-+)"),
                (CELL_R, "Net-(R1-Pad2)"),
                (SDA, "SDA"),
                (SCL, "SCL"),
            ],
            parts,
        );
        let found = backup_supply_lint(&b, &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn the_right_way_round_a_grounded_backup_pad_and_a_cell_less_board_are_silent() {
        // Main rail on VCC, cell on VBAT.
        let ok = lint(vec![rtc(V33, CELL), coin_cell(), regulator()]);
        assert!(ok.is_empty(), "{ok:#?}");
        // The backup pad grounded because it is unused.
        let unused = lint(vec![rtc(V33, GND), coin_cell(), regulator()]);
        assert!(unused.is_empty(), "{unused:#?}");
        // No cell anywhere: nothing shows the supplies are swapped.
        let no_cell = lint(vec![rtc(V33, V33), regulator()]);
        assert!(no_cell.is_empty(), "{no_cell:#?}");
        // Both supplies from the cell: nothing on VBAT is the main rail.
        let both_cell = lint(vec![rtc(CELL, CELL), coin_cell(), regulator()]);
        assert!(both_cell.is_empty(), "{both_cell:#?}");
        // A part not fitted is not judged.
        let mut dnp = rtc(CELL, V33);
        dnp.dnp = true;
        let skipped = lint(vec![dnp, coin_cell(), regulator()]);
        assert!(skipped.is_empty(), "{skipped:#?}");
    }
}
