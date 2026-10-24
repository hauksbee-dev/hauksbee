//! Minimum capacitance on a supply input or output pin, from the card's
//! `[[models.supply_cap]]` rows.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A motor driver wants a bulk capacitor on its supply to absorb the energy the
//! motor pumps back; a load switch wants an input capacitor so the rail does not
//! dip or ring when the switch turns on, and an output capacitor to soften the
//! inrush and hold the load. The datasheet states the least capacitance. The
//! card records it per pin role (`min_f`, optionally `min_single_f` when one
//! capacitor, not a pile of small ones, must reach a size) and this check sums
//! the fitted capacitors from that pin's net to the part's ground and compares.
//!
//! A linear regulator's output capacitor is the existing `cout_*` rows judged
//! by `regulator_cout`; a `supply_cap` row for it is refused by the card
//! validator, so there is one source of truth. This check owns every other
//! supply pin: regulator and converter inputs, load-switch inputs and outputs,
//! driver supplies.
//!
//! What is counted: fitted capacitors with one pad on the pin's net and the
//! other on the part's ground (its own `gnd` net, or any ground net when the
//! part has none), at their nominal value. A pin's net also spans whatever a
//! fitted ferrite bead, inductor, fuse, net tie or link of at most 0.1 ohm
//! joins to it (the same wire-like parts `power_states.rs` treats as one rail),
//! so a bulk capacitor on the far side of the output ferrite counts. A
//! capacitor behind a real resistor is not on the pin and does not count. The schematic cannot show how
//! close to the pin a capacitor sits, so the remedy asks for it next to the pin.
//! A capacitor whose value does not read as a capacitance makes the check
//! abstain, naming it: an unknown size is never counted as 0 F.
//!
//! A row the card conditions on a load (`applies_when = "usb_downstream"`: the
//! datasheet's "for the USB application, each downstream facing port requires
//! 120 uF") is judged only when the pin's rail reaches that load, here the VBUS
//! pad of a USB receptacle; on any other load the row is silent and the card's
//! general-application row, if it has one, is the figure.
//!
//! ## What it never claims
//!
//! A `recommended` row is a Low note, never a fault. Ceramic capacitance falls
//! under DC bias, which the board does not record, so a finding that is short
//! only by that margin says the figure is nominal.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use hauksbee_bind::binder::is_ground;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    pin_is_no_connect, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin,
    Severity, ABSTENTION_MARKER, REMEDY_MARKER,
};
use hauksbee_ir::load_class::LoadClass;
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{CapGrade, ModelEntry, ModelLibrary, SupplyCap};

use super::envelope::role_net_id;
use super::regulator_cout::{capacitance, caps_across_output, farads, short_of};

/// What the capacitors on one pin's net come to.
struct Found<'a> {
    /// Each readable capacitor and its value.
    caps: Vec<(&'a Component, f64)>,
    /// Capacitors across the net with no readable capacitance.
    unvalued: Vec<&'a str>,
}

impl Found<'_> {
    fn total(&self) -> f64 {
        self.caps.iter().map(|(_, f)| f).sum()
    }

    fn largest(&self) -> f64 {
        self.caps.iter().map(|(_, f)| *f).fold(0.0, f64::max)
    }

    /// `C1 10 µF, C2 100 nF`, or the words for none.
    fn list(&self) -> String {
        if self.caps.is_empty() {
            return "no capacitor from the pin's net to ground".to_string();
        }
        let mut s = String::new();
        for (i, (c, f)) in self.caps.iter().enumerate() {
            let _ = write!(
                s,
                "{}{} {}",
                if i == 0 { "" } else { ", " },
                c.reference,
                farads(*f)
            );
        }
        format!(
            "{} from the pin's net to ground ({s})",
            farads(self.total())
        )
    }
}

/// Whether `c` is a fitted part that joins its two nets into one rail: an
/// inductor, ferrite bead, fuse, net tie or a link of at most 0.1 ohm.
fn is_wire_like(c: &Component) -> bool {
    use super::power_states::{is_fuse, is_net_tie, ohms};
    if !AssemblyState::of(c).is_present() || c.pins.len() > 4 {
        return false;
    }
    matches!(
        RefClass::of(&c.reference),
        RefClass::Inductor | RefClass::FerriteBead
    ) || is_fuse(c)
        || is_net_tie(c)
        || (hauksbee_extract::is_plain_resistor(c) && ohms(c).is_some_and(|o| o <= 0.1))
}

/// The nets one rail spans from `net`, through wire-like parts.
fn rail_nets(board: &ExtractedBoard, net: i64) -> Vec<i64> {
    let mut nets = vec![net];
    let mut cursor = 0;
    while cursor < nets.len() {
        let here = nets[cursor];
        cursor += 1;
        for (c, _) in board.net_members(here) {
            if !is_wire_like(c) {
                continue;
            }
            let ends: BTreeSet<i64> = c
                .pins
                .iter()
                .filter_map(|p| p.net)
                .filter(|n| *n != 0)
                .collect();
            if ends.len() != 2 {
                continue;
            }
            for n in ends {
                if !nets.contains(&n) && !board.net(n).is_some_and(|x| is_ground(&x.name)) {
                    nets.push(n);
                }
            }
        }
    }
    nets
}

/// The fitted capacitors from the pin's rail (its net and whatever a wire-like
/// part joins to it) to the part's return, each once.
fn rail_caps(board: &ExtractedBoard, net: i64, gnd: Option<i64>) -> Vec<&Component> {
    let mut caps: Vec<&Component> = Vec::new();
    for n in rail_nets(board, net) {
        for c in caps_across_output(board, n, gnd) {
            if !caps.iter().any(|seen| seen.reference == c.reference) {
                caps.push(c);
            }
        }
    }
    caps
}

/// Whether the rail from `net` reaches a load of `class`.
fn rail_reaches(board: &ExtractedBoard, net: i64, class: LoadClass) -> bool {
    match class {
        LoadClass::UsbDownstream => rail_nets(board, net).into_iter().any(|n| {
            board
                .net_members(n)
                .into_iter()
                .any(|(c, p)| is_usb_vbus_pad(c, p))
        }),
    }
}

/// Whether pad `p` of `c` is the VBUS pin of a fitted USB receptacle: a
/// connector whose symbol, value or footprint says USB, on a pad named VBUS (or
/// the supply pad a layout-only extraction numbers: no name, pad 1 or a USB-C
/// VBUS pad).
fn is_usb_vbus_pad(c: &Component, p: &Pin) -> bool {
    if !AssemblyState::of(c).is_present() || !hauksbee_extract::is_connector_like(c) {
        return false;
    }
    let text = format!("{} {} {}", c.lib_id, c.value, c.footprint).to_ascii_lowercase();
    if !text.contains("usb") {
        return false;
    }
    let name = p.function.trim().to_ascii_lowercase();
    if name.is_empty() {
        return matches!(
            p.number.as_str(),
            "1" | "A4" | "A9" | "B4" | "B9" | "A4B9" | "A9B4"
        );
    }
    name.contains("vbus") || matches!(name.as_str(), "vcc" | "5v" | "+5v" | "v+" | "power")
}

/// One finding per fitted part pin whose card states a capacitance the net
/// does not carry.
#[must_use]
pub fn supply_cap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for (comp, model) in super::resolved_components(board, lib) {
        if model.supply_cap.is_empty() {
            continue;
        }
        // The return a decoupling capacitor goes to: whichever ground role
        // the card names, power ground first.
        let gnd = ["gnd", "pgnd", "vss", "agnd"]
            .iter()
            .find_map(|role| role_net_id(comp, &model, role));
        for row in &model.supply_cap {
            let mut seen = BTreeSet::new();
            for (pad, _) in model
                .pins
                .iter()
                .filter(|(_, role)| role.eq_ignore_ascii_case(&row.role))
            {
                let Some(pin) = comp.pins.iter().find(|p| p.number == *pad) else {
                    continue;
                };
                // A pin marked no-connect has nothing to decouple.
                if pin_is_no_connect(pin) {
                    continue;
                }
                let Some(net_id) = pin.net.filter(|id| *id != 0) else {
                    continue;
                };
                // Pads of one role on one net (two VIN pads tied) are one pin.
                if !seen.insert(net_id) || gnd == Some(net_id) {
                    continue;
                }
                let Some(net) = board.net(net_id) else {
                    continue;
                };
                if is_ground(&net.name) || super::is_unconnected_net(&net.name) {
                    continue;
                }
                // A pin alone on an auto-named net (`Net-(U1-IN2)`: a stub wire
                // or label that goes nowhere) is unused, not unbypassed. A rail
                // the board names itself stays judged.
                if net.name.trim_start_matches('/').starts_with("Net-(")
                    && board.net_members(net_id).len() <= 1
                {
                    continue;
                }
                // A row stated for one kind of load applies only where the rail
                // reaches that load.
                if row
                    .applies_when
                    .is_some_and(|class| !rail_reaches(board, net_id, class))
                {
                    continue;
                }
                let across = rail_caps(board, net_id, gnd);
                let found = Found {
                    caps: across
                        .iter()
                        .filter_map(|c| Some((*c, capacitance(c)?)))
                        .collect(),
                    unvalued: across
                        .iter()
                        .filter(|c| capacitance(c).is_none())
                        .map(|c| c.reference.as_str())
                        .collect(),
                };
                if let Some(f) = judge((comp, &model), (pad, &net.name), row, &found) {
                    report.findings.push(f);
                }
            }
        }
    }
    report
}

/// The finding for one pin, or `None` when it carries what the card states.
fn judge(
    (comp, model): (&Component, &ModelEntry),
    (pad, net): (&str, &str),
    row: &SupplyCap,
    found: &Found<'_>,
) -> Option<LintFinding> {
    let total_short = short_of(found.total(), row.min_f);
    let single_short = row
        .min_single_f
        .is_some_and(|s| short_of(found.largest(), s));
    if !total_short && !single_short {
        return None;
    }
    let refs: Vec<String> = std::iter::once(comp.reference.clone())
        .chain(found.caps.iter().map(|(c, _)| c.reference.clone()))
        .chain(found.unvalued.iter().map(|r| (*r).to_string()))
        .collect();
    let head = format!(
        "{} ({}) {} pin {pad} ({}) on net '{net}'",
        comp.reference,
        comp.value.trim(),
        row.side.word(),
        row.role,
    );
    if !found.unvalued.is_empty() {
        // Capacitors are there but their size is not: abstain, never fail.
        let caps = found.unvalued.join(", ");
        let has = if found.unvalued.len() == 1 {
            "has"
        } else {
            "have"
        };
        return Some(LintFinding {
            check: LintCheck::SupplyCapacitance,
            severity: Severity::Low,
            message: format!(
                "{head}: the supply-capacitance check{ABSTENTION_MARKER} against the {} card '{}' \
                 states ({}), because {caps} on the net {has} no readable capacitance value. \
                 {REMEDY_MARKER}Give {caps} a capacitance value (\"10u\", \"100n\", \"0.1uF\") so \
                 the requirement can be checked.",
                farads(row.min_f),
                model.id,
                row.basis.trim(),
            ),
            refs,
            nets: vec![net.to_string()],
        });
    }
    let required = row.grade == CapGrade::Required;
    let wants = if required { "requires" } else { "recommends" };
    let mut reason = format!(
        "{head} has {}, but the datasheet {wants} at least {}",
        found.list(),
        farads(row.min_f),
    );
    if let Some(single) = row.min_single_f {
        let _ = write!(
            reason,
            ", with one capacitor of at least {}",
            farads(single)
        );
    }
    let _ = write!(reason, " (card '{}': {})", model.id, row.basis.trim());
    let why = if row.note.trim().is_empty() {
        String::new()
    } else {
        format!(" {}", row.note.trim())
    };
    let nominal = " Nominal values are counted; a ceramic loses capacitance under DC bias, so \
                   leave margin.";
    // The least to add: the total shortfall, or the single capacitor's size.
    let add_total = if total_short {
        row.min_f - found.total()
    } else {
        0.0
    };
    let fix = match (single_short, row.min_single_f) {
        (true, Some(single)) if single >= add_total => format!(
            "Add one capacitor of at least {} from '{net}' to ground, next to pin {pad}",
            farads(single)
        ),
        _ => format!(
            "Add at least {} from '{net}' to ground, next to pin {pad}",
            farads(add_total)
        ),
    };
    let (severity, tail) = if !required {
        (
            Severity::Low,
            " This is a recommendation, so a note, not a fault.",
        )
    } else if found.caps.is_empty() {
        (Severity::High, "")
    } else {
        (Severity::Medium, "")
    };
    Some(LintFinding {
        check: LintCheck::SupplyCapacitance,
        severity,
        message: format!("{reason}.{why}{tail}{nominal} {REMEDY_MARKER}{fix}."),
        refs,
        nets: vec![net.to_string()],
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// A load switch (pads 1 vin, 2 gnd, 3 vout, 4 en) whose datasheet-style
    /// rows ask for 1 uF in (one capacitor) and 0.1 uF out, and a recommended
    /// 10 uF bulk on a second supply pin (pad 5, role vbulk).
    const CARD: &str = r#"
[[models]]
id = "supply_cap_test_switch"
kind = "digital"
description = "supply cap fixture"
[models.match]
value_re = "^CAPTEST$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vin"
"2" = "gnd"
"3" = "vout"
"4" = "en"
"5" = "vbulk"
[[models.supply_cap]]
role = "vin"
side = "input"
grade = "required"
min_f = 1.0e-6
min_single_f = 1.0e-6
basis = "fixture datasheet section 8.1, p.12: 1 uF or larger input capacitor required"
note = "It stops the rail dipping when the switch turns on."
[[models.supply_cap]]
role = "vout"
side = "output"
grade = "required"
min_f = 1.0e-7
basis = "fixture datasheet section 8.2, p.12: 0.1 uF minimum output capacitor"
[[models.supply_cap]]
role = "vbulk"
side = "input"
grade = "recommended"
min_f = 1.0e-5
basis = "fixture datasheet section 8.3, p.13: 10 uF bulk recommended"
"#;

    fn library() -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("supply_cap", "card");
        std::fs::write(dir.join("models.toml"), CARD).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "test card must load: {errors:?}");
        lib
    }

    /// Nets: 1 GND, 2 +5V (vin), 3 +3V3 (vout), 4 EN, 5 +12V (vbulk).
    /// `caps` are (value, net) pairs to ground.
    fn lint(caps: &[(&str, i64)]) -> Vec<LintFinding> {
        let mut parts = vec![part(
            "U1",
            "CAPTEST",
            &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5)],
        )];
        for (i, (v, net)) in caps.iter().enumerate() {
            parts.push(part(&format!("C{}", i + 1), v, &[("1", *net), ("2", 1)]));
        }
        let b = board(
            &[(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "EN"), (5, "+12V")],
            parts,
        );
        supply_cap_lint(&b, &library()).findings
    }

    fn on<'a>(f: &'a [LintFinding], net: &str) -> Vec<&'a LintFinding> {
        f.iter().filter(|f| f.nets == [net]).collect()
    }

    #[test]
    fn a_load_switch_with_no_capacitor_at_all_is_flagged_on_both_sides() {
        let f = lint(&[("10u", 5)]);
        let vin = on(&f, "+5V");
        let vout = on(&f, "+3V3");
        assert_eq!((vin.len(), vout.len()), (1, 1), "{f:?}");
        assert_eq!(vin[0].severity, Severity::High);
        assert_eq!(vout[0].severity, Severity::High);
        for s in [
            "input pin 1",
            "no capacitor",
            "requires at least 1 µF",
            "fixture datasheet section 8.1, p.12",
            "next to pin 1",
            "supply_cap_test_switch",
        ] {
            assert!(vin[0].message.contains(s), "{s:?} in {}", vin[0].message);
        }
        assert!(
            vout[0].message.contains("output pin 3"),
            "{}",
            vout[0].message
        );
        assert!(vin[0].own_remedy().is_some());
        assert_eq!(vin[0].check, LintCheck::SupplyCapacitance);
    }

    #[test]
    fn the_stated_capacitance_on_each_side_is_silent() {
        let f = lint(&[("1u", 2), ("100n", 3), ("10u", 5)]);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_shortfall_says_how_much_is_needed() {
        // The output wants 0.1 uF, so 47 nF is short by 53 nF.
        let f = lint(&[("1u", 2), ("47n", 3), ("10u", 5)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].message.contains("Add at least 53 nF"),
            "{}",
            f[0].message
        );
    }

    #[test]
    fn a_pile_of_small_capacitors_does_not_meet_a_single_capacitor_requirement() {
        // Ten 100 nF reach the 1 uF total but no one capacitor does.
        let small: Vec<(&str, i64)> = std::iter::repeat_n(("100n", 2), 10).collect();
        let mut caps = small;
        caps.push(("100n", 3));
        caps.push(("10u", 5));
        let f = lint(&caps);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(
            f[0].message.contains("one capacitor of at least 1 µF"),
            "{}",
            f[0].message
        );
    }

    #[test]
    fn a_recommendation_is_a_note_not_a_fault() {
        let f = lint(&[("1u", 2), ("100n", 3), ("1u", 5)]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Low);
        assert!(f[0].message.contains("recommends"), "{}", f[0].message);
        assert!(f[0].message.contains("not a fault"), "{}", f[0].message);
    }

    #[test]
    fn an_unreadable_capacitor_value_abstains() {
        let f = lint(&[("TBD", 2), ("100n", 3), ("10u", 5)]);
        let vin = on(&f, "+5V");
        assert_eq!(vin.len(), 1, "{f:?}");
        assert_eq!(vin[0].severity, Severity::Low);
        assert!(vin[0].is_abstention(), "{}", vin[0].message);
        assert!(vin[0].message.contains("C1"), "{}", vin[0].message);
    }

    #[test]
    fn a_dnp_capacitor_is_not_counted() {
        let mut parts = vec![part(
            "U1",
            "CAPTEST",
            &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5)],
        )];
        let mut c = part("C1", "1u", &[("1", 2), ("2", 1)]);
        c.dnp = true;
        parts.push(c);
        parts.push(part("C2", "100n", &[("1", 3), ("2", 1)]));
        parts.push(part("C3", "10u", &[("1", 5), ("2", 1)]));
        let b = board(
            &[(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "EN"), (5, "+12V")],
            parts,
        );
        let f = supply_cap_lint(&b, &library()).findings;
        assert_eq!(on(&f, "+5V").len(), 1, "{f:?}");
    }

    /// The load switch with its input pad (1) on `net` and of pin type `kind`;
    /// an output capacitor and the bulk capacitor are fitted, so only the input
    /// could fire.
    fn input_on(net: Option<i64>, kind: &str) -> Vec<LintFinding> {
        let mut u1 = part(
            "U1",
            "CAPTEST",
            &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5)],
        );
        u1.pins[0].net = net;
        u1.pins[0].kind = kind.into();
        let parts = vec![
            u1,
            part("J1", "CONN", &[("1", 2), ("2", 8)]),
            part("C2", "100n", &[("1", 3), ("2", 1)]),
            part("C3", "10u", &[("1", 5), ("2", 1)]),
        ];
        let b = board(
            &[
                (1, "GND"),
                (2, "+5V"),
                (3, "+3V3"),
                (4, "EN"),
                (5, "+12V"),
                (6, "unconnected-(U1-VIN)"),
                (7, "Net-(U1-VIN)"),
                (8, "SENSE"),
            ],
            parts,
        );
        supply_cap_lint(&b, &library()).findings
    }

    #[test]
    fn an_unused_supply_pin_is_not_asked_for_a_capacitor() {
        // No net at all (an unrouted pin).
        let f = input_on(None, "power_in");
        assert!(f.is_empty(), "{f:?}");
        // A KiCad no-connect net.
        let f = input_on(Some(6), "power_in");
        assert!(f.is_empty(), "{f:?}");
        // An auto-named stub net holding only the pin.
        let f = input_on(Some(7), "power_in");
        assert!(f.is_empty(), "{f:?}");
        // A pin the symbol marks no-connect, even on a rail another part feeds.
        let f = input_on(Some(2), "no_connect");
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_used_supply_pin_without_a_capacitor_still_fires() {
        let f = input_on(Some(2), "power_in");
        assert_eq!(on(&f, "+5V").len(), 1, "{f:?}");
    }

    #[test]
    fn a_capacitor_behind_a_resistor_is_not_on_the_pin() {
        let mut parts = vec![part(
            "U1",
            "CAPTEST",
            &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5)],
        )];
        parts.push(part("R1", "1k", &[("1", 2), ("2", 6)]));
        parts.push(part("C1", "1u", &[("1", 6), ("2", 1)]));
        parts.push(part("C2", "100n", &[("1", 3), ("2", 1)]));
        parts.push(part("C3", "10u", &[("1", 5), ("2", 1)]));
        let b = board(
            &[
                (1, "GND"),
                (2, "+5V"),
                (3, "+3V3"),
                (4, "EN"),
                (5, "+12V"),
                (6, "FILT"),
            ],
            parts,
        );
        let f = supply_cap_lint(&b, &library()).findings;
        assert_eq!(on(&f, "+5V").len(), 1, "{f:?}");
    }

    /// The shipped AP22815 card (pads 1 out, 2 gnd, 3 flg, 4 en, 5 in) states
    /// its input and output capacitors from the Diodes datasheet: a switch on
    /// a board with neither is flagged on both pins, and the datasheet's own
    /// 1 uF in and 0.1 uF out leave it silent.
    #[test]
    fn the_shipped_load_switch_card_demands_its_datasheet_capacitors() {
        let lib = ModelLibrary::builtin();
        let run = |caps: &[(&str, i64)]| {
            let mut parts = vec![part(
                "U1",
                "AP22815",
                &[("1", 3), ("2", 1), ("4", 4), ("5", 2)],
            )];
            for (i, (v, net)) in caps.iter().enumerate() {
                parts.push(part(&format!("C{}", i + 1), v, &[("1", *net), ("2", 1)]));
            }
            let b = board(&[(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "EN")], parts);
            supply_cap_lint(&b, &lib).findings
        };
        let bare = run(&[]);
        assert_eq!(on(&bare, "+5V").len(), 1, "{bare:?}");
        assert_eq!(on(&bare, "+3V3").len(), 1, "{bare:?}");
        assert!(on(&bare, "+5V")[0].message.contains("1 µF"));
        assert!(on(&bare, "+3V3")[0].message.contains("100 nF"));
        let fitted = run(&[("1u", 2), ("100n", 3)]);
        assert!(fitted.is_empty(), "{fitted:?}");
    }

    /// The shipped load-switch and motor-driver cards state their datasheet
    /// capacitors: bare, each part is flagged on the rail the stated pin sits
    /// on, and with the capacitors its datasheet shows the board is silent.
    /// Each row is the part's value, its (pad, net) wiring (nets: 1 GND, 2 +5V
    /// supply, 3 +3V3 output, 4 VLOGIC), the nets that must be flagged bare and
    /// the capacitors (value, net) that satisfy every row.
    #[test]
    fn the_shipped_switch_and_driver_cards_demand_their_datasheet_capacitors() {
        type Case = (
            &'static str,
            &'static [(&'static str, i64)],
            &'static [&'static str],
            &'static [(&'static str, i64)],
        );
        const CASES: [Case; 7] = [
            // TPS22918 SLVSD76C PDF p.4: CIN 1 uF minimum.
            (
                "TPS22918DBVR",
                &[("1", 2), ("2", 1), ("3", 4), ("6", 3)],
                &["+5V"],
                &[("1u", 2)],
            ),
            // TPS22919 SLVSEN5B PDF p.17: 1 uF input sufficient in most situations.
            (
                "TPS22919DCKR",
                &[("1", 2), ("2", 1), ("3", 4), ("6", 3)],
                &["+5V"],
                &[("1u", 2)],
            ),
            // AP2281 DS31359: 1 uF in, 0.1 uF out.
            (
                "AP2281-1WG-7",
                &[("1", 3), ("2", 1), ("3", 4), ("4", 2), ("5", 1), ("6", 2)],
                &["+5V", "+3V3"],
                &[("1u", 2), ("100n", 3)],
            ),
            // DRV8833 SLVSAR1E PDF p.3: VM 10 uF, VINT 2.2 uF.
            (
                "DRV8833PWP",
                &[("12", 2), ("13", 1), ("14", 3)],
                &["+5V", "+3V3"],
                &[("10u", 2), ("2.2u", 3)],
            ),
            // DRV8825 SLVSA73F PDF p.3 and p.21: VMA/VMB 0.1 uF, V3P3OUT 0.47 uF.
            (
                "DRV8825PWP",
                &[("4", 2), ("11", 2), ("14", 1), ("15", 3), ("28", 1)],
                &["+5V", "+3V3"],
                &[("100u", 2), ("470n", 3)],
            ),
            // A4988 4988-DS PDF p.10: VREG 0.22 uF; p.1: VDD 0.22 uF, VBB 100 uF.
            (
                "A4988SETTR",
                &[("8", 3), ("15", 4), ("22", 2), ("28", 2), ("18", 1)],
                &["+3V3"],
                &[("100u", 2), ("220n", 3), ("220n", 4)],
            ),
            // TB6612FNG PDF p.7: C1/C2 10 uF on VM, C3 10 uF on VCC.
            (
                "TB6612FNG",
                &[("20", 4), ("24", 2), ("13", 2), ("14", 2), ("18", 1)],
                &["+5V", "VLOGIC"],
                &[("10u", 2), ("10u", 4)],
            ),
        ];
        let lib = ModelLibrary::builtin();
        let nets = [(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "VLOGIC")];
        for (value, pads, flagged, caps) in CASES {
            let run = |caps: &[(&str, i64)]| {
                let mut parts = vec![part("U1", value, pads)];
                for (i, (v, net)) in caps.iter().enumerate() {
                    parts.push(part(&format!("C{}", i + 1), v, &[("1", *net), ("2", 1)]));
                }
                supply_cap_lint(&board(&nets, parts), &lib).findings
            };
            let bare = run(&[]);
            for net in flagged {
                assert!(
                    bare.iter().any(|f| f.nets == [*net]),
                    "{value}: no finding on {net}: {bare:?}"
                );
            }
            let fitted = run(caps);
            assert!(fitted.is_empty(), "{value}: {fitted:?}");
        }
    }

    /// A capacitor on the far side of a ferrite bead or a 0 ohm link is on the
    /// pin's rail: the bead is a wire at DC. Behind a real resistor it is not.
    #[test]
    fn a_capacitor_beyond_a_ferrite_or_a_zero_ohm_link_counts() {
        for (link_ref, link_value, counts) in [
            ("FB1", "100@100MHz", true),
            ("R1", "0", true),
            ("F1", "500mA", true),
            ("R1", "1k", false),
        ] {
            let mut parts = vec![part(
                "U1",
                "CAPTEST",
                &[("1", 2), ("2", 1), ("3", 3), ("4", 4), ("5", 5)],
            )];
            parts.push(part(link_ref, link_value, &[("1", 3), ("2", 6)]));
            parts.push(part("C1", "1u", &[("1", 2), ("2", 1)]));
            parts.push(part("C2", "10u", &[("1", 6), ("2", 1)]));
            parts.push(part("C3", "10u", &[("1", 5), ("2", 1)]));
            let b = board(
                &[
                    (1, "GND"),
                    (2, "+5V"),
                    (3, "+3V3"),
                    (4, "EN"),
                    (5, "+12V"),
                    (6, "LOAD"),
                ],
                parts,
            );
            let f = supply_cap_lint(&b, &library()).findings;
            assert_eq!(
                on(&f, "+3V3").is_empty(),
                counts,
                "{link_ref} {link_value}: {f:?}"
            );
        }
    }

    /// A load switch whose datasheet states 120 uF only "for the USB
    /// application" and a 10 nF ceramic for the general application.
    const USB_SWITCH: &str = r#"
[[models]]
id = "supply_cap_usb_switch"
kind = "digital"
description = "usb load switch fixture"
[models.match]
value_re = "^USBSW$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vin"
"2" = "gnd"
"3" = "vout"
"4" = "en"
[[models.supply_cap]]
role = "vout"
side = "output"
grade = "required"
min_f = 1.2e-4
applies_when = "usb_downstream"
basis = "fixture datasheet, Output Filter Capacitor: each downstream facing port requires at least 120 uF"
[[models.supply_cap]]
role = "vout"
side = "output"
grade = "recommended"
min_f = 1.0e-8
basis = "fixture datasheet, Output Filter Capacitor: general application, 0.01 uF to 0.1 uF"
"#;

    /// The switch feeding `HDMI_5V` (100 nF out), which feeds `conn` on pad 1.
    fn usb_switch_feeding(conn: Component, out_cap: &str) -> Vec<LintFinding> {
        let dir = hauksbee_testkit::fresh_scratch_dir("supply_cap", "usb-switch");
        std::fs::write(dir.join("models.toml"), USB_SWITCH).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "test card must load: {errors:?}");
        let parts = vec![
            part("U1", "USBSW", &[("1", 2), ("2", 1), ("3", 3), ("4", 4)]),
            part("C1", "1u", &[("1", 2), ("2", 1)]),
            part("C2", out_cap, &[("1", 3), ("2", 1)]),
            conn,
        ];
        let b = board(&[(1, "GND"), (2, "V5"), (3, "HDMI_5V"), (4, "EN")], parts);
        supply_cap_lint(&b, &lib).findings
    }

    fn receptacle(reference: &str, lib_id: &str, vbus_name: &str) -> Component {
        let mut c = part(reference, lib_id, &[("1", 3), ("2", 1)]);
        c.lib_id = lib_id.into();
        c.footprint = lib_id.into();
        c.pins[0].function = vbus_name.into();
        c
    }

    #[test]
    fn a_usb_port_requirement_applies_only_when_the_switch_feeds_a_usb_port() {
        // HDMI +5V pin: the 120 uF row is for a USB port, so nothing fires, and
        // the general-application row (10 nF) is met by 100 nF.
        let hdmi = receptacle("J1", "Connector:HDMI_A", "+5V");
        let f = usb_switch_feeding(hdmi, "100n");
        assert!(f.is_empty(), "{f:?}");
        // Control: the same net on a USB-A VBUS pad needs the 120 uF.
        let usb = receptacle("J2", "Connector:USB_A", "VBUS");
        let f = usb_switch_feeding(usb, "100n");
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].message.contains("requires at least 120"),
            "{}",
            f[0].message
        );
        // And a USB port that has the 120 uF is clean.
        let usb = receptacle("J2", "Connector:USB_A", "VBUS");
        assert!(usb_switch_feeding(usb, "150u").is_empty());
        // The USB part's ground pin is not the VBUS pad: nothing fires.
        let mut other = receptacle("J3", "Connector:USB_A", "D+");
        other.pins[0].net = Some(3);
        let f = usb_switch_feeding(other, "100n");
        assert!(f.is_empty(), "{f:?}");
    }
}
