//! S1 allocation-hygiene gate: a counting `GlobalAlloc` wrapper, compiled
//! only under `cfg(test)`, proving the solver's per-step loops perform zero
//! heap allocations on a representative nonlinear board once warmed.
//!
//! The counter is thread-local so parallel unit tests never count each
//! other's allocations; the thread-locals are `const`-initialized `Cell`s of
//! `Copy` types, so touching them from inside `alloc` never allocates.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Passthrough `System` allocator that tallies alloc/realloc events on the
/// armed thread. Registered as the `#[global_allocator]` in `lib.rs`.
pub struct CountingAlloc;

#[inline]
fn bump() {
    ARMED.with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

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
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
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
    (COUNT.with(|c| c.get()), r)
}

#[cfg(test)]
mod tests {
    use super::fixtures;
    use super::*;
    use crate::newton::{dc_operating_point, newton_solve, Workspace};
    use crate::options::{Integration, Partitioning, SolverOptions, StepControl};
    use crate::partitioned::PartitionedTransient;
    use crate::stamp::IntegCoeffs;
    use crate::system::ReactiveState;

    fn audit_opts() -> SolverOptions {
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

    #[test]
    fn counter_observes_a_deliberate_allocation() {
        let (n, v) = count_allocs(|| {
            let mut v: Vec<u64> = Vec::new();
            v.push(1);
            v.push(2);
            v
        });
        assert!(n >= 1, "counter saw {n} allocations for a Vec growth");
        assert_eq!(v, vec![1, 2]);
    }

    /// The monolithic per-step Newton loop is allocation-free once warmed.
    #[test]
    fn monolithic_newton_per_step_loop_is_alloc_free() {
        let (circuit, _) = fixtures::build_shunt_array(90);
        let opts = audit_opts();
        let mut ws = Workspace::new(&circuit);
        dc_operating_point(&mut ws, &circuit, &opts).expect("mirror array DC op");
        let coeffs = IntegCoeffs::for_step(opts.integration, 1e-6, 1e-6, true);
        let state = ReactiveState::new(circuit.devices.len());
        for _ in 0..3 {
            newton_solve(
                &mut ws, &circuit, &opts, 0.0, 1e-6, coeffs, &state, true, false, opts.gmin, 1.0,
            );
        }
        let (allocs, iters) = count_allocs(|| {
            (0..50)
                .map(|_| {
                    newton_solve(
                        &mut ws, &circuit, &opts, 0.0, 1e-6, coeffs, &state, true, false,
                        opts.gmin, 1.0,
                    )
                    .iters
                })
                .sum::<usize>()
        });
        assert_eq!(
            allocs, 0,
            "monolithic Newton allocated {allocs} times over {iters} iterations"
        );
    }

    /// The partitioned per-step sweep (sequential arm) is allocation-free once
    /// warmed.
    #[test]
    fn partitioned_sweep_is_alloc_free() {
        let (circuit, _) = fixtures::build_shunt_array(90);
        let opts = SolverOptions {
            partitioning: Partitioning::Auto,
            parallel: crate::options::ParallelPolicy::Off,
            ..audit_opts()
        };
        let mut engine =
            PartitionedTransient::try_build(&circuit, &opts).expect("mirror array must partition");
        let h = 1e-6;
        engine
            .sweep_for_audit(&circuit, h, h, true)
            .expect("warm sweep");
        for k in 0..3 {
            engine
                .sweep_for_audit(&circuit, h, (k + 2) as f64 * h, false)
                .expect("warm relax sweep");
        }
        let (allocs, _) = count_allocs(|| {
            for k in 0..50 {
                engine
                    .sweep_for_audit(&circuit, h, (k + 10) as f64 * h, false)
                    .expect("measured sweep");
            }
        });
        assert_eq!(allocs, 0, "partitioned sweep allocated {allocs} times");
    }
}
