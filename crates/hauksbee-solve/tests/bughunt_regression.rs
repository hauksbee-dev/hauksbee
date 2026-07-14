//! Regression tests for defects found in the 2026-07 bug hunt. Each test pins
//! a specific fixed defect so it cannot silently return.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{Partitioning, SolverOptions, StepControl, Transient};

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

/// Build the RC divider used by the two Auto-vs-Off differential gates below,
/// with an optional `tc1` temperature coefficient on the series resistor.
/// V + R + C is exactly the topology `Partitioning::Auto` peels into a
/// one-state linear island, so the fast state-space path (not the monolithic
/// stamp) owns the resistor value and the step exponential.
fn rc_tc1(tc1: Option<f64>) -> Circuit {
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
        tc1,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 1e-6,
        ic: Some(0.0),
    });
    c
}

/// Run the circuit under the given partitioning and return (time, v(out)).
fn run_out(c: &Circuit, opts: SolverOptions, tstop: f64) -> (Vec<f64>, Vec<f64>) {
    let wf = Transient::new(opts).run(c, tstop).unwrap();
    let out = wf.node(c, "out").expect("out node").to_vec();
    (wf.time.clone(), out)
}

/// Bug-hunt #4: a `tc1` resistor classified into a linear island must be
/// temperature-derated exactly like the monolithic stamp derates it. The
/// island compiler used to bake the NOMINAL ohms into its A/B matrices, so at
/// `temperature_c` != 27 the default `Partitioning::Auto` waveform silently
/// disagreed with the `Partitioning::Off` reference (~95% relative error on
/// this fixture) — a wrong thermal-derating sweep with no error anywhere.
#[test]
fn tc1_resistor_matches_monolithic_in_linear_island() {
    // tc1 = 4000 ppm/K at 100 C scales R by 1 + 0.004 * 73 = 1.292: the two
    // paths' time constants differ by ~29% if the derating is dropped.
    let c = rc_tc1(Some(0.004));
    let dt = 1e-5;
    let tstop = 2e-3; // two nominal time constants, well into the knee
    let run = |partitioning: Partitioning| {
        run_out(
            &c,
            SolverOptions {
                step: StepControl::Fixed { dt },
                partitioning,
                temperature_c: 100.0,
                ..SolverOptions::default()
            },
            tstop,
        )
    };
    let (_, mono) = run(Partitioning::Off);
    let (_, auto) = run(Partitioning::Auto);
    assert_eq!(mono.len(), auto.len(), "sample counts must agree");
    let mut worst = 0.0f64;
    for (m, a) in mono.iter().zip(&auto) {
        worst = worst.max((m - a).abs() / m.abs().max(0.1));
    }
    assert!(
        worst < 1e-3,
        "Auto vs Off diverge under tc1 derating at 100 C: worst rel err {worst:.3e}"
    );
}
