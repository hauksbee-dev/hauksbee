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
    // Exhaustive on purpose: a new check must say whether its verdict rests on
    // a drafted card, or a low-confidence card would raise gating findings of it.
    match check {
        LintCheck::OperatingEnvelope | LintCheck::BackPower => Some(Basis::Envelope),
        LintCheck::PowerPinWiring
        | LintCheck::SupplyGroundSwapped
        | LintCheck::SymbolCardPinMismatch => Some(Basis::Pins),
        // The card's thresholds, roles, strap levels or bus behaviour decide these
        // verdicts: a card the drafter was unsure of must not fail a run alone.
        LintCheck::StrapPin
        | LintCheck::McuResourceConflict
        | LintCheck::DeviceDecode
        | LintCheck::I2cBusLoading
        | LintCheck::FloatingInput
        | LintCheck::DcOverstress
        | LintCheck::RegulatorHeldOff
        | LintCheck::ControlHeldOff
        | LintCheck::PullupIneffective
        | LintCheck::AudioBusWiring
        | LintCheck::PowerDomainBackfeed
        | LintCheck::PowerDomainBusLockup
        | LintCheck::RequiredPinBias
        | LintCheck::SupplyCapacitance
        | LintCheck::HoldUpDroop
        | LintCheck::PinMuxCapability
        | LintCheck::FirmwareUnconnectedPad
        // The card's open-drain and internal pull-up pad tables decide it.
        | LintCheck::MissingOpenDrainPullup => Some(Basis::Card),
        // Read from the schematic, the layout or the netlist alone.
        LintCheck::MissingI2cPullup
        | LintCheck::MissingSdPullup
        | LintCheck::FloatingControlPin
        | LintCheck::LedCurrentSanity
        | LintCheck::OutputContention
        | LintCheck::DesignatorFootprintMismatch
        | LintCheck::ValuePackageSanity
        | LintCheck::PlaceholderValue
        | LintCheck::UncheckedMcu
        | LintCheck::PinFunction
        | LintCheck::UnroutedPad
        | LintCheck::KelvinSense
        | LintCheck::PcbSchematicMismatch
        | LintCheck::RailConflict
        | LintCheck::MissingDecoupling
        | LintCheck::GroundIsland
        | LintCheck::UnwiredConnector
        | LintCheck::LedReversed
        | LintCheck::SiblingMismatch
        | LintCheck::UnlimitedBaseDrive
        | LintCheck::CapNoDcPath
        | LintCheck::FloatingGate
        | LintCheck::GateUnderdrive
        | LintCheck::DiodeReversed
        | LintCheck::PolarisedCapReversed
        | LintCheck::RailUnpowered
        | LintCheck::DatasheetMismatch
        | LintCheck::PartFieldMismatch
        | LintCheck::SignalDirection
        | LintCheck::DebugHeaderPinout
        | LintCheck::BusLineSwap
        | LintCheck::UsbCcTermination
        | LintCheck::SwitchShort => None,
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
    // The footprint's pad count is of every pad, wired or not, as `models extract`
    // counts it: a part with unconnected pads is not a smaller package.
    let bound = hauksbee_models::card_coverage::Board {
        refdes: comp.reference.clone(),
        value: comp.value.clone(),
        footprint: comp.footprint.clone(),
        pads: comp
            .pins
            .iter()
            .map(|p| {
                let net = p
                    .net
                    .filter(|n| *n != 0)
                    .and_then(|n| board.net(n))
                    .map_or_else(|| "(unconnected)".to_string(), |n| n.name.clone());
                (p.number.clone(), net)
            })
            .collect(),
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

    /// A drafted MCU card with one active-low boot strap (VIL 0.9 V, VIH 2.0 V)
    /// and `source_extra` appended to its source table.
    fn strap_card(tier: &str, source_extra: &str) -> String {
        format!(
            r#"
[[models]]
id = "dr_strap"
kind = "mcu"
description = "drafted strap fixture"
[models.match]
value_re = "^DR_STRAP$"
[models.pins]
"1" = "vdd"
"2" = "boot0"
"3" = "gnd"
[[models.straps]]
role = "boot0"
level = "low"
boot_select = true
internal_pull = "none"
vil = [0.0, 0.9]
vih = [0.0, 2.0]
[models.source]
tier = "{tier}"
{source_extra}
"#
        )
    }

    /// A 10k pull-up to 3V3 on a strap that must read low: a real board fault
    /// as judged by the card, whatever the card's trust.
    fn strap_findings(lib: &ModelLibrary) -> Vec<LintFinding> {
        let b = board(
            &[(1, "GND"), (2, "+3V3"), (3, "/BOOT0")],
            vec![
                part("U1", "DR_STRAP", &[("1", 2), ("2", 3), ("3", 1)]),
                part("R1", "10k", &[("1", 3), ("2", 2)]),
            ],
        );
        let mut report = crate::checks::straps::strap_lint(&b, lib);
        cap_doubted_cards(&mut report, &b, lib);
        report.findings
    }

    const LOW_CONFIDENCE: &str = "[[models.source.doubt]]\narea = \"card\"\nreason = \"the drafter reported low confidence: strap thresholds guessed\"\n";

    /// A drafted card the drafter itself rated low confidence must not fail a
    /// run: its strap finding is a non-gating note that links the card.
    #[test]
    fn a_low_confidence_drafted_card_raises_no_gating_strap_finding() {
        let found = strap_findings(&library(
            "lowconf",
            &strap_card("datasheet-derived", LOW_CONFIDENCE),
            true,
        ));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Low, "{}", found[0].message);
        let m = &found[0].message;
        assert!(m.starts_with("Check this card"), "{m}");
        assert!(m.contains("drafted, low confidence, verify"), "{m}");
        assert!(m.contains("not a board fault"), "{m}");
        assert!(hauksbee_extract::model_card_file_in(m).is_some(), "{m}");
        // The same card once reviewed (no doubt) keeps the gating finding.
        let reviewed = strap_findings(&library(
            "reviewed",
            &strap_card("datasheet-derived", ""),
            true,
        ));
        assert_eq!(reviewed.len(), 1, "{reviewed:#?}");
        assert!(reviewed[0].severity != Severity::Low, "{reviewed:#?}");
        assert!(!reviewed[0].message.starts_with("Check this card"));
        // A curated card is never capped, whatever rows it carries.
        let curated = strap_findings(&library(
            "curated",
            &strap_card("curated-library", LOW_CONFIDENCE),
            true,
        ));
        assert!(curated[0].severity != Severity::Low, "{curated:#?}");
    }

    /// A drafted card whose VIH is above its own supply is stamped at lint
    /// time (a card drafted before the validator refused it), so the divider
    /// verdict it feeds is a note too.
    #[test]
    fn a_vih_above_its_supply_makes_a_legacy_drafted_card_doubted() {
        let text = strap_card("datasheet-derived", "").replace(
            "[models.source]",
            "[models.params]\nvih = 5.0\n[[models.envelope]]\nkind = \"supply_range\"\npin = \"vdd\"\nmin_v = 1.65\nmax_v = 3.6\nbasis = \"ROC\"\n[models.source]",
        );
        let found = strap_findings(&library("vih", &text, true));
        assert!(!found.is_empty());
        assert!(
            found.iter().all(|f| f.severity == Severity::Low),
            "{found:#?}"
        );
    }

    /// A QFN-8-1EP card (pads 1..=8 and the exposed pad 9) bound to a footprint
    /// of `lead_pads` leads plus an exposed pad numbered 9 and repeated
    /// `ep_copies` times (thermal vias, paste), plus one unnumbered mechanical pad.
    fn pad_map_doubts(lead_pads: usize, ep_copies: usize, unwired_lead: bool) -> Vec<String> {
        let mut text = card("datasheet-derived", "");
        text = text.replace(
            "\"1\" = \"vdd\"\n\"2\" = \"gnd\"\n",
            "\"1\" = \"vdd\"\n\"2\" = \"gnd\"\n\"3\" = \"in3\"\n\"4\" = \"in4\"\n\"5\" = \"in5\"\n\"6\" = \"in6\"\n\"7\" = \"in7\"\n\"8\" = \"in8\"\n\"9\" = \"ep\"\n",
        );
        let lib = library("padmap", &text, true);
        let numbers: Vec<String> = (1..=lead_pads)
            .map(|n| n.to_string())
            .chain((0..ep_copies).map(|_| "9".to_string()))
            .chain(std::iter::once(String::new()))
            .collect();
        // Pads 2 and 9 are ground, an unwired lead is on no net, the rest are signals.
        let net = |n: &str| match n {
            "2" | "9" => 1,
            "8" if unwired_lead => 0,
            _ => 2,
        };
        let pads: Vec<(&str, i64)> = numbers.iter().map(|n| (n.as_str(), net(n))).collect();
        let mut u1 = part("U1", "DR_PART", &pads);
        u1.footprint = "Package_DFN_QFN:QFN-8-1EP_3x3mm_P0.5mm_EP1.5x1.5mm_ThermalVias".into();
        let b = board(&[(1, "GND"), (2, "+3V3")], vec![u1.clone()]);
        match card_of(&b, &lib, &u1) {
            Card::Drafted(d) => d.doubts.into_iter().map(|d| d.reason).collect(),
            _ => panic!("the fixture card is a drafted one"),
        }
    }

    #[test]
    fn an_exposed_pad_repeated_for_its_thermal_vias_is_one_pad() {
        // 8 leads + EP "9" printed 12 times + an unnumbered pad: 9 distinct pads.
        let doubts = pad_map_doubts(8, 12, false);
        assert!(
            doubts.iter().all(|d| !d.contains("pad map is read from")),
            "{doubts:?}"
        );
        // A lead left unconnected is still a pad of the footprint.
        let doubts = pad_map_doubts(8, 12, true);
        assert!(
            doubts.iter().all(|d| !d.contains("pad map is read from")),
            "{doubts:?}"
        );
    }

    #[test]
    fn a_footprint_of_more_distinct_pads_than_the_card_has_is_still_another_package() {
        // 12 leads: the card's 9 pins really are another package's.
        let doubts = pad_map_doubts(12, 12, false);
        assert!(
            doubts
                .iter()
                .any(|d| d.contains("12 pads") || d.contains("13 pads")),
            "{doubts:?}"
        );
    }
}
