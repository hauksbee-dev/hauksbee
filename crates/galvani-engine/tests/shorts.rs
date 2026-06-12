//! End-to-end tests for the geometric-DRC → simulation path.
//!
//! 1. Detect a deliberate copper short from a board's layout, apply it, and
//!    confirm a `short` fault surfaces and the bridged nets are pulled together.
//! 2. The what-if API: short a rail to ground on a healthy board and confirm the
//!    consequence (a series resistor driven over its power rating faults).
//! 3. A clean board with no overlaps applies zero shorts.

use galvani_engine::GalvaniEngine;
use galvani_extract::{ExtractedBoard, ViolationKind};
use galvani_server::engine::Engine;
use galvani_server::protocol::SolverControls;

/// A tiny two-net board: +5V and SIG, each with a pad, plus two tracks that
/// physically CROSS on F.Cu, a deliberate copper short between +5V and SIG.
/// R1 bleeds SIG to GND so the shorted rail has somewhere to push current.
const SHORTED_BOARD: &str = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "+5V")
  (net 2 "SIG")
  (net 3 "GND")

  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2))

  (footprint "Resistor_SMD:R_0402_1005Metric" (layer "F.Cu") (at 20 0)
    (property "Reference" "R1" (at 0 0))
    (property "Value" "100" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 0.6 0.6) (layers "F.Cu") (net 2))
    (pad "2" smd rect (at 1 0) (size 0.6 0.6) (layers "F.Cu") (net 3))
  )
)
"#;

#[test]
fn drc_detects_then_simulation_applies_the_short() {
    // The layout has one true overlap: +5V crossing SIG.
    let report = ExtractedBoard::drc(SHORTED_BOARD).expect("drc");
    assert_eq!(report.short_count(), 1, "one copper short detected");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.kind, ViolationKind::Short);
    let names = [f.net_a_name.as_str(), f.net_b_name.as_str()];
    assert!(names.contains(&"+5V") && names.contains(&"SIG"));

    // Build the engine and apply the detected shorts.
    let mut engine =
        GalvaniEngine::from_board_file(SHORTED_BOARD, None, "/b.kicad_pcb").expect("engine");
    let applied = engine.apply_drc_shorts(&report);
    assert_eq!(applied, 1, "one bridge stamped");

    // Step the sim; a `short` fault must surface through the fault channel, and
    // the bridged nets must be pulled to (nearly) the same voltage.
    let mut saw_short_fault = false;
    let mut last_v5 = 0.0;
    let mut last_sig = 0.0;
    for _ in 0..20 {
        let frame = engine.step(1e-4);
        for fault in &frame.faults {
            if fault.kind == "short" {
                saw_short_fault = true;
            }
        }
        last_v5 = *frame.net_voltages.get("+5V").unwrap_or(&0.0);
        last_sig = *frame.net_voltages.get("SIG").unwrap_or(&0.0);
    }
    assert!(saw_short_fault, "a `short` fault is surfaced for the applied bridge");
    // +5V is an ideal rail; the milliohm bridge drags SIG up to it.
    assert!(
        (last_v5 - last_sig).abs() < 0.05,
        "+5V ({last_v5:.3}) and SIG ({last_sig:.3}) are bridged to the same potential"
    );
    assert!(last_sig > 4.5, "SIG is pulled up near the +5V rail ({last_sig:.3})");
}

#[test]
fn whatif_short_rail_to_ground_overdrives_series_resistor() {
    // A healthy board: a tiny 1 Ω 0402 resistor from +5V to a SENSE net, SENSE
    // bled to GND through 10k. Normal operation: SENSE sits near +5V (the 10k
    // barely loads R1), so almost the whole 5 V is across the 10k and R1
    // dissipates almost nothing. Then short SENSE straight to GND on demand (a
    // solder bridge): now the full 5 V sits across the 1 Ω R1 → 25 W, far past
    // its 1/16 W 0402 rating → a sustained overpower fault.
    let board = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "+5V")
  (net 2 "SENSE")
  (net 3 "GND")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0))
    (fp_text value 1 (at 0 0))
    (pad 1 smd rect (at 0 0) (net 1 "+5V"))
    (pad 2 smd rect (at 1 0) (net 2 "SENSE"))
  )
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R2 (at 0 0))
    (fp_text value 10k (at 0 0))
    (pad 1 smd rect (at 0 0) (net 2 "SENSE"))
    (pad 2 smd rect (at 1 0) (net 3 "GND"))
  )
)
"#;
    let mut engine = GalvaniEngine::from_board_file(board, None, "/b.kicad_pcb").expect("engine");
    let mut controls = SolverControls::default();
    controls.destructive_faults = false;
    engine.set_controls(controls);

    // Warm up healthy: no fault.
    for _ in 0..10 {
        let frame = engine.step(1e-4);
        assert!(frame.faults.is_empty(), "healthy board does not fault before the short");
    }

    // Apply the what-if short: SENSE straight to GND, so the full rail voltage
    // falls across the 1 Ω R1.
    assert!(engine.short_nets("SENSE", "GND"), "bridge applied");

    let mut overpower = false;
    let mut short_fault = false;
    for _ in 0..40 {
        let frame = engine.step(1e-4);
        for f in &frame.faults {
            match f.kind.as_str() {
                "short" => short_fault = true,
                "overpower" if f.component == "R1" => overpower = true,
                _ => {}
            }
        }
        if overpower {
            break;
        }
    }
    assert!(short_fault, "the applied short is surfaced as a fault");
    assert!(overpower, "shorting the rail across R1 raises an overpower fault on R1");
}

#[test]
fn clean_board_applies_no_shorts() {
    // Two well-separated tracks: no overlap, nothing to apply.
    let board = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "") (net 1 "A") (net 2 "B")
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 5) (end 10 5) (width 0.25) (layer "F.Cu") (net 2))
)
"#;
    let report = ExtractedBoard::drc(board).expect("drc");
    assert_eq!(report.short_count(), 0);
    let mut engine = GalvaniEngine::from_board_file(board, None, "/b.kicad_pcb").expect("engine");
    assert_eq!(engine.apply_drc_shorts(&report), 0, "no bridges on a clean board");
}
