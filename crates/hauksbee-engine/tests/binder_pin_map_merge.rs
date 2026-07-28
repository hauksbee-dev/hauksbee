//! The MCU pad-role merge: a DB model's pin map names pads for ONE package's
//! numbering (the ESP32-S3 entry ships the WROOM-1 module's four strap pads),
//! and the old binder treated any non-empty model map as exhaustive, so on a
//! bare-chip footprint every OTHER pad's own pinfunction was discarded. On the
//! Watchy v3 QFN-56 that left all display pins (RES/DC/CS, SCK/MOSI, SDA/SCL,
//! each named "GPIOnn/..." in the board file itself) with no GPIO driver at
//! all: firmware could never drive them, and the live sim presented their
//! static pull-up/floating levels as measurements.
//!
//! The fix merges per pad: pinfunction-derived roles fill every pad the model
//! map does not name, and the model map still wins on the pads it does (its
//! curated roles carry semantic suffixes the plain derivation would weaken).

use hauksbee_engine::binder::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// A minimal bare-chip ESP32-S3 in Watchy-v3 style: pinfunction-named GPIO
/// pads the model's WROOM-1 strap map does not cover (pads 16 and 36), plus
/// pad 27, which the model map names ("27" = gpio0).
const BOARD: &str = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "")
  (net 1 "GND")
  (net 2 "SCL")
  (net 3 "MOSI")
  (net 4 "BOOT")

  (footprint "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm" (layer "F.Cu")
    (at 100 100)
    (fp_text reference "U4" (at 0 0) (layer "F.SilkS"))
    (fp_text value "ESP32-S3" (at 0 2) (layer "F.Fab"))
    (pad "16" smd rect (at -3 0) (size 1 1) (layers "F.Cu")
      (net 2 "SCL") (pinfunction "GPIO11/ADC2_CH0"))
    (pad "36" smd rect (at -3 1) (size 1 1) (layers "F.Cu")
      (net 3 "MOSI") (pinfunction "SPICLK_N/GPIO48"))
    (pad "27" smd rect (at -3 2) (size 1 1) (layers "F.Cu")
      (net 4 "BOOT") (pinfunction "GPIO0"))
    (pad "57" smd rect (at 3 0) (size 1 1) (layers "F.Cu")
      (net 1 "GND") (pinfunction "GND"))
  )

  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu")
    (at 110 100)
    (fp_text reference "R1" (at 0 0) (layer "F.SilkS"))
    (fp_text value "10k" (at 0 2) (layer "F.Fab"))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SCL"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 1 "GND"))
  )
)"#;

#[test]
fn model_pin_map_merges_with_footprint_pinfunctions() {
    let board = ExtractedBoard::from_auto(BOARD).expect("board parses");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let mcu = bound
        .mcus
        .iter()
        .find(|m| m.reference == "U4")
        .expect("the ESP32-S3 binds as an MCU");

    // Pads the model map does NOT name: the footprint's own pinfunctions must
    // produce GPIO drivers (GPIO11 = bank '0' bit 11, GPIO48 = bank '1' bit 16).
    assert!(
        mcu.gpio_drivers.contains_key(&('0', 11)),
        "pad 16's GPIO11/ADC2_CH0 pinfunction must stamp a driver; got {:?}",
        mcu.gpio_drivers.keys().collect::<Vec<_>>()
    );
    assert!(
        mcu.gpio_drivers.contains_key(&('1', 16)),
        "pad 36's SPICLK_N/GPIO48 pinfunction must stamp a driver (bank '1' bit 16)"
    );

    // A pad the model map DOES name keeps the curated role: pad 27 = gpio0.
    assert_eq!(
        mcu.pad_roles.get("27").map(String::as_str),
        Some("gpio0"),
        "the model's strap-pad role must still win on the pads it names"
    );
    assert!(mcu.gpio_drivers.contains_key(&('0', 0)));
}
