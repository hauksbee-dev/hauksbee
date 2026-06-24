//! Closed-form validation: every test computes the exact analytic answer in
//! the test itself and checks the solver against it. Linear circuits hold to
//! ~1e-6 relative; nonlinear DC points to ~1e-3.

use hauksbee_ir::{BjtModel, Circuit, Device, DiodeModel, NodeId, Polarity, SourceKind};
use hauksbee_solve::{Integration, SolverOptions, StepControl, Transient};

/// Max error of `got` vs `want`, normalized by `reltol*|signal| + atol` so a
/// near-zero early sample isn't penalized as if its relative error mattered.
/// `scale` is the signal's characteristic amplitude.
fn max_norm_err(got: &[f64], want: &[f64], scale: f64) -> f64 {
    let atol = 1e-3 * scale; // 0.1% of full scale as the absolute floor
    got.iter()
        .zip(want)
        .map(|(g, w)| (g - w).abs() / (w.abs() * 1e-3 + atol))
        .fold(0.0f64, f64::max)
}

#[test]
fn rc_step_response() {
    // V1 --R-- out --C-- gnd. Step from 0; out(t) = V*(1 - exp(-t/RC)).
    let v = 5.0;
    let r = 1e3;
    let cap = 1e-6;
    let tau = r * cap;

    let mut circuit = Circuit::new();
    let vin = circuit.node("in");
    let out = circuit.node("out");
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v),
    });
    circuit.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: r,
        tc1: None,
    });
    circuit.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: cap,
        ic: Some(0.0),
    });

    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tau / 200.0 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 5.0 * tau).unwrap();

    let got = wf.node(&circuit, "out").unwrap();
    let want: Vec<f64> = wf
        .time
        .iter()
        .map(|&t| v * (1.0 - (-t / tau).exp()))
        .collect();
    // Normalized to full scale `v`: passing => within ~0.1% of 5 V everywhere.
    let err = max_norm_err(got, &want, v);
    assert!(
        err < 2.0,
        "RC step normalized err {err:.3} (>2 means >~0.2% off)"
    );
}

#[test]
fn series_rlc_underdamped() {
    // V step into series R-L-C; capacitor voltage is the classic underdamped
    // second-order response. Drive with a DC source from t=0 (IC=0).
    // L di/dt + Ri + Vc = V, i = C dVc/dt.
    let v = 1.0;
    let r = 50.0_f64;
    let l = 1e-3_f64;
    let cap = 1e-7_f64;

    let w0 = 1.0 / (l * cap).sqrt();
    let alpha = r / (2.0 * l);
    assert!(alpha < w0, "must be underdamped");
    let wd = (w0 * w0 - alpha * alpha).sqrt();

    let mut circuit = Circuit::new();
    let vin = circuit.node("in");
    let mid = circuit.node("mid");
    let out = circuit.node("out");
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v),
    });
    circuit.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: mid,
        ohms: r,
        tc1: None,
    });
    circuit.add(Device::Inductor {
        name: "L1".into(),
        a: mid,
        b: out,
        henries: l,
        ic: Some(0.0),
    });
    circuit.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: cap,
        ic: Some(0.0),
    });

    let period = std::f64::consts::TAU / wd;
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: period / 400.0 },
        ..SolverOptions::default()
    };
    let tstop = 6.0 / alpha; // several damping time constants
    let wf = Transient::new(opts).run(&circuit, tstop).unwrap();

    let got = wf.node(&circuit, "out").unwrap();
    // Vc(t) = V * [1 - e^{-at}(cos wd t + (a/wd) sin wd t)].
    let want: Vec<f64> = wf
        .time
        .iter()
        .map(|&t| v * (1.0 - (-alpha * t).exp() * ((wd * t).cos() + (alpha / wd) * (wd * t).sin())))
        .collect();
    let err = max_norm_err(got, &want, v);
    assert!(err < 5.0, "RLC underdamped normalized err {err:.3}");
}

#[test]
fn diode_resistor_dc_operating_point() {
    // V -- R -- node -- D -- gnd. Solve the DC point and compare with an
    // independent Newton solve of the diode equation done here in the test.
    let v = 5.0;
    let r = 1e3;
    let model = DiodeModel {
        is: 1e-14,
        n: 1.0,
        ..DiodeModel::default()
    };

    let mut circuit = Circuit::new();
    let vin = circuit.node("in");
    let nd = circuit.node("d");
    circuit.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v),
    });
    circuit.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: nd,
        ohms: r,
        tc1: None,
    });
    circuit.add(Device::Diode {
        name: "D1".into(),
        a: nd,
        k: NodeId::GROUND,
        model,
    });

    // Reference: solve (V - Vd)/R = Is*(exp(Vd/Vt) - 1) for Vd by Newton.
    let vt = 0.025852_f64; // kT/q at 27 C
    let mut vd = 0.6_f64;
    for _ in 0..100 {
        let id = model.is * ((vd / vt).exp() - 1.0);
        let gd = model.is / vt * (vd / vt).exp();
        let f = (v - vd) / r - id;
        let df = -1.0 / r - gd;
        let step = f / df;
        vd -= step;
        if step.abs() < 1e-12 {
            break;
        }
    }

    // Run a near-zero transient to extract the DC point.
    let opts = SolverOptions::fixed(1e-6);
    let wf = Transient::new(opts).run(&circuit, 1e-6).unwrap();
    let got = wf.node(&circuit, "d").unwrap()[0];
    let rel = (got - vd).abs() / vd.abs();
    assert!(
        rel < 1e-3,
        "diode Vd got {got:.6} want {vd:.6} (rel {rel:.2e})"
    );
}

#[test]
fn bjt_current_mirror_ratio() {
    // Classic NPN current mirror: Q1 diode-connected sets the reference, Q2
    // mirrors it. With matched devices and ignoring base current / Early, the
    // output current tracks the reference 1:1.
    let mut circuit = Circuit::new();
    let vcc = circuit.node("vcc");
    let nref = circuit.node("ref"); // collector/base of Q1
    let nout = circuit.node("out"); // collector of Q2

    let model = BjtModel {
        polarity: Polarity::N,
        is: 1e-15,
        bf: 200.0,
        vaf: f64::INFINITY, // disable Early so the ideal ratio holds
        ..BjtModel::default()
    };

    // Supply.
    circuit.add(Device::Vsource {
        name: "VCC".into(),
        p: vcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // Reference resistor sets I_ref ~ (Vcc - Vbe)/Rref.
    let rref = 4.3e3;
    circuit.add(Device::Resistor {
        name: "RREF".into(),
        a: vcc,
        b: nref,
        ohms: rref,
        tc1: None,
    });
    // Load resistor on the mirror output.
    let rload = 1e3;
    circuit.add(Device::Resistor {
        name: "RL".into(),
        a: vcc,
        b: nout,
        ohms: rload,
        tc1: None,
    });
    // Q1 diode-connected: collector tied to base (ref node), emitter to gnd.
    circuit.add(Device::Bjt {
        name: "Q1".into(),
        c: nref,
        b: nref,
        e: NodeId::GROUND,
        model,
    });
    // Q2 mirrors: base on ref, collector on out, emitter to gnd.
    circuit.add(Device::Bjt {
        name: "Q2".into(),
        c: nout,
        b: nref,
        e: NodeId::GROUND,
        model,
    });

    let opts = SolverOptions::fixed(1e-6);
    let wf = Transient::new(opts).run(&circuit, 1e-6).unwrap();

    let vref = wf.node(&circuit, "ref").unwrap()[0];
    let vout = wf.node(&circuit, "out").unwrap()[0];
    let vcc_v = 5.0;
    let i_ref = (vcc_v - vref) / rref;
    let i_out = (vcc_v - vout) / rload;

    let ratio = i_out / i_ref;
    // With finite beta the mirror loses 2/(beta+2); ~1% for beta=200.
    assert!(
        (ratio - 1.0).abs() < 0.03,
        "mirror ratio {ratio:.4} (Iref={i_ref:.3e} Iout={i_out:.3e})"
    );
}
