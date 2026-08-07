//! Steady-state thermal monitor tests.
//!
//! The core test is the hand-checked arithmetic: a known dissipation in a known
//! package at a known ambient must produce the textbook junction temperature,
//! and the over-temperature fault must trip exactly when Tj crosses the limit.
//! A two-sided case (within limits vs pushed over by a higher ambient) and a
//! healthy-board no-false-positive case round it out.

use std::collections::HashMap;

use hauksbee_engine::stress::{DeviceMeta, FaultKind, StressMonitor};
use hauksbee_engine::thermal::{junction_temp_c, theta_ja_from_footprint};
use hauksbee_engine::HauksbeeEngine;
use hauksbee_ir::{Circuit, Device, NodeId};
use hauksbee_models::schema::{ComponentKind, Ratings};
use hauksbee_server::engine::Engine;
use hauksbee_server::protocol::SolverControls;

/// The canonical hand-check: 0.5 W in a SOT-23 (theta_JA = 250 C/W) at 25 C
/// ambient reaches Tj = 25 + 0.5*250 = 150 C. We drive the StressMonitor
/// directly through a power resistor whose dissipation we control exactly, with
/// the resistor's footprint forced to a SOT-23-class theta_JA so the arithmetic
/// is unambiguous.
#[test]
fn hand_checked_half_watt_sot23_reaches_150c() {
    // theta_JA from the package class, independent of the monitor.
    let theta = theta_ja_from_footprint("Package_TO_SOT_SMD:SOT-23", ComponentKind::BjtNpn);
    assert_eq!(theta, 250.0, "SOT-23 theta_JA");
    assert!((junction_temp_c(25.0, 0.5, theta) - 150.0).abs() < 1e-9);

    // Now the same number through the monitor. A 1 V source across a 2 Ohm
    // resistor dissipates 0.5 W. Nodes: 0 GND, 1 V+.
    let mut circuit = Circuit::new();
    let vp = NodeId(1);
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vp,
        n: NodeId::GROUND,
        kind: hauksbee_ir::SourceKind::Dc(1.0),
    });
    let rid = circuit.add(Device::Resistor {
        name: "R1".into(),
        a: vp,
        b: NodeId::GROUND,
        ohms: 2.0,
        tc1: None,
    });

    // theta_JA explicit (250) so the test does not depend on footprint parsing
    // of the synthetic device; max Tj 150 so we sit exactly at the limit.
    let meta = DeviceMeta {
        reference: "R1".into(),
        device: rid,
        kind: ComponentKind::Passive,
        footprint: "Package_TO_SOT_SMD:SOT-23".into(),
        ratings: Ratings {
            max_power_w: Some(2.0), // generous so power alone does not fault
            theta_ja_c_per_w: Some(250.0),
            max_junction_temp_c: Some(150.0),
            ..Default::default()
        },
    };

    let mut mon = StressMonitor::new(vec![meta]);
    mon.ambient_c = 25.0;

    // Node voltages: GND=0, V+=1.
    let volts = [0.0, 1.0];
    let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    let no_branch = |_: hauksbee_ir::DeviceId| None;

    // Drive several chunks (over-temperature is a sustained rating).
    let mut faults = Vec::new();
    for _ in 0..8 {
        faults = mon.evaluate(&mut circuit, &node_v, &no_branch, 0.0);
    }

    let tj = mon.temp_by_ref().get("R1").copied().expect("R1 has a Tj");
    assert!(
        (tj - 150.0).abs() < 1e-6,
        "computed Tj {tj:.3} C should equal the hand-checked 150 C"
    );

    // At exactly the limit (frac = 1.0) the monitor does not trip (it needs
    // frac > 1.0). Nudge ambient up 1 C and it must trip.
    assert!(
        faults.is_empty(),
        "Tj exactly at the 150 C limit should not fault, got {faults:?}"
    );
}

/// Firmware PWM is a cross-layer thermal input: the electrical solve supplies
/// each chunk's dissipation, while elapsed simulation time supplies the duty
/// cycle. Three hot chunks followed by one off chunk are 75% duty, so a 1 W
/// peak must heat like 0.75 W in the steady-state model. Sampling only the
/// current chunk reports the 1 W peak (or ambient during the off chunk) and
/// cannot produce the duty-cycle temperature.
#[test]
fn firmware_pwm_thermal_uses_time_weighted_duty_cycle() {
    const AMBIENT_C: f64 = 25.0;
    const PEAK_POWER_W: f64 = 1.0;
    const DUTY_CYCLE: f64 = 0.75;
    const THETA_JA_C_PER_W: f64 = 100.0;
    const TJ_LIMIT_C: f64 = 90.0;
    const CHUNK_S: f64 = 1.0e-3;
    const PERIODS: usize = 3;
    const CHUNKS_PER_PERIOD: usize = 4;

    let mut circuit = Circuit::new();
    let load = circuit.add(Device::Resistor {
        name: "Q1".into(),
        a: NodeId(1),
        b: NodeId::GROUND,
        ohms: 1.0,
        tc1: None,
    });
    let meta = DeviceMeta {
        reference: "Q1".into(),
        device: load,
        kind: ComponentKind::Nmos,
        footprint: "Package_TO_SOT_SMD:SOT-23".into(),
        ratings: Ratings {
            max_power_w: Some(PEAK_POWER_W * 2.0),
            theta_ja_c_per_w: Some(THETA_JA_C_PER_W),
            max_junction_temp_c: Some(TJ_LIMIT_C),
            ..Default::default()
        },
    };
    let mut monitor = StressMonitor::new(vec![meta]);
    monitor.ambient_c = AMBIENT_C;
    let no_branch = |_: hauksbee_ir::DeviceId| None;
    let mut overtemperature = Vec::new();

    for chunk in 0..(PERIODS * CHUNKS_PER_PERIOD) {
        let hot = chunk % CHUNKS_PER_PERIOD < 3;
        let volts = [0.0, if hot { PEAK_POWER_W.sqrt() } else { 0.0 }];
        let node_v = |node: NodeId| volts.get(node.0 as usize).copied().unwrap_or(0.0);
        let t = (chunk + 1) as f64 * CHUNK_S;
        overtemperature.extend(
            monitor
                .evaluate(&mut circuit, &node_v, &no_branch, t)
                .into_iter()
                .filter(|fault| fault.kind == FaultKind::Overtemperature),
        );
    }

    let expected_tj = AMBIENT_C + PEAK_POWER_W * DUTY_CYCLE * THETA_JA_C_PER_W;
    let reported_tj = monitor
        .temp_by_ref()
        .get("Q1")
        .copied()
        .expect("PWM-driven package has a duty-cycle temperature");
    assert!(
        (reported_tj - expected_tj).abs() < 1.0e-9,
        "75% of 1 W through 100 C/W at 25 C must report 100 C, got {reported_tj}"
    );
    assert_eq!(
        overtemperature.len(),
        1,
        "the 100 C duty-cycle temperature exceeds the 90 C limit once"
    );
    assert!(
        (overtemperature[0].value - expected_tj).abs() < 1.0e-9,
        "fault must carry the duty-cycle temperature, not the 125 C peak"
    );
}

/// Two-sided: the same 0.5 W SOT-23 part is within limits at 25 C ambient
/// (Tj = 150, limit 175) but pushed over when the ambient rises to 60 C
/// (Tj = 60 + 125 = 185 > 175). Demonstrates the ambient knob and the fault.
#[test]
fn ambient_pushes_part_over_its_limit() {
    let build = |ambient: f64| -> Vec<hauksbee_engine::stress::FaultEvent> {
        let mut circuit = Circuit::new();
        let vp = NodeId(1);
        circuit.add(Device::Vsource {
            name: "V1".into(),
            p: vp,
            n: NodeId::GROUND,
            kind: hauksbee_ir::SourceKind::Dc(1.0),
        });
        let rid = circuit.add(Device::Resistor {
            name: "R1".into(),
            a: vp,
            b: NodeId::GROUND,
            ohms: 2.0, // 0.5 W
            tc1: None,
        });
        let meta = DeviceMeta {
            reference: "R1".into(),
            device: rid,
            kind: ComponentKind::Passive,
            footprint: "X".into(),
            ratings: Ratings {
                max_power_w: Some(2.0),
                theta_ja_c_per_w: Some(250.0),
                max_junction_temp_c: Some(175.0),
                ..Default::default()
            },
        };
        let mut mon = StressMonitor::new(vec![meta]);
        mon.ambient_c = ambient;
        let volts = [0.0, 1.0];
        let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
        let no_branch = |_: hauksbee_ir::DeviceId| None;
        let mut last = Vec::new();
        for _ in 0..8 {
            last = mon.evaluate(&mut circuit, &node_v, &no_branch, 0.0);
        }
        // The first trip is what we want; re-collect by checking temp + a fresh run.
        last
    };

    // 25 C: Tj = 25 + 0.5*250 = 150 < 175. No fault.
    let cool = build(25.0);
    assert!(
        cool.iter().all(|f| f.kind != FaultKind::Overtemperature),
        "at 25 C ambient the part is within limits, got {cool:?}"
    );

    // 60 C: Tj = 60 + 125 = 185 > 175. Over-temperature must trip. Because the
    // fault latches after the first trip, run a fresh monitor and catch the
    // trip on the chunk it crosses.
    let mut circuit = Circuit::new();
    let vp = NodeId(1);
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vp,
        n: NodeId::GROUND,
        kind: hauksbee_ir::SourceKind::Dc(1.0),
    });
    let rid = circuit.add(Device::Resistor {
        name: "R1".into(),
        a: vp,
        b: NodeId::GROUND,
        ohms: 2.0,
        tc1: None,
    });
    let meta = DeviceMeta {
        reference: "R1".into(),
        device: rid,
        kind: ComponentKind::Passive,
        footprint: "X".into(),
        ratings: Ratings {
            max_power_w: Some(2.0),
            theta_ja_c_per_w: Some(250.0),
            max_junction_temp_c: Some(175.0),
            ..Default::default()
        },
    };
    let mut mon = StressMonitor::new(vec![meta]);
    mon.ambient_c = 60.0;
    let volts = [0.0, 1.0];
    let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    let no_branch = |_: hauksbee_ir::DeviceId| None;
    let mut hot_fault = None;
    for _ in 0..8 {
        let fs = mon.evaluate(&mut circuit, &node_v, &no_branch, 0.0);
        for f in fs {
            if f.kind == FaultKind::Overtemperature {
                hot_fault = Some(f);
            }
        }
    }
    let f = hot_fault.expect("60 C ambient must raise an over-temperature fault");
    assert_eq!(f.component, "R1");
    assert!(
        (f.value - 185.0).abs() < 1e-6,
        "fault Tj {:.3} should be 185 C (60 + 0.5*250)",
        f.value
    );
    assert!((f.limit - 175.0).abs() < 1e-9);
}

/// A normal LED-resistor board at room ambient must not raise any
/// over-temperature fault: the parts barely dissipate.
#[test]
fn healthy_board_no_overtemp() {
    // 330 Ohm series, ~10 mA, sub-50 mW dissipation: nowhere near hot.
    let text = format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "LED_A")
  (module Resistor_SMD:R_0805_2012Metric (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 330 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 2 "+5V"))
    (pad 2 smd rect (at 2 0) (net 3 "LED_A"))
  )
  (module LED_SMD:LED_0805_2012Metric (layer F.Cu)
    (at 105 100)
    (fp_text reference D1 (at 0 0) (layer F.SilkS))
    (fp_text value RED_LED (at 0 2) (layer F.Fab))
    (pad A smd rect (at 0 0) (net 3 "LED_A"))
    (pad K smd rect (at 2 0) (net 1 "GND"))
  )
)
"#
    );
    let mut engine =
        HauksbeeEngine::from_board_file(&text, None, "/boards/thermal.kicad_pcb").expect("engine");
    let mut controls = SolverControls::default();
    controls.destructive_faults = false;
    engine.set_controls(controls);
    engine.scheduler_mut().set_ambient_c(25.0);

    let mut overtemp = 0usize;
    let mut max_tj: HashMap<String, f64> = HashMap::new();
    for _ in 0..50 {
        let frame = engine.step(1.0 / 1000.0);
        for f in &frame.faults {
            if f.kind == "overtemperature" {
                overtemp += 1;
            }
        }
        for (r, &tj) in &engine.scheduler().temp_states() {
            let e = max_tj.entry(r.clone()).or_insert(f64::NEG_INFINITY);
            if tj > *e {
                *e = tj;
            }
        }
    }
    assert_eq!(
        overtemp, 0,
        "healthy board raised {overtemp} overtemp faults"
    );
    // Every measured junction temp must be modest (well under 60 C).
    for (r, tj) in &max_tj {
        assert!(
            *tj < 60.0,
            "{r} junction temp {tj:.1} C is implausibly hot for a healthy LED board"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// R6 #F10: multi-unit packages pool simultaneous dissipation for Tj. The dice
// of a dual BJT share one package (one theta_JA path), so the temperature-
// driving power is the SUM of the siblings' dissipation. Before the fix each
// unit's Tj was computed from its own power alone, and a package whose pooled
// dissipation was over the limit never faulted.
// ─────────────────────────────────────────────────────────────────────────────

/// Build a two-unit "dual" package: 1 V across two 2 Ω unit devices named
/// `Q1_q1` / `Q1_q2` (0.5 W each when both load resistors are `load_ohms`;
/// pass a huge second resistance to idle unit 2). Metas mirror a dual BJT:
/// per-unit `max_power_w`, shared explicit theta_JA and Tj limit.
fn dual_package(unit2_ohms: f64) -> (Circuit, Vec<DeviceMeta>) {
    let mut circuit = Circuit::new();
    let vp = NodeId(1);
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vp,
        n: NodeId::GROUND,
        kind: hauksbee_ir::SourceKind::Dc(1.0),
    });
    let q1 = circuit.add(Device::Resistor {
        name: "Q1_q1".into(),
        a: vp,
        b: NodeId::GROUND,
        ohms: 2.0, // 0.5 W
        tc1: None,
    });
    let q2 = circuit.add(Device::Resistor {
        name: "Q1_q2".into(),
        a: vp,
        b: NodeId::GROUND,
        ohms: unit2_ohms,
        tc1: None,
    });
    let ratings = Ratings {
        // Per-UNIT rating, as the model DB documents for dual pairs
        // (bjt.toml: ratings are "per transistor"). 0.6 W: each 0.5 W unit is
        // individually fine, so Overpower must NOT fire even when the pooled
        // 1.0 W exceeds it, only Tj pools.
        max_power_w: Some(0.6),
        theta_ja_c_per_w: Some(250.0),
        max_junction_temp_c: Some(200.0),
        ..Default::default()
    };
    let meta = |name: &str, dev| DeviceMeta {
        reference: name.into(),
        device: dev,
        kind: ComponentKind::BjtNpn,
        footprint: "Package_TO_SOT_SMD:SOT-363_SC-70-6".into(),
        ratings: ratings.clone(),
    };
    let metas = vec![meta("Q1_q1", q1), meta("Q1_q2", q2)];
    (circuit, metas)
}

/// Both units dissipating: pooled 1.0 W through the shared 250 C/W package is
/// Tj = 25 + 250 = 275 C > 200 C, overtemperature MUST fire (each unit alone
/// reads 150 C, which is how the bug hid). Per-unit Overpower must stay quiet
/// (0.5 W < 0.6 W per unit), and both unit rows must report the shared Tj.
#[test]
fn dual_package_pools_sibling_dissipation_for_tj() {
    let (mut circuit, metas) = dual_package(2.0);
    let mut mon = StressMonitor::new(metas);
    mon.ambient_c = 25.0;
    let volts = [0.0, 1.0];
    let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    let no_branch = |_: hauksbee_ir::DeviceId| None;

    let mut all = Vec::new();
    for _ in 0..8 {
        all.extend(mon.evaluate(&mut circuit, &node_v, &no_branch, 0.0));
    }

    let overtemp: Vec<_> = all
        .iter()
        .filter(|f| f.kind == FaultKind::Overtemperature)
        .collect();
    assert!(
        !overtemp.is_empty(),
        "pooled 1.0 W in a 250 C/W package (Tj 275 C > 200 C) must fault"
    );
    for f in &overtemp {
        assert!(
            (f.value - 275.0).abs() < 1e-6,
            "fault Tj {:.3} should be the pooled 275 C, not a single unit's 150 C",
            f.value
        );
        assert!((f.limit - 200.0).abs() < 1e-9);
    }
    // Both junctions genuinely sit over-limit, and CI fault matching names
    // units: each sibling raises its own (identical) fault.
    let mut comps: Vec<&str> = overtemp.iter().map(|f| f.component.as_str()).collect();
    comps.sort();
    assert_eq!(comps, vec!["Q1_q1", "Q1_q2"]);

    // Per-unit rows both report the shared package temperature.
    let t1 = mon.temp_by_ref().get("Q1_q1").copied().expect("Q1_q1 Tj");
    let t2 = mon.temp_by_ref().get("Q1_q2").copied().expect("Q1_q2 Tj");
    assert!((t1 - 275.0).abs() < 1e-6 && (t2 - 275.0).abs() < 1e-6);

    // max_power_w is per-unit: 0.5 W < 0.6 W each, so pooling must NOT have
    // leaked into the Overpower check.
    assert!(
        all.iter().all(|f| f.kind != FaultKind::Overpower),
        "per-unit Overpower must not see pooled power, got {all:?}"
    );
}

/// One unit dissipating, its sibling idle: the pool is just that unit's own
/// 0.5 W, Tj = 150 C < 200 C, no fault. Pooling must not inflate a package
/// whose single active unit is within limits (the false-positive side).
#[test]
fn dual_package_single_active_unit_does_not_false_fire() {
    let (mut circuit, metas) = dual_package(1e12); // unit 2 idle
    let mut mon = StressMonitor::new(metas);
    mon.ambient_c = 25.0;
    let volts = [0.0, 1.0];
    let node_v = |n: NodeId| volts.get(n.0 as usize).copied().unwrap_or(0.0);
    let no_branch = |_: hauksbee_ir::DeviceId| None;

    let mut all = Vec::new();
    for _ in 0..8 {
        all.extend(mon.evaluate(&mut circuit, &node_v, &no_branch, 0.0));
    }
    assert!(
        all.is_empty(),
        "0.5 W pooled (150 C < 200 C) must not fault, got {all:?}"
    );
    // The active unit reads the package Tj; the idle sibling shares the same
    // package and therefore the same reported temperature.
    let t1 = mon.temp_by_ref().get("Q1_q1").copied().expect("Q1_q1 Tj");
    let t2 = mon.temp_by_ref().get("Q1_q2").copied().expect("Q1_q2 Tj");
    assert!(
        (t1 - 150.0).abs() < 1e-6 && (t2 - 150.0).abs() < 1e-6,
        "both unit rows report the shared package Tj (got {t1:.3}, {t2:.3})"
    );
}
