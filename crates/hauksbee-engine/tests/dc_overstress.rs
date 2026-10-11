//! Ratings judged at the DC operating point (gap G8 from the large internal test board).
//!
//! A logic output driving an NPN base through 47 ohm pushes far more current
//! out of the gate's output than its 25 mA rating while the junction clamps
//! the node near 0.8 V. The static suite must say so, naming the part and
//! the current; the same stage with a 10k base resistor is legal and quiet.

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};
use hauksbee_models::ModelLibrary;

/// U1 74HC04 with input 1 grounded (output 1 high) driving Q1 BC847's base
/// through `R1`; Q1's emitter is grounded and its collector pulled to +5V by
/// 1k.
fn stage(base_ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "DRIVE")
  (net 4 "BASE")
  (net 5 "COLL")
  (module Package_SO:SOIC-14_3.9x8.7mm_P1.27mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC04 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 3 "DRIVE"))
    (pad 7 smd rect (at 0 2) (net 1 "GND"))
    (pad 14 smd rect (at 0 3) (net 2 "+5V"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {base_ohms} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "DRIVE"))
    (pad 2 smd rect (at 1 0) (net 4 "BASE"))
  )
  (module Package_TO_SOT_SMD:SOT-23 (layer F.Cu)
    (at 120 100)
    (fp_text reference Q1 (at 0 0) (layer F.SilkS))
    (fp_text value BC847 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "BASE"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 5 "COLL"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 130 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+5V"))
    (pad 2 smd rect (at 1 0) (net 5 "COLL"))
  )
)
"#
    )
}

fn overstress(base_ohms: &str) -> Vec<hauksbee_extract::LintFinding> {
    let board = ExtractedBoard::from_auto(&stage(base_ohms)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    lint.of_check(LintCheck::DcOverstress).cloned().collect()
}

#[test]
fn a_logic_output_into_a_base_through_47_ohm_is_overstressed() {
    let found = overstress("47");
    let f = found
        .iter()
        .find(|f| f.refs == ["U1"])
        .unwrap_or_else(|| panic!("the 74HC04 output is driven past 25 mA: {found:#?}"));
    assert_eq!(f.severity, Severity::High);
    // The output drives (VOH - VBE) / (47 + its own resistance + the BC847's
    // base resistance, crowded from RB 150 toward RBM by IRB to about 18 ohm
    // at this current): a few tens of mA, above the 25 mA pin rating.
    let ma: f64 = f
        .message
        .split("passes ")
        .nth(1)
        .and_then(|s| s.split(" mA").next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("message names the current: {}", f.message));
    assert!((30.0..60.0).contains(&ma), "{ma} mA: {}", f.message);
    assert!(
        f.message.contains("over its 25.0 mA rating"),
        "{}",
        f.message
    );
}

#[test]
fn the_same_stage_with_a_10k_base_resistor_is_quiet() {
    let found = overstress("10k");
    assert!(found.is_empty(), "{found:#?}");
}
