//! Dynamic promotion of analog-capable MCU pins (05-cosim-fidelity §4.1,
//! closes TARSKI_RESULTS §5.2).
//!
//! On the Arduino Nano the analog header A0..A5 are dual-purpose: ADC channels
//! that are also ordinary GPIO on PC0..PC5. The Tarski board drives A2 = OE'_S
//! and A3 = SRCLR'_S as DIGITAL control for the 74HC595 chain. The old binder
//! claimed any "a0".."a7" role as an ADC channel and stamped no output driver,
//! so those nets floated; a floating-low SRCLR'_S would hold the whole chain
//! cleared.
//!
//! The fix binds an analog-capable pin BOTH ways: it keeps the ADC channel
//! mapping AND stamps a tri-stated GPIO driver. The driver starts disabled (a
//! near-open leg, electrically inert), so an undriven pin stays a pure ADC
//! input; the scheduler enables it on the pin's first firmware drive, promoting
//! it to a GPIO output. This file proves all three halves: the dual bind, the
//! promotion-drives-the-net gate, and the ADC-only regression.

#![cfg(feature = "avr")]

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// Arduino Nano (module, header-level pin map) plus a passive load on three
/// analog-header pins:
///   - pad 19 (A0) -> net "AIN"     : 10k/10k divider from +5V = 2.5 V (pure ADC)
///   - pad 21 (A2) -> net "OE_S"    : 10k pulldown             (driven as GPIO)
///   - pad 22 (A3) -> net "SRCLR_S" : 10k pulldown             (driven as GPIO)
///   - pad 27 (5V) -> +5V rail, pad 4 (GND) -> ground
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "AIN")
  (net 4 "OE_S")
  (net 5 "SRCLR_S")

  (module Module:Arduino_Nano (layer F.Cu)
    (at 100 100)
    (fp_text reference A1 (at 0 0) (layer F.SilkS))
    (fp_text value Arduino_Nano (at 0 2) (layer F.Fab))
    (pad 4  smd rect (at -3 0) (net 1 "GND"))
    (pad 27 smd rect (at -3 1) (net 2 "+5V"))
    (pad 19 smd rect (at 3 0) (net 3 "AIN"))
    (pad 21 smd rect (at 3 1) (net 4 "OE_S"))
    (pad 22 smd rect (at 3 2) (net 5 "SRCLR_S"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 3 "AIN"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 112 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "AIN"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 114 100)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "OE_S"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 116 100)
    (fp_text reference R4 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "SRCLR_S"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)"#;

/// (a) Unit-level: a module MCU bind maps A2/A3 to BOTH an ADC channel and a
/// DISABLED GPIO driver. This is the structural dual-bind the dynamic-promotion
/// design requires.
#[test]
fn module_apin_binds_adc_and_disabled_gpio_driver() {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    assert_eq!(bound.mcus.len(), 1, "one Arduino Nano");
    let mcu = &bound.mcus[0];
    assert!(mcu.module, "the Nano binds as a module (header pin map)");

    // A2 = PC2 and A3 = PC3: each keeps its ADC channel mapping...
    assert!(
        mcu.adc_nets.contains_key(&2),
        "A2 keeps ADC channel 2; adc: {:?}",
        mcu.adc_nets.keys().collect::<Vec<_>>()
    );
    assert!(
        mcu.adc_nets.contains_key(&3),
        "A3 keeps ADC channel 3; adc: {:?}",
        mcu.adc_nets.keys().collect::<Vec<_>>()
    );
    // ...AND gets a GPIO driver stamped, initially tri-stated (disabled).
    for (port, bit, label) in [('C', 2u8, "A2/OE_S"), ('C', 3u8, "A3/SRCLR_S")] {
        let drv = mcu.gpio_drivers.get(&(port, bit)).unwrap_or_else(|| {
            panic!(
                "{label} must bind a GPIO driver on P{port}{bit}; drivers: {:?}",
                mcu.gpio_drivers.keys().collect::<Vec<_>>()
            )
        });
        assert!(
            !drv.enabled,
            "{label} driver must start DISABLED (tri-stated) so the undriven pin \
             is a pure ADC input with zero electrical effect"
        );
    }

    // A0 is a genuine analog input here, but the design binds every A0..A5 pin
    // both ways: it too carries an ADC channel and a disabled driver (it is only
    // ever promoted if the firmware drives it, which this board never does).
    assert!(mcu.adc_nets.contains_key(&0), "A0 keeps ADC channel 0");
    let a0 = mcu
        .gpio_drivers
        .get(&('C', 0))
        .expect("A0 carries a (disabled) GPIO driver too");
    assert!(!a0.enabled, "A0 driver stays disabled: the pin is never driven");
}

/// (b) The gate test (08-validation-and-test-campaign §2). Drive a synthetic
/// board's A-pin through the scheduler's edge-application path and assert the
/// net reaches the driven voltage in the next solve.
///
/// The scheduler promotes a pin on its first firmware edge by calling exactly
/// `driver.set_enabled(circuit, true)` then `driver.set_volts(circuit, level)`
/// (scheduler.rs, the pin_edges loop). We invoke that path directly on the bound
/// board's A2 driver rather than booting real firmware, then let the real
/// scheduler solve one step and read the net.
#[test]
fn binder_analog_pin_promoted_when_driven() {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let mut bound = bind_board(&board, &lib);

    let oe_s = bound.node("OE_S").expect("OE_S net exists");

    // Firmware-side edge application: promote A2 (PC2) by enabling its driver and
    // setting it high, exactly as the scheduler does on the pin's first edge.
    {
        let drv = bound.mcus[0]
            .gpio_drivers
            .get_mut(&('C', 2))
            .expect("A2 GPIO driver present");
        assert!(!drv.enabled, "A2 starts tri-stated before the drive");
        drv.set_enabled(&mut bound.circuit, true);
        drv.set_volts(&mut bound.circuit, 5.0);
    }

    // Run the real scheduler solve (no firmware: the idle AVR core produces no
    // edges of its own, so the only drive on OE_S is the promotion above).
    let mut engine = HauksbeeEngine::from_bound(bound, None, "/boards/promote.kicad_pcb")
        .expect("build engine from bound board");
    engine.scheduler_mut().step(1e-3);

    let v = engine
        .scheduler()
        .net_voltage("OE_S")
        .expect("OE_S solved voltage");
    // Thevenin 50 Ω from 5 V against the 10 k pulldown: ~4.975 V. Before the fix
    // there was no driver at all and OE_S floated at the pulldown's 0 V.
    assert!(
        v > 4.5,
        "promoted A2 must DRIVE OE_S to ~5 V, got {v:.3} V (net {oe_s:?})"
    );
}

/// (c) Regression: an A-pin used as ADC only (never driven) still presents its
/// true analog voltage, and its driver stays disabled so the scheduler injects
/// that voltage rather than skipping it. The disabled driver being electrically
/// inert (not clamping the net toward 0 V) is the load-bearing property.
#[test]
fn adc_only_apin_receives_injected_analog_volts() {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // A0's driver is disabled: `set_analog_in` is NOT skipped for it (the skip
    // fires only for a pin whose driver is ENABLED, i.e. promoted to output).
    assert!(
        !bound.mcus[0]
            .gpio_drivers
            .get(&('C', 0))
            .expect("A0 driver present")
            .enabled,
        "A0 driver disabled -> ADC injection proceeds for this pin"
    );

    let mut engine = HauksbeeEngine::from_bound(bound, None, "/boards/adc_only.kicad_pcb")
        .expect("build engine from bound board");
    engine.scheduler_mut().step(1e-3);

    // The 10k/10k divider from +5V settles at 2.5 V. If the disabled A0 driver
    // were not inert it would drag AIN toward its 0 V source; asserting 2.5 V
    // proves inertness AND that the value the scheduler injects into ADC0 is the
    // genuine analog reading.
    let v = engine
        .scheduler()
        .net_voltage("AIN")
        .expect("AIN solved voltage");
    assert!(
        (v - 2.5).abs() < 0.3,
        "ADC-only A0 net must read the true 2.5 V divider (disabled driver \
         inert), got {v:.3} V"
    );
}
