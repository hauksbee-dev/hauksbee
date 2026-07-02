//! Source-breakpoint regression: the adaptive step controller must land on
//! PWL vertices and PULSE corners instead of striding across them.
//!
//! The failure this pins: the LTE estimator is a second difference over
//! accepted steps, so a sub-step stimulus arriving after a long quiet stretch
//! (during which dt grew) produces no curvature signal and is silently
//! aliased away. Lore #8 (`docs/dev-plans/research/tarski-saga.md` section 5)
//! is the co-sim face of the same failure; this is the analog face, and the
//! PWL edge drive of `docs/dev-plans/05-cosim-fidelity.md` section 1.3 relies
//! on it being fixed.

use hauksbee_ir::{Circuit, Device, NodeId, PwlPoint, SourceKind};
use hauksbee_solve::{SolverOptions, StepControl, Transient};

/// A 2 us PWL pulse at t = 400 us into an RC (tau = 2 us), after 400 us of
/// dead-flat quiet during which the adaptive controller grows dt toward
/// dt_max = 100 us. Without breakpoints the trial steps stride the pulse and
/// the capacitor never charges; with them, a step lands on every corner and
/// the capacitor visibly responds.
fn pulse_circuit() -> (Circuit, NodeId) {
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "Vpulse".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(vec![
            PwlPoint { t: 0.0, v: 0.0 },
            PwlPoint { t: 400e-6, v: 0.0 },
            PwlPoint { t: 400.2e-6, v: 5.0 },
            PwlPoint { t: 402e-6, v: 5.0 },
            PwlPoint { t: 402.2e-6, v: 0.0 },
        ]),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 2e-9,
        ic: Some(0.0),
    });
    (c, out)
}

#[test]
fn adaptive_step_lands_on_pwl_corners_and_sees_the_pulse() {
    let (c, out) = pulse_circuit();
    let opts = SolverOptions {
        step: StepControl::Adaptive {
            dt_initial: 1e-6,
            dt_min: 1e-9,
            dt_max: 100e-6,
        },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 500e-6).expect("transient");

    // The capacitor must have charged substantially during the pulse: with
    // tau = 2 us and a 2 us flat top, it reaches ~63% of 5 V. Anything above
    // 2 V proves the pulse was integrated, not aliased.
    let vout = wf.node(&c, "out").expect("out waveform");
    let peak = vout.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        peak > 2.0,
        "capacitor peak {peak:.3} V: the controller aliased the 2 us pulse"
    );

    // And the mechanism, not just the outcome: an accepted step landed on
    // (within round-off of) each PWL corner inside the run.
    for corner in [400e-6, 400.2e-6, 402e-6, 402.2e-6] {
        let hit = wf
            .time
            .iter()
            .any(|&t| (t - corner).abs() <= f64::max(1e-15, corner * 1e-9));
        assert!(hit, "no accepted step landed on the {corner:.6e} s corner");
    }
}

#[test]
fn fixed_step_grid_is_untouched_by_breakpoints() {
    // The fixed-step path is a user contract: the same circuit run at a
    // fixed 50 us grid must keep exactly its uniform grid (and therefore,
    // honestly, alias the 2 us pulse). Breakpoints are an adaptive-path
    // mechanism only.
    let (c, _) = pulse_circuit();
    let opts = SolverOptions::fixed(50e-6);
    let wf = Transient::new(opts).run(&c, 500e-6).expect("transient");
    for (i, &t) in wf.time.iter().enumerate() {
        let expect = i as f64 * 50e-6;
        assert!(
            (t - expect).abs() < 1e-12,
            "fixed grid disturbed at sample {i}: {t} vs {expect}"
        );
    }
}

#[test]
fn pulse_source_corners_are_registered() {
    // A periodic PULSE source: the controller must land on the leading-edge
    // corner of every period even when the flat stretches would let dt grow
    // past the period. 10 kHz pulse, 1 us edges, 10 us width, into the same
    // RC. Check the first three periods' leading edges.
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "Vp".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Pulse {
            v1: 0.0,
            v2: 5.0,
            delay: 20e-6,
            rise: 1e-6,
            fall: 1e-6,
            width: 10e-6,
            period: 100e-6,
        },
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 2e-9,
        ic: Some(0.0),
    });
    let opts = SolverOptions {
        step: StepControl::Adaptive {
            dt_initial: 1e-6,
            dt_min: 1e-9,
            dt_max: 200e-6,
        },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 350e-6).expect("transient");
    for corner in [20e-6, 120e-6, 220e-6] {
        let hit = wf
            .time
            .iter()
            .any(|&t| (t - corner).abs() <= f64::max(1e-15, corner * 1e-9));
        assert!(hit, "no accepted step landed on the {corner:.6e} s pulse edge");
    }
}
