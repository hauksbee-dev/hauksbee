//! The AVR cards own two facts the rest of the tool reads: which pad map a
//! package gets, and the core clock a part runs at when nothing else states one.
//!
//! Package: the `ATmega328P` ships in three numberings (DS40002061A section 1,
//! Figure 1-1, PDF page 12): 28-pin PDIP, 32-lead TQFP / 32-pad MLF, and the
//! 28-pad MLF. A footprint that names a package must reach that package's card;
//! the emonTx V3.4 read "VCC = -1.8 V" when its TQFP-32 got the DIP numbering.
//!
//! Clock: `default_clock_hz` is the one place a bare chip's internal RC rate (or
//! a module's own crystal) is written as a number.

use hauksbee_models::ModelLibrary;

fn id_for_footprint(footprint: &str) -> Option<String> {
    super::query_id(Some("ATmega328P"), Some("ATmega328P"), Some(footprint))
}

/// Every spelling of a 32-lead/pad package reaches the 32-pad card, including the
/// `LQFP` the card used to miss; a lead count that merely starts with 32 does not.
#[test]
fn the_atmega328p_footprint_table_picks_the_package_card() {
    for (footprint, id) in [
        ("Package_QFP:TQFP-32_7x7mm_P0.8mm", "atmega328p_32pad"),
        ("Package_QFP:LQFP-32_7x7mm_P0.8mm", "atmega328p_32pad"),
        ("Housings_QFP:TQFP-32_7x7mm_Pitch0.8mm", "atmega328p_32pad"),
        ("TQFP32-08", "atmega328p_32pad"),
        ("lqfp32", "atmega328p_32pad"),
        ("QFP-32", "atmega328p_32pad"),
        (
            "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm_EP3.1x3.1mm",
            "atmega328p_32pad",
        ),
        ("Package_DFN_QFN:VQFN-32-1EP_5x5mm", "atmega328p_32pad"),
        ("MLF-32", "atmega328p_32pad"),
        (
            "Package_DFN_QFN:QFN-28-1EP_4x4mm_P0.45mm_EP2.4x2.4mm",
            "atmega328p_28mlf",
        ),
        ("MLF-28", "atmega328p_28mlf"),
        ("Package_DIP:DIP-28_W7.62mm", "atmega328p"),
        ("", "atmega328p"),
        ("QFN-320", "atmega328p"),
        ("QFN-3256", "atmega328p"),
        ("Package_QFP:TQFP-44_10x10mm_P0.8mm", "atmega328p"),
    ] {
        assert_eq!(
            id_for_footprint(footprint).as_deref(),
            Some(id),
            "footprint {footprint:?}"
        );
    }
}

/// The three numberings, read off Figure 1-1: VCC is pad 7 on the DIP, pad 4 on
/// the 32-pad packages and pad 3 on the 28-pad MLF.
#[test]
fn the_three_atmega328p_numberings_differ_where_the_datasheet_says() {
    let pins_of = |footprint: &str| {
        super::expect_query(Some("ATmega328P"), Some("ATmega328P"), Some(footprint))
    };
    let dip = pins_of("Package_DIP:DIP-28_W7.62mm");
    super::assert_pins(&dip, &super::pins("7=vcc 8=gnd 9=pb6_xtal1 1=pc6_reset"));
    let tqfp = pins_of("Package_QFP:LQFP-32_7x7mm_P0.8mm");
    super::assert_pins(
        &tqfp,
        &super::pins("4=vcc 3=gnd 7=pb6_xtal1 29=pc6_reset 19=adc6 22=adc7"),
    );
    let mlf = pins_of("Package_DFN_QFN:QFN-28-1EP_4x4mm_P0.45mm");
    super::assert_pins(
        &mlf,
        &super::pins("3=vcc 4=gnd 5=pb6_xtal1 25=pc6_reset 16=avcc 17=aref"),
    );
    assert_eq!(mlf.pins.len(), 29, "28 pads plus the bottom pad");
}

/// The clock a bare chip or module falls back to is the card's, as a number.
#[test]
fn default_clocks_come_from_the_cards() {
    let lib = ModelLibrary::builtin_shared();
    for (backend, part, module, hz) in [
        ("simavr:atmega328p", "ATmega328P", false, 8_000_000),
        // An ordering code no card's value regex spells still gets its chip's clock.
        ("simavr:atmega328p", "ATmega328P-PU", false, 8_000_000),
        ("simavr:atmega328p", "Arduino_Nano", true, 16_000_000),
        ("simavr:atmega32u4", "ProMicro", true, 16_000_000),
        ("simavr:atmega32u4", "ATmega32U4-AU", false, 8_000_000),
        ("simavr:attiny85", "ATtiny85-20PU", false, 8_000_000),
        ("simavr:atmega1284p", "ATmega1284P", false, 8_000_000),
    ] {
        assert_eq!(
            lib.mcu_default_clock_hz(backend, part, module),
            Some(hz),
            "{backend} {part}"
        );
    }
    assert_eq!(
        lib.mcu_default_clock_hz("simavr:atmega4809", "x", false),
        None
    );
    assert_eq!(
        lib.mcu_default_clock_hz("renode:stm32f103", "STM32F103", false),
        None
    );
}

/// The number must say what the card's prose says: `internal_clock` is the
/// sentence the binder shows, `default_clock_hz` the value the co-sim runs.
#[test]
fn the_default_clock_number_matches_the_internal_clock_sentence() {
    for value in [
        "ATmega328P",
        "ATmega1284P",
        "ATmega1284",
        "ATmega32U4",
        "ATtiny85",
    ] {
        let card = super::expect_query(Some(value), Some(value), None);
        let sentence = card.params.get_str("internal_clock").expect("RC card");
        let words: Vec<&str> = sentence.split_whitespace().collect();
        let mhz: f64 = words[words.iter().position(|w| *w == "MHz").expect("MHz") - 1]
            .parse()
            .expect("number before MHz");
        assert!(
            mhz.mul_add(-1e6, card.params.get_f64("default_clock_hz").expect("hz"))
                .abs()
                < 1.0,
            "{}: {sentence}",
            card.id
        );
    }
}

/// The parts the co-sim can be asked to build are the parts the cards name.
#[test]
fn the_supported_simavr_parts_are_the_cards_parts() {
    assert_eq!(
        ModelLibrary::builtin_shared().simavr_parts(),
        [
            "atmega1284",
            "atmega1284p",
            "atmega168",
            "atmega328p",
            "atmega32u4",
            "attiny85"
        ]
    );
}
