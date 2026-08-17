//! Regression: a GPIO the firmware hands back (DDR output -> input) must have
//! its Thevenin driver DISABLED, not left clamped at the stale driven level.
//!
//! Enabling a pin's driver on its first firmware edge (and from
//! `pins_configured_output`) without ever releasing it leaves an open-drain bus
//! hand-off latched at the last driven voltage forever; the
//! exact "latched bus" failure the AVR backend's DDR hook comments say was
//! fixed on the observation side.
//!
//! The probe firmware (testdata/firmware/gpio_release) drives PC2 (Nano A2)
//! HIGH for ~28 ms, then sets DDRC back to input while leaving PORTC2 = 1,
//! deliberately emitting NO PORT edge, so the release is visible only through
//! the direction report. With a 10k pull-down on the net, the release must
//! let the net fall to ~0 V; a latched driver would hold it at 5 V.

// The board binds `simavr:atmega328p`, whose in-process core needs the
// GPL-gated `avr` feature (excluded from the GPL-free renode/qemu build).
#![cfg(feature = "avr")]

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;

/// Nano with A2 (pad 21, PC2) on net "BUS", pulled down through 10k.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "BUS")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4 thru_hole circle (at 0 4) (size 1 1) (net 1 "GND"))
    (pad 27 thru_hole circle (at 0 27) (size 1 1) (net 2 "+5V"))
    (pad 21 thru_hole circle (at 0 21) (size 1 1) (net 3 "BUS"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (size 1 1) (net 3 "BUS"))
    (pad 2 thru_hole circle (at 0 2) (size 1 1) (net 1 "GND"))
  )
)
"#;

fn firmware() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/gpio_release/release.hex")
}

#[test]
fn released_gpio_driver_lets_the_net_go() {
    let fw = firmware();
    assert!(
        fw.exists(),
        "build the fixture first: make -C testdata/firmware/gpio_release ({fw:?})"
    );

    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");

    // 1 ms frames across the firmware's drive-then-release timeline.
    let frame_dt = 1e-3_f64;
    let mut driven_v: f64 = 0.0; // max BUS voltage while the pin is driven
    let mut final_v: f64 = f64::NAN; // BUS voltage well after the release
    for i in 0..80 {
        let frame = engine.step(frame_dt);
        let v = frame.net_voltages.get("BUS").copied().unwrap_or(0.0);
        if i < 20 {
            driven_v = driven_v.max(v);
        }
        final_v = v;
    }

    // Driven phase: PC2 output-high through the 10k pull-down reads ~5 V.
    assert!(
        driven_v > 3.0,
        "while configured as an output the BUS net must be driven high, got {driven_v:.2} V"
    );
    // Released phase: DDR went back to input (no PORT edge!), so the driver
    // must be disabled and the pull-down must win. A latched driver holds 5 V.
    assert!(
        final_v < 0.7,
        "after the firmware releases the pin (DDR -> input) the BUS net must \
         fall through its pull-down, got {final_v:.2} V; the Thevenin driver \
         was left enabled (latched bus)"
    );
}
