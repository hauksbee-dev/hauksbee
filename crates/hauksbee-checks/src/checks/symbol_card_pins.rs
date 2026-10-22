//! A schematic symbol and the part's card that disagree about a pin.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The schematic names a part's pins (`XTAL1`, `VDD`, `~{RESET}`) and the
//! part's card names the same pads in its own words. Both are meant to be the
//! datasheet's pin table, so for one pad number they must state the same pin.
//! When they state different ones the board is read two ways at once: the
//! netlist wires a crystal to what the symbol calls `XTAL2` while every check
//! and the simulation take that pad for `XTAL1`; a pad the card calls ground is
//! wired to a supply. The symbol may have been drawn for another package, a
//! pin number may have been edited, or the card may be wrong; the datasheet's
//! pin table for this package decides which.
//!
//! Judged only on a definite contradiction. Both names must state a function
//! from the one table in `hauksbee_ir::pin_functions` (crystal pins, reset,
//! supply, ground, analog reference, I2C and SWD lines: functions that read the
//! same from either side of the pad) and the two sets of functions must share
//! nothing. A multiplexed name that includes the card's function
//! (`PB6/XTAL1` against `pb6_xtal1`), a generic pin name (`PA4`, `GPIO7`, `1`),
//! a name the table does not know, a card that does not name the pad, a card
//! the binder only guessed at, a card whose pin map does not fit the footprint
//! and a part not fitted are all silent. `TX`/`RX` and `MOSI`/`MISO` are left
//! out on purpose: a module symbol may name those from either side.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_ir::pin_functions::{functions_named, PinFunction};
use hauksbee_models::{Confidence, ModelLibrary};

/// The words for a set of functions, `crystal input (XTAL1)` or `a or b`.
fn said(functions: &BTreeSet<PinFunction>) -> String {
    functions
        .iter()
        .map(|f| f.label())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Flag every pad whose symbol name and card role state different pins.
#[must_use]
pub fn symbol_card_pin_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let res = hauksbee_bind::binder::resolve(lib, part);
        if !matches!(res.confidence, Confidence::Exact | Confidence::Family) {
            continue;
        }
        let Some(model) = res.model else { continue };
        if !super::card_fits(board, comp, &model) {
            continue;
        }
        let mut lines = Vec::new();
        let mut nets = Vec::new();
        let mut power = false;
        let mut seen = BTreeSet::new();
        for pin in &comp.pins {
            let Some(role) = model.pins.get(&pin.number) else {
                continue;
            };
            if pin.function.trim().is_empty() || !seen.insert(pin.number.clone()) {
                continue;
            }
            let (symbol, card) = (functions_named(&pin.function), functions_named(role));
            if symbol.is_empty() || card.is_empty() || !symbol.is_disjoint(&card) {
                continue;
            }
            power |= symbol.iter().chain(&card).any(|f| f.is_power());
            lines.push(format!(
                "pad {} is '{}' on the symbol ({}) but '{role}' on the card ({})",
                pin.number,
                pin.function.trim(),
                said(&symbol),
                said(&card),
            ));
            if let Some(name) = pin.net.and_then(|id| board.net(id)).map(|n| &n.name) {
                if !nets.contains(name) {
                    nets.push(name.clone());
                }
            }
        }
        if lines.is_empty() {
            continue;
        }
        report.findings.push(LintFinding {
            check: LintCheck::SymbolCardPinMismatch,
            severity: if power {
                Severity::High
            } else {
                Severity::Medium
            },
            message: format!(
                "{} ({}): the schematic symbol and the card '{}' disagree about which pin is \
                 which: {}. The netlist wires the pad by its number, so one of the two reads the \
                 board wrongly: the checks and the simulation would judge another pin than the \
                 one drawn. Compare both against the datasheet pin table for this package, then \
                 correct the symbol (a pin renamed, renumbered, or the symbol of another package) \
                 or the card",
                comp.reference,
                comp.value,
                model.id,
                lines.join("; "),
            ),
            refs: vec![comp.reference.clone()],
            nets,
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const NETS: [(i64, &str); 4] = [(1, "GND"), (2, "XA"), (3, "XB"), (4, "+5V")];

    /// An ATmega328P (DIP-28 card: pad 9 `pb6_xtal1`, pad 10 `pb7_xtal2`,
    /// pad 7 `vcc`, pad 8 `gnd`) with the symbol's pin names given.
    fn mcu(names: &[(&str, &str, i64)]) -> hauksbee_extract::Component {
        let mut u = part(
            "U1",
            "ATmega328P",
            &names.iter().map(|(n, _, net)| (*n, *net)).collect::<Vec<_>>(),
        );
        for (pin, (_, function, _)) in u.pins.iter_mut().zip(names) {
            pin.function = (*function).into();
        }
        u
    }

    fn run(u: hauksbee_extract::Component) -> Vec<LintFinding> {
        symbol_card_pin_lint(&board(&NETS, vec![u]), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn crystal_pins_swapped_between_the_symbol_and_the_card_are_flagged() {
        let found = run(mcu(&[("9", "XTAL2", 2), ("10", "XTAL1", 3)]));
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::SymbolCardPinMismatch);
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.refs, ["U1"]);
        assert_eq!(f.nets, ["XA", "XB"]);
        for want in [
            "pad 9 is 'XTAL2' on the symbol (crystal output (XTAL2)) but 'pb6_xtal1' on the card",
            "pad 10 is 'XTAL1'",
            "datasheet pin table",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
    }

    #[test]
    fn a_supply_pad_the_card_calls_ground_is_a_high_finding() {
        let found = run(mcu(&[("7", "GND", 1), ("8", "VCC", 4)]));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
    }

    #[test]
    fn agreeing_unknown_generic_and_multiplexed_names_are_silent() {
        // The symbol of the same package, in any notation.
        assert!(run(mcu(&[
            ("9", "PB6/XTAL1", 2),
            ("10", "PB7(XTAL2)", 3),
            ("7", "VDD", 4),
            ("8", "GND", 1)
        ]))
        .is_empty());
        // Generic or unknown names state no function.
        assert!(run(mcu(&[("9", "PB6", 2), ("10", "OSC32_OUT", 3), ("2", "D0", 1)])).is_empty());
        // A pad the card does not name, and a pin with no name at all.
        assert!(run(mcu(&[("99", "XTAL1", 2), ("9", "", 3)])).is_empty());
        // Names the table keeps out because they depend on the reader's side.
        assert!(run(mcu(&[("2", "TXD", 2), ("3", "RXD", 3)])).is_empty());
        // Not fitted.
        let mut dnp = mcu(&[("9", "XTAL2", 2)]);
        dnp.dnp = true;
        assert!(run(dnp).is_empty());
        // No card at all.
        let mut other = part("U2", "NoSuchPart", &[("9", 2)]);
        other.pins[0].function = "XTAL2".into();
        assert!(run(other).is_empty());
    }
}
