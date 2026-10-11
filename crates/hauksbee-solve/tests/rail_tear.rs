//! Exactness + speed test for the shunt-fed analog rail tear.
//!
//! A large current-mirror array fuses into one giant nonlinear island when
//! every block hangs off the *same* analog supply rail and that rail is NOT an
//! ideal source: it is fed from +5V through a current-sense shunt. So the rail
//! voltage sags with the total array current, coupling every block to every
//! other through that one shared node.
//!
//! This builds the same structure at `n` blocks and checks that the partitioned
//! (rail-tear) path reproduces the monolithic answer to <= 1e-6 and is faster.

use crate::shared::add_capacitor;
use hauksbee_ir::{Circuit, NodeId};
use hauksbee_solve::{Partitioning, Transient};
use std::time::Instant;

use crate::fixtures::build_shunt_array;
use crate::shared::tight_opts;

fn run(
    c: &Circuit,
    part: Partitioning,
    tstop: f64,
    dt: f64,
) -> (Transientish, std::time::Duration) {
    let t0 = Instant::now();
    let wf = Transient::new(tight_opts(part, dt)).run(c, tstop).unwrap();
    let dt_wall = t0.elapsed();
    (Transientish(wf), dt_wall)
}

struct Transientish(hauksbee_solve::Waveforms);

/// Solve the shunt-fed array both monolithically (the bit-exact oracle) and via
/// the rail-tear partition, and return the worst node-voltage disagreement plus
/// both wall times. `blocks` sets the array size; `tstop` the run length.
fn solve_both(blocks: usize, tstop: f64) -> (f64, f64, f64, usize, usize) {
    let (c, hold_nodes) = build_shunt_array(blocks);
    let dt = 1e-6;

    // The tear must be detected and the array must fragment per-block.
    let part = hauksbee_solve::Partition::analyze_with_tears(&c);
    assert_eq!(part.tears.len(), 1, "expected exactly one VDD_A tear");
    let nl_islands = part.islands.iter().filter(|i| !i.linear).count();
    assert!(
        nl_islands >= blocks,
        "tear did not fragment the array: {nl_islands} nonlinear islands for {blocks} blocks"
    );

    let (mono, mono_t) = run(&c, Partitioning::Off, tstop, dt);
    let (parted, part_t) = run(&c, Partitioning::Auto, tstop, dt);

    let mut names: Vec<String> = hold_nodes;
    names.push("VDD_A".to_string());
    for k in 0..blocks {
        names.push(format!("ref{k}"));
    }
    let mut max_abs = 0.0f64;
    for name in &names {
        let a = mono.0.final_node(&c, name).unwrap_or(0.0);
        let b = parted.0.final_node(&c, name).unwrap_or(0.0);
        max_abs = max_abs.max((a - b).abs());
    }
    (
        max_abs,
        mono_t.as_secs_f64(),
        part_t.as_secs_f64(),
        nl_islands,
        mono.0.time.len(),
    )
}

/// EXACTNESS GATE. A modest shunt-fed array, solved both ways over a full
/// transient, must agree with the monolithic reference to <= 1e-6 V at every
/// node. Runs in the default suite so a regression in the tear's exactness is
/// caught immediately. (Empirically the agreement is ~1e-13, i.e. floating-point
/// round-off; the bordered-block-diagonal tear is mathematically exact.)
#[test]
fn rail_tear_matches_monolithic_exactly() {
    let (max_abs, _mono_t, _part_t, nl, steps) = solve_both(24, 60e-6);
    println!("rail tear vs monolithic: {nl} blocks, {steps} steps, max |Δv| = {max_abs:.3e}");
    assert!(
        max_abs <= 1e-6,
        "rail-tear diverged from monolithic: {max_abs:.3e} (must be <= 1e-6)"
    );
}

/// CONSERVATIVE-CORRECTNESS GUARD. A rail-to-ground decoupling capacitor sits
/// directly on the rail (both terminals pinned after a tear), so it would be
/// dropped from every island and its current excluded from the balance. The
/// detector must REFUSE to tear such a rail (falling back to the exact
/// monolithic path) rather than silently diverge. Real boards carry many
/// such bypass caps on their analog rails, so this guard is load-bearing.
#[test]
fn rail_with_ground_bypass_cap_is_not_torn() {
    let (mut c, _m) = build_shunt_array(24);
    // Add a decoupling cap directly from the rail to ground.
    let avdd = c.node("VDD_A");
    add_capacitor(&mut c, "C_byp", avdd, NodeId::GROUND, 100e-9, Some(0.0));
    let part = hauksbee_solve::Partition::analyze_with_tears(&c);
    assert_eq!(
        part.tears.len(),
        0,
        "must refuse to tear a rail with a rail-to-ground bypass cap (would drop its current)"
    );

    // And the solve must still be exact (monolithic fallback).
    let dt = 1e-6;
    let (mono, _mt) = run(&c, Partitioning::Off, 30e-6, dt);
    let (auto, _at) = run(&c, Partitioning::Auto, 30e-6, dt);
    let mut max_abs = 0.0f64;
    for k in 0..24 {
        for nm in [format!("hold{k}"), format!("ref{k}")] {
            let a = mono.0.final_node(&c, &nm).unwrap_or(0.0);
            let b = auto.0.final_node(&c, &nm).unwrap_or(0.0);
            max_abs = max_abs.max((a - b).abs());
        }
    }
    let a = mono.0.final_node(&c, "VDD_A").unwrap_or(0.0);
    let b = auto.0.final_node(&c, "VDD_A").unwrap_or(0.0);
    max_abs = max_abs.max((a - b).abs());
    assert!(
        max_abs <= 1e-6,
        "fallback diverged from monolithic: {max_abs:.3e}"
    );
}

/// Speed/scaling benchmark (ignored: prints, no assert beyond exactness). The
/// tear's win grows with the fused island size, since it replaces one large
/// sparse factorization with many tiny independent ones plus a scalar balance.
#[test]
#[ignore = "benchmark: prints timings, run on demand"]
fn rail_tear_speed_scaling() {
    for &blocks in &[24usize, 90, 240] {
        let (max_abs, mono_t, part_t, nl, steps) = solve_both(blocks, 200e-6);
        println!(
            "  blocks={blocks:>4} ({nl} islands, {steps} steps): mono {:>8.1?}  tear {:>8.1?}  -> {:.2}x   max|Δv|={max_abs:.2e}",
            std::time::Duration::from_secs_f64(mono_t),
            std::time::Duration::from_secs_f64(part_t),
            mono_t / part_t.max(1e-9),
        );
        assert!(
            max_abs <= 1e-6,
            "exactness broke at {blocks} blocks: {max_abs:.3e}"
        );
    }
}
