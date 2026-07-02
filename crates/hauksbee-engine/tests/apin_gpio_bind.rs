//! FIX 2 proof: a dual-purpose analog pin (PC0..PC5) used as a DIGITAL CONTROL
//! line binds as a GPIO output driver, not an ADC probe, while a genuine analog
//! pin still binds as ADC.
//!
//! On the ATmega328P A0..A5 are PC0..PC5, dual-purpose ADC/GPIO. The Tarski
//! board drives A2 = OE'_S and A3 = SRCLR'_S as digital control for the 74HC595
//! chain. The old binder claimed any "a0".."a7" role as an ADC channel before
//! checking GPIO, so those pins got no output driver and floated, holding the
//! 595 chain cleared. The fix resolves by usage: a net carrying a 595 control
//! input (here SRCLR_n / OE_n) pulls the A-pin onto GPIO.

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
fn apin_digital_control_binds_gpio_analog_stays_adc() {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    assert_eq!(bound.mcus.len(), 1, "one ATmega328P");
    let mcu = &bound.mcus[0];

    // A2 = PC2 (OE'_S) and A3 = PC3 (SRCLR'_S) are driven as digital control,
    // so they must bind as GPIO output drivers, NOT as ADC channels.
    assert!(
        mcu.gpio_drivers.contains_key(&('C', 2)),
        "PC2 (A2, OE_S) must bind as GPIO; drivers: {:?}",
        mcu.gpio_drivers.keys().collect::<Vec<_>>()
    );
    assert!(
        mcu.gpio_drivers.contains_key(&('C', 3)),
        "PC3 (A3, SRCLR_S) must bind as GPIO; drivers: {:?}",
        mcu.gpio_drivers.keys().collect::<Vec<_>>()
    );
    // And they must NOT be claimed as ADC channels 2/3.
    assert!(
        !mcu.adc_nets.contains_key(&2),
        "PC2 must NOT be an ADC channel (it is digital control)"
    );
    assert!(
        !mcu.adc_nets.contains_key(&3),
        "PC3 must NOT be an ADC channel (it is digital control)"
    );

    // A0 = PC0 reads a genuine 2.5 V divider: it must stay an ADC input with no
    // output driver (regression guard for boards that legitimately use A-pins
    // as ADC).
    assert!(
        mcu.adc_nets.contains_key(&0),
        "PC0 (A0, analog divider) must stay an ADC channel; adc: {:?}",
        mcu.adc_nets.keys().collect::<Vec<_>>()
    );
    assert!(
        !mcu.gpio_drivers.contains_key(&('C', 0)),
        "PC0 reads analog: it must NOT get a GPIO output driver"
    );

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
