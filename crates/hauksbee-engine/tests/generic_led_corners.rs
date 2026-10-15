//! An LED the board names only as an LED binds the ASSUMED forward-voltage
//! corner set, and the DC stress pass judges its current at both corners.
//!
//! Before, a two-pad `Device:LED` valued "LED" off a stock LED footprint, or a
//! CHIPLED under a name-style designator, stayed OPEN: its series resistor and
//! driver carried no current and every rating claim on them read clean.

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};
use hauksbee_models::ModelLibrary;

/// `rail` -> R1 (`ohms`) -> D1 ("LED", `LED_SMD` 0603: pad 1 cathode, pad 2
/// anode) -> GND.
fn indicator(rail: &str, ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "{rail}")
  (net 3 "ANODE")
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {ohms} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "{rail}"))
    (pad 2 smd rect (at 1 0) (net 3 "ANODE"))
  )
  (module LED_SMD:LED_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value LED (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 1 0) (net 3 "ANODE"))
  )
)
"#
    )
}

fn overstress(rail: &str, ohms: &str) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_auto(&indicator(rail, ohms)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    lint.of_check(LintCheck::DcOverstress).cloned().collect()
}

/// The stated condition on D1: its 25 mA is the ASSUMED generic card's, not
/// the fitted LED's, so passing it is never a failing finding.
fn stated_on_d1(found: &[LintFinding]) -> &LintFinding {
    let f = found
        .iter()
        .find(|f| f.refs == ["D1"])
        .unwrap_or_else(|| panic!("the LED is overdriven: {found:#?}"));
    assert_eq!(f.severity, Severity::Low, "{}", f.message);
    assert!(
        hauksbee_models::rating_basis::is_condition(&f.message)
            && f.message.contains("judged against a generic")
            && f.message.contains("give D1 its own model"),
        "{}",
        f.message
    );
    f
}

/// 5 V through 10 ohm: about 60 mA even at the 3.5 V blue corner, over the
/// generic card's ASSUMED 25 mA at both corners. That rating is no datasheet's,
/// so it is a stated condition, never a failing finding.
#[test]
fn an_led_overdriven_at_every_corner_is_a_stated_condition() {
    let found = overstress("+5V", "10");
    stated_on_d1(&found);
    assert!(
        found.iter().all(|f| f.severity == Severity::Low),
        "{found:#?}"
    );
}

/// 3.3 V through 33 ohm: about 35 mA into a 1.9 V red die, nothing into a
/// 3.5 V blue one. Only the low-Vf corner passes its ASSUMED rating, and the
/// condition names that corner's card.
#[test]
fn an_led_overdriven_only_at_the_low_vf_corner_states_that_corner() {
    let found = overstress("+3V3", "33");
    let f = stated_on_d1(&found);
    assert!(
        f.message.contains("led_generic_corner_low_vf"),
        "{}",
        f.message
    );
}

/// Control: 5 V through 1k is a few mA at either corner.
#[test]
fn a_1k_indicator_is_quiet_at_both_corners() {
    let found = overstress("+5V", "1k");
    assert!(found.is_empty(), "{found:#?}");
}
