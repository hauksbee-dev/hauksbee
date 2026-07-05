//! S1 allocation-hygiene enforcement gate (plan §4.4).
//!
//! A counting `GlobalAlloc` wrapper, compiled ONLY under `cfg(test)`, that
//! proves the solver's per-step loop performs **zero heap allocations** on a
//! representative nonlinear board after the S1 hoists (§4.1 `Symbolic::solve`
//! scratch, §4.2 dense-island `vfree`, §4.3 Newton `lin_point`/`prev_iterate`).
//!
//! Why a hand-rolled counter and not `stats_alloc`/`cap`: no sibling crate in
//! this workspace already depends on an allocation-counting crate, and the plan
//! (§4.4, §10) explicitly names "a counting `GlobalAlloc` wrapper compiled in
//! under a cfg" as the intended mechanism. So this adds zero dependencies.
//!
//! Why the counter is **thread-local**, not a global atomic: `cargo test` runs
//! the crate's unit tests in parallel on many threads sharing this one global
//! allocator. A global armed-flag would count every other test's allocations
//! that happen to land inside our measurement window, producing flaky false
//! positives. Arming per-thread means only the measuring thread's allocations
//! are counted, so the gate is robust regardless of what else is running. The
//! thread-locals are `const`-initialized `Cell`s of `Copy`, `Drop`-free types,
//! so accessing them from inside `alloc` never itself allocates or recurses.
//!
//! Why unit test (crate-internal) and not an integration test: the faithful
//! "per-step loop" is `newton::newton_solve` and `PartitionedTransient::sweep`,
//! both crate-private, and measuring through the public `Transient::run` would
//! also count the waveform-recording `Vec` growth (legitimate output storage,
//! not the solver hot path) and the one-time setup allocations. Driving the
//! per-step functions directly isolates exactly the loop the gate is about.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// When true on THIS thread, count each allocation into `COUNT`.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Per-thread allocation event tally (alloc + realloc growth).
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Passthrough `System` allocator that tallies alloc/realloc events on the
/// armed thread. Registered as the `#[global_allocator]` in `lib.rs` under
/// `#[cfg(test)]` only, so non-test builds are completely unaffected.
pub struct CountingAlloc;

#[inline]
fn bump() {
    ARMED.with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

// The representative nonlinear board (plan §4.4): the shunt-fed current-mirror
// array, shared verbatim with the S2 benchmark harness and the rail-tear
// exactness test. Included by path (relative to `src/`) for the same single-
// source reason those consumers use. Declared at module top level so the
// `#[path]` resolves against `src/`, not the nested `tests` inline module.
#[cfg(test)]
#[path = "../benches/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Frees are not allocations; do not count. (Reuse of a freed buffer is
        // exactly what the hoists achieve, so counting frees would be noise.)
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A `Vec` growth (e.g. a per-step `push`) reallocs; that IS a heap
        // allocation event on the hot path, so count it.
        bump();
        System.realloc(ptr, layout, new_size)
    }
}

/// Run `f` with per-thread allocation counting armed, returning the number of
/// alloc/realloc events that occurred during it.
fn count_allocs<R>(f: impl FnOnce() -> R) -> (u64, R) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    let n = COUNT.with(|c| c.get());
    (n, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::newton::{dc_operating_point, newton_solve, Workspace};
    use crate::options::{Integration, Partitioning, SolverOptions, StepControl};
    use crate::partitioned::PartitionedTransient;
    use crate::stamp::IntegCoeffs;
    use crate::system::ReactiveState;
    use super::fixtures;

    fn audit_opts() -> SolverOptions {
        // Mirror the rail_tear test's options so the board solves on the same
        // path the exactness gate exercises.
        SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Fixed { dt: 1e-6 },
            reltol: 1e-9,
            vntol: 1e-9,
            max_newton: 200,
            gmin: 1e-9,
            partitioning: Partitioning::Off,
            ..SolverOptions::default()
        }
    }

    /// First: prove the counter actually observes allocations, so a later
    /// "0 allocations" read means the loop is clean, not that the counter is
    /// broken/disarmed.
    #[test]
    fn counter_observes_a_deliberate_allocation() {
        let (n, v) = count_allocs(|| {
            // A heap Vec push that must realloc from empty.
            let mut v: Vec<u64> = Vec::new();
            v.push(1);
            v.push(2);
            v
        });
        assert!(n >= 1, "counter saw {n} allocations for a Vec growth; must be >= 1");
        assert_eq!(v, vec![1, 2]);
    }

    /// §4.1 + §4.3 GATE: the monolithic per-step Newton loop performs zero heap
    /// allocations on the 90-block mirror array once warmed. This is the loop
    /// that runs millions of times over a transient (one solve per Newton iter
    /// per step); the `Symbolic::solve` `y` buffer (§4.1) and the `lin_point`/
    /// `prev_iterate` clones (§4.3) were the allocations, now hoisted.
    #[test]
    fn monolithic_newton_per_step_loop_is_alloc_free() {
        let (circuit, _membranes) = fixtures::build_shunt_array(90);
        let opts = audit_opts();

        let mut ws = Workspace::new(&circuit);
        // Warm up: solve the DC operating point (this legitimately allocates in
        // setup/homotopy; counting is disarmed here).
        dc_operating_point(&mut ws, &circuit, &opts).expect("mirror array DC op");

        let n_dev = circuit.devices.len();
        let coeffs = IntegCoeffs::for_step(opts.integration, 1e-6, true);
        let state = ReactiveState::new(n_dev);

        // Warm the Newton path itself: from the operating point, a few re-solves
        // trigger any first-call lazy growth (there should be none, but this
        // guarantees the measured window is steady state).
        for _ in 0..3 {
            newton_solve(
                &mut ws, &circuit, &opts, 0.0, 1e-6, coeffs, &state, true, false, opts.gmin, 1.0,
            );
        }

        // MEASURE: repeated warmed Newton solves. Each runs the full per-iter
        // loop (snapshot lin_point -> stamp -> refactor -> solve -> converge),
        // exactly where §4.1 and §4.3 lived.
        let (allocs, iters) = count_allocs(|| {
            let mut total = 0usize;
            for _ in 0..50 {
                let r = newton_solve(
                    &mut ws, &circuit, &opts, 0.0, 1e-6, coeffs, &state, true, false, opts.gmin,
                    1.0,
                );
                total += r.iters;
            }
            total
        });

        println!(
            "[alloc-audit] monolithic Newton: {allocs} heap allocations over 50 solves ({iters} total Newton iterations) on the 90-block mirror array"
        );
        assert_eq!(
            allocs, 0,
            "monolithic per-step Newton loop allocated {allocs} times over {iters} iterations; the S1 hoists (§4.1/§4.3) must make it allocation-free"
        );
    }

    /// §4.2 (+ island §4.1) GATE: the partitioned per-step sweep performs zero
    /// heap allocations once warmed. The linear-island reconstruction reuses the
    /// pre-allocated `lin_vfree` buffer (§4.2) and every nonlinear island's inner
    /// Newton solve reuses its workspace `solve_scratch` (§4.1).
    ///
    /// Measured on the SEQUENTIAL execution arm (`ParallelPolicy::Off`): the
    /// S1 property under audit is the solver hot path's own buffers, which the
    /// S4 pooled arm runs identically (same per-island code, same scratch).
    /// The pool's work-stealing deques allocate as jobs are pushed — runtime
    /// machinery outside the solver's numerics — so auditing the pooled arm
    /// would count rayon internals, not solver leaks. The pooled arm's own
    /// gate is the §3.5 bit-identical determinism test.
    #[test]
    fn partitioned_sweep_is_alloc_free() {
        let (circuit, _membranes) = fixtures::build_shunt_array(90);
        let opts = SolverOptions {
            partitioning: Partitioning::Auto,
            parallel: crate::options::ParallelPolicy::Off,
            ..audit_opts()
        };

        let mut engine =
            PartitionedTransient::try_build(&circuit, &opts).expect("mirror array must partition");

        let h = 1e-6;
        // Warm: advance one real step (first=true) then relax a couple of times
        // (first=false), so island state and any lazy buffers are established.
        engine.sweep_for_audit(&circuit, h, h, true).expect("warm sweep");
        for k in 0..3 {
            engine
                .sweep_for_audit(&circuit, h, (k + 2) as f64 * h, false)
                .expect("warm relax sweep");
        }

        // MEASURE: steady relaxation sweeps (first=false), the per-step inner
        // loop. Covers §4.2 (linear islands, if any) and §4.1 (nonlinear island
        // Newton solves).
        let (allocs, _) = count_allocs(|| {
            for k in 0..50 {
                engine
                    .sweep_for_audit(&circuit, h, (k + 10) as f64 * h, false)
                    .expect("measured sweep");
            }
        });

        println!(
            "[alloc-audit] partitioned sweep: {allocs} heap allocations over 50 relaxation sweeps on the 90-block mirror array"
        );
        assert_eq!(
            allocs, 0,
            "partitioned per-step sweep allocated {allocs} times; the S1 hoists (§4.1/§4.2) must make it allocation-free"
        );
    }
}
