//! S3 acceptance gate:
//! `AssemblyMode::Planned` (two-tier compiled assembly through the `StampPlan`)
//! must match the interpreted reference to SOLVER TOLERANCE on the graded
//! boards, reltol/vntol, the run's own convergence convention, not a bespoke
//! bound. Bit-identity is deliberately NOT required here: the two-tier split
//! reorders the floating-point accumulation, which legitimately moves the last
//! bits. The bit-for-bit guarantee belongs to `Partitioning::Off` +
//! `AssemblyMode::Interpreted` (the default), which these tests leave alone
//! and the ngspice/KiCad/analytic suites pin.
//!
//! Run with `--nocapture` to see the measured deviations (the report numbers).

use hauksbee_ir::{Circuit, Device, NodeId};
use hauksbee_solve::{AssemblyMode, Partitioning, SolverOptions, Transient};

use crate::fixtures::{build_rc_ladder, build_shunt_array};
use crate::shared::tight_opts;
use crate::shared::{add_capacitor, add_resistor, add_vdc, max_deviation};

/// The graded-board options from `benches/graded_boards.rs` /
/// `tests/rail_tear.rs`, so this gate drives the engine exactly as the
/// benchmark does. Tight reltol/vntol (1e-9): the bound below inherits it.
fn opts(part: Partitioning, dt: f64, assembly: AssemblyMode) -> SolverOptions {
    SolverOptions {
        assembly,
        ..tight_opts(part, dt)
    }
}

/// Run `c` to `tstop` interpreted and planned under `part`, and require the
/// two waveforms to agree within the solver's own tolerance.
fn assert_planned_matches(label: &str, c: &Circuit, part: Partitioning, dt: f64, tstop: f64) {
    let run = |assembly| {
        Transient::new(opts(part, dt, assembly))
            .run(c, tstop)
            .unwrap_or_else(|e| panic!("{label} {assembly:?} did not converge: {e:?}"))
    };
    let (interp, planned) = (run(AssemblyMode::Interpreted), run(AssemblyMode::Planned));
    let (max_abs, ratio) = max_deviation(&interp, &planned, 1e-9, 1e-9);
    println!("{label} planned-vs-interpreted: max|dV|={max_abs:.3e} worst tol ratio={ratio:.3e}");
    assert!(
        ratio <= 1.0,
        "planned assembly out of solver tolerance on {label}: ratio {ratio}"
    );
}

/// RC ladder (linear backbone board): monolithic `Off`, planned vs
/// interpreted. Exercises the resistor cond-ops, the capacitor reactive tier,
/// the Vsource incidence constants, and the RHS-only history re-stamp.
#[test]
fn planned_matches_interpreted_rc_ladder() {
    let c = build_rc_ladder(300);
    assert_planned_matches("rc_ladder_300", &c, Partitioning::Off, 1e-7, 20e-6);
}

/// 24-block shunt-fed mirror array (nonlinear board): monolithic `Off`,
/// planned vs interpreted. Exercises the BJT tier-2 slotted re-stamp against
/// a hub-shaped rail row.
#[test]
fn planned_matches_interpreted_mirror_array() {
    let (c, _hold_nodes) = build_shunt_array(24);
    assert_planned_matches("mirror_24/mono", &c, Partitioning::Off, 1e-6, 60e-6);
}

/// Same mirror array through the PARTITIONED path (`Auto`): every nonlinear
/// island builds its own Workspace/StampPlan via `clone_remapped`, so this
/// pins the plan-§5.4 requirement that islands get the two-tier assembly for
/// free and still match their interpreted counterpart to tolerance.
#[test]
fn planned_matches_interpreted_partitioned_islands() {
    let (c, _hold_nodes) = build_shunt_array(24);
    assert_planned_matches("mirror_24/torn", &c, Partitioning::Auto, 1e-6, 60e-6);
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
    add_vdc(&mut c, "V1", vin, NodeId::GROUND, 1.0);
    add_resistor(&mut c, "R1", vin, n1, 50.0);
    c.add(Device::inductor("L1", n1, out, 1e-6));
    add_capacitor(&mut c, "C1", out, NodeId::GROUND, 1e-9, None);
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
        let ratio = (a - b).abs() / reltol.mul_add(a.abs().max(b.abs()), vntol);
        println!("adaptive_rlc {name}: interpreted={a:.9} planned={b:.9} tol ratio={ratio:.3e}");
        worst = worst.max(ratio);
    }
    assert!(
        worst <= 1.0,
        "planned adaptive endpoint out of solver tolerance: ratio {worst}"
    );
}
