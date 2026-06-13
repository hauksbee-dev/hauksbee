//! Closed-form validation for the transient decoupling layer.
//!
//! These are solver-vs-hand-math cross-checks: each test computes the exact
//! analytic rail sag for a decoupling network under a current step and asserts
//! the solver matches to better than 1%. This is the calibration that lets the
//! scenario layer's sag/dip numbers be trusted.

use hauksbee_ir::{Circuit, Device, NodeId, PwlPoint, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};

/// Worst (maximum) relative error of `got` vs `want`, ignoring the first few
/// samples (the t=0 operating point and the step edge, where a finite dt smears
/// the discontinuity). `floor` is an absolute denominator floor so a near-zero
/// reference is not penalised.
fn max_rel_err(time: &[f64], got: &[f64], want: &dyn Fn(f64) -> f64, t_skip: f64, floor: f64) -> f64 {
    time.iter()
        .zip(got)
        .filter(|(t, _)| **t >= t_skip)
        .map(|(t, g)| {
            let w = want(*t);
            (g - w).abs() / (w.abs().max(floor))
        })
        .fold(0.0f64, f64::max)
}

#[test]
fn ideal_cap_constant_current_discharge_with_esr() {
    // A capacitor C (with series ESR R_esr) initially charged to V0, discharged
    // by a constant current step I out of the node, with no other path.
    //
    //   node ──[ R_esr ]── cap_top ──[ C ]── gnd
    //   Isource pulls I out of `node` to ground for t > 0.
    //
    // Hand math: the cap current is the full I, so
    //   v_cap(t) = V0 - (I/C) * t            (linear ramp-down)
    //   v_node(t) = v_cap(t) - I * R_esr     (instantaneous ESR drop)
    // => v_node(t) = V0 - I*R_esr - (I/C)*t
    //
    // The ESR sets the instantaneous sag; C sets the slope. This is exactly the
    // decoupling sag a fast load step sees before the bulk supply responds.
    let v0 = 3.3;
    let cap = 10e-6;
    let r_esr = 0.030; // 0603 MLCC class ESR
    let i_step = 0.5; // 500 mA load step (ESP32 TX class)

    let mut circuit = Circuit::new();
    let node = circuit.node("RAIL");
    let cap_top = circuit.node("CAP_TOP");

    // ESR resistor between the rail node and the ideal cap plate.
    circuit.add(Device::Resistor {
        name: "Resr".into(),
        a: node,
        b: cap_top,
        ohms: r_esr,
        tc1: None,
    });
    // Ideal capacitor, pre-charged to V0.
    circuit.add(Device::Capacitor {
        name: "C1".into(),
        a: cap_top,
        b: NodeId::GROUND,
        farads: cap,
        ic: Some(v0),
    });
    // Constant current step I drawn out of the node: Isource p->n internal, so
    // p = node, n = GROUND pulls I out of the node. PWL: 0 until 1 us, then I.
    let t_edge = 1e-6;
    circuit.add(Device::Isource {
        name: "Iload".into(),
        p: node,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(vec![
            PwlPoint { t: 0.0, v: 0.0 },
            PwlPoint { t: t_edge, v: 0.0 },
            PwlPoint { t: t_edge + 1e-9, v: i_step },
            PwlPoint { t: 1.0, v: i_step },
        ]),
    });

    // Run only over the linear ramp region (well before the cap empties).
    let tstop = 20e-6;
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tstop / 4000.0 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, tstop).unwrap();
    let got = wf.node(&circuit, "RAIL").unwrap();

    // Exact node voltage for t > t_edge (measure relative to the step start).
    let want = |t: f64| {
        if t <= t_edge {
            v0
        } else {
            v0 - i_step * r_esr - (i_step / cap) * (t - t_edge)
        }
    };
    // Skip a couple of dt past the edge so the PWL ramp isn't penalised.
    let err = max_rel_err(&wf.time, got, &want, t_edge + 2.0 * (tstop / 4000.0), 0.1);
    assert!(err < 0.01, "RC decoupling sag (ESR+ramp) rel err {err:.5} (>1%)");

    // Spot-check the two pieces of physics independently.
    // 1. Instantaneous ESR sag right after the edge: ~ I*R_esr = 15 mV.
    let just_after = got
        .iter()
        .zip(&wf.time)
        .find(|(_, t)| **t >= t_edge + 5.0 * (tstop / 4000.0))
        .map(|(v, _)| *v)
        .unwrap();
    let expected_esr_sag = i_step * r_esr;
    let measured_esr_sag = v0 - just_after;
    assert!(
        (measured_esr_sag - expected_esr_sag).abs() < 0.1 * expected_esr_sag + 1e-3,
        "instantaneous ESR sag {measured_esr_sag:.4} V vs hand {expected_esr_sag:.4} V"
    );
    // 2. Discharge slope dV/dt = I/C = 0.5/10u = 50,000 V/s.
    let slope_expected = i_step / cap;
    let n = wf.time.len();
    let (t1, v1) = (wf.time[n / 2], got[n / 2]);
    let (t2, v2) = (wf.time[n - 1], got[n - 1]);
    let slope_measured = (v1 - v2) / (t2 - t1);
    assert!(
        (slope_measured - slope_expected).abs() < 0.01 * slope_expected,
        "discharge slope {slope_measured:.1} V/s vs hand {slope_expected:.1} V/s"
    );
}

#[test]
fn supplied_rail_step_sag_first_order() {
    // A stiffer, more board-like network with a single, fully-closed-form
    // answer. Ideal rail V0 behind supply resistance R_s feeds a node decoupled
    // by capacitor C (ESR folded into R_s = 0 here for a clean single-pole).
    //
    //   V0 ──[ R_s ]── node ──[ C ]── gnd ,  load step I out of node at t=0.
    //
    // KCL at the node: (V0 - v)/R_s = C dv/dt + I.
    // Steady state v_inf = V0 - I*R_s. Homogeneous time constant tau = R_s*C.
    //   v(t) = v_inf + (V0 - v_inf) * exp(-t/tau) = V0 - I*R_s*(1 - exp(-t/tau)).
    // The rail sags from V0 toward V0 - I*R_s with time constant R_s*C: the
    // textbook load-step droop the bulk cap fills in.
    let v0 = 5.0;
    let r_s = 0.2; // 200 mOhm supply path
    let cap = 100e-6; // bulk cap
    let i_step = 0.3;
    let tau = r_s * cap;

    let mut circuit = Circuit::new();
    let rail = circuit.node("VIN");
    let node = circuit.node("NODE");
    circuit.add(Device::Vsource {
        name: "V0".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v0),
    });
    circuit.add(Device::Resistor {
        name: "Rs".into(),
        a: rail,
        b: node,
        ohms: r_s,
        tc1: None,
    });
    // Cap starts at V0 (the pre-step operating point: no load => no R_s drop).
    circuit.add(Device::Capacitor {
        name: "Cbulk".into(),
        a: node,
        b: NodeId::GROUND,
        farads: cap,
        ic: Some(v0),
    });
    let t_edge = 1e-6;
    circuit.add(Device::Isource {
        name: "Iload".into(),
        p: node,
        n: NodeId::GROUND,
        kind: SourceKind::Pwl(vec![
            PwlPoint { t: 0.0, v: 0.0 },
            PwlPoint { t: t_edge, v: 0.0 },
            PwlPoint { t: t_edge + 1e-9, v: i_step },
            PwlPoint { t: 1.0, v: i_step },
        ]),
    });

    let tstop = 6.0 * tau;
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tau / 500.0 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, tstop).unwrap();
    let got = wf.node(&circuit, "NODE").unwrap();

    let want = |t: f64| {
        if t <= t_edge {
            v0
        } else {
            v0 - i_step * r_s * (1.0 - (-(t - t_edge) / tau).exp())
        }
    };
    let err = max_rel_err(&wf.time, got, &want, t_edge + 4.0 * (tau / 500.0), 0.1);
    assert!(err < 0.01, "first-order load-step sag rel err {err:.5} (>1%)");

    // Final sag should be I*R_s = 60 mV.
    let v_final = *got.last().unwrap();
    let sag = v0 - v_final;
    let expected = i_step * r_s;
    assert!(
        (sag - expected).abs() < 0.01 * expected,
        "steady sag {sag:.4} V vs hand I*Rs = {expected:.4} V"
    );
}

#[test]
fn profile_isource_agrees_in_both_solver_paths() {
    // The same PWL-current-step decoupling network must give the same rail sag
    // whether the solver runs monolithically (Partitioning::Off) or partitions
    // into linear islands (Partitioning::Auto). This is the verification the
    // transient layer needs: a load profile stamped as an Isource is carried
    // correctly by the partitioned path's current-input-column machinery, not
    // just the monolithic path. A purely linear RC + current source is exactly
    // the topology the partitioner peels into a linear island.
    let v0 = 3.3;
    let r_s = 0.1;
    let cap = 47e-6;
    let i_step = 0.4;
    let t_edge = 1e-6;

    let build = || {
        let mut circuit = Circuit::new();
        let rail = circuit.node("VIN");
        let node = circuit.node("NODE");
        circuit.add(Device::Vsource {
            name: "V0".into(),
            p: rail,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(v0),
        });
        circuit.add(Device::Resistor {
            name: "Rs".into(),
            a: rail,
            b: node,
            ohms: r_s,
            tc1: None,
        });
        circuit.add(Device::Capacitor {
            name: "Cbulk".into(),
            a: node,
            b: NodeId::GROUND,
            farads: cap,
            ic: Some(v0),
        });
        circuit.add(Device::Isource {
            name: "Iload".into(),
            p: node,
            n: NodeId::GROUND,
            kind: SourceKind::Pwl(vec![
                PwlPoint { t: 0.0, v: 0.0 },
                PwlPoint { t: t_edge, v: 0.0 },
                PwlPoint { t: t_edge + 1e-9, v: i_step },
                PwlPoint { t: 1.0, v: i_step },
            ]),
        });
        circuit
    };

    let tau = r_s * cap;
    let tstop = 6.0 * tau;
    let dt = tau / 500.0;
    let run = |part: Partitioning| {
        let opts = SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Fixed { dt },
            partitioning: part,
            ..SolverOptions::default()
        };
        let c = build();
        let wf = Transient::new(opts).run(&c, tstop).unwrap();
        wf.node(&c, "NODE").unwrap().to_vec()
    };

    let mono = run(Partitioning::Off);
    let part = run(Partitioning::Auto);

    // Both must match the closed form, and each other, to <1%.
    let want = |t: f64| {
        if t <= t_edge {
            v0
        } else {
            v0 - i_step * r_s * (1.0 - (-(t - t_edge) / tau).exp())
        }
    };
    let n = mono.len().min(part.len());
    let mut worst_self = 0.0f64;
    let mut worst_pair = 0.0f64;
    for k in 0..n {
        let t = k as f64 * dt;
        if t < t_edge + 4.0 * dt {
            continue;
        }
        let w = want(t).max(0.1);
        worst_self = worst_self.max((part[k] - want(t)).abs() / w);
        worst_pair = worst_pair.max((part[k] - mono[k]).abs() / w);
    }
    assert!(worst_self < 0.01, "partitioned path vs hand math {worst_self:.5} (>1%)");
    assert!(worst_pair < 1e-3, "partitioned vs monolithic disagree {worst_pair:.6}");
}
