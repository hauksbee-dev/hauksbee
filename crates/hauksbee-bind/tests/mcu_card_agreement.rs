//! The ATmega328P pad map has two readers: the cards in `db/mcu.toml` (matched
//! on the footprint) and the binder's hand-copied tables, which answer when the
//! value carries an ordering-code suffix the cards' `value_re` does not spell
//! (`ATmega328P-PU`). Until the binder reads only the card, the two must agree
//! on every footprint, so a package the card learns cannot be missed by the
//! binder (the emonTx V3.4 read "VCC = -1.8 V" when its LQFP-32 got the DIP map).
//!
//! Remove `KNOWN_DIVERGENCES` (and this file's reason to exist) when the binder's
//! copy is deleted and the fallback resolves through the card.

use hauksbee_bind::binder::library_resolution;
use hauksbee_extract::Component;
use hauksbee_models::{ModelEntry, ModelLibrary};

fn resolve(value: &str, footprint: &str) -> ModelEntry {
    let comp = Component {
        reference: "U1".into(),
        value: value.into(),
        footprint: footprint.into(),
        ..Default::default()
    };
    library_resolution(ModelLibrary::builtin_shared(), &comp)
        .model
        .unwrap_or_else(|| panic!("{value} / {footprint} must resolve"))
}

/// Footprints on which the card and the binder's fallback agree.
const AGREED: &[(&str, &str)] = &[
    ("Package_QFP:TQFP-32_7x7mm_P0.8mm", "atmega328p_32pad"),
    ("Package_QFP:LQFP-32_7x7mm_P0.8mm", "atmega328p_32pad"),
    ("Housings_QFP:TQFP-32_7x7mm_Pitch0.8mm", "atmega328p_32pad"),
    ("TQFP32-08", "atmega328p_32pad"),
    ("lqfp32", "atmega328p_32pad"),
    (
        "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm",
        "atmega328p_32pad",
    ),
    ("Package_DFN_QFN:VQFN-32-1EP_5x5mm", "atmega328p_32pad"),
    ("MLF-32", "atmega328p_32pad"),
    ("Package_DIP:DIP-28_W7.62mm", "atmega328p"),
    ("", "atmega328p"),
    ("QFN-320", "atmega328p"),
    ("Package_QFP:TQFP-44_10x10mm_P0.8mm", "atmega328p"),
];

/// Footprints where the card is right and the binder's copy is not (DS40002061A
/// Figure 1-1, PDF page 12: the 28-pad MLF has its own numbering). The binder
/// gives these the DIP-28 map. Step 2 deletes the binder's copy and this list.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[
    (
        "Package_DFN_QFN:QFN-28-1EP_4x4mm_P0.45mm_EP2.4x2.4mm",
        "atmega328p_28mlf",
    ),
    ("MLF-28", "atmega328p_28mlf"),
];

#[test]
fn the_card_and_the_binder_fallback_pick_the_same_pad_map_for_every_footprint() {
    for (footprint, id) in AGREED {
        let card = resolve("ATmega328P", footprint);
        assert_eq!(card.id, *id, "card for {footprint:?}");
        // The `-PU` suffix is not in the card's value_re, so this is the binder's
        // own table answering for the same footprint.
        let binder = resolve("ATmega328P-PU", footprint);
        assert_eq!(binder.id, "atmega328p_fallback", "{footprint:?}");
        assert_eq!(
            card.pins, binder.pins,
            "the binder's pad map for {footprint:?} must be the card's {id}"
        );
    }
}

#[test]
fn the_known_divergences_are_exactly_the_28_pad_mlf() {
    let dip = resolve("ATmega328P", "Package_DIP:DIP-28_W7.62mm");
    for (footprint, id) in KNOWN_DIVERGENCES {
        let card = resolve("ATmega328P", footprint);
        assert_eq!(card.id, *id, "card for {footprint:?}");
        let binder = resolve("ATmega328P-PU", footprint);
        assert_ne!(
            card.pins, binder.pins,
            "{footprint:?}: fixed? delete this row"
        );
        assert_eq!(
            binder.pins, dip.pins,
            "{footprint:?}: the binder's copy has only the DIP and 32-pad maps"
        );
    }
}
