//! Feature 2 tests: fault / stress monitor.
//!
//! Three engine-level scenarios (LED over-current, destructive LED open,
//! 0402 resistor over-power) plus a direct stress-monitor unit for polarized
//! capacitor reverse bias, and a healthy-board no-false-positive check.

use std::collections::HashMap;

use hauksbee_engine::stress::{resistor_power_from_footprint, DeviceMeta, StressMonitor};
use hauksbee_engine::{bind_board, HauksbeeEngine};
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Circuit, Device, NodeId};
use hauksbee_models::schema::{ComponentKind, Ratings};
use hauksbee_server::engine::Engine;
use hauksbee_server::protocol::SolverControls;

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

fn engine_for(text: &str, destructive: bool) -> HauksbeeEngine {
    let mut engine =
        HauksbeeEngine::from_board_file(text, None, "/boards/fault.kicad_pcb").expect("engine");
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
    assert!(
        destroyed_at.is_some(),
        "LED should be destroyed in destructive mode"
    );
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
        (resistor_power_from_footprint("Resistor_SMD:R_0402_1005Metric") - 1.0 / 16.0).abs() < 1e-9
    );

    let mut engine = engine_for(&board, false);
    // A 1 W 0402 is simultaneously over its 62.5 mW power rating *and* its
    // junction-temperature limit (Tj = 25 + 1*600 = 625 C), so the monitor
    // raises both an `overpower` and an `overtemperature` fault. Collect all
    // faults and assert the over-power one specifically is present.
    let mut overpower: Option<(String, f64)> = None;
    for _ in 0..50 {
        let frame = engine.step(1e-4);
        for f in &frame.faults {
            if f.kind == "overpower" {
                overpower = Some((f.component.clone(), f.value));
            }
        }
        if overpower.is_some() {
            break;
        }
    }
    let (component, power) = overpower.expect("0402 resistor at 1 W should raise overpower");
    assert_eq!(component, "R1");
    assert!(
        power > 0.5,
        "reported power {power:.3} W is the real dissipation"
    );
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
    let no_branch = |_: hauksbee_ir::DeviceId| None;

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

#[test]
fn overloaded_nmos_overcurrent_faults() {
    // Drive the stress monitor directly: an NMOS held fully on, whose channel
    // carries ~2 A against a 1 A continuous rating, must raise `overcurrent`.
    // Regression for the Mosfet arm of operating_point() hardcoding current
    // and power to zero, with the zeros, the Overcurrent / Overpower /
    // Overtemperature checks could never fire for any MOSFET.
    let mut circuit = Circuit::new();
    let drain = circuit.node("DRAIN");
    let gate = circuit.node("GATE");
    let source = circuit.node("GND");
    // Power-FET-ish level-1 card: beta = KP·(W/L) = 0.5 A/V², Vth = 2 V.
    let fet = circuit.add(Device::Mosfet {
        name: "Q1".into(),
        d: drain,
        g: gate,
        s: source,
        b: None,
        model: hauksbee_ir::MosfetModel {
            vto: 2.0,
            kp: 0.5,
            ..Default::default()
        },
    });

    let mut ratings = Ratings::default();
    ratings.max_current_a = Some(1.0);
    let meta = DeviceMeta {
        reference: "Q1".into(),
        device: fet,
        kind: ComponentKind::Nmos,
        footprint: "Package_TO_SOT_THT:TO-220-3_Vertical".into(),
        ratings,
    };
    let mut mon = StressMonitor::new(vec![meta]);

    // Vgs = 5 V (3 V of overdrive), Vds = 2 V → triode:
    // Id = beta·(vov·vds − vds²/2) = 0.5·(3·2 − 2) = 2 A, 2× the rating.
    let node_v = |n: NodeId| {
        if n == drain {
            2.0
        } else if n == gate {
            5.0
        } else {
            0.0
        }
    };
    let no_branch = |_: hauksbee_ir::DeviceId| None;

    // A continuous rating must be sustained past the SUSTAIN_CHUNKS window.
    let mut raised = None;
    for chunk in 0..10 {
        let faults = mon.evaluate(&mut circuit, &node_v, &no_branch, chunk as f64 * 1e-4);
        if let Some(f) = faults
            .into_iter()
            .find(|f| f.kind.as_str() == "overcurrent")
        {
            raised = Some(f);
            break;
        }
    }
    let f = raised.expect("overloaded NMOS should raise overcurrent");
    assert_eq!(f.component, "Q1");
    assert!(
        (f.value - 2.0).abs() < 0.1,
        "reported current {:.3} A is the solved channel current",
        f.value
    );
    assert_eq!(f.limit, 1.0);

    // The conducting channel dissipates Vds·Id ≈ 4 W, so the power-gated
    // thermal path must be live too: the monitor publishes a junction
    // temperature above ambient. (Impossible before the fix, power was 0,
    // so no MOSFET ever got a temperature or an Overtemperature check.)
    let tj = mon
        .temp_by_ref()
        .get("Q1")
        .copied()
        .expect("Tj published for a dissipating FET");
    assert!(tj > 25.0, "junction temp {tj:.1} C sits above ambient");
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
    let lib = hauksbee_models::ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    // The monitor should have picked up the LED and the resistors.
    assert!(bound.device_meta.len() >= 3, "metadata for analog parts");

    let mut engine =
        HauksbeeEngine::from_board_file(SYNTH_BOARD, Some(&demo_firmware()), "/b.kicad_pcb")
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
    assert!(
        max_stress < 1.0,
        "no component should be at its limit ({max_stress:.2})"
    );
}

/// Path to the demo firmware (mirrors tests/common but kept local so this test
/// file is self-contained; the brief asks for distinct new test files).
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
        let stress = frame
            .component_states
            .get("D1")
            .and_then(|m| m.get("stress"))
            .copied()
            .unwrap_or(-1.0);
        eprintln!(
            "step {i}: LED_A={v:.3} stress(D1)={stress:.3} faults={}",
            frame.faults.len()
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// R6 #F2: pin-overcurrent for MCU / logic pins is wired through the PinDriver
// legs. Before the fix, gather_device_meta admitted only whole-analog kinds,
// so build_checks's Mcu|Digital|ShiftRegister|Dac|Adc PinOvercurrent arm was
// unreachable dead code and MCU pin violations were silently unreported.
// ─────────────────────────────────────────────────────────────────────────────

/// Monitor-level: a per-pin meta over a driver's hidden Vsource raises
/// `pin_overcurrent` when the pin's branch current exceeds `max_pin_current_a`,
/// and stays quiet under the limit. The Vsource operating-point arm reports the
/// branch current, exactly the current the pin sources/sinks through its
/// Thevenin leg.
#[test]
fn pin_overcurrent_fires_on_driver_leg_current() {
    use hauksbee_engine::stress::FaultKind;

    let run = |pin_amps: f64| -> Vec<hauksbee_engine::stress::FaultEvent> {
        let mut circuit = Circuit::new();
        let drv = NodeId(1);
        // The shape bind_mcu stamps: hidden driver node, Vsource to ground,
        // series resistor to the (here: omitted) net.
        let vd = circuit.add(Device::Vsource {
            name: "Vdrv_U1_B5".into(),
            p: drv,
            n: NodeId::GROUND,
            kind: hauksbee_ir::SourceKind::Dc(5.0),
        });
        let meta = DeviceMeta {
            reference: "U1:PB5".into(),
            device: vd,
            kind: ComponentKind::Mcu,
            footprint: "Package_QFP:TQFP-32_7x7mm_P0.8mm".into(),
            ratings: Ratings {
                max_pin_current_a: Some(0.04), // ATmega-class 40 mA abs max
                ..Default::default()
            },
        };
        let mut mon = StressMonitor::new(vec![meta]);
        let node_v = |_: NodeId| 0.0;
        let branch = move |id: hauksbee_ir::DeviceId| -> Option<f64> {
            if id == vd {
                Some(pin_amps)
            } else {
                None
            }
        };
        let mut all = Vec::new();
        for _ in 0..8 {
            all.extend(mon.evaluate(&mut circuit, &node_v, &branch, 0.0));
        }
        all
    };

    // 60 mA through a 40 mA pin: sustained pin_overcurrent, named per-pin.
    let over = run(0.06);
    let f = over
        .iter()
        .find(|f| f.kind == FaultKind::PinOvercurrent)
        .expect("60 mA through a 40 mA-rated pin must raise pin_overcurrent");
    assert_eq!(f.component, "U1:PB5", "fault names the offending pin");
    assert!((f.value - 0.06).abs() < 1e-12);
    assert!((f.limit - 0.04).abs() < 1e-12);

    // 20 mA through the same pin: within rating, no fault of any kind.
    let under = run(0.02);
    assert!(
        under.is_empty(),
        "20 mA through a 40 mA-rated pin must not fault, got {under:?}"
    );
}

/// Binder-level: binding a 74HC595 (25 mA/pin rating in the model DB) produces
/// one per-pin DeviceMeta over each stamped output driver's Vsource; the
/// structural half that makes the PinOvercurrent arm reachable on real boards.
#[test]
fn binder_gathers_per_pin_metas_for_logic_outputs() {
    // One 74HC595 with two connected outputs (QA, QB), full control/supply
    // wiring, each output loaded by a pulldown.
    let text = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "QA")
  (net 4 "QB")
  (net 5 "SER")
  (net 6 "SRCLK")
  (net 7 "RCLK")
  (module Package_SO:SOIC-16_3.9x9.9mm_P1.27mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value 74HC595 (at 0 2) (layer F.Fab))
    (pad 15 smd rect (at 0 0) (net 3 "QA"))
    (pad 1 smd rect (at 0 1) (net 4 "QB"))
    (pad 14 smd rect (at 0 2) (net 5 "SER"))
    (pad 11 smd rect (at 0 3) (net 6 "SRCLK"))
    (pad 12 smd rect (at 0 4) (net 7 "RCLK"))
    (pad 10 smd rect (at 0 5) (net 2 "+5V"))
    (pad 13 smd rect (at 0 6) (net 1 "GND"))
    (pad 16 smd rect (at 0 7) (net 2 "+5V"))
    (pad 8 smd rect (at 0 8) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "QA"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 112 100)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "QB"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;
    let board = ExtractedBoard::from_auto(text).expect("parse board");
    let lib = hauksbee_models::ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // Exactly the two connected outputs get pin metas, keyed "<ref>:<pin>".
    let mut pin_refs: Vec<&str> = bound
        .device_meta
        .iter()
        .filter(|m| m.reference.starts_with("U1:"))
        .map(|m| m.reference.as_str())
        .collect();
    pin_refs.sort();
    assert_eq!(
        pin_refs,
        vec!["U1:qa", "U1:qb"],
        "one per-pin meta per stamped output driver"
    );
    for m in bound.device_meta.iter().filter(|m| m.reference.starts_with("U1:")) {
        assert_eq!(m.kind, ComponentKind::ShiftRegister);
        assert_eq!(
            m.ratings.max_pin_current_a,
            Some(0.025),
            "pin meta carries the DB's 25 mA/pin rating"
        );
        // The monitored device is the driver's hidden Vsource, whose branch
        // current is the pin current.
        let dev = &bound.circuit.devices[m.device.0 as usize];
        assert!(
            matches!(dev, Device::Vsource { .. }),
            "pin meta must monitor the PinDriver Vsource, got {dev:?}"
        );
    }
    // No whole-package meta for the logic part: it has no single
    // through-current an analog meta could honestly score.
    assert!(
        !bound.device_meta.iter().any(|m| m.reference == "U1"),
        "no package-level meta for a logic IC"
    );
}
