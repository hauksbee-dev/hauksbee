//! Over-current is judged at a logic output's strong-drive corner.
//!
//! A logic card's `ro` comes from the datasheet's minimum VOH under load: the
//! weak corner, right for whether a level is met. A typical part is about
//! three times stronger (74HCT1G125: 93 ohm at VOH min, 30 ohm at VOH typ),
//! so an LED hung off the output with no series resistance sits just inside
//! every rating at the weak corner and well past them at the typical one.

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};
use hauksbee_models::ModelLibrary;

/// U1 74HCT1G125 (pin 1 OE#, 2 A, 3 GND, 4 Y, 5 VCC) with OE# grounded and A
/// tied high, so Y drives high; Y -> R1 (`ohms`) -> a red LED to ground.
fn indicator(ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "DRIVE")
  (net 4 "ANODE")
  (module Package_TO_SOT_SMD:SOT-353_SC-70-5 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HCT1G125GW (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 2 "+5V"))
    (pad 3 smd rect (at 0 2) (net 1 "GND"))
    (pad 4 smd rect (at 1 2) (net 3 "DRIVE"))
    (pad 5 smd rect (at 1 0) (net 2 "+5V"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {ohms} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "DRIVE"))
    (pad 2 smd rect (at 1 0) (net 4 "ANODE"))
  )
  (module LED_SMD:LED_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value RED_LED (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 1 0) (net 4 "ANODE"))
  )
)
"#
    )
}

fn overstress(ohms: &str) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_auto(&indicator(ohms)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    lint.of_check(LintCheck::DcOverstress).cloned().collect()
}

/// 0 ohm: about 24 mA at the weak corner (inside the LED's 25 mA and the
/// pin's 35 mA), about 60 mA at the strong one, past both.
#[test]
fn an_led_straight_off_a_logic_output_overstresses_both_at_the_strong_corner() {
    let found = overstress("0");
    for part in ["U1", "D1"] {
        let f = found
            .iter()
            .find(|f| f.refs == [part])
            .unwrap_or_else(|| panic!("{part} is over its rating: {found:#?}"));
        assert_eq!(f.severity, Severity::High, "{}", f.message);
        assert!(f.message.contains("strong-drive corner"), "{}", f.message);
    }
    let pin = found.iter().find(|f| f.refs == ["U1"]).unwrap();
    assert!(pin.message.contains("35 mA"), "{}", pin.message);
}

/// Control: through 1k the output delivers a few mA at either corner.
#[test]
fn the_same_led_through_1k_is_quiet_at_both_corners() {
    let found = overstress("1k");
    assert!(found.is_empty(), "{found:#?}");
}
