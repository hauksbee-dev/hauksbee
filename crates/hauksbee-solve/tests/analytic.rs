//! Closed-form validation: every test computes the exact analytic answer in
//! the test itself and checks the solver against it. Linear circuits hold to
//! ~1e-6 relative; nonlinear DC points to ~1e-3.

use crate::shared::{add_capacitor, add_resistor, add_vdc};
use hauksbee_ir::{BjtModel, Circuit, DiodeModel, NodeId, Polarity};
use hauksbee_solve::{Integration, SolverOptions, StepControl, Transient};
use hauksbee_testkit::circuits::{bjt, diode, ind};

/// Max error of `got` vs `want`, normalized by `reltol*|signal| + atol` so a
/// near-zero early sample isn't penalized as if its relative error mattered.
/// `scale` is the signal's characteristic amplitude.
fn max_norm_err(got: &[f64], want: &[f64], scale: f64) -> f64 {
    let atol = 1e-3 * scale; // 0.1% of full scale as the absolute floor
    got.iter()
        .zip(want)
        .map(|(g, w)| (g - w).abs() / w.abs().mul_add(1e-3, atol))
        .fold(0.0f64, f64::max)
}

#[test]
fn rc_step_response() {
    // V1 --R-- out --C-- gnd. Step from 0; out(t) = V*(1 - exp(-t/RC)).
    let v = 5.0;
    let r = 1e3;
    let cap = 1e-6;
    let tau = r * cap;

    let circuit = crate::shared::rc_lowpass(v, r, cap);

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
    add_vdc(&mut circuit, "V1", vin, NodeId::GROUND, v);
    add_resistor(&mut circuit, "R1", vin, mid, r);
    ind(&mut circuit, "L1", mid, out, l);
    add_capacitor(&mut circuit, "C1", out, NodeId::GROUND, cap, Some(0.0));

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
        .map(|&t| {
            v * (-alpha * t)
                .exp()
                .mul_add(-(alpha / wd).mul_add((wd * t).sin(), (wd * t).cos()), 1.0)
        })
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
    add_vdc(&mut circuit, "V1", vin, NodeId::GROUND, v);
    add_resistor(&mut circuit, "R1", vin, nd, r);
    diode(&mut circuit, "D1", nd, NodeId::GROUND, model);

    // Reference: solve (V - Vd)/R = Is*(exp(Vd/Vt) - 1) for Vd by Newton.
    let vt = 0.025_852_f64; // kT/q at 27 C
    let mut vd = 0.6_f64;
    for _ in 0..100 {
        let id = model.is * (vd / vt).exp_m1();
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
    add_vdc(&mut circuit, "VCC", vcc, NodeId::GROUND, 5.0);
    // Reference resistor sets I_ref ~ (Vcc - Vbe)/Rref.
    let rref = 4.3e3;
    add_resistor(&mut circuit, "RREF", vcc, nref, rref);
    // Load resistor on the mirror output.
    let rload = 1e3;
    add_resistor(&mut circuit, "RL", vcc, nout, rload);
    // Q1 diode-connected: collector tied to base (ref node), emitter to gnd.
    bjt(&mut circuit, "Q1", nref, nref, NodeId::GROUND, &model);
    // Q2 mirrors: base on ref, collector on out, emitter to gnd.
    bjt(&mut circuit, "Q2", nout, nref, NodeId::GROUND, &model);

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

/// A base driven hard through 47 ohm (gap G8): with the card's IRB/RBM the
/// base resistance crowds down from RB toward RBM, so the base current is
/// set by the external resistor, not by the zero-bias RB. The BC847 vendor
/// values; the DC point must solve and land on the modulated resistance's
/// self-consistent root.
#[test]
fn bjt_base_resistance_falls_with_base_current() {
    let modulated = BjtModel {
        is: 1.124e-14,
        bf: 281.7,
        br: 7.047,
        rb: 150.0,
        re: 0.804,
        rc: 0.2454,
        irb: 8.356e-4,
        rbm: Some(0.8289),
        ..BjtModel::default()
    };
    let base_current = |model: &BjtModel| {
        let mut circuit = Circuit::new();
        let vcc = circuit.node("vcc");
        let nb = circuit.node("b");
        let nc = circuit.node("c");
        add_vdc(&mut circuit, "V1", vcc, NodeId::GROUND, 5.0);
        add_resistor(&mut circuit, "RB", vcc, nb, 47.0);
        add_resistor(&mut circuit, "RC", vcc, nc, 1e3);
        bjt(&mut circuit, "Q1", nc, nb, NodeId::GROUND, model);
        let mut ws = hauksbee_solve::Workspace::new(&circuit);
        hauksbee_solve::dc_operating_point(&mut ws, &circuit, &SolverOptions::default())
            .expect("dc solves");
        let vb = ws.x[ws.layout.node(nb).unwrap()];
        (5.0 - vb) / 47.0
    };
    let fixed = base_current(&BjtModel {
        irb: f64::INFINITY,
        rbm: None,
        ..modulated
    });
    let ib = base_current(&modulated);
    // Zero-bias RB: (5 - 0.8) / (47 + 150) is about 21 mA.
    assert!((0.015..0.025).contains(&fixed), "fixed rb: {fixed}");
    // Self-consistency: 5 = ib (47 + rbb(ib) + re) + vbe_j, vbe_j about 0.8 V,
    // so rbb is about 17 ohm and ib about 64 mA.
    let rbb = modulated.base_resistance(ib, 1.0);
    assert!((5.0..30.0).contains(&rbb), "rbb {rbb}");
    let vbe_j = ib.mul_add(-(47.0 + rbb), 5.0);
    assert!(
        (0.7..1.0).contains(&vbe_j),
        "ib {ib} rbb {rbb} vbe_j {vbe_j}"
    );
}
