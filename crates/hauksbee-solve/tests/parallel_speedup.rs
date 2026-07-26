//! S4 speedup measurement (ignored: prints a table, asserts only sanity).
//! Run with:
//!
//! ```sh
//! cargo test -p hauksbee-solve --release --test parallel_speedup -- --ignored --nocapture
//! ```
//!
//! Compares the torn mirror arrays under `ParallelPolicy::Off` (the sequential
//! reference, which does the same per-step work S3 shipped) against
//! `ParallelPolicy::Auto` (the S4 pool), with the monolithic solve for
//! context, and confirms the RC ladder guard board, where `Auto` correctly
//! declines to parallelize, pays nothing. Every timed run asserts the same
//! rail-sag sanity window as the criterion harness so a speed win that breaks
//! the numbers cannot look like a win.

use hauksbee_ir::Circuit;
use hauksbee_solve::{
    Integration, ParallelPolicy, Partitioning, SolverOptions, StepControl, Transient, Waveforms,
};
use std::time::Instant;

#[path = "../benches/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
use fixtures::{build_rc_ladder, build_shunt_array};

fn opts(dt: f64, part: Partitioning, parallel: ParallelPolicy) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning: part,
        parallel,
        ..SolverOptions::default()
    }
}

/// Best-of-N wall time: the minimum approximates the uncontended cost on a
/// machine with background load (other harnesses run on this box), which a
/// single sample or a mean cannot.
fn timed(c: &Circuit, tstop: f64, dt: f64, part: Partitioning, par: ParallelPolicy) -> (Waveforms, f64) {
    let mut best = f64::INFINITY;
    let mut wf = None;
    for _ in 0..5 {
        let t0 = Instant::now();
        let w = Transient::new(opts(dt, part, par)).run(c, tstop).expect("run");
        best = best.min(t0.elapsed().as_secs_f64());
        wf = Some(w);
    }
    (wf.unwrap(), best)
}

fn assert_mirror_sane(c: &Circuit, wf: &Waveforms) {
    let rail = wf.final_node(c, "ANALOG_VDD").expect("rail present");
    assert!(
        rail.is_finite() && rail > 4.99 && rail < 5.0,
        "rail out of sane sag window: {rail}"
    );
}

#[test]
#[ignore]
fn s4_parallel_speedup_table() {
    let dt = 1e-6;
    let tstop = 200e-6;
    println!("board            mono(Off)   torn+seq    torn+par    par vs seq   torn+par vs mono");
    for &blocks in &[24usize, 90, 240] {
        let (c, _m) = build_shunt_array(blocks);
        let (wf_mono, t_mono) = timed(&c, tstop, dt, Partitioning::Off, ParallelPolicy::Off);
        assert_mirror_sane(&c, &wf_mono);
        let (wf_seq, t_seq) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Off);
        assert_mirror_sane(&c, &wf_seq);
        let (wf_par, t_par) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Auto);
        assert_mirror_sane(&c, &wf_par);
        println!(
            "mirror/{blocks:<8} {:>9.2?}  {:>9.2?}  {:>9.2?}   {:>6.2}x      {:>6.2}x",
            std::time::Duration::from_secs_f64(t_mono),
            std::time::Duration::from_secs_f64(t_seq),
            std::time::Duration::from_secs_f64(t_par),
            t_seq / t_par.max(1e-12),
            t_mono / t_par.max(1e-12),
        );
    }

    // Thread-count scaling on the largest array (informs PAR_MAX_THREADS).
    let (c, _m) = build_shunt_array(240);
    print!("mirror/240 scaling: ");
    let (_, t1) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Threads(1));
    for &n in &[1usize, 2, 4, 8, 12] {
        let (wf, t) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Threads(n));
        assert_mirror_sane(&c, &wf);
        print!("{n}t {:.2?} ({:.2}x)  ", std::time::Duration::from_secs_f64(t), t1 / t);
    }
    println!();

    // Guard board: the RC ladder is a single large linear island, so the
    // partitioned engine declines and Auto parallelism must cost nothing.
    let c = build_rc_ladder(1000);
    let tstop = 100e-6;
    let (wf_off, t_off) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Off);
    let (wf_auto, t_auto) = timed(&c, tstop, dt, Partitioning::Auto, ParallelPolicy::Auto);
    // Bit-identical (monolithic fallback both ways) and no wall-time cliff.
    for (a, b) in wf_off
        .node_voltages
        .iter()
        .flatten()
        .zip(wf_auto.node_voltages.iter().flatten())
    {
        assert_eq!(a.to_bits(), b.to_bits(), "ladder fallback must not see the policy");
    }
    println!(
        "rc_ladder/1000: Off {:?} vs Auto {:?} ({:+.1}%)",
        std::time::Duration::from_secs_f64(t_off),
        std::time::Duration::from_secs_f64(t_auto),
        (t_auto - t_off) / t_off * 100.0
    );
}
