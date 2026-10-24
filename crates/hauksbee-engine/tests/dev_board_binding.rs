//! A plug-in dev board placed whole on a carrier binds the processor it carries.
//!
//! A carrier that plugs in an ESP32-DevKitC, a Seeed XIAO, an Adafruit Feather or
//! an Arduino Nano draws only the dev board's header; the processor, its crystal
//! and its regulator are on the dev board. The cards in `db/mcu_boards.toml` name
//! the processor and map each header pin to it, by the pin's silkscreen label
//! (`IO13`, `D10`, `A12/IO13`) because the pad numbers depend on whichever library
//! drew the footprint. What must hold:
//!
//! * each such board binds its own chip's backend, its header pins reach the
//!   right processor pins, and its on-board crystal is the clock;
//! * a board whose value names it but whose chip the library cannot run (Nano
//!   Every, Teensy 4.1, Feather M0) is left unresolved and the reason names the
//!   chip, instead of running the classic Nano's `ATmega328P` firmware model;
//! * a header footprint shared by boards with different chips, with a value that
//!   names no board, says so;
//! * labels never re-map another card's pads, and a plain header binds nothing;
//! * a Nano-footprint board with blink firmware runs and toggles D13.

use hauksbee_bind::binder::BoundBoard;
use hauksbee_bind::mcu_clock::ClockKind;
use hauksbee_engine::binder::bind_board;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::ModelLibrary;

fn comp(value: &str, lib: &str, footprint: &str, pins: &[(&str, i64, &str)]) -> Component {
    crate::support::component(
        "U1".to_string(),
        value.to_string(),
        lib.to_string(),
        footprint.to_string(),
        pins.iter()
            .map(|(n, net, f)| crate::support::pin(n, Some(*net), f, ""))
            .collect(),
    )
}

fn bind_one(c: Component) -> BoundBoard {
    let board = ExtractedBoard {
        name: "carrier".into(),
        nets: ["GND", "+3V3", "LED", "TX", "SENSE", "+5V"]
            .iter()
            .zip(1..)
            .map(|(name, i)| crate::support::net(i, (*name).into()))
            .collect(),
        components: vec![c],
    };
    bind_board(&board, &ModelLibrary::builtin())
}

fn reason(bound: &BoundBoard) -> String {
    let row = &bound.report.rows[0];
    format!("{} {:?}", row.outcome.label(), row.outcome)
}

/// The bound processor's backend, module crystal (Hz) and pad -> role map.
fn bound(
    c: Component,
) -> (
    String,
    Option<u64>,
    std::collections::HashMap<String, String>,
) {
    let bound = bind_one(c);
    let mcu = bound
        .mcus
        .first()
        .unwrap_or_else(|| panic!("a processor binds: {}", reason(&bound)));
    assert_eq!(mcu.clock.kind, ClockKind::Module, "{}", mcu.clock.detail);
    (
        mcu.backend.clone(),
        mcu.clock.crystal_hz,
        mcu.pad_roles.clone(),
    )
}

fn role<'a>(roles: &'a std::collections::HashMap<String, String>, pad: &str) -> &'a str {
    roles.get(pad).map_or("", String::as_str)
}

// ---- 1. each dev board binds its own chip, header pins by label --------------

#[test]
fn an_esp32_devkitc_binds_by_header_label_whatever_the_pad_numbers() {
    // A library that numbers the two headers J2-n / J3-n, not 1..38.
    let (backend, hz, roles) = bound(comp(
        "ESP32-DevKitC-32E",
        "PCM_Espressif:ESP32-DevKitC",
        "PCM_Espressif:ESP32-DevKitC",
        &[
            ("J2-1", 2, "3V3"),
            ("J2-2", 2, "EN"),
            ("J2-14", 1, "GND"),
            ("J2-15", 3, "IO13"),
            ("J3-4", 4, "TX"),
            ("J2-19", 6, "5V"),
        ],
    ));
    assert_eq!(backend, "qemu:esp32");
    assert_eq!(hz, Some(40_000_000));
    assert_eq!(role(&roles, "J2-15"), "gpio13");
    assert_eq!(role(&roles, "J3-4"), "gpio1", "TX is U0TXD = GPIO1");
    assert_eq!(role(&roles, "J2-2"), "en");
    assert_eq!(
        role(&roles, "J2-19"),
        "vbus_5v",
        "a 3.3 V board's 5 V pin is the regulator input, not a logic supply"
    );
}

#[test]
fn a_xiao_rp2040_binds_by_value_with_its_header_map_and_crystal() {
    let (backend, hz, roles) = bound(comp(
        "XIAO RP2040",
        "Seeed_Studio_XIAO_Series:XIAO-RP2040-DIP",
        "Seeed_Studio_XIAO_Series:XIAO-RP2040-DIP",
        &[
            ("1", 5, "GPIO26/A0/D0"),
            ("7", 4, "GPIO0/TX/D6"),
            ("11", 3, "D10"),
            ("12", 2, "3V3"),
            ("13", 1, "GND"),
            ("14", 6, "5V"),
        ],
    ));
    assert_eq!(backend, "renode:rp2040");
    assert_eq!(hz, Some(12_000_000));
    assert_eq!(role(&roles, "1"), "gpio26_adc0");
    assert_eq!(role(&roles, "7"), "gpio0");
    assert_eq!(role(&roles, "11"), "gpio3", "D10 / MOSI is GPIO3");
}

#[test]
fn a_xiao_whose_value_names_nothing_is_named_by_its_footprint() {
    let (backend, _, roles) = bound(comp(
        "~",
        "",
        "Seeed_Studio_XIAO_Series:XIAO-RP2040-DIP",
        &[("1", 5, "D0"), ("12", 2, "3V3"), ("13", 1, "GND")],
    ));
    assert_eq!(backend, "renode:rp2040");
    assert_eq!(role(&roles, "1"), "gpio26_adc0");
}

#[test]
fn each_xiao_esp32_binds_its_own_chip_family() {
    let pins: &[(&str, i64, &str)] = &[
        ("7", 4, "D6"),
        ("11", 3, "D10"),
        ("12", 2, "3V3"),
        ("13", 1, "GND"),
    ];
    let (backend, hz, roles) = bound(comp("XIAO ESP32C3", "", "", pins));
    assert_eq!((backend.as_str(), hz), ("qemu:esp32c3", Some(40_000_000)));
    assert_eq!(
        (role(&roles, "7"), role(&roles, "11")),
        ("gpio21", "gpio10")
    );
    let (backend, hz, roles) = bound(comp("XIAO-ESP32S3-DIP", "", "", pins));
    assert_eq!((backend.as_str(), hz), ("qemu:esp32s3", Some(40_000_000)));
    assert_eq!((role(&roles, "7"), role(&roles, "11")), ("gpio43", "gpio9"));
}

#[test]
fn a_feather_binds_its_chip_and_keeps_reset_apart_from_the_regulator_enable() {
    let (backend, hz, roles) = bound(comp(
        "Adafruit_Feather_HUZZAH32_ESP32",
        "MCU_Module:Adafruit_Feather_HUZZAH32_ESP32",
        "Module:Adafruit_Feather",
        &[
            ("1", 5, "RESET"),
            ("2", 2, "3V3"),
            ("4", 1, "GND"),
            ("25", 3, "A12/IO13"),
            ("27", 6, "EN"),
        ],
    ));
    assert_eq!((backend.as_str(), hz), ("qemu:esp32", Some(40_000_000)));
    assert_eq!(role(&roles, "25"), "gpio13");
    assert_eq!(role(&roles, "1"), "en", "RESET is the ESP32's EN");
    assert_eq!(role(&roles, "27"), "reg_en", "EN is the regulator's enable");

    // Pads without labels fall back to the Feather footprint's numbering.
    let unnamed: &[(&str, i64, &str)] = &[("25", 3, "~"), ("4", 1, "~"), ("2", 2, "~")];
    let (backend, hz, roles) = bound(comp(
        "Feather RP2040",
        "",
        "Module:Adafruit_Feather",
        unnamed,
    ));
    assert_eq!((backend.as_str(), hz), ("renode:rp2040", Some(12_000_000)));
    assert_eq!(role(&roles, "25"), "gpio13");
    let (backend, hz, roles) = bound(comp("Feather 32u4", "", "Module:Adafruit_Feather", unnamed));
    assert_eq!(
        (backend.as_str(), hz),
        ("simavr:atmega32u4", Some(8_000_000))
    );
    assert_eq!(role(&roles, "25"), "pc7", "D13 is PC7 on the 32u4 Feather");
}

#[test]
fn a_pico_symbol_names_the_board_when_the_value_does_not() {
    let (backend, hz, _) = bound(comp(
        "~",
        "MCU_Module:RaspberryPi_Pico_Extensive",
        "Module:RaspberryPi_Pico_SMD",
        &[("1", 4, "GPIO0"), ("3", 1, "GND"), ("36", 2, "3V3")],
    ));
    assert_eq!((backend.as_str(), hz), ("renode:rp2040", Some(12_000_000)));
}

#[test]
fn a_dev_board_card_says_its_regulator_sources_the_3v3_pin() {
    let lib = ModelLibrary::builtin();
    for value in [
        "XIAO RP2040",
        "XIAO ESP32C3",
        "XIAO ESP32S3",
        "Feather RP2040",
        "HUZZAH32",
    ] {
        let q = hauksbee_models::ComponentQuery::new(None, Some(value.into()), None);
        let model = lib.resolve(&q).model.unwrap_or_else(|| panic!("{value}"));
        let out = model
            .supply_outputs
            .first()
            .unwrap_or_else(|| panic!("{value}"));
        assert_eq!(out.role, "3v3", "{value}");
        assert!(out.min > 3.2 && out.max < 3.4, "{value}: {out:?}");
    }
}

// ---- 2. a board the library cannot run says which chip it carries -----------

#[test]
fn a_dev_board_named_by_value_but_not_runnable_is_refused_naming_its_chip() {
    for (value, footprint, chip) in [
        ("Arduino_Nano_Every", "Module:Arduino_Nano", "ATmega4809"),
        ("Arduino_Nano_ESP32", "Module:Arduino_Nano", "ESP32-S3"),
        (
            "Arduino_Nano_RP2040_Connect",
            "Module:Arduino_Nano",
            "RP2040",
        ),
        ("Teensy4.1", "teensy:Teensy41", "RT1062"),
        ("Adafruit_Feather_M0", "Module:Adafruit_Feather", "SAMD21"),
        (
            "Seeeduino XIAO",
            "Seeed_Studio_XIAO_Series:XIAO-SAMD21-DIP",
            "SAMD21",
        ),
        // The chip token inside these values ("ESP32", "nRF52") must not let the
        // family router bind the bare chip by pin-name guesses.
        (
            "XIAO-nRF52840-DIP",
            "Seeed_Studio_XIAO_Series:XIAO-nRF52840-DIP",
            "nRF52840",
        ),
    ] {
        let b = bind_one(comp(
            value,
            "",
            footprint,
            &[("4", 1, "GND"), ("16", 3, "D13"), ("2", 4, "TX")],
        ));
        assert!(b.mcus.is_empty(), "{value} must not bind a processor");
        let why = reason(&b);
        assert!(
            why.contains(chip),
            "{value}: the reason names {chip}: {why}"
        );
    }
}

#[test]
fn a_feather_footprint_with_a_value_that_names_no_feather_says_why() {
    let b = bind_one(comp(
        "Feather",
        "",
        "Module:Adafruit_Feather",
        &[("2", 2, "3V3"), ("4", 1, "GND"), ("25", 3, "D13")],
    ));
    assert!(b.mcus.is_empty());
    assert!(
        reason(&b).contains("does not say which chip"),
        "{}",
        reason(&b)
    );
}

// ---- 3. silent controls ------------------------------------------------------

#[test]
fn header_labels_never_remap_a_card_that_has_none() {
    // A Pico's pad 4 is GP2 whatever a symbol calls it; the Nano's pad 16 is D13.
    let (_, _, roles) = bound(comp(
        "Pico",
        "",
        "Module:RaspberryPi_Pico_Common_THT",
        &[
            ("4", 3, "D10"),
            ("1", 4, "TX"),
            ("3", 1, "GND"),
            ("36", 2, "3V3"),
        ],
    ));
    assert_eq!((role(&roles, "4"), role(&roles, "1")), ("gpio2", "gpio0"));
    let b = bind_one(comp(
        "Arduino_Nano_v3.x",
        "",
        "Module:Arduino_Nano",
        &[("16", 3, "IO13"), ("4", 1, "GND"), ("27", 6, "+5V")],
    ));
    let mcu = b.mcus.first().unwrap_or_else(|| panic!("{}", reason(&b)));
    assert_eq!(mcu.backend, "simavr:atmega328p");
    assert_eq!(role(&mcu.pad_roles, "16"), "d13_sck");
}

#[test]
fn a_plain_header_with_dev_board_pin_names_binds_no_processor() {
    let b = bind_one(comp(
        "Conn_02x19",
        "Connector_Generic:Conn_02x19_Odd_Even",
        "Connector_PinHeader_2.54mm:PinHeader_2x19_P2.54mm_Vertical",
        &[
            ("1", 2, "3V3"),
            ("14", 1, "GND"),
            ("15", 3, "IO13"),
            ("23", 4, "TX"),
        ],
    ));
    assert!(b.mcus.is_empty(), "{}", reason(&b));
}

// ---- 4. a Nano-footprint board runs its firmware ------------------------------

#[cfg(feature = "avr")]
mod nano_cosim {
    use std::fmt::Write as _;

    use hauksbee_engine::HauksbeeEngine;
    use hauksbee_extract::ExtractedBoard;
    use hauksbee_frontdoor_api::engine::Engine;
    use hauksbee_testkit::avr_firmware::{intel_hex, port_toggle_image};

    /// An Arduino Nano placed by its KiCad footprint (header pads only: no
    /// `ATmega328P`, crystal or regulator drawn), D13 driving a LED load and RESET
    /// pulled up, as a carrier board draws it.
    fn nano_carrier() -> ExtractedBoard {
        let mut s = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n");
        for (i, name) in ["", "GND", "+5V", "RESET", "D13"].iter().enumerate() {
            writeln!(s, "  (net {i} \"{name}\")").unwrap();
        }
        s.push_str(
            "  (module Module:Arduino_Nano (layer F.Cu)\n    (at 100 100)\n    \
             (fp_text reference A1 (at 0 0) (layer F.SilkS))\n    \
             (fp_text value Arduino_Nano_v3.x (at 0 2) (layer F.Fab))\n",
        );
        for (pad, net, name) in [
            (4, 1, "GND"),
            (29, 1, "GND"),
            (27, 2, "+5V"),
            (3, 3, "RESET"),
            (28, 3, "RESET"),
            (16, 4, "D13"),
        ] {
            writeln!(
                s,
                "    (pad {pad} thru_hole circle (at 0 {pad}) (size 1 1) (net {net} \"{name}\"))"
            )
            .unwrap();
        }
        s.push_str("  )\n");
        for (r, a, b) in [
            ("R1", (3, "RESET"), (2, "+5V")),
            ("R2", (4, "D13"), (1, "GND")),
        ] {
            write!(
                s,
                "  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)\n    (at 120 100)\n    \
                 (fp_text reference {r} (at 0 0) (layer F.SilkS))\n    \
                 (fp_text value 10k (at 0 2) (layer F.Fab))\n    \
                 (pad 1 smd rect (at 0 0) (size 1 1) (net {} \"{}\"))\n    \
                 (pad 2 smd rect (at 1 0) (size 1 1) (net {} \"{}\"))\n  )\n",
                a.0, a.1, b.0, b.1
            )
            .unwrap();
        }
        s.push_str(")\n");
        ExtractedBoard::from_auto(&s).expect("parse board")
    }

    #[test]
    fn a_nano_footprint_board_runs_blink_firmware_and_toggles_d13() {
        let dir = tempfile::tempdir().unwrap();
        let fw = dir.path().join("blink.hex");
        std::fs::write(&fw, intel_hex(&port_toggle_image(50, 0))).unwrap();
        let lib = hauksbee_models::ModelLibrary::builtin();
        let bound = hauksbee_engine::binder::bind_board(&nano_carrier(), &lib);
        assert_eq!(
            bound
                .mcus
                .iter()
                .map(|m| m.backend.as_str())
                .collect::<Vec<_>>(),
            vec!["simavr:atmega328p"]
        );
        let mut e = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build the co-sim");
        let (mut high, mut low) = (false, false);
        for _ in 0..10 {
            let v = e.step(1e-3).net_voltages.get("D13").copied().unwrap_or(0.0);
            high |= v > 2.5;
            low |= v < 1.0;
        }
        assert!(
            high && low,
            "the Nano's ATmega328P runs and toggles D13 (PB5)"
        );
    }
}
