//! S4 DETERMINISM GATE.
//!
//! The parallel island sweep is an explicit double-buffered Jacobi exchange:
//! every island reads a frozen previous-generation buffer and writes owned
//! outputs to disjoint slots, so the accepted waveforms must be BIT-IDENTICAL
//! across thread counts, not close, identical. If this test ever fails, a
//! write is aliasing between islands or a read is leaking across the buffer
//! swap; that is a real bug in the sweep, never noise to explain away.
//!
//! Every graded benchmark board (§9.1 fixtures) runs at the sequential
//! reference (`ParallelPolicy::Off`) and at pinned pools of 1, 2, 4, and 8
//! threads (`ParallelPolicy::Threads(n)`, which force-engages the pool even
//! where `Auto` would decline, so small boards prove the buffer discipline
//! too). All waveforms are compared to the sequential run with `f64::to_bits`
//! equality. This is a plain `#[test]`: it runs on every `cargo test`, no
//! flags, exactly because it is the load-bearing gate for the whole stage.

use crate::shared::{add_capacitor, add_vdc};
use hauksbee_ir::{Circuit, NodeId};
use hauksbee_solve::{
    AssemblyMode, ParallelPolicy, Partitioning, SolverOptions, Transient, Waveforms,
};
use hauksbee_testkit::circuits::res;

use crate::fixtures::{build_rc_ladder, build_shunt_array};
use crate::shared::tight_opts;

/// The thread counts the gate pins, per the plan. `Off` (run separately) is
/// the sequential reference every pooled run must match bit-for-bit.
const THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];

fn opts(dt: f64, parallel: ParallelPolicy) -> SolverOptions {
    SolverOptions {
        parallel,
        ..tight_opts(Partitioning::Auto, dt)
    }
}

fn run(c: &Circuit, tstop: f64, dt: f64, parallel: ParallelPolicy) -> Waveforms {
    Transient::new(opts(dt, parallel))
        .run(c, tstop)
        .expect("board solves")
}

/// Bit-exact waveform comparison: every sample time and every node voltage,
/// compared through `to_bits` so -0.0/0.0 or last-ulp drift cannot hide.
fn assert_bit_identical(reference: &Waveforms, got: &Waveforms, board: &str, threads: usize) {
    assert_eq!(
        reference.time.len(),
        got.time.len(),
        "{board} @ {threads} threads: step count diverged"
    );
    for (k, (a, b)) in reference.time.iter().zip(got.time.iter()).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "{board} @ {threads} threads: time diverged at sample {k}: {a:?} vs {b:?}"
        );
    }
    assert_eq!(reference.node_voltages.len(), got.node_voltages.len());
    for (node, (wa, wb)) in reference
        .node_voltages
        .iter()
        .zip(got.node_voltages.iter())
        .enumerate()
    {
        for (k, (a, b)) in wa.iter().zip(wb.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "{board} @ {threads} threads: node {node} diverged at sample {k}: {a:?} vs {b:?}"
            );
        }
    }
}

fn check_board(name: &str, c: &Circuit, tstop: f64, dt: f64) {
    let reference = run(c, tstop, dt, ParallelPolicy::Off);
    for &threads in &THREAD_COUNTS {
        let got = run(c, tstop, dt, ParallelPolicy::Threads(threads));
        assert_bit_identical(&reference, &got, name, threads);
        println!(
            "[determinism] {name}: {threads} thread(s) bit-identical to sequential over {} samples x {} nodes",
            reference.time.len(),
            reference.node_voltages.len()
        );
    }
}

/// A fan of independent RC legs off one ideal rail: `n` LINEAR islands (the
/// source pin is the only shared node), exercising the linear phase-(a)
/// parallel path, which the mirror arrays (all-nonlinear islands) do not.
#[allow(clippy::cast_precision_loss)] // leg index < n, tiny
fn build_rc_fan(n: usize) -> Circuit {
    let mut c = Circuit::new();
    let rail = c.node("rail");
    add_vdc(&mut c, "V1", rail, NodeId::GROUND, 1.0);
    for k in 0..n {
        let mid = c.node(&format!("leg{k}"));
        // stagger so legs are not numerically identical
        res(&mut c, &format!("R{k}"), rail, mid, 1e3 + k as f64);
        add_capacitor(
            &mut c,
            &format!("C{k}"),
            mid,
            NodeId::GROUND,
            1e-9,
            Some(0.0),
        );
    }
    c
}

/// The graded mirror arrays (torn path: every block re-solves inside the rail
/// balance, which is the parallel hot loop on board-shaped circuits).
#[test]
fn mirror_arrays_bit_identical_across_thread_counts() {
    for &blocks in &[24usize, 90, 240] {
        let (c, _hold_nodes) = build_shunt_array(blocks);
        // Short but real march: enough steps for the rail balance to work
        // per step; the property is per-step, not per-duration.
        check_board(&format!("mirror_array/{blocks}"), &c, 20e-6, 1e-6);
    }
}

/// The linear-island fan (linear phase-(a) parallel path).
#[test]
fn rc_fan_bit_identical_across_thread_counts() {
    let c = build_rc_fan(40);
    check_board("rc_fan/40", &c, 20e-6, 1e-6);
}

/// The RC ladder guard board. One big linear island: `try_build` declines and
/// the monolithic engine runs regardless of thread policy, so this asserts
/// the policy cannot leak into the fallback path either.
#[test]
fn rc_ladder_bit_identical_across_thread_counts() {
    let c = build_rc_ladder(200);
    check_board("rc_ladder/200", &c, 20e-6, 1e-6);
}

/// S3 composition under S4: islands inherit `AssemblyMode::Planned` through
/// `clone_remapped` (their sub-workspaces compile their own `StampPlans`), and
/// that must be exactly as thread-count-invariant as the interpreted path.
#[test]
fn planned_assembly_islands_bit_identical_across_thread_counts() {
    let (c, _hold_nodes) = build_shunt_array(90);
    let dt = 1e-6;
    let tstop = 20e-6;
    let planned = |parallel: ParallelPolicy| {
        Transient::new(SolverOptions {
            assembly: AssemblyMode::Planned,
            ..opts(dt, parallel)
        })
        .run(&c, tstop)
        .expect("planned torn run")
    };
    let reference = planned(ParallelPolicy::Off);
    for &threads in &THREAD_COUNTS {
        let got = planned(ParallelPolicy::Threads(threads));
        assert_bit_identical(&reference, &got, "mirror_array/90+planned", threads);
        println!(
            "[determinism] mirror_array/90+planned: {threads} thread(s) bit-identical to sequential"
        );
    }
}
