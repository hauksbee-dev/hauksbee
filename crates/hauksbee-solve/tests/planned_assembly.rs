//! S3 acceptance gate (`docs/dev-plans/03-solver-performance.md` §5, §11):
//! `AssemblyMode::Planned` (two-tier compiled assembly through the StampPlan)
//! must match the interpreted reference to SOLVER TOLERANCE on the graded
//! boards — reltol/vntol, the run's own convergence convention, not a bespoke
//! bound. Bit-identity is deliberately NOT required here: the two-tier split
//! reorders the floating-point accumulation, which legitimately moves the last
//! bits. The bit-for-bit guarantee belongs to `Partitioning::Off` +
//! `AssemblyMode::Interpreted` (the default), which these tests leave alone
//! and the ngspice/KiCad/analytic suites pin.
//!
//! Run with `--nocapture` to see the measured deviations (the report numbers).

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{
    AssemblyMode, Integration, Partitioning, SolverOptions, StepControl, Transient, Waveforms,
};

#[path = "../benches/fixtures.rs"]
mod fixtures;
use fixtures::{build_rc_ladder, build_shunt_array};

/// The graded-board options from `benches/graded_boards.rs` /
/// `tests/rail_tear.rs`, so this gate drives the engine exactly as the
/// benchmark does. Tight reltol/vntol (1e-9): the bound below inherits it.
fn opts(part: Partitioning, dt: f64, assembly: AssemblyMode) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning: part,
        assembly,
        ..SolverOptions::default()
    }
}

/// Compare two waveform sets sample-for-sample over every node. Returns
/// `(max_abs_err, worst_tol_ratio)` where the ratio is `|a-b| / (reltol *
/// max(|a|,|b|) + vntol)`; a ratio <= 1.0 means "within solver tolerance".
fn max_deviation(a: &Waveforms, b: &Waveforms, reltol: f64, vntol: f64) -> (f64, f64) {
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
            let bound = reltol * va.abs().max(vb.abs()) + vntol;
            max_abs = max_abs.max(err);
            worst_ratio = worst_ratio.max(err / bound);
        }
    }
    (max_abs, worst_ratio)
}

/// RC ladder (linear backbone board): monolithic `Off`, planned vs
/// interpreted. Exercises the resistor cond-ops, the capacitor reactive tier,
/// the Vsource incidence constants, and the RHS-only history re-stamp.
#[test]
fn planned_matches_interpreted_rc_ladder() {
    let c = build_rc_ladder(300);
    let dt = 1e-7;
    let tstop = 20e-6;
    let interp = Transient::new(opts(Partitioning::Off, dt, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("interpreted rc ladder converged");
    let planned = Transient::new(opts(Partitioning::Off, dt, AssemblyMode::Planned))
        .run(&c, tstop)
        .expect("planned rc ladder converged");
    let (max_abs, ratio) = max_deviation(&interp, &planned, 1e-9, 1e-9);
    println!(
        "rc_ladder_300 planned-vs-interpreted: max|dV|={max_abs:.3e} worst tol ratio={ratio:.3e}"
    );
    assert!(
        ratio <= 1.0,
        "planned assembly out of solver tolerance on the RC ladder: ratio {ratio}"
    );
}

/// 24-block shunt-fed mirror array (nonlinear board): monolithic `Off`,
/// planned vs interpreted. Exercises the BJT tier-2 slotted re-stamp against
/// a hub-shaped rail row.
#[test]
fn planned_matches_interpreted_mirror_array() {
    let (c, _membranes) = build_shunt_array(24);
    let dt = 1e-6;
    let tstop = 60e-6;
    let interp = Transient::new(opts(Partitioning::Off, dt, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("interpreted mirror converged");
    let planned = Transient::new(opts(Partitioning::Off, dt, AssemblyMode::Planned))
        .run(&c, tstop)
        .expect("planned mirror converged");
    let (max_abs, ratio) = max_deviation(&interp, &planned, 1e-9, 1e-9);
    println!(
        "mirror_24/mono planned-vs-interpreted: max|dV|={max_abs:.3e} worst tol ratio={ratio:.3e}"
    );
    assert!(
        ratio <= 1.0,
        "planned assembly out of solver tolerance on the mirror array: ratio {ratio}"
    );
}

/// Same mirror array through the PARTITIONED path (`Auto`): every nonlinear
/// island builds its own Workspace/StampPlan via `clone_remapped`, so this
/// pins the plan-§5.4 requirement that islands get the two-tier assembly for
/// free and still match their interpreted counterpart to tolerance.
#[test]
fn planned_matches_interpreted_partitioned_islands() {
    let (c, _membranes) = build_shunt_array(24);
    let dt = 1e-6;
    let tstop = 60e-6;
    let interp = Transient::new(opts(Partitioning::Auto, dt, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("interpreted torn mirror converged");
    let planned = Transient::new(opts(Partitioning::Auto, dt, AssemblyMode::Planned))
        .run(&c, tstop)
        .expect("planned torn mirror converged");
    let (max_abs, ratio) = max_deviation(&interp, &planned, 1e-9, 1e-9);
    println!(
        "mirror_24/torn planned-vs-interpreted: max|dV|={max_abs:.3e} worst tol ratio={ratio:.3e}"
    );
    assert!(
        ratio <= 1.0,
        "planned island assembly out of solver tolerance: ratio {ratio}"
    );
}

/// Adaptive-step series RLC: exercises the first-step backward-Euler
/// coefficient switch and per-step dt changes, i.e. the reactive tier's
/// `multiplier * coeffs.g` replay at a CHANGING integration factor, plus the
/// inductor branch constants. Adaptive grids can legitimately differ between
/// the two runs (last-bit differences move LTE decisions), so the gate is on
/// the settled endpoint, not the intermediate grid.
#[test]
fn planned_matches_interpreted_adaptive_rlc() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let n1 = c.node("n1");
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
        b: n1,
        ohms: 50.0,
        tc1: None,
    });
    c.add(Device::Inductor {
        name: "L1".into(),
        a: n1,
        b: out,
        henries: 1e-6,
        ic: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: None,
    });
    // ~5x the RLC settling time so both runs reach the same steady state.
    let tstop = 2e-6;
    let base = SolverOptions {
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let interp = Transient::new(SolverOptions {
        assembly: AssemblyMode::Interpreted,
        ..base
    })
    .run(&c, tstop)
    .expect("interpreted rlc converged");
    let planned = Transient::new(SolverOptions {
        assembly: AssemblyMode::Planned,
        ..base
    })
    .run(&c, tstop)
    .expect("planned rlc converged");
    let (reltol, vntol) = (base.reltol, base.vntol);
    let mut worst = 0.0f64;
    for name in ["in", "n1", "out"] {
        let a = interp.final_node(&c, name).expect("node present");
        let b = planned.final_node(&c, name).expect("node present");
        let ratio = (a - b).abs() / (reltol * a.abs().max(b.abs()) + vntol);
        println!("adaptive_rlc {name}: interpreted={a:.9} planned={b:.9} tol ratio={ratio:.3e}");
        worst = worst.max(ratio);
    }
    assert!(
        worst <= 1.0,
        "planned adaptive endpoint out of solver tolerance: ratio {worst}"
    );
}
