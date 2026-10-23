//! Outputs that release their pin are not in contention.
//!
//! The extract layer's output-contention check reads pin electrical types: two
//! pins a symbol author typed `output` on one net fire. An SPI data-out pad
//! (MISO, SDO, DO) is typed `output` yet the datasheet has it go high
//! impedance whenever its chip select is inactive, so several peripherals share
//! one MISO line by design. A card states that with
//! `[models.pin_capability.tri_state]` (a pad and the role of the input that
//! releases it, cited to the datasheet), and this pass drops the contention
//! finding when the card makes it impossible:
//!
//! - every driver on the net is a card-stated tri-state pad, and
//! - no two of them share one enable net, and
//! - no enable net is wired permanently active (an active-low chip select tied
//!   to ground, an active-high enable tied to a positive rail).
//!
//! One driver that is not a stated tri-state pad (a push-pull output with no
//! enable) keeps the finding: it fights whichever slave is selected. Two
//! drivers on the same select line keep it too, as does a permanently selected
//! one. A part with no card says nothing, so its net is judged as before.

use std::collections::{BTreeSet, HashMap};

use hauksbee_extract::{
    is_connector_like, is_ground, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Pin,
};
use hauksbee_models::netclass::is_positive_supply;
use hauksbee_models::{ModelEntry, ModelLibrary};

/// A chip select or output enable that is asserted at its low level, by the
/// spelling of its role.
fn active_low(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.ends_with("_n")
        || r.ends_with('n') && r.len() > 2 && !r.ends_with("en")
        || r.starts_with('n')
        || matches!(
            r.as_str(),
            "cs" | "csb" | "cs_b" | "ce" | "ncs" | "ss" | "sel_n"
        )
}

fn is_output(p: &Pin) -> bool {
    matches!(
        p.kind.trim().to_ascii_lowercase().as_str(),
        "output" | "power_out"
    )
}

/// What one driver pin on a contended net is, by its card.
enum Drive {
    /// A pad the card states is released while its enable is inactive: the
    /// enable's role and the net it is wired to.
    Released { role: String, net: Option<i64> },
    /// Anything else: always driving.
    Always,
}

fn drive_of(c: &Component, p: &Pin, model: Option<&ModelEntry>) -> Drive {
    let Some(role) = model.and_then(|m| m.tri_state_enable(&p.number)) else {
        return Drive::Always;
    };
    let model = model.expect("a stated tri-state pad has a card");
    let net = model
        .pins
        .iter()
        .filter(|(_, r)| r.eq_ignore_ascii_case(role))
        .find_map(|(pad, _)| {
            c.pins
                .iter()
                .find(|q| &q.number == pad)
                .and_then(|q| q.net)
                .filter(|n| *n != 0)
        });
    Drive::Released {
        role: role.to_string(),
        net,
    }
}

/// Whether the card makes the finding's contention impossible.
fn released(
    board: &ExtractedBoard,
    models: &HashMap<&str, ModelEntry>,
    finding: &LintFinding,
) -> bool {
    let nets: Vec<i64> = finding
        .nets
        .iter()
        .filter_map(|name| board.nets.iter().find(|n| &n.name == name).map(|n| n.id))
        .collect();
    let mut drives = Vec::new();
    for &id in &nets {
        for (c, p) in board.net_members(id) {
            if !finding.refs.contains(&c.reference) || is_connector_like(c) {
                continue;
            }
            let model = models.get(c.reference.as_str());
            let stated = model.is_some_and(|m| m.tri_state_enable(&p.number).is_some());
            if is_output(p) || stated {
                drives.push(drive_of(c, p, model));
            }
        }
    }
    if drives.is_empty() || drives.iter().any(|d| matches!(d, Drive::Always)) {
        return false;
    }
    let mut seen = BTreeSet::new();
    for d in &drives {
        let Drive::Released { role, net } = d else {
            continue;
        };
        let Some(net) = *net else { continue };
        if !seen.insert(net) {
            return false;
        }
        let name = board.net(net).map_or("", |n| n.name.as_str());
        let low = active_low(role);
        if (low && is_ground(name)) || (!low && is_positive_supply(name)) {
            return false;
        }
    }
    true
}

/// Remove the output-contention findings the cards show cannot happen.
pub fn drop_released_contention(
    report: &mut NetLintReport,
    board: &ExtractedBoard,
    lib: &ModelLibrary,
) {
    if !report
        .findings
        .iter()
        .any(|f| f.check == LintCheck::OutputContention)
    {
        return;
    }
    let models: HashMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    report
        .findings
        .retain(|f| f.check != LintCheck::OutputContention || !released(board, &models, f));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin};

    const GND: i64 = 1;
    const V33: i64 = 2;
    const MISO: i64 = 3;

    /// A W25Q80DV: /CS pad 1, DO pad 2 typed `output`, supply on 8, ground on 4.
    fn flash(reference: &str, cs: i64) -> Component {
        let mut do_pin = pin("2", MISO);
        do_pin.kind = "output".into();
        Component {
            reference: reference.into(),
            value: "W25Q80DVSSIG".into(),
            lib_id: "Memory_Flash:W25Q80DV".into(),
            footprint: "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm".into(),
            pins: vec![pin("1", cs), do_pin, pin("4", GND), pin("8", V33)],
            ..Default::default()
        }
    }

    fn run(parts: Vec<Component>) -> (usize, usize) {
        let b = board(
            "t",
            &[
                (GND, "GND"),
                (V33, "+3V3"),
                (MISO, "SPI_MISO"),
                (10, "CS_A"),
                (11, "CS_B"),
            ],
            parts,
        );
        let lib = ModelLibrary::builtin();
        let mut report = b.net_lint();
        let before = report
            .findings
            .iter()
            .filter(|f| f.check == LintCheck::OutputContention)
            .count();
        drop_released_contention(&mut report, &b, &lib);
        let after = report
            .findings
            .iter()
            .filter(|f| f.check == LintCheck::OutputContention)
            .count();
        (before, after)
    }

    #[test]
    fn peripherals_with_separate_chip_selects_share_miso_without_contention() {
        let (before, after) = run(vec![flash("U1", 10), flash("U2", 11)]);
        assert_eq!(before, 1, "the pin-type check sees two outputs");
        assert_eq!(after, 0, "the cards release both under their own CS");
    }

    #[test]
    fn a_shared_or_permanently_active_chip_select_keeps_the_finding() {
        let (_, same_cs) = run(vec![flash("U1", 10), flash("U2", 10)]);
        assert_eq!(same_cs, 1, "both slaves answer together");
        let (_, tied_low) = run(vec![flash("U1", 10), flash("U2", GND)]);
        assert_eq!(tied_low, 1, "a slave with CS tied low always drives");
    }

    #[test]
    fn a_push_pull_output_with_no_enable_keeps_the_finding() {
        let mut driver = Component {
            reference: "U3".into(),
            value: "DRIVER".into(),
            footprint: "Package:SOT23".into(),
            pins: vec![pin("1", MISO)],
            ..Default::default()
        };
        driver.pins[0].kind = "output".into();
        let (before, after) = run(vec![flash("U1", 10), driver]);
        assert_eq!((before, after), (1, 1));
    }
}
