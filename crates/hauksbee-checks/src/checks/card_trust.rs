//! The one place a finding resting on a drafted card's doubtful field is
//! downgraded to "check this card".
//!
//! A card drafted from a datasheet is not a reviewed one ([`hauksbee_models::card_trust`]).
//! The drafting validator stamps what it could not confirm on the card
//! (`[[models.source.doubt]]`), and a drafted card nobody has re-extracted since
//! is read the same way at lint time. A finding of a card-derived check (the
//! operating envelope and back-power rows, the power-pin wiring) whose every
//! carded part is such a card is a statement about the card, not a board fault:
//! it is capped at Low, worded as a note to check the card, and names the card
//! file. A finding that involves a reviewed or library card, a card with no
//! doubt covering its basis, or any check that does not read the card alone,
//! keeps its severity and words.

use std::collections::BTreeMap;

use hauksbee_bind::binder::resolve;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{Component, ExtractedBoard, LintCheck, NetLintReport, Severity};
use hauksbee_models::card_trust::{self as trust, Basis};
use hauksbee_models::{ComponentKind, Doubt, ModelLibrary};

/// What a finding of `check` rests on, when that is one field group of the cards
/// of the parts it names.
const fn basis_of(check: LintCheck) -> Option<Basis> {
    match check {
        LintCheck::OperatingEnvelope | LintCheck::BackPower => Some(Basis::Envelope),
        LintCheck::PowerPinWiring | LintCheck::SupplyGroundSwapped => Some(Basis::Pins),
        _ => None,
    }
}

/// A part's drafted card and what is doubtful on it.
struct Drafted {
    card: String,
    origin: Option<String>,
    doubts: Vec<Doubt>,
}

/// What a part's card is, for the trust pass.
enum Card {
    /// No card of substance (a passive, an unbound part).
    None,
    /// A reviewed or library card.
    Reviewed,
    /// A drafted card.
    Drafted(Drafted),
}

/// What `comp`'s card is, for the trust pass.
fn card_of(board: &ExtractedBoard, lib: &ModelLibrary, comp: &Component) -> Card {
    let Some(part) = AssemblyState::of(comp).fitted() else {
        return Card::None;
    };
    let res = resolve(lib, part);
    let Some(model) = res.model else {
        return Card::None;
    };
    if model.kind == ComponentKind::Passive {
        return Card::None;
    }
    let tier = res
        .provenance
        .as_ref()
        .map(hauksbee_ir::evidence::ModelSource::tier);
    if !tier.is_some_and(|t| trust::is_drafted(t, res.layer)) {
        return Card::Reviewed;
    }
    let mut doubts = model.doubts.clone();
    // A card drafted before the validator stamped its doubts is read the same way.
    let pads: Vec<(String, String)> = comp
        .pins
        .iter()
        .filter_map(|p| {
            Some((
                p.number.clone(),
                board.net(p.net.filter(|n| *n != 0)?)?.name.clone(),
            ))
        })
        .collect();
    let bound = hauksbee_models::card_coverage::Board {
        refdes: comp.reference.clone(),
        value: comp.value.clone(),
        footprint: comp.footprint.clone(),
        pads: pads.clone(),
    };
    doubts.extend(
        trust::entry_problems(&model)
            .into_iter()
            .chain(trust::board_problems(&model, &pads))
            .chain(hauksbee_models::card_coverage::bound_pad_problem(
                &model, &bound,
            ))
            .map(|p| p.doubt()),
    );
    Card::Drafted(Drafted {
        card: model.id,
        origin: res.origin,
        doubts,
    })
}

/// Cap every finding in `report` that rests only on a doubted field of a drafted card.
pub fn cap_doubted_cards(report: &mut NetLintReport, board: &ExtractedBoard, lib: &ModelLibrary) {
    let mut cards: BTreeMap<String, Card> = BTreeMap::new();
    for f in &mut report.findings {
        let Some(basis) = basis_of(f.check) else {
            continue;
        };
        if f.severity == Severity::Low || f.refs.is_empty() {
            continue;
        }
        for r in &f.refs {
            cards.entry(r.clone()).or_insert_with(|| {
                board
                    .components
                    .iter()
                    .find(|c| &c.reference == r)
                    .map_or(Card::None, |c| card_of(board, lib, c))
            });
        }
        let carded: Vec<(&String, Option<&Drafted>)> = f
            .refs
            .iter()
            .filter_map(|r| match cards.get(r)? {
                Card::None => None,
                Card::Reviewed => Some((r, None)),
                Card::Drafted(d) => Some((r, Some(d))),
            })
            .collect();
        // Every part with a card must be a drafted one with a doubt covering this basis.
        let doubted: Option<Vec<(&String, &Drafted, &Doubt)>> = carded
            .iter()
            .map(|(r, card)| {
                let d = (*card)?;
                Some((*r, d, trust::doubted(&d.doubts, basis)?))
            })
            .collect();
        let Some(doubted) = doubted.filter(|d| !d.is_empty()) else {
            continue;
        };
        let (part, drafted, doubt) = doubted[0];
        f.severity = Severity::Low;
        f.message = trust::doubt_note(part, &drafted.card, doubt, &f.message)
            + &drafted.origin.as_ref().map_or_else(String::new, |o| {
                format!("{}{o}", hauksbee_extract::MODEL_CARD_MESSAGE_MARKER)
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use hauksbee_extract::LintFinding;

    /// A drafted card for a part that needs 4.5..5.5 V, with `extra` appended to its source table.
    fn card(tier: &str, source_extra: &str) -> String {
        format!(
            r#"
[[models]]
id = "dr_part"
kind = "digital"
description = "drafted fixture"
[models.match]
value_re = "^DR_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 4.5
max_v = 5.5
basis = "Recommended Operating Conditions, VDD row"
[models.source]
tier = "{tier}"
{source_extra}
"#
        )
    }

    fn library(tag: &str, text: &str, user: bool) -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("card_trust", tag);
        std::fs::write(dir.join("models.toml"), text).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = if user {
            lib.load_user_dir(&dir)
        } else {
            Vec::new()
        };
        assert!(errors.is_empty(), "{errors:?}");
        lib
    }

    /// The board puts the part's VDD on a 3.3 V rail its 4.5..5.5 V window excludes.
    fn findings(lib: &ModelLibrary) -> Vec<LintFinding> {
        let b = board(
            &[(1, "GND"), (2, "+3V3")],
            vec![part("U1", "DR_PART", &[("1", 2), ("2", 1)])],
        );
        let mut report = crate::checks::envelope::envelope_lint(&b, lib);
        let before = report.findings.len();
        cap_doubted_cards(&mut report, &b, lib);
        assert_eq!(report.findings.len(), before);
        report.findings
    }

    const DOUBT: &str =
        "[[models.source.doubt]]\narea = \"envelope\"\nreason = \"sense input capped\"\n";

    #[test]
    fn a_finding_on_a_doubted_drafted_card_is_a_note_to_check_the_card() {
        let doubted = findings(&library("doubted", &card("datasheet-derived", DOUBT), true));
        assert_eq!(doubted.len(), 1, "{doubted:#?}");
        assert_eq!(doubted[0].severity, Severity::Low);
        assert!(
            doubted[0].message.starts_with("Check this card"),
            "{}",
            doubted[0].message
        );
        assert!(
            doubted[0].message.contains("not a board fault"),
            "{}",
            doubted[0].message
        );
        assert!(
            hauksbee_extract::model_card_file_in(&doubted[0].message).is_some(),
            "the card file is linked: {}",
            doubted[0].message
        );
        // The same card with no doubt is judged as ever.
        let clean = findings(&library("clean", &card("datasheet-derived", ""), true));
        assert_eq!(clean.len(), 1);
        assert!(clean[0].severity != Severity::Low, "{:?}", clean[0]);
        assert!(!clean[0].message.starts_with("Check this card"));
        // A doubt on a pin map does not cover an envelope finding... it does (a wrong
        // pad is why the window was read), but an unrelated area does not.
        let other = card(
            "datasheet-derived",
            "[[models.source.doubt]]\narea = \"pins\"\nreason = \"x\"\n",
        );
        assert!(findings(&library("pins", &other, true))[0]
            .message
            .starts_with("Check this card"));
    }

    #[test]
    fn a_reviewed_card_keeps_its_findings_whatever_it_says() {
        // The same text as a curated card in a user file: not drafted, so never capped.
        let curated = card("curated-library", DOUBT);
        let found = findings(&library("curated", &curated, true));
        assert_eq!(found.len(), 1);
        assert!(
            !found[0].message.starts_with("Check this card"),
            "{}",
            found[0].message
        );
        assert!(found[0].severity != Severity::Low);
    }
}
