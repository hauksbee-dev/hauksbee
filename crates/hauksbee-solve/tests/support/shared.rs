//! Helpers shared across the integration-test modules of the `it` binary
//! (declared once in `tests/main.rs`): the standard RC low-pass fixture and
//! the waveform comparisons the parity gates use.

#![allow(dead_code)]

use hauksbee_ir::{Circuit, DeviceId, NodeId};
use hauksbee_solve::{
    dc_operating_point, Integration, Partitioning, SolverOptions, StepControl, Transient,
    Waveforms, Workspace,
};
use hauksbee_testkit::circuits;

/// `V1(in, v) -> R1(r) -> out -> C1(farads, IC=0) -> gnd`: the RC step fixture.
pub fn rc_lowpass(v: f64, r: f64, farads: f64) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let out = c.node("out");
    add_vdc(&mut c, "V1", vin, NodeId::GROUND, v);
    add_resistor(&mut c, "R1", vin, out, r);
    add_capacitor(&mut c, "C1", out, NodeId::GROUND, farads, Some(0.0));
    c
}

/// Add a plain resistor (no temperature coefficient): the shape almost every
/// fixture here wants, written once instead of a seven-line struct literal at
/// each use. Thin wrapper over `hauksbee_testkit::circuits::res`, kept under
/// this name since most of this crate's tests already call it that way.
pub fn add_resistor(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) -> DeviceId {
    circuits::res(c, name, a, b, ohms)
}

/// Add a capacitor: the same shape as [`add_resistor`], with the initial
/// condition kept explicit because fixtures differ on whether they set one.
pub fn add_capacitor(
    c: &mut Circuit,
    name: &str,
    a: NodeId,
    b: NodeId,
    farads: f64,
    ic: Option<f64>,
) -> DeviceId {
    match ic {
        Some(ic) => circuits::cap_ic(c, name, a, b, farads, ic),
        None => circuits::cap(c, name, a, b, farads),
    }
}

/// Add a DC voltage source, the only source kind the fixtures here build by
/// hand often enough to be worth a name.
pub fn add_vdc(c: &mut Circuit, name: &str, p: NodeId, n: NodeId, volts: f64) -> DeviceId {
    circuits::vdc_between(c, name, p, n, volts)
}

/// Driver island sensed by controlled/behavioral sources: `V1 -> Rd -> ctrl`,
/// with a capacitor to ground.
pub fn add_driver_leg(c: &mut Circuit) -> NodeId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    add_vdc(&mut *c, "V1", vin, NodeId::GROUND, 2.0);
    add_resistor(&mut *c, "Rd", vin, ctrl, 1e3);
    add_capacitor(&mut *c, "Cd", ctrl, NodeId::GROUND, 100e-9, Some(0.0));
    ctrl
}

/// RC load island (`out`) driven by a controlled or behavioral source.
pub fn add_load_leg(c: &mut Circuit) -> NodeId {
    let out = c.node("out");
    add_resistor(&mut *c, "Rl", out, NodeId::GROUND, 1e3);
    add_capacitor(&mut *c, "Cl", out, NodeId::GROUND, 100e-9, Some(0.0));
    out
}

/// Driver with an in-line zero-volt ammeter: `V1 -> Rd -> ctrl -> Cd`, plus
/// `ctrl -> Vs(0V) -> cm -> Rs -> gnd`. Returns the ammeter's id for F/H or
/// behavioral branch-current control; its current is `v_ctrl / 1k`.
pub fn add_sensed_driver_leg(c: &mut Circuit) -> DeviceId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    let cm = c.node("cm");
    add_vdc(&mut *c, "V1", vin, NodeId::GROUND, 2.0);
    add_resistor(&mut *c, "Rd", vin, ctrl, 1e3);
    add_capacitor(&mut *c, "Cd", ctrl, NodeId::GROUND, 100e-9, Some(0.0));
    let vs = add_vdc(&mut *c, "Vs", ctrl, cm, 0.0);
    add_resistor(&mut *c, "Rs", cm, NodeId::GROUND, 1e3);
    vs
}

/// Four independent RC legs behind the input source cut make the automatic
/// partition heuristics fragment the circuit and engage the partitioned engine.
pub fn add_partition_rc_legs(c: &mut Circuit) {
    let vin = c.node("in");
    for k in 0..4 {
        let leg = c.node(&format!("leg{k}"));
        add_resistor(&mut *c, &format!("Rx{k}"), vin, leg, 1e3);
        add_capacitor(
            &mut *c,
            &format!("Cx{k}"),
            leg,
            NodeId::GROUND,
            1e-9,
            Some(0.0),
        );
    }
}

/// Fixed-step trapezoidal transient options with partitioning `Off`: the
/// bit-exact monolith the parity gates compare against.
pub fn trap_fixed_off(dt: f64) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    }
}

/// A workspace holding `c`'s DC operating point under default options.
pub fn dc_solved(c: &Circuit) -> Workspace {
    let mut ws = Workspace::new(c);
    dc_operating_point(&mut ws, c, &SolverOptions::default()).unwrap();
    ws
}

/// The never-silently-dropped gate: over a 1 ms transient (>= 10 tau of every
/// RC the leg fixtures build, so settled) at a fixed 1 us step, node `out`
/// under `Partitioning::Auto` must settle within 0.02 V of `want` (a dropped
/// device leaves it at 0 V) and track the bit-exact `Off` monolith within 5 mV.
pub fn assert_auto_settles_like_off(c: &Circuit, want: f64, label: &str) {
    let opts_off = trap_fixed_off(1e-6);
    let opts_auto = SolverOptions {
        partitioning: Partitioning::Auto,
        ..opts_off
    };
    let off = Transient::new(opts_off).run(c, 1e-3).unwrap();
    let auto = Transient::new(opts_auto).run(c, 1e-3).unwrap();
    let w_off = off.node(c, "out").unwrap();
    let w_auto = auto.node(c, "out").unwrap();
    let last_auto = *w_auto.last().unwrap();
    assert!(
        (last_auto - want).abs() < 0.02,
        "{label} steady state must be about {want:.4} V, got {last_auto} \
         (0 V would mean the device was silently dropped)"
    );
    let max_abs = max_abs_diff(w_off, w_auto);
    assert!(
        max_abs < 5e-3,
        "Auto vs Off diverged on the {label} waveform: {max_abs:.3e}"
    );
}

/// Largest sample-for-sample absolute difference between two series.
pub fn max_abs_diff(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .fold(0.0f64, |m, (x, y)| m.max((x - y).abs()))
}

/// Compare two waveform sets sample-for-sample over every node. Returns
/// `(max_abs_err, worst_tol_ratio)` where the ratio is `|a-b| / (reltol *
/// max(|a|,|b|) + vntol)`; a ratio <= 1.0 means "within solver tolerance".
pub fn max_deviation(a: &Waveforms, b: &Waveforms, reltol: f64, vntol: f64) -> (f64, f64) {
    assert_eq!(
        a.time.len(),
        b.time.len(),
        "runs produced different sample grids"
    );
    assert_eq!(a.node_voltages.len(), b.node_voltages.len());
    let mut max_abs = 0.0f64;
    let mut worst_ratio = 0.0f64;
    for (wa, wb) in a.node_voltages.iter().zip(b.node_voltages.iter()) {
        for (&va, &vb) in wa.iter().zip(wb.iter()) {
            assert!(va.is_finite() && vb.is_finite(), "non-finite sample");
            let err = (va - vb).abs();
            let bound = reltol.mul_add(va.abs().max(vb.abs()), vntol);
            max_abs = max_abs.max(err);
            worst_ratio = worst_ratio.max(err / bound);
        }
    }
    (max_abs, worst_ratio)
}

/// First-order-hold sample of a series at time `t`, clamped at the ends.
pub fn lerp_at(times: &[f64], vals: &[f64], t: f64) -> f64 {
    match times.binary_search_by(|x| x.partial_cmp(&t).unwrap()) {
        Ok(i) => vals[i],
        Err(0) => vals[0],
        Err(i) if i >= times.len() => *vals.last().unwrap(),
        Err(i) => {
            let (t0, t1) = (times[i - 1], times[i]);
            ((t - t0) / (t1 - t0)).mul_add(vals[i] - vals[i - 1], vals[i - 1])
        }
    }
}

/// Tight fixed-step options matching `benches/graded_boards.rs`, so the
/// partitioned-solve tests drive the engine identically to the benchmark:
/// fixed step (the partitioned path is fixed-step-only today) with tight
/// tolerances so the numbers are the converged operating point.
pub fn tight_opts(partitioning: Partitioning, dt: f64) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning,
        ..SolverOptions::default()
    }
}
