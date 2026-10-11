//! Torn-vs-monolithic exactness for the §3.2 BJT physics (dev-plan 04).
//!
//! A series-resistance BJT owns device-private INTERNAL unknowns that exist
//! only in a `Layout`, never in the netlist: the partitioner cannot see them,
//! so a torn island containing the BJT re-allocates them locally in its own
//! sub-layout (`Layout::new` on the sub-circuit), island membership is by
//! construction, not by analysis. A charge-storing BJT additionally banks two
//! junction charges in `ReactiveState`, which the partitioned driver must
//! seed and advance exactly like the monolithic one (a sub-island BJT reading
//! zero charge history every step is the failure mode the diode arc's §3.1
//! mirror arms guard against).
//!
//! This builds the large-board rail-tear shape, PNP mirrors with emitters on a
//! shunt-fed `VDD_A`, but with models carrying rb/re/rc AND cje/cjc/tf,
//! so the torn path exercises internal-node sub-layouts and both charge banks
//! at once (PNP polarity also pins the charge sign-folding). The tear must
//! fire, the array must fragment, and the torn transient must match the
//! monolithic reference at every load, reference and rail node.

use crate::shared::tight_opts;
use crate::shared::{add_capacitor, add_resistor, add_vdc};
use hauksbee_ir::{BjtModel, Circuit, NodeId, Polarity};
use hauksbee_solve::{Partitioning, Transient};
use hauksbee_testkit::circuits::bjt;

fn series_charge_model() -> BjtModel {
    BjtModel {
        polarity: Polarity::P,
        is: 1e-15,
        bf: 150.0,
        vaf: 80.0,
        nf: 1.0,
        rb: 100.0,
        re: 2.0,
        rc: 20.0,
        cje: 10e-12,
        cjc: 4e-12,
        tf: 400e-12,
        ..BjtModel::default()
    }
}

/// The shunt-fed mirror array (the rail-tear fixture's shape) with §3.2
/// physics on every transistor.
fn build_array(n: usize) -> (Circuit, Vec<String>) {
    let mut c = Circuit::new();
    let p5 = c.node("+5V");
    add_vdc(&mut c, "V5", p5, NodeId::GROUND, 5.0);
    let avdd = c.node("VDD_A");
    add_resistor(&mut c, "R_shunt", p5, avdd, 1e3);
    let model = series_charge_model();
    let mut hold_nodes = Vec::new();
    for k in 0..n {
        let rref = c.node(&format!("ref{k}"));
        add_resistor(&mut c, &format!("RR{k}"), avdd, rref, 10e3);
        bjt(&mut c, &format!("Q1_{k}"), rref, rref, avdd, &model);
        let hold = c.node(&format!("hold{k}"));
        bjt(&mut c, &format!("Q2_{k}"), hold, rref, avdd, &model);
        // Load RC, so the mirror current integrates into a real
        // waveform and the charge banks see moving junction voltages.
        add_resistor(&mut c, &format!("RH{k}"), hold, NodeId::GROUND, 47e3);
        add_capacitor(
            &mut c,
            &format!("CH{k}"),
            hold,
            NodeId::GROUND,
            1e-9,
            Some(0.0),
        );
        hold_nodes.push(format!("hold{k}"));
    }
    (c, hold_nodes)
}

fn run(c: &Circuit, part: Partitioning, tstop: f64, dt: f64) -> hauksbee_solve::Waveforms {
    Transient::new(tight_opts(part, dt)).run(c, tstop).unwrap()
}

/// EXACTNESS GATE (§3.2): the torn solve of a series-R + charge-storing BJT
/// array must reproduce the monolithic answer at every probed node.
#[test]
fn torn_series_r_bjt_matches_monolithic() {
    let blocks = 12;
    let (c, hold_nodes) = build_array(blocks);

    // The tear must fire and fragment the array (same predicate the rail-tear
    // suite pins), proving the series-R BJTs did not confuse the partitioner:
    // their internal unknowns are layout-private and invisible to it.
    let part = hauksbee_solve::Partition::analyze_with_tears(&c);
    assert_eq!(part.tears.len(), 1, "expected exactly one VDD_A tear");
    let nl_islands = part.islands.iter().filter(|i| !i.linear).count();
    assert!(
        nl_islands >= blocks,
        "tear did not fragment the array: {nl_islands} nonlinear islands for {blocks} blocks"
    );

    let dt = 1e-6;
    let tstop = 60e-6;
    let mono = run(&c, Partitioning::Off, tstop, dt);
    let torn = run(&c, Partitioning::Auto, tstop, dt);

    let mut names: Vec<String> = hold_nodes;
    names.push("VDD_A".to_string());
    for k in 0..blocks {
        names.push(format!("ref{k}"));
    }
    let mut max_abs = 0.0f64;
    let mut worst = String::new();
    for name in &names {
        let a = mono.final_node(&c, name).unwrap_or(0.0);
        let b = torn.final_node(&c, name).unwrap_or(0.0);
        if (a - b).abs() > max_abs {
            max_abs = (a - b).abs();
            worst = name.clone();
        }
    }
    println!(
        "torn series-R BJT array: {nl_islands} islands, {} steps, max |Δv| = {max_abs:.3e} at {worst}",
        mono.time.len()
    );
    assert!(
        max_abs <= 1e-6,
        "torn series-R/charge BJT diverged from monolithic: {max_abs:.3e} at {worst}"
    );
}
