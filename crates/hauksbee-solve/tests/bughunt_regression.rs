//! Regression tests for defects found in the 2026-07 bug hunt. Each test pins
//! a specific fixed defect so it cannot silently return.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{SolverOptions, StepControl, Transient};

fn rc(farads: f64) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
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
        farads,
        ic: Some(0.0),
    });
    c
}

/// Bug-hunt #2: the final Fixed step must not overshoot `tstop`. With dt = 1 ns
/// and tstop = 3.5 ns (not a multiple of dt), the last emitted sample used to
/// land at 4 ns, because the `dt_min` floor was applied AFTER the `tstop` clamp
/// — forcing the sub-dt final step back up to a full dt past the stop time.
#[test]
fn fixed_step_does_not_overshoot_tstop() {
    let c = rc(1e-9);
    let dt = 1e-9;
    let tstop = 3.5e-9;
    let opts = SolverOptions {
        step: StepControl::Fixed { dt },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, tstop).unwrap();
    let last = *wf.time.last().expect("at least one sample");
    assert!(
        last <= tstop + dt * 1e-6,
        "final sample time {last:.4e} s overshoots tstop {tstop:.4e} s (by {:.4e} s)",
        last - tstop
    );
}

/// Bug-hunt #3 (black-box): a zero-farad capacitor must not poison the solve
/// with NaN/Inf. The linear fast path used to divide the state matrix by the
/// capacitance; with the guard, the island falls back to the MNA path (0 F = an
/// open) and every emitted voltage stays finite.
#[test]
fn zero_farad_cap_yields_finite_waveform() {
    let c = rc(0.0);
    let opts = SolverOptions {
        step: StepControl::Fixed { dt: 1e-6 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 1e-4).unwrap();
    let out = wf.node(&c, "out").expect("out node");
    assert!(
        out.iter().all(|v| v.is_finite()),
        "zero-farad cap produced a non-finite node voltage: {out:?}"
    );
}
