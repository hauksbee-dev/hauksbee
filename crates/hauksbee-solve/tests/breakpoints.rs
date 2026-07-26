//! Source-breakpoint regression: the adaptive step controller must land on
//! PWL vertices and PULSE corners instead of striding across them.
//!
//! The failure this pins: the LTE estimator is a second difference over
//! accepted steps, so a sub-step stimulus arriving after a long quiet stretch
//! (during which dt grew) produces no curvature signal and is silently
//! aliased away. Lore #8 (`docs/learn/tarski-saga.md` section 5)
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
            PwlPoint {
                t: 400.2e-6,
                v: 5.0,
            },
            PwlPoint { t: 402e-6, v: 5.0 },
            PwlPoint {
                t: 402.2e-6,
                v: 0.0,
            },
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
    let (c, _out) = pulse_circuit();
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
        assert!(
            hit,
            "no accepted step landed on the {corner:.6e} s pulse edge"
        );
    }
}

/// R15 efficiency regression: a breakpoint LANDING must not collapse the
/// adaptive controller's dt. The truncation comment promises "dt itself is not
/// reduced, so after the corner the controller resumes at its own rhythm",
/// but the LTE accept used to recompute `next_dt = (h*factor).clamp(..)` from
/// the TRUNCATED trial h (the sliver `bp - t`), so a corner landing collapsed
/// dt to ~2x the sliver (the sliver's tiny LTE saturates the growth factor at
/// 2.0) and forced a geometric regrow after every corner. Waveform-neutral,
/// every step was still LTE-accepted, but each corner cost a regrow ramp
/// instead of exactly one exact landing.
///
/// Fixture: a dead-flat board (source constant, cap pre-charged to the DC
/// point) whose only feature is a FLAT PWL vertex at 130 us, a mandatory
/// landing with no value change, so the LTE is ~0 everywhere and the dt
/// trajectory is pure controller behavior: dt doubles each accepted step
/// (1, 2, 4, ... 64 us), making the landing sliver a small, known fraction of
/// the pre-corner dt.
#[test]
fn breakpoint_landing_does_not_collapse_controller_dt() {
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "Vflat".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(vec![
            PwlPoint { t: 0.0, v: 1.0 },
            PwlPoint { t: 130e-6, v: 1.0 }, // flat vertex: breakpoint, no edge
            PwlPoint { t: 500e-6, v: 1.0 },
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
        farads: 1e-9,
        ic: Some(1.0), // pre-settled: the march is quiet from t = 0
    });
    let opts = SolverOptions {
        step: StepControl::Adaptive {
            dt_initial: 1e-6,
            dt_min: 1e-9,
            dt_max: 64e-6,
        },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 400e-6).expect("transient");

    // The corner is a mandatory landing: find its accepted sample.
    let corner = 130e-6;
    let i = wf
        .time
        .iter()
        .position(|&t| (t - corner).abs() <= corner * 1e-9)
        .expect("an accepted step must land exactly on the flat PWL vertex");
    assert!(
        i >= 2 && i + 1 < wf.time.len(),
        "corner sample needs neighbours"
    );
    let pre_dt = wf.time[i - 1] - wf.time[i - 2]; // controller rhythm before
    let sliver = wf.time[i] - wf.time[i - 1]; // the truncated landing step
    let post_dt = wf.time[i + 1] - wf.time[i]; // first step after the corner

    // Fixture-drift guard: the landing must be a genuine truncation (a small
    // fraction of the controller's step), or the collapse regime isn't being
    // exercised at all. On the doubling grid the sliver is 3 us vs 64 us.
    assert!(
        sliver < 0.15 * pre_dt,
        "fixture drift: landing sliver {sliver:.3e} is not small vs pre-corner \
         dt {pre_dt:.3e}; the truncation path is not exercised"
    );
    // The regression: the controller resumes at its own rhythm, not at ~2x
    // the sliver. (The collapsed behavior gives post_dt = 2*sliver, an order
    // of magnitude below pre_dt on this grid.)
    assert!(
        post_dt >= 0.9 * pre_dt,
        "dt collapsed across the breakpoint landing: pre-corner dt {pre_dt:.3e}, \
         landing sliver {sliver:.3e}, post-corner dt {post_dt:.3e} \
         (collapse-and-regrow gives ~{:.3e} here)",
        2.0 * sliver
    );
    // Waveform sanity: the board is dead flat, so the fix must not have
    // touched the accepted solution, every sample sits at the DC point.
    let vout = wf.node(&c, "out").expect("out waveform");
    for (k, &v) in vout.iter().enumerate() {
        // 1e-6 absolute: the only deviation on a settled board is the gmin
        // leakage through the 1k series resistor (~1e-9 V), far below this.
        assert!(
            (v - 1.0).abs() < 1e-6,
            "flat board drifted at sample {k} (t={:.3e}): v(out)={v}",
            wf.time[k]
        );
    }
}
