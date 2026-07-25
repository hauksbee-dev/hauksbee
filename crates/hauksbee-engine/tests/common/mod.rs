//! Shared test helpers and fixtures.

use std::path::PathBuf;

/// Path to the repo `testdata/` directory, resolved relative to the crate.
pub fn testdata(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(rel)
}

/// The demo blink/ADC firmware shipped in testdata.
pub fn demo_firmware() -> PathBuf {
    testdata("firmware/demo/demo.hex")
}

/// The Tarski InputSystem netlist, with its Arduino Nano fitted.
///
/// A101 carries KiCad's DNP flag because the Nano is a module bought
/// separately and plugged into headers, so it is not on the assembly BOM. It
/// is very much on the working board, and without the fit the binder correctly
/// drops the only processor and every firmware test becomes vacuous. Loading
/// through one helper keeps that decision in a single place.
pub fn tarski_board() -> hauksbee_extract::ExtractedBoard {
    let text = std::fs::read_to_string(testdata("tarski_inputsystem.net"))
        .expect("tarski netlist present");
    let mut board = hauksbee_extract::ExtractedBoard::from_auto(&text).expect("parse netlist");
    let fitted = board.fit(&["A101".to_string()]).expect("A101 is on the board");
    assert_eq!(fitted, 1, "A101 should have been DNP and is now fitted");
    board
}

/// A minimal synthetic `.kicad_pcb` (KiCad-5 `module` style, bare atoms):
///
/// - U1: ATmega328P (TQFP-32 pad map) — VCC/AVCC on +5V, GND pads on GND,
///   PB5 (pad 19) on net "D13", ADC0 (pad 23) on net "ADC0".
/// - R1 330Ω from D13 to LED_A.
/// - D1 RED_LED from LED_A (anode) to GND (cathode).
/// - R2 10k from +5V to ADC0, R3 10k from ADC0 to GND (a 2.5 V divider).
///
/// Net ids: 1 GND, 2 +5V, 3 D13, 4 LED_A, 5 ADC0.
pub const SYNTH_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "D13")
  (net 4 "LED_A")
  (net 5 "ADC0")

  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7  smd rect (at -3 0) (net 2 "+5V"))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 19 smd rect (at 3 0) (net 3 "D13"))
    (pad 20 smd rect (at 3 1) (net 2 "+5V"))
    (pad 22 smd rect (at 3 2) (net 1 "GND"))
    (pad 23 smd rect (at 3 3) (net 5 "ADC0"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 330 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "D13"))
    (pad 2 thru_hole circle (at 2 0) (net 4 "LED_A"))
  )

  (module LED_THT:LED_D5.0mm (layer F.Cu)
    (at 112 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value RED_LED (at 0 2) (layer F.Fab))
    (pad A thru_hole circle (at 0 0) (net 4 "LED_A"))
    (pad K thru_hole circle (at 2 0) (net 1 "GND"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 105 110)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 5 "ADC0"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 105 115)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "ADC0"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
