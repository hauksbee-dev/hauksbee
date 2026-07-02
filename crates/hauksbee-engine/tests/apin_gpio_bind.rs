//! Dual bind of analog-capable pins on a BARE ATmega328P (roles "pc2_adc2"
//! style, complementing the module-level "a2" coverage in
//! `binder_gpio_promotion.rs`).
//!
//! On the ATmega328P A0..A5 are PC0..PC5, dual-purpose ADC/GPIO. The Tarski
//! board drives A2 = OE'_S and A3 = SRCLR'_S as digital control for the 74HC595
//! chain. The original binder claimed any analog-capable role as an ADC channel
//! before checking GPIO, so those pins got no output driver and floated,
//! holding the 595 chain cleared. Under dynamic promotion (05-cosim-fidelity
//! §4.1) every A-pin binds BOTH ways: the ADC channel mapping stays AND a
//! tri-stated (disabled, electrically inert) GPIO driver is stamped; the
//! scheduler enables the driver on the pin's first firmware drive.

#![cfg(feature = "avr")]

use hauksbee_engine::binder::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// Bare ATmega328P (TQFP-32 pad map) plus one 74HC595:
///   - PC2 (pad 25) -> net "OE_S"   -> 595 ~OE  (pad 13)   : digital control
///   - PC3 (pad 26) -> net "SRCLR_S"-> 595 ~MR  (pad 10)   : digital control
///   - PC0 (pad 23) -> net "AIN0"   -> 10k/10k divider      : genuine analog
///   - PB5 (pad 19) -> net "SCLK"   -> 595 SHCP (pad 11)    : ordinary GPIO
///   - PB3 (pad 17) -> net "MOSI"   -> 595 DS   (pad 14)    : ordinary GPIO (SER)
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "OE_S")
  (net 4 "SRCLR_S")
  (net 5 "AIN0")
  (net 6 "SCLK")
  (net 7 "MOSI")
  (net 8 "QA")

  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7  smd rect (at -3 0) (net 2 "+5V"))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 17 smd rect (at 3 0) (net 7 "MOSI"))
    (pad 19 smd rect (at 3 1) (net 6 "SCLK"))
    (pad 23 smd rect (at 3 2) (net 5 "AIN0"))
    (pad 25 smd rect (at 3 3) (net 3 "OE_S"))
    (pad 26 smd rect (at 3 4) (net 4 "SRCLR_S"))
  )

  (module Package_SO:SOIC-16_3.9x9.9mm_P1.27mm (layer F.Cu)
    (at 120 100)
    (fp_text reference IC1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC595 (at 0 2) (layer F.Fab))
    (pad 8  smd rect (at 0 0) (net 1 "GND"))
    (pad 10 smd rect (at 0 1) (net 4 "SRCLR_S"))
    (pad 11 smd rect (at 0 2) (net 6 "SCLK"))
    (pad 13 smd rect (at 0 3) (net 3 "OE_S"))
    (pad 14 smd rect (at 0 4) (net 7 "MOSI"))
    (pad 15 smd rect (at 0 5) (net 8 "QA"))
    (pad 16 smd rect (at 0 6) (net 2 "+5V"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 5 "AIN0"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 112 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "AIN0"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)"#;

#[test]
fn bare_apin_binds_adc_and_disabled_gpio_driver() {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    assert_eq!(bound.mcus.len(), 1, "one ATmega328P");
    let mcu = &bound.mcus[0];

    // Every A-pin binds BOTH ways: PC2 (OE'_S), PC3 (SRCLR'_S) and PC0 (the
    // genuine analog divider) each keep their ADC channel AND carry a GPIO
    // driver that starts tri-stated. Which way the pin is USED is decided at
    // run time by the firmware (first drive promotes to GPIO).
    for (ch, port, bit, label) in [
        (0u8, 'C', 0u8, "PC0/AIN0"),
        (2, 'C', 2, "PC2/OE_S"),
        (3, 'C', 3, "PC3/SRCLR_S"),
    ] {
        assert!(
            mcu.adc_nets.contains_key(&ch),
            "{label} keeps ADC channel {ch}; adc: {:?}",
            mcu.adc_nets.keys().collect::<Vec<_>>()
        );
        let drv = mcu.gpio_drivers.get(&(port, bit)).unwrap_or_else(|| {
            panic!(
                "{label} must carry a GPIO driver on P{port}{bit}; drivers: {:?}",
                mcu.gpio_drivers.keys().collect::<Vec<_>>()
            )
        });
        assert!(
            !drv.enabled,
            "{label} driver must start DISABLED (tri-stated): an undriven pin \
             stays a pure ADC input with zero electrical effect"
        );
    }

    // The ordinary GPIO control pins are unaffected.
    assert!(
        mcu.gpio_drivers.contains_key(&('B', 5)),
        "PB5 (SCLK) GPIO present"
    );
    assert!(
        mcu.gpio_drivers.contains_key(&('B', 3)),
        "PB3 (MOSI/SER) GPIO present"
    );
}
