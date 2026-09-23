//! A failed analog chunk must carry its cause, not just its extent.
//!
//! `solve_chunk` used to discard the solver's error string (`Err(_)`), so a
//! non-convergent run reported failed windows with no reason attached, and
//! finding the offending element meant manual bisection. The scheduler now
//! keeps the most recent solver message (device-named when a behavioural
//! fault caused the failure) and exposes it alongside the failed windows.

use crate::support::impossible_board;

use hauksbee_engine::scheduler::Scheduler;
use hauksbee_solve::SolverOptions;

#[test]
fn failed_chunk_retains_the_solver_error() {
    let mut sched = Scheduler::new(impossible_board(), None, SolverOptions::default())
        .expect("build scheduler for the impossible board");
    assert!(
        sched.last_solve_error().is_none(),
        "a fresh run carries no failure reason"
    );
    sched.step(1e-4);
    assert!(sched.failed_chunk_count() >= 1, "the chunk must fail");
    let reason = sched
        .last_solve_error()
        .expect("a failed chunk keeps the solver's message");
    assert!(
        !reason.trim().is_empty(),
        "the retained reason is a real message, got {reason:?}"
    );

    // A run reset clears the stale reason with the rest of the accounting.
    sched.reset_run_state().unwrap();
    assert!(
        sched.last_solve_error().is_none(),
        "reset_run_state clears the failure reason"
    );
}
