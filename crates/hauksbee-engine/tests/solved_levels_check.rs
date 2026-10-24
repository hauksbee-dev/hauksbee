//! The static `check` judges supply limits on the levels its DC solve gave.
//!
//! A TXB0101's VCCA pin (1.2 to 3.6 V recommended, 4.6 V absolute maximum) sits
//! on a net no name gives a level to: the tap of a 10k/10k divider from the
//! named +12V rail. Rail-name tables know nothing of that net and abstain; the
//! solved operating point puts it at 6 V, so the pin is called out of range. The
//! same pin on a rail whose NAME states the level keeps the name's judgement
//! (the solve only reads the name back).

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding};
use hauksbee_models::ModelLibrary;

/// U1 TXB0101DCK with VCCA on `vcca_net`; VCCB on +3V3. R1 joins `+12V` to
/// `MID`, R2 joins `MID` to ground.
fn board(vcca_net: &str) -> String {
    let vcca = if vcca_net == "MID" { 3 } else { 5 };
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+12V")
  (net 3 "MID")
  (net 4 "+3V3")
  (net 5 "+5V")
  (net 6 "SIG_A")
  (net 7 "SIG_B")
  (module Package_TO_SOT_SMD:SOT-363_SC-70-6 (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value TXB0101DCK (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {vcca} "{vcca_net}"))
    (pad 2 smd rect (at 0 1) (net 1 "GND"))
    (pad 3 smd rect (at 0 2) (net 6 "SIG_A"))
    (pad 4 smd rect (at 0 3) (net 7 "SIG_B"))
    (pad 5 smd rect (at 0 4) (net 4 "+3V3"))
    (pad 6 smd rect (at 0 5) (net 4 "+3V3"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+12V"))
    (pad 2 smd rect (at 1 0) (net 3 "MID"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "MID"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
  )
)
"#
    )
}

fn envelope(vcca_net: &str) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_auto(&board(vcca_net)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    lint.of_check(LintCheck::OperatingEnvelope)
        .filter(|f| f.refs == ["U1"])
        .cloned()
        .collect()
}

#[test]
fn a_divider_tap_no_name_describes_is_judged_on_its_solved_level() {
    let found = envelope("MID");
    let f = found
        .iter()
        .find(|f| f.nets.iter().any(|n| n == "MID") && f.message.contains("vcca"))
        .unwrap_or_else(|| panic!("VCCA at the 6 V divider tap is out of range: {found:#?}"));
    assert!(
        f.message.contains("6 V"),
        "the message names the solved level: {}",
        f.message
    );
    assert!(
        !f.message.contains("abstained"),
        "a solved net is judged, not abstained on: {}",
        f.message
    );
}

#[test]
fn a_rail_whose_name_states_the_level_keeps_the_names_judgement() {
    let found = envelope("+5V");
    let f = found
        .iter()
        .find(|f| f.nets.iter().any(|n| n == "+5V") && f.message.contains("vcca"))
        .unwrap_or_else(|| panic!("VCCA on +5V is out of range: {found:#?}"));
    // The solve only reads the name's 5 V back: the finding keeps saying the
    // level is the name's, with its tolerance band, not a measured 5 V.
    assert!(f.message.contains("±5 %"), "{}", f.message);
}
