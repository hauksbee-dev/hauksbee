//! Co-sim chunks across comparator output transitions.
//!
//! An astable comparator loop (a diode-steered relaxation oscillator, and a
//! cascade where its flips drive a second comparator that feeds a
//! self-resetting integrate-and-fire stage) flips many times per chunk and
//! across chunk boundaries. Every chunk must be carried by the PRIMARY march,
//! continued from the previous chunk's end state: no failed chunk and no
//! fallback rung. The solve-level edge and period checks for the same boards
//! live in `hauksbee-solve/tests/comparator_flip.rs`.

use crate::support::bound_board;

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_ir::{Circuit, NodeId};
use hauksbee_solve::SolverOptions;
use hauksbee_testkit::circuits::{comparator_cascade, comparator_relaxation};

/// Runs `board` for `count` chunks of `chunk_s` on the default adaptive
/// options and returns the scheduler.
fn run(board: BoundBoard, chunk_s: f64, count: usize) -> Scheduler {
    let mut sched = Scheduler::new(board, None, SolverOptions::default()).expect("build scheduler");
    sched.chunk_s = chunk_s;
    for _ in 0..count {
        sched.step(chunk_s);
    }
    sched
}

fn assert_primary_only(sched: &Scheduler) {
    assert_eq!(sched.failed_chunk_count(), 0, "no chunk may fail");
    assert_eq!(
        sched.fallback_chunk_count(),
        0,
        "every chunk is the primary march's, windows {:?}",
        sched.fallback_windows()
    );
}

/// The span a net covered over the run's chunk ends.
fn span(sched: &Scheduler, net: &str) -> (f64, f64) {
    sched
        .net_range(net)
        .unwrap_or_else(|| panic!("{net} sampled"))
}

#[test]
fn relaxation_oscillator_chunks_march_on_the_primary_alone() {
    let mut c = Circuit::new();
    let (out, ct) = comparator_relaxation(&mut c, "", 7e-9);
    let board = bound_board("relaxation", c, &[("ct", ct), ("out", out)]);
    // 1 us chunks against a ~5.7 us period: flips inside chunks and on
    // their boundaries alike, each chunk continuing the last one's latch.
    let sched = run(board, 1e-6, 60);
    assert_primary_only(&sched);
    // Sampled at chunk ends, the cap stays in its 2.0..3.0 V band (plus what
    // it overshoots in a delay) and covers most of it, after the first
    // chunk's decay from the pinned t = 0 point at the ~3.6 V charge target.
    let (lo, hi) = span(&sched, "ct");
    assert!(
        lo > 1.95 && hi < 3.7 && lo < 2.3 && hi > 2.7,
        "ct spans {lo:.3}..{hi:.3} V"
    );
    let (lo, hi) = span(&sched, "out");
    assert!(
        lo < 0.1 && hi > 4.9,
        "out swings rail to rail: {lo:.3}..{hi:.3} V"
    );
}

#[test]
fn cascaded_comparator_chunks_march_on_the_primary_alone() {
    let c = comparator_cascade(7e-9);
    let nets: Vec<(&str, NodeId)> = ["out1", "o2", "m", "o3"]
        .into_iter()
        .map(|n| (n, c.find_node(n).expect("cascade net")))
        .collect();
    let board = bound_board("cascade", c, &nets);
    let sched = run(board, 1e-6, 60);
    assert_primary_only(&sched);
    let (lo, hi) = span(&sched, "m");
    assert!(
        lo < 1.0 && hi > 1.5 && hi < 2.2,
        "the membrane charges and resets: {lo:.3}..{hi:.3} V"
    );
}
