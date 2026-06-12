//! Feature 2 tests: fault / stress monitor.
//!
//! Three engine-level scenarios (LED over-current, destructive LED open,
//! 0402 resistor over-power) plus a direct stress-monitor unit for polarized
//! capacitor reverse bias, and a healthy-board no-false-positive check.

use std::collections::HashMap;

use galvani_engine::stress::{resistor_power_from_footprint, DeviceMeta, StressMonitor};
use galvani_engine::{bind_board, GalvaniEngine};
use galvani_extract::ExtractedBoard;
use galvani_ir::{Circuit, Device, NodeId};
use galvani_models::schema::{ComponentKind, Ratings};
use galvani_server::engine::Engine;
use galvani_server::protocol::SolverControls;

/// A red LED `D1` driven from +5V through a series resistor `R1` of the given
/// value, LED cathode to GND. Nets: 1 GND, 2 +5V, 3 LED_A.
fn led_board(series_ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "LED_A")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {series_ohms} (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 3 "LED_A"))
  )
  (module LED_THT:LED_D5.0mm (layer F.Cu)
    (at 105 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value RED_LED (at 0 2) (layer F.Fab))
    (pad A thru_hole circle (at 0 0) (net 3 "LED_A"))
    (pad K thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#
    )
}

fn engine_for(text: &str, destructive: bool) -> GalvaniEngine {
    let mut engine =
        GalvaniEngine::from_board_file(text, None, "/boards/fault.kicad_pcb").expect("engine");
    let mut controls = SolverControls::default();
    controls.destructive_faults = destructive;
    engine.set_controls(controls);
    engine
}

#[test]
fn led_overcurrent_raises_fault() {
    // 47 Ω series: I ≈ (5 − 1.9)/47 ≈ 66 mA. Over the 25 mA continuous rating
    // but under the 100 mA surge ceiling, so this is a sustained `overcurrent`.
    let mut engine = engine_for(&led_board("47"), false);
    let mut got: Option<(String, String)> = None;
    let mut led_conducting = false;
    for _ in 0..50 {
        let frame = engine.step(1e-4);
        for f in &frame.faults {
            got = Some((f.component.clone(), f.kind.clone()));
        }
        // LED_A sits at the diode drop (~1.9 V) while it conducts.
        if *frame.net_voltages.get("LED_A").unwrap_or(&0.0) > 1.0 {
            led_conducting = true;
        }
        if got.is_some() {
            break;
        }
    }
    let (component, kind) = got.expect("a fault should be raised for the over-driven LED");
    assert_eq!(component, "D1", "fault names the LED");
    assert_eq!(kind, "overcurrent", "fault kind is continuous overcurrent");
    assert!(led_conducting, "LED was conducting before the fault");
}

#[test]
fn destructive_led_opens_and_current_collapses() {
    // Same over-driven LED, but destructive mode: the LED should open and the
    // LED-net current should collapse toward zero afterward.
    let mut engine = engine_for(&led_board("47"), true);
    let mut destroyed_at: Option<usize> = None;
    let mut v_after = 5.0;
    for i in 0..400 {
        let frame = engine.step(1e-4);
        for f in &frame.faults {
            if f.component == "D1" && f.destroyed {
                destroyed_at = Some(i);
            }
        }
        if destroyed_at.is_some() && i > destroyed_at.unwrap() + 20 {
            // After the open, the LED anode net floats up to ~5 V (no current
            // through the series R), and the diode current is ~0. Sample the
            // resistor current via the rail: terminal voltage at LED_A rises to
            // near +5V because no current flows through R1.
            v_after = *frame.net_voltages.get("LED_A").unwrap_or(&0.0);
            break;
        }
    }
    assert!(destroyed_at.is_some(), "LED should be destroyed in destructive mode");
    // With the diode open, no current flows, so the drop across R1 is ~0 and
    // LED_A sits near +5V.
    assert!(
        v_after > 4.5,
        "after LED open, LED_A floats to {v_after:.3} V (current collapsed)"
    );
}

#[test]
fn resistor_overpower_faults() {
    // A 0402 resistor (1/16 W = 62.5 mW rating) of 25 Ω straight across +5V
    // dissipates 5²/25 = 1 W, ~16× its rating → sustained overpower fault.
    let board = format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (module Resistor_SMD:R_0402_1005Metric (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 25 (at 0 2) (layer F.Fab))
    (pad 1 smd roundrect (at 0 0) (net 2 "+5V"))
    (pad 2 smd roundrect (at 2 0) (net 1 "GND"))
  )
)
"#
    );
    // Confirm the footprint-derived rating is the 0402 value.
    assert!(
        (resistor_power_from_footprint("Resistor_SMD:R_0402_1005Metric") - 1.0 / 16.0).abs()
            < 1e-9
    );

    let mut engine = engine_for(&board, false);
    let mut got: Option<(String, String, f64)> = None;
    for _ in 0..50 {
        let frame = engine.step(1e-4);
        for f in &frame.faults {
            got = Some((f.component.clone(), f.kind.clone(), f.value));
        }
        if got.is_some() {
            break;
        }
    }
    let (component, kind, power) = got.expect("0402 resistor at 1 W should fault");
    assert_eq!(component, "R1");
    assert_eq!(kind, "overpower");
    assert!(power > 0.5, "reported power {power:.3} W is the real dissipation");
}

#[test]
fn polarized_cap_reverse_bias_faults() {
    // Drive the stress monitor directly: a polarized cap with its + terminal
    // held below its − terminal (reverse biased by 2 V) must raise reverse_bias.
    let mut circuit = Circuit::new();
    let pos = circuit.node("CAP_POS");
    let neg = circuit.node("CAP_NEG");
    // Hold CAP_NEG at +2 V, CAP_POS at ground → reverse bias of 2 V.
    let cap = circuit.add(Device::Capacitor {
        name: "C1".into(),
        a: pos,
        b: neg,
        farads: 1e-6,
        ic: None,
    });

    let mut ratings = Ratings::default();
    ratings.polarized = true;
    ratings.max_voltage_v = Some(16.0);
    let meta = DeviceMeta {
        reference: "C1".into(),
        device: cap,
        kind: ComponentKind::Passive,
        footprint: "Capacitor_SMD:CP_Elec_4x5.4".into(),
        ratings,
    };

    let mut mon = StressMonitor::new(vec![meta]);
    // Node voltages: pos = 0, neg = +2.
    let node_v = |n: NodeId| {
        if n == pos {
            0.0
        } else if n == neg {
            2.0
        } else {
            0.0
        }
    };
    let no_branch = |_: galvani_ir::DeviceId| None;

    // Reverse bias must persist past the sustain window before it raises.
    let mut raised = None;
    for chunk in 0..10 {
        let faults = mon.evaluate(&mut circuit, &node_v, &no_branch, chunk as f64 * 1e-4);
        if let Some(f) = faults.into_iter().next() {
            raised = Some(f);
            break;
        }
    }
    let f = raised.expect("reverse bias on polarized cap should fault");
    assert_eq!(f.component, "C1");
    assert_eq!(f.kind.as_str(), "reverse_bias");
    assert!(f.value > 1.5, "reverse magnitude {:.2} V reported", f.value);
}

// This test runs a real firmware co-sim, so it needs an MCU backend. The demo
// firmware is AVR; gate it on the `avr` feature so a renode-only build (no
// libsimavr) skips it cleanly rather than panicking at engine construction.
#[cfg(feature = "avr")]
#[test]
fn healthy_board_raises_no_faults() {
    // The synthetic demo board (MCU + 330 Ω + LED + 10k/10k divider) is within
    // all ratings and must produce zero faults over a real co-sim run.
    let board = ExtractedBoard::from_auto(SYNTH_BOARD).expect("parse synth");
    let lib = galvani_models::ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    // The monitor should have picked up the LED and the resistors.
    assert!(bound.device_meta.len() >= 3, "metadata for analog parts");

    let mut engine =
        GalvaniEngine::from_board_file(SYNTH_BOARD, Some(&demo_firmware()), "/b.kicad_pcb")
            .expect("engine");
    let mut controls = SolverControls::default();
    controls.destructive_faults = false;
    engine.set_controls(controls);

    let mut total_faults = 0usize;
    let mut max_stress = 0.0f64;
    for _ in 0..500 {
        let frame = engine.step(1e-3);
        total_faults += frame.faults.len();
        for st in frame.component_states.values() {
            if let Some(s) = st.get("stress") {
                max_stress = max_stress.max(*s);
            }
        }
    }
    assert_eq!(total_faults, 0, "healthy board must not fault");
    // The 330 Ω LED branch runs ~10 mA; stress should stay well under 1.0.
    assert!(max_stress < 1.0, "no component should be at its limit ({max_stress:.2})");
}

/// Path to the demo firmware (mirrors tests/common but kept local so this test
/// file is self-contained — the brief asks for distinct new test files).
#[cfg(feature = "avr")]
fn demo_firmware() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/demo/demo.hex")
}

/// The synthetic demo board fixture (same topology as tests/common::SYNTH_BOARD).
#[cfg(feature = "avr")]
const SYNTH_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
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

/// Suppress unused-warning for the helper consts in some builds.
#[allow(dead_code)]
fn _unused() {
    let _ = HashMap::<String, f64>::new();
}

#[test]
fn dbg_led() {
    let mut engine = engine_for(&led_board("47"), false);
    for i in 0..20 {
        let frame = engine.step(1e-4);
        let v = *frame.net_voltages.get("LED_A").unwrap_or(&0.0);
        let stress = frame.component_states.get("D1").and_then(|m| m.get("stress")).copied().unwrap_or(-1.0);
        eprintln!("step {i}: LED_A={v:.3} stress(D1)={stress:.3} faults={}", frame.faults.len());
    }
}
