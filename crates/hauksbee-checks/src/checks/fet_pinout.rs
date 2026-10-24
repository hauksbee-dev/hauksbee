//! A FET symbol whose gate, source and drain numbers disagree with the package.
//!
//! A schematic symbol numbers a FET's three pins (`Q_PMOS_GDS` puts the drain
//! on pin 2), the footprint numbers its pads, and the part's datasheet says
//! which electrode is on which pad of the package. The binder reads a FET's
//! electrodes from the symbol's own pin names, so a symbol drawn for another
//! pinout than the ordered part's reads cleanly while every pad on the real
//! part is the wrong electrode: the gate net lands on the source, a drain on
//! the gate.
//!
//! The package decides, in this order:
//!
//! 1. A card that numbers the pads of the part (a datasheet pad table): a
//!    disagreement is High.
//! 2. The common default for the package from the pin-rule table
//!    (`pin_rules.toml`): SOT-23, SOT-323 and SC-70 are gate, source, drain on
//!    pads 1, 2 and 3 for most vendors, and the TO-220 / DPAK / D2PAK family is
//!    gate, drain, source. A disagreement is Medium, and the message says the
//!    basis is assumed: some vendors number a package otherwise, so the ordered
//!    part's datasheet is the last word.
//!
//! Judged only on a definite contradiction: a single FET whose three pads are
//! all named gate, source or drain by the symbol and all placed by the basis.
//! A pad named otherwise (`~`, `S1`), a package no rule or card covers, a lettered
//! pad (an Eagle `G`/`D`/`S`) and a part not fitted are silent.

use std::collections::BTreeMap;

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

/// The electrode a symbol pin name states, `G`/`S`/`D` or the full word.
fn symbol_role(function: &str) -> Option<&'static str> {
    match function.trim().to_ascii_uppercase().as_str() {
        "G" | "GATE" => Some("gate"),
        "S" | "SOURCE" => Some("source"),
        "D" | "DRAIN" => Some("drain"),
        _ => None,
    }
}

/// The short letter for a role, for the `G-S-D` order strings.
fn letter(role: &str) -> char {
    match role {
        "gate" => 'G',
        "source" => 'S',
        _ => 'D',
    }
}

/// The card's own electrode for a numeric pad, when it states one.
fn card_role(model: &ModelEntry, pad: &str) -> Option<&'static str> {
    match model.pins.get(pad)?.to_ascii_lowercase().as_str() {
        "gate" | "g" => Some("gate"),
        "source" | "s" => Some("source"),
        "drain" | "d" => Some("drain"),
        _ => None,
    }
}

/// One single FET's pads: `(pad, symbol electrode, expected electrode, from card)`.
struct Pads {
    rows: Vec<(String, &'static str, &'static str, bool)>,
}

fn pads_of(comp: &Component, model: &ModelEntry, lib: &ModelLibrary) -> Option<Pads> {
    if comp.distinct_pad_count() != 3 {
        return None;
    }
    let mut seen = BTreeMap::new();
    for pin in &comp.pins {
        let number = pin.number.trim();
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let role = symbol_role(&pin.function)?;
        if seen
            .insert(number.to_string(), role)
            .is_some_and(|prior| prior != role)
        {
            return None;
        }
    }
    let distinct: std::collections::BTreeSet<_> = seen.values().copied().collect();
    if seen.len() != 3 || distinct.len() != 3 {
        return None;
    }
    let pin_rules = lib.pin_rules();
    let mut rows = Vec::new();
    for (pad, symbol) in seen {
        let (expected, from_card) = if let Some(r) = card_role(model, &pad) {
            (r, true)
        } else {
            let rule = pin_rules.role_for_pad(&comp.footprint, Some(model.kind), 3, &pad)?;
            let electrode = match rule.role.as_str() {
                "gate" => "gate",
                "source" => "source",
                "drain" => "drain",
                _ => return None,
            };
            (electrode, false)
        };
        rows.push((pad, symbol, expected, from_card));
    }
    Some(Pads { rows })
}

/// Run the check.
#[must_use]
pub fn fet_pinout_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for (comp, model) in super::resolved_components(board, lib) {
        if !matches!(model.kind, ComponentKind::Nmos | ComponentKind::Pmos) {
            continue;
        }
        let Some(pads) = pads_of(comp, &model, lib) else {
            continue;
        };
        if pads
            .rows
            .iter()
            .all(|(_, symbol, expected, _)| symbol == expected)
        {
            continue;
        }
        let from_card = pads.rows.iter().all(|r| r.3);
        let order = |pick: fn(&(String, &'static str, &'static str, bool)) -> &'static str| {
            pads.rows
                .iter()
                .map(|r| format!("{}={}", r.0, letter(pick(r))))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (symbol_order, expected_order) = (order(|r| r.1), order(|r| r.2));
        let basis = if from_card {
            format!("the card '{}' numbers the pads {expected_order}", model.id)
        } else {
            format!(
                "the {} footprint's standard pinout (assumed: the common default for the package, not read from this part's datasheet) is {expected_order}",
                comp.footprint
            )
        };
        let mut nets = Vec::new();
        for pin in &comp.pins {
            let wrong = pads
                .rows
                .iter()
                .any(|(pad, s, e, _)| *pad == pin.number.trim() && s != e);
            if let Some(name) = pin.net.and_then(|id| board.net(id)).map(|n| &n.name) {
                if wrong && !nets.contains(name) {
                    nets.push(name.clone());
                }
            }
        }
        report.findings.push(LintFinding {
            check: LintCheck::SymbolCardPinMismatch,
            severity: if from_card {
                Severity::High
            } else {
                Severity::Medium
            },
            message: format!(
                "{} ({}): the schematic symbol numbers the pins {symbol_order} (G gate, S source, D drain), but {basis}. The netlist wires the pads by number, so on the ordered part the nets land on other electrodes than the symbol shows: a gate net on the source, a drain on the gate. {}confirm the pad order in the datasheet of the ordered part for this package (SOT-23 MOSFETs are usually G-S-D but not every vendor numbers them so), then re-number the symbol or change the part",
                comp.reference,
                comp.value,
                hauksbee_extract::REMEDY_MARKER,
            ),
            refs: vec![comp.reference.clone()],
            nets,
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V5: i64 = 2;
    const SW: i64 = 3;
    const GATE: i64 = 4;

    /// A FET with `order` giving the electrode on pads 1, 2 and 3.
    fn fet(lib_id: &str, footprint: &str, order: [&str; 3]) -> Component {
        let nets = |f: &str| match f {
            "G" => GATE,
            "S" => V5,
            _ => SW,
        };
        Component {
            reference: "Q1".into(),
            value: "AO3401A".into(),
            lib_id: lib_id.into(),
            footprint: footprint.into(),
            pins: order
                .iter()
                .enumerate()
                .map(|(i, f)| pin_fn(&(i + 1).to_string(), nets(f), f))
                .collect(),
            ..Default::default()
        }
    }

    fn lint(q: Component) -> Vec<LintFinding> {
        let b = board(
            "t",
            &[(GND, "GND"), (V5, "+5V"), (SW, "SW_OUT"), (GATE, "GATE")],
            vec![q],
        );
        fet_pinout_lint(&b, &ModelLibrary::builtin()).findings
    }

    const SOT23: &str = "Package_TO_SOT_SMD:SOT-23";
    const TO220: &str = "Package_TO_SOT_THT:TO-220-3_Vertical";

    #[test]
    fn a_gds_symbol_on_a_sot23_footprint_is_flagged_as_an_assumed_pinout() {
        let found = lint(fet("Device:Q_PMOS_GDS", SOT23, ["G", "D", "S"]));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        assert_eq!(found[0].check, LintCheck::SymbolCardPinMismatch);
        assert!(
            found[0].message.contains("1=G, 2=D, 3=S")
                && found[0].message.contains("1=G, 2=S, 3=D")
                && found[0].message.contains("assumed"),
            "{}",
            found[0].message
        );
        assert_eq!(found[0].nets, ["SW_OUT", "+5V"]);
    }

    #[test]
    fn a_gsd_symbol_on_a_to220_footprint_is_flagged_and_gds_is_not() {
        let bad = lint(fet("Device:Q_NMOS_GSD", TO220, ["G", "S", "D"]));
        assert_eq!(bad.len(), 1, "{bad:#?}");
        let ok = lint(fet("Device:Q_NMOS_GDS", TO220, ["G", "D", "S"]));
        assert!(ok.is_empty(), "{ok:#?}");
    }

    #[test]
    fn a_matching_symbol_an_unknown_package_and_a_dnp_part_are_silent() {
        let ok = lint(fet("Device:Q_PMOS_GSD", SOT23, ["G", "S", "D"]));
        assert!(ok.is_empty(), "{ok:#?}");
        // A package no rule covers: nothing to compare against.
        let unknown = lint(fet(
            "Device:Q_PMOS_GDS",
            "Package_SO:SOIC-8",
            ["G", "D", "S"],
        ));
        assert!(unknown.is_empty(), "{unknown:#?}");
        // A pad the symbol does not name as an electrode: nothing is claimed.
        let mut unnamed = fet("Device:Q_PMOS_GDS", SOT23, ["G", "D", "S"]);
        unnamed.pins[1].function = "~".into();
        assert!(lint(unnamed).is_empty());
        let mut dnp = fet("Device:Q_PMOS_GDS", SOT23, ["G", "D", "S"]);
        dnp.dnp = true;
        assert!(lint(dnp).is_empty());
    }
}
