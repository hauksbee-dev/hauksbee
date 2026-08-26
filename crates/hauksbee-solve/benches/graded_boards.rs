//! S2 graded-board benchmark harness. Criterion micro/meso benchmarks over a ladder of board-shaped fixtures,
//! from a trivial linear RC ladder up to the 240-block shunt-fed mirror array.
//!
//! Two things every benchmark here does, on purpose:
//!
//!  1. A *fixed, small* workload (fixed step count, short `tstop`) so the whole
//!     suite finishes in about a minute and `cargo bench -- --test` smoke-runs
//!     each case exactly once in CI.
//!  2. A cheap CORRECTNESS ASSERT on a final node voltage (§9's load-bearing
//!     detail). A speed win that quietly breaks the numbers must not be able to
//!     look like a win. The asserts are deliberately LOOSE - sanity windows, not
//!     exactness. The exact gates (tear-vs-monolith to 1e-6, bit-identical
//!     across threads) live in the test suite (`tests/rail_tear.rs`), which is
//!     where a tight bound belongs; here we only catch a solve that has gone
//!     obviously wrong (collapsed rail, NaN, unconverged garbage).
//!
//! The mirror array is benchmarked at 24/90/240 blocks with BOTH
//! `Partitioning::Auto` (torn) and `Partitioning::Off` (monolithic reference) as
//! separate benchmark ids, so criterion's own table shows the torn-vs-mono ratio
//! directly. As of S2 that ratio is < 1 (the tear is currently slower; the S3/S4
//! optimizations that make it win have not landed). Capturing that honestly is
//! the entire point of standing the harness up before touching the numerics.

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};

use hauksbee_ir::Circuit;
use hauksbee_solve::{
    dc_operating_point, AssemblyMode, Integration, Partitioning, SolverOptions, StepControl,
    Transient, Waveforms, Workspace,
};

#[path = "fixtures.rs"]
mod fixtures;
use fixtures::{build_rc_ladder, build_shunt_array};

/// Tight solver options matching `tests/rail_tear.rs`, so a benchmark run and
/// the exactness test drive the engine identically. Fixed-step (the partitioned
/// path is fixed-step-only today) with tight tolerances so the numbers are the
/// converged operating point, not a loose transient.
fn opts(part: Partitioning, dt: f64) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning: part,
        ..SolverOptions::default()
    }
}

/// Same options with the S3 two-tier compiled assembly opted in
/// (`AssemblyMode::Planned`). Benchmarked as SEPARATE ids next to the
/// interpreted ones so criterion's table shows the compiled-assembly ratio
/// directly, exactly like the torn-vs-mono pairing above.
fn opts_planned(part: Partitioning, dt: f64) -> SolverOptions {
    SolverOptions {
        assembly: AssemblyMode::Planned,
        ..opts(part, dt)
    }
}

// --- RC ladder ------------------------------------------------------------

const RC_STAGES: usize = 1000;
const RC_DT: f64 = 1e-7; // 100 ns; stage tau is 1 us, so near stages charge.
const RC_TSTOP: f64 = 20e-6; // 200 fixed steps.

fn run_rc_ladder(c: &Circuit) -> Waveforms {
    Transient::new(opts(Partitioning::Off, RC_DT))
        .run(c, RC_TSTOP)
        .expect("rc ladder transient converged")
}

fn run_rc_ladder_planned(c: &Circuit) -> Waveforms {
    Transient::new(opts_planned(Partitioning::Off, RC_DT))
        .run(c, RC_TSTOP)
        .expect("rc ladder planned transient converged")
}

/// Sanity window for the RC ladder final state. The driven end (`n0`) charges
/// well toward the 1 V source over the run (~0.87 V at these constants); the far
/// end (`n999`) has barely moved. We only assert the qualitative shape: source
/// held, near end substantially charged and not overshooting, far end bounded
/// and non-negative, everything finite.
fn assert_rc_sane(c: &Circuit, wf: &Waveforms) {
    let vin = wf.final_node(c, "in").expect("in node present");
    let near = wf.final_node(c, "n0").expect("n0 node present");
    let far = wf.final_node(c, "n999").expect("n999 node present");
    assert!((vin - 1.0).abs() < 1e-9, "source node drifted: {vin}");
    assert!(
        near.is_finite() && near > 0.5 && near <= 1.0 + 1e-6,
        "rc near end out of sane window: {near}"
    );
    assert!(
        far.is_finite() && far >= -1e-6 && far <= 1.0 + 1e-6,
        "rc far end out of sane window: {far}"
    );
}

fn bench_rc_ladder(cr: &mut Criterion) {
    let mut group = cr.benchmark_group("rc_ladder");
    group.sample_size(30);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(6));
    // Build once, outside the timed loop: we benchmark the solve, not the
    // fixture construction.
    let c = build_rc_ladder(RC_STAGES);
    group.bench_function("rc_ladder_1k", |b| {
        b.iter(|| {
            let wf = run_rc_ladder(black_box(&c));
            assert_rc_sane(&c, &wf);
            black_box(wf.time.len())
        });
    });
    group.bench_function("rc_ladder_1k/planned", |b| {
        b.iter(|| {
            let wf = run_rc_ladder_planned(black_box(&c));
            assert_rc_sane(&c, &wf);
            black_box(wf.time.len())
        });
    });
    group.finish();
}

// --- Mirror array (torn vs monolithic) ------------------------------------

const MIRROR_DT: f64 = 1e-6;
const MIRROR_TSTOP: f64 = 60e-6; // 60 fixed steps; enough to reach the OP.
const MIRROR_SIZES: [usize; 3] = [24, 90, 240];

fn run_mirror(c: &Circuit, part: Partitioning) -> Waveforms {
    Transient::new(opts(part, MIRROR_DT))
        .run(c, MIRROR_TSTOP)
        .expect("mirror array transient converged")
}

fn run_mirror_planned(c: &Circuit, part: Partitioning) -> Waveforms {
    Transient::new(opts_planned(part, MIRROR_DT))
        .run(c, MIRROR_TSTOP)
        .expect("mirror array planned transient converged")
}

/// Sanity window for a shunt-fed mirror array. The rail is fed from +5 V through
/// a 1 kOhm shunt and MUST sag under load: at these device values it settles to
/// ~4.9988 V. A torn solve that dropped block currents from the balance would
/// leave the rail pinned at 5.0; a broken solve would NaN or collapse it. So the
/// load-bearing assert is "rail sags, but only a little": strictly below 5 V and
/// above 4.99. Membranes sit near 0 V here (tiny mirror currents), so we only
/// assert they are finite and bounded into the rail.
fn assert_mirror_sane(c: &Circuit, wf: &Waveforms) {
    let rail = wf.final_node(c, "ANALOG_VDD").expect("rail node present");
    let mem0 = wf.final_node(c, "mem0").expect("mem0 node present");
    assert!(
        rail.is_finite() && rail > 4.99 && rail < 5.0,
        "rail out of sane sag window: {rail}"
    );
    assert!(
        mem0.is_finite() && mem0 >= -1e-2 && mem0 <= 5.0,
        "membrane out of sane window: {mem0}"
    );
}

fn bench_mirror_array(cr: &mut Criterion) {
    let mut group = cr.benchmark_group("mirror_array");
    // The 240-block monolithic run is ~10 ms and the torn run ~20 ms, so a small
    // pinned sample keeps the whole sweep well under a minute while still giving
    // criterion a stable median.
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(6));
    for &n in &MIRROR_SIZES {
        let (c, _membranes) = build_shunt_array(n);
        // Torn (Partitioning::Auto) and monolithic (Off) as SEPARATE ids so the
        // ratio between them is visible straight out of criterion's report.
        group.bench_function(format!("{n}/torn"), |b| {
            b.iter(|| {
                let wf = run_mirror(black_box(&c), Partitioning::Auto);
                assert_mirror_sane(&c, &wf);
                black_box(wf.time.len())
            });
        });
        group.bench_function(format!("{n}/mono"), |b| {
            b.iter(|| {
                let wf = run_mirror(black_box(&c), Partitioning::Off);
                assert_mirror_sane(&c, &wf);
                black_box(wf.time.len())
            });
        });
        // S3 compiled assembly, same fixtures: mono and torn with
        // `AssemblyMode::Planned`, so the assembly win (and the island
        // version of it) reads straight off the criterion table.
        group.bench_function(format!("{n}/mono_planned"), |b| {
            b.iter(|| {
                let wf = run_mirror_planned(black_box(&c), Partitioning::Off);
                assert_mirror_sane(&c, &wf);
                black_box(wf.time.len())
            });
        });
        group.bench_function(format!("{n}/torn_planned"), |b| {
            b.iter(|| {
                let wf = run_mirror_planned(black_box(&c), Partitioning::Auto);
                assert_mirror_sane(&c, &wf);
                black_box(wf.time.len())
            });
        });
    }
    group.finish();
}

// --- DC operating point of the largest array ------------------------------

/// DC operating point only (no transient) of the 240-block array. Isolates the
/// cold-start Newton + homotopy cost that every transient pays once up front, on
/// the largest fixture. Reads the rail directly off the solved unknowns.
fn bench_dc_mirror(cr: &mut Criterion) {
    let mut group = cr.benchmark_group("dc_op");
    group.sample_size(30);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(6));
    let (mut c, _membranes) = build_shunt_array(240);
    let o = opts(Partitioning::Off, MIRROR_DT);
    // `node` is idempotent for an existing name: this returns the rail's id
    // without adding anything, so we can map it into the unknown vector below.
    let rail_id = c.node("ANALOG_VDD");
    group.bench_function("dc_mirror_240", |b| {
        b.iter(|| {
            let mut ws = Workspace::new(black_box(&c));
            dc_operating_point(&mut ws, &c, &o).expect("dc op converged");
            // Same sanity gate as the transient rail: the DC rail must sag into
            // the (4.99, 5.0) window, else the operating point is wrong.
            let rail = ws
                .layout
                .node(rail_id)
                .map(|idx| ws.x[idx])
                .expect("rail mapped into the unknown vector");
            assert!(
                rail.is_finite() && rail > 4.99 && rail < 5.0,
                "dc rail out of sane sag window: {rail}"
            );
            black_box(rail)
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_rc_ladder,
    bench_mirror_array,
    bench_dc_mirror
);
criterion_main!(benches);
