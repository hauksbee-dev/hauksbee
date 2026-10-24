//! A module placed on a board binds a processor, and the module's own crystal
//! is in the model.
//!
//! Real boards place a whole module (a Pico, an ESP32-WROOM can, an Arduino
//! Nano) and never draw the crystal or flash inside it. Three things must hold:
//!
//! * a module named by its value, or by a footprint made for exactly that
//!   module, binds the processor its card names, with the module's pad map;
//! * a module whose footprint is shared by boards with different chips (Pico /
//!   Pico 2, Nano / Nano Every) or whose pad map the library lacks is left
//!   unresolved, and the report says exactly that;
//! * the module card declares its internal crystal (cited to the vendor
//!   datasheet), so the co-sim clock sees one: the RP2040 does not stall in
//!   oscillator start-up on a board that never draws the Pico's 12 MHz crystal.
//!
//! The clean counterparts stay silent: a bare RP2040 with no crystal still
//! stalls (the module's crystal must not leak onto a chip placed on its own),
//! and a bare chip footprint is never taken for a module.

use hauksbee_bind::binder::BoundBoard;
use hauksbee_bind::mcu_clock::ClockKind;
use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;

fn comp(r: &str, value: &str, lib: &str, footprint: &str, pins: &[(&str, i64, &str)]) -> Component {
    crate::support::component(
        r.to_string(),
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
        name: "module".into(),
        nets: vec![
            crate::support::net(1, "GND".into()),
            crate::support::net(2, "+3V3".into()),
            crate::support::net(3, "LED".into()),
        ],
        components: vec![c],
    };
    bind_board(&board, &ModelLibrary::builtin())
}

const PICO_PINS: &[(&str, i64, &str)] = &[
    ("3", 1, "GND"),
    ("36", 2, "3V3"),
    ("39", 2, "VSYS"),
    ("1", 3, "GPIO0"),
];
const WROOM_PINS: &[(&str, i64, &str)] = &[
    ("1", 1, "GND"),
    ("2", 2, "3V3"),
    ("3", 2, "EN"),
    ("25", 3, "IO0"),
    ("15", 1, "GND"),
];
const NANO_PINS: &[(&str, i64, &str)] = &[("4", 1, "GND"), ("27", 2, "+5V"), ("12", 3, "D9")];

fn reason(bound: &BoundBoard) -> String {
    let row = &bound.report.rows[0];
    format!("{} {:?}", row.outcome.label(), row.outcome)
}

// ---- 1. the module's processor is bound --------------------------------------

#[test]
fn a_pico_named_by_value_binds_the_rp2040_with_its_internal_crystal() {
    let bound = bind_one(comp(
        "U1",
        "Pico",
        "MCU_RaspberryPi_and_Boards:Pico",
        "Module:RaspberryPi_Pico_Common_THT",
        PICO_PINS,
    ));
    let mcu = bound
        .mcus
        .first()
        .unwrap_or_else(|| panic!("{}", reason(&bound)));
    assert_eq!(mcu.backend, "renode:rp2040");
    assert_eq!(mcu.clock.kind, ClockKind::Module, "{}", mcu.clock.detail);
    assert!(mcu.clock.external_present());
    assert_eq!(mcu.clock.crystal_hz, Some(12_000_000));
    assert!(
        mcu.clock.detail.contains("ABM8-272-T3"),
        "the clock sentence cites the datasheet: {}",
        mcu.clock.detail
    );
}

#[test]
fn the_smd_pico_value_binds_too() {
    let bound = bind_one(comp(
        "U1",
        "RPi_Pico_SMD",
        "MCU_RaspberryPi_and_Boards:Pico",
        "Module:RaspberryPi_Pico_SMD",
        PICO_PINS,
    ));
    assert_eq!(
        bound.mcus.first().map(|m| m.backend.as_str()),
        Some("renode:rp2040"),
        "{}",
        reason(&bound)
    );
}

#[test]
fn an_esp32_module_named_only_by_its_footprint_binds_the_esp32() {
    let bound = bind_one(comp(
        "U1",
        "MCU",
        "Custom:Module",
        "RF_Module:ESP32-WROOM-32",
        WROOM_PINS,
    ));
    let mcu = bound
        .mcus
        .first()
        .unwrap_or_else(|| panic!("{}", reason(&bound)));
    assert_eq!(mcu.backend, "qemu:esp32");
    assert_eq!(mcu.clock.kind, ClockKind::Module);
    assert_eq!(mcu.clock.crystal_hz, Some(40_000_000));
    let warning = bound.report.rows[0].warning.clone().unwrap_or_default();
    assert!(
        warning.contains("identified as ESP32-WROOM-32 by its footprint"),
        "the guess is disclosed: {warning}"
    );
}

#[test]
fn each_espressif_module_footprint_binds_its_own_chip_family() {
    for (footprint, backend) in [
        ("RF_Module:ESP32-WROVER-E", "qemu:esp32"),
        ("RF_Module:ESP32-S3-WROOM-1", "qemu:esp32s3"),
        ("RF_Module:ESP32-C3-MINI-1", "qemu:esp32c3"),
    ] {
        let bound = bind_one(comp("U1", "MCU", "Custom:Module", footprint, WROOM_PINS));
        assert_eq!(
            bound.mcus.first().map(|m| m.backend.as_str()),
            Some(backend),
            "{footprint}: {}",
            reason(&bound)
        );
    }
}

// ---- 2. an ambiguous or unmapped module says so, plainly ----------------------

#[test]
fn a_pico_footprint_with_no_part_value_is_unresolved_and_says_why() {
    let bound = bind_one(comp(
        "U1",
        "~",
        "Connector:Conn_02x20_Odd_Even",
        "Module:RaspberryPi_Pico_Common_THT",
        PICO_PINS,
    ));
    assert!(bound.mcus.is_empty(), "no processor is guessed");
    let why = reason(&bound);
    assert!(why.contains("UNRESOLVED"), "{why}");
    assert!(
        why.contains("does not say which chip") && why.contains("'Pico 2'"),
        "the sentence names the ambiguity and the fix: {why}"
    );
}

#[test]
fn a_pico_2_is_named_as_the_rp2350_not_bound_as_the_rp2040() {
    let bound = bind_one(comp(
        "U1",
        "Pico 2",
        "MCU_RaspberryPi_and_Boards:Pico",
        "Module:RaspberryPi_Pico_Common_THT",
        PICO_PINS,
    ));
    assert!(bound.mcus.is_empty());
    assert!(reason(&bound).contains("RP2350"), "{}", reason(&bound));
}

#[test]
fn a_nano_footprint_with_a_generic_value_is_not_assumed_to_be_an_atmega328p() {
    let bound = bind_one(comp(
        "A1",
        "Nano",
        "Connector:Conn_01x15",
        "Module:Arduino_Nano",
        NANO_PINS,
    ));
    assert!(bound.mcus.is_empty());
    assert!(
        reason(&bound).contains("Nano Every"),
        "names the other boards on the footprint: {}",
        reason(&bound)
    );
}

#[test]
fn an_arduino_pro_mini_without_a_pad_map_says_so() {
    let bound = bind_one(comp(
        "U1",
        "Arduino Pro Mini",
        "Custom:ProMini",
        "Module:Arduino_Pro_Mini",
        NANO_PINS,
    ));
    assert!(bound.mcus.is_empty());
    assert!(
        reason(&bound).contains("no header pad map"),
        "{}",
        reason(&bound)
    );
}

// ---- 3. clean boards stay silent ---------------------------------------------

#[test]
fn a_bare_rp2040_chip_gets_no_module_crystal() {
    let bound = bind_one(comp(
        "U1",
        "RP2040",
        "MCU_RaspberryPi:RP2040",
        "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm",
        &[("57", 1, "GND"), ("1", 2, "IOVDD"), ("20", 3, "XIN")],
    ));
    let mcu = bound
        .mcus
        .first()
        .unwrap_or_else(|| panic!("{}", reason(&bound)));
    assert_ne!(mcu.clock.kind, ClockKind::Module, "{}", mcu.clock.detail);
    assert!(
        !mcu.clock.external_present(),
        "no crystal is drawn: {}",
        mcu.clock.detail
    );
}

#[test]
fn a_bare_chip_footprint_is_never_taken_for_a_module() {
    for (value, footprint) in [
        ("ESP32-PICO-D4", "RF_Module:ESP32-PICO-D4"),
        ("10k", "Resistor_SMD:R_0603_1608Metric"),
        (
            "PinHeader",
            "Connector_PinHeader_2.54mm:PinHeader_2x20_P2.54mm_Vertical",
        ),
    ] {
        let bound = bind_one(comp(
            "J1",
            value,
            "Connector:Conn_02x20_Odd_Even",
            footprint,
            &[("1", 3, "1"), ("2", 1, "2")],
        ));
        let why = reason(&bound);
        assert!(
            !why.contains("does not say which chip") && !why.contains("no header pad map"),
            "{value}: {why}"
        );
    }
}

// ---- 4. the board runs: the Pico's crystal is in the model --------------------

fn pico_board_text(with_module: bool) -> String {
    // A Pico placed by value on a board that draws neither its crystal nor its
    // flash; `with_module == false` is the same board with a bare RP2040 and no
    // crystal, which must still stall.
    let pads = if with_module {
        r#"(pad 1 smd rect (at -3 0) (net 4 "UART_TX"))
    (pad 3 smd rect (at -3 1) (net 1 "GND"))
    (pad 36 smd rect (at -3 2) (net 2 "+3V3"))
    (pad 39 smd rect (at -3 3) (net 2 "+3V3"))
    (pad 30 smd rect (at -3 4) (net 5 "RUN"))"#
    } else {
        r#"(pad 1 smd rect (at -3 0) (net 2 "+3V3"))
    (pad 2 smd rect (at -3 1) (net 4 "UART_TX"))
    (pad 10 smd rect (at -3 2) (net 2 "+3V3"))
    (pad 22 smd rect (at -3 3) (net 2 "+3V3"))
    (pad 26 smd rect (at -3 4) (net 5 "RUN"))
    (pad 33 smd rect (at -3 5) (net 2 "+3V3"))
    (pad 42 smd rect (at -3 6) (net 2 "+3V3"))
    (pad 49 smd rect (at -3 7) (net 2 "+3V3"))
    (pad 57 smd rect (at 3 0) (net 1 "GND"))"#
    };
    let (footprint, value) = if with_module {
        ("Module:RaspberryPi_Pico_Common_THT", "Pico")
    } else {
        (
            "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm",
            "RP2040",
        )
    };
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "LED")
  (net 4 "UART_TX")
  (net 5 "RUN")

  (module {footprint} (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    {pads}
  )
  (module Resistor:R (layer F.Cu)
    (at 110 105)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "UART_TX"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 110)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "RUN"))
    (pad 2 thru_hole circle (at 2 0) (net 2 "+3V3"))
  )
)"#
    )
}

fn run_pico(with_module: bool) -> Option<hauksbee_cosim::execution::ExecutionEvidence> {
    hauksbee_testkit::skip_if!(!crate::support::renode_runnable() => None, "Renode not installed");
    let fw = crate::support::testdata("firmware/rp2040_blink_uart/blink_uart.elf")?;
    let mut engine = HauksbeeEngine::from_board_file(
        &pico_board_text(with_module),
        Some(&fw),
        "/boards/pico_module.kicad_pcb",
    )
    .expect("build the Pico engine");
    engine.scheduler_mut().chunk_s = 1e-3;
    // Start-up time is virtual, but how much of it fits in a step varies with
    // load on the machine: run until the firmware has reached main, not for a
    // fixed count that a busy box can miss.
    for i in 0..250 {
        let _ = engine.step(1e-3);
        if i % 25 == 24 {
            engine.scheduler_mut().refresh_execution_evidence();
            if engine
                .scheduler()
                .execution_evidence()
                .first()
                .is_some_and(|e| e.executed)
            {
                break;
            }
        }
    }
    engine.scheduler_mut().refresh_execution_evidence();
    engine.scheduler().execution_evidence().first().cloned()
}

#[test]
fn a_pico_board_runs_its_firmware_without_the_crystal_drawn() {
    let Some(ev) = run_pico(true) else { return };
    assert!(
        !ev.stalled_on_external_clock,
        "the Pico's own crystal is in the model: {ev:#?}"
    );
    assert!(ev.executed, "the firmware runs: {ev:#?}");
    assert!(ev.uart_bytes > 0, "UART0 spoke: {ev:#?}");
    assert!(
        ev.clock_source.contains("ABM8-272-T3"),
        "the evidence names the module crystal and its datasheet: {}",
        ev.clock_source
    );
}

#[test]
fn a_bare_rp2040_with_no_crystal_still_stalls() {
    let Some(ev) = run_pico(false) else { return };
    assert!(
        ev.stalled_on_external_clock,
        "a chip placed on its own has no module crystal: {ev:#?}"
    );
    assert!(!ev.executed, "{ev:#?}");
}
