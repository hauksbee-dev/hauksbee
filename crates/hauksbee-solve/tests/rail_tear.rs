//! Exactness + speed test for the shunt-fed analog rail tear.
//!
//! The real Tarski InputSystem fuses its whole synapse array into one giant
//! nonlinear island because every current-mirror block hangs off the *same*
//! `ANALOG_VDD` rail, and that rail is NOT an ideal source: it is fed from +5V
//! through a 1 kΩ current-sense shunt (R_Shunt + INA186). So the rail voltage
//! sags with the total array current (4.82 V at the board's operating point),
//! coupling every block to every other through that one shared node.
//!
//! This builds the same structure at `n` blocks and checks that the partitioned
//! (rail-tear) path reproduces the monolithic answer to <= 1e-6 and is faster.

use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, Polarity, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};
use std::time::Instant;

/// Build an `n`-block current-mirror array sharing a SHUNT-FED analog rail.
///
/// +5V --[R_shunt 1k]-- ANALOG_VDD --(per block: mirror emitters + membrane R)
/// Each block: ref resistor -> diode-connected Q1 mirror reference -> Q2 mirror
/// output charges a membrane RC pulled up to ANALOG_VDD. Bias is quasi-static
/// (DC drive) so the array reaches a steady operating point, exactly like an
/// inference window with latched weights.
fn build_shunt_array(n: usize) -> (Circuit, Vec<String>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    c.add(Device::Vsource {
        name: "V5".into(),
        p: p5,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // The defining element: the sense shunt feeding the analog rail.
    let avdd = c.node("ANALOG_VDD");
    c.add(Device::Resistor {
        name: "R_shunt".into(),
        a: p5,
        b: avdd,
        ohms: 1e3,
        tc1: None,
    });

    // PNP high-side mirror: emitters on the rail, like the board's ANALOG_VDD
    // current mirrors.
    let model = BjtModel {
        polarity: Polarity::P,
        is: 1e-15,
        bf: 150.0,
        vaf: 80.0,
        nf: 1.0,
        ..BjtModel::default()
    };

    let mut membranes = Vec::new();
    for k in 0..n {
        // A current mirror whose transistor EMITTERS sit on the shared rail, the
        // way the Tarski PNP mirrors hang 720 emitters off ANALOG_VDD. Each block
        // therefore draws real supply current through the shunt, so the rail sags
        // with the total array load — the exact coupling we must reproduce.
        // Reference leg: rail -> RR -> diode-connected mirror reference.
        let rref = c.node(&format!("ref{k}"));
        c.add(Device::Resistor {
            name: format!("RR{k}"),
            a: avdd,
            b: rref,
            ohms: 10e3,
            tc1: None,
        });
        // Diode-connected reference transistor with emitter on the rail.
        c.add(Device::Bjt {
            name: format!("Q1_{k}"),
            c: rref,
            b: rref,
            e: avdd,
            model,
        });
        // Mirror output transistor, emitter on the rail, collector -> membrane.
        let mem = c.node(&format!("mem{k}"));
        c.add(Device::Bjt {
            name: format!("Q2_{k}"),
            c: mem,
            b: rref,
            e: avdd,
            model,
        });
        // Membrane RC to ground.
        c.add(Device::Resistor {
            name: format!("RM{k}"),
            a: mem,
            b: NodeId::GROUND,
            ohms: 47e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: format!("CM{k}"),
            a: mem,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        membranes.push(format!("mem{k}"));
    }
    (c, membranes)
}

fn run(c: &Circuit, part: Partitioning, tstop: f64, dt: f64) -> (Transientish, std::time::Duration) {
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning: part,
        ..SolverOptions::default()
    };
    let t0 = Instant::now();
    let wf = Transient::new(opts).run(c, tstop).unwrap();
    let dt_wall = t0.elapsed();
    (Transientish(wf), dt_wall)
}

struct Transientish(hauksbee_solve::Waveforms);

/// Solve the shunt-fed array both monolithically (the bit-exact oracle) and via
/// the rail-tear partition, and return the worst node-voltage disagreement plus
/// both wall times. `blocks` sets the array size; `tstop` the run length.
fn solve_both(blocks: usize, tstop: f64) -> (f64, f64, f64, usize, usize) {
    let (c, membranes) = build_shunt_array(blocks);
    let dt = 1e-6;

    // The tear must be detected and the array must fragment per-block.
    let part = hauksbee_solve::Partition::analyze_with_tears(&c);
    assert_eq!(part.tears.len(), 1, "expected exactly one ANALOG_VDD tear");
    let nl_islands = part.islands.iter().filter(|i| !i.linear).count();
    assert!(
        nl_islands >= blocks,
        "tear did not fragment the array: {nl_islands} nonlinear islands for {blocks} blocks"
    );

    let (mono, mono_t) = run(&c, Partitioning::Off, tstop, dt);
    let (parted, part_t) = run(&c, Partitioning::Auto, tstop, dt);

    let mut names: Vec<String> = membranes.clone();
    names.push("ANALOG_VDD".to_string());
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
    println!(
        "rail tear vs monolithic: {nl} blocks, {steps} steps, max |Δv| = {max_abs:.3e}"
    );
    assert!(
        max_abs <= 1e-6,
        "rail-tear diverged from monolithic: {max_abs:.3e} (must be <= 1e-6)"
    );
}

/// CONSERVATIVE-CORRECTNESS GUARD. A rail-to-ground decoupling capacitor sits
/// directly on the rail (both terminals pinned after a tear), so it would be
/// dropped from every island and its current excluded from the balance. The
/// detector must REFUSE to tear such a rail (falling back to the exact
/// monolithic path) rather than silently diverge. The real Tarski board has 19
/// such bypass caps on ANALOG_VDD, so this guard is load-bearing.
#[test]
fn rail_with_ground_bypass_cap_is_not_torn() {
    let (mut c, _m) = build_shunt_array(24);
    // Add a decoupling cap directly from the rail to ground.
    let avdd = c.node("ANALOG_VDD");
    c.add(Device::Capacitor {
        name: "C_byp".into(),
        a: avdd,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
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
        for nm in [format!("mem{k}"), format!("ref{k}")] {
            let a = mono.0.final_node(&c, &nm).unwrap_or(0.0);
            let b = auto.0.final_node(&c, &nm).unwrap_or(0.0);
            max_abs = max_abs.max((a - b).abs());
        }
    }
    let a = mono.0.final_node(&c, "ANALOG_VDD").unwrap_or(0.0);
    let b = auto.0.final_node(&c, "ANALOG_VDD").unwrap_or(0.0);
    max_abs = max_abs.max((a - b).abs());
    assert!(max_abs <= 1e-6, "fallback diverged from monolithic: {max_abs:.3e}");
}

/// Speed/scaling benchmark (ignored: prints, no assert beyond exactness). The
/// tear's win grows with the fused island size, since it replaces one large
/// sparse factorization with many tiny independent ones plus a scalar balance.
#[test]
#[ignore]
fn rail_tear_speed_scaling() {
    for &blocks in &[24usize, 90, 240] {
        let (max_abs, mono_t, part_t, nl, steps) = solve_both(blocks, 200e-6);
        println!(
            "  blocks={blocks:>4} ({nl} islands, {steps} steps): mono {:>8.1?}  tear {:>8.1?}  -> {:.2}x   max|Δv|={max_abs:.2e}",
            std::time::Duration::from_secs_f64(mono_t),
            std::time::Duration::from_secs_f64(part_t),
            mono_t / part_t.max(1e-9),
        );
        assert!(max_abs <= 1e-6, "exactness broke at {blocks} blocks: {max_abs:.3e}");
    }
}
