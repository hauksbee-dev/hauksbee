//! Torn-vs-monolithic exactness for the §3.2 BJT physics (dev-plan 04).
//!
//! A series-resistance BJT owns device-private INTERNAL unknowns that exist
//! only in a `Layout`, never in the netlist: the partitioner cannot see them,
//! so a torn island containing the BJT re-allocates them locally in its own
//! sub-layout (`Layout::new` on the sub-circuit) — island membership is by
//! construction, not by analysis. A charge-storing BJT additionally banks two
//! junction charges in `ReactiveState`, which the partitioned driver must
//! seed and advance exactly like the monolithic one (a sub-island BJT reading
//! zero charge history every step is the failure mode the diode arc's §3.1
//! mirror arms guard against).
//!
//! This builds the flagship rail-tear shape — PNP mirrors with emitters on a
//! shunt-fed ANALOG_VDD — but with models carrying rb/re/rc AND cje/cjc/tf,
//! so the torn path exercises internal-node sub-layouts and both charge banks
//! at once (PNP polarity also pins the charge sign-folding). The tear must
//! fire, the array must fragment, and the torn transient must match the
//! monolithic reference at every membrane, reference and rail node.

use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, Polarity, SourceKind};
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};

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
    c.add(Device::Vsource {
        name: "V5".into(),
        p: p5,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    let avdd = c.node("ANALOG_VDD");
    c.add(Device::Resistor {
        name: "R_shunt".into(),
        a: p5,
        b: avdd,
        ohms: 1e3,
        tc1: None,
    });
    let model = series_charge_model();
    let mut membranes = Vec::new();
    for k in 0..n {
        let rref = c.node(&format!("ref{k}"));
        c.add(Device::Resistor {
            name: format!("RR{k}"),
            a: avdd,
            b: rref,
            ohms: 10e3,
            tc1: None,
        });
        c.add(Device::Bjt {
            name: format!("Q1_{k}"),
            c: rref,
            b: rref,
            e: avdd,
            model,
        });
        let mem = c.node(&format!("mem{k}"));
        c.add(Device::Bjt {
            name: format!("Q2_{k}"),
            c: mem,
            b: rref,
            e: avdd,
            model,
        });
        // Membrane RC load, so the mirror current integrates into a real
        // waveform and the charge banks see moving junction voltages.
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

fn run(c: &Circuit, part: Partitioning, tstop: f64, dt: f64) -> hauksbee_solve::Waveforms {
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
    Transient::new(opts).run(c, tstop).unwrap()
}

/// EXACTNESS GATE (§3.2): the torn solve of a series-R + charge-storing BJT
/// array must reproduce the monolithic answer at every probed node.
#[test]
fn torn_series_r_bjt_matches_monolithic() {
    let blocks = 12;
    let (c, membranes) = build_array(blocks);

    // The tear must fire and fragment the array (same predicate the rail-tear
    // suite pins), proving the series-R BJTs did not confuse the partitioner:
    // their internal unknowns are layout-private and invisible to it.
    let part = hauksbee_solve::Partition::analyze_with_tears(&c);
    assert_eq!(part.tears.len(), 1, "expected exactly one ANALOG_VDD tear");
    let nl_islands = part.islands.iter().filter(|i| !i.linear).count();
    assert!(
        nl_islands >= blocks,
        "tear did not fragment the array: {nl_islands} nonlinear islands for {blocks} blocks"
    );

    let dt = 1e-6;
    let tstop = 60e-6;
    let mono = run(&c, Partitioning::Off, tstop, dt);
    let torn = run(&c, Partitioning::Auto, tstop, dt);

    let mut names: Vec<String> = membranes;
    names.push("ANALOG_VDD".to_string());
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
