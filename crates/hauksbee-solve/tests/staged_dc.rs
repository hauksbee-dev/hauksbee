//! Staged-DC convergence fallback on a stiff diode-laden network.
//!
//! The pulse-stretcher pathology that collapses the Tarski cold DC solve, in
//! miniature: stretch nodes that hang off an ideal driver through reverse-biased
//! signal diodes and a DC-open cap, so each is floating (gmin-defined) and the
//! cold Jacobian is ill-conditioned. The test checks the full diode circuit
//! converges to a finite, self-consistent operating point and that it matches
//! the diodes-off relaxed reference where that reference is physically valid
//! (the rail, and the floating nodes both solvers pin near 0).
//!
//! The staged path's load-bearing proof on the real board is the Tarski DC
//! probe (ANALOG_VDD: 0 V collapse -> 5.0 V); this test guards the mechanism in
//! isolation and that it does not regress the ordinary diode solve.

use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, SourceKind};
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

fn build(n_stage: usize, diode_is: f64) -> (Circuit, NodeId, Vec<NodeId>) {
    let mut c = Circuit::new();
    let rail = c.node("RAIL");
    c.add(Device::Vsource {
        name: "VR".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // Ideal driver held LOW at rest (a comparator output before it fires); the
    // stretch diodes hang off it through an output resistance, the Tarski
    // PinDriver topology that produced the singular Vsource branch.
    let drv_hidden = c.node("DRV_H");
    let drv = c.node("DRV");
    c.add(Device::Vsource { name: "VDRV".into(), p: drv_hidden, n: NodeId::GROUND, kind: SourceKind::Dc(0.0) });
    c.add(Device::Resistor { name: "RDRV".into(), a: drv_hidden, b: drv, ohms: 50.0, tc1: None });

    let model = DiodeModel { is: diode_is, n: 1.9, rs: 0.65, ..DiodeModel::default() };
    let mut stage_nodes = Vec::new();
    for i in 0..n_stage {
        let s = c.node(&format!("S{i}")); // stretch node, floating at rest
        c.add(Device::Diode { name: format!("Dfwd{i}"), a: drv, k: s, model });
        c.add(Device::Capacitor { name: format!("Cs{i}"), a: s, b: NodeId::GROUND, farads: 5.8e-9, ic: None });
        c.add(Device::Diode { name: format!("Drev{i}"), a: NodeId::GROUND, k: s, model });
        stage_nodes.push(s);
    }
    (c, rail, stage_nodes)
}

fn node_v(ws: &Workspace, node: NodeId) -> f64 {
    ws.layout.node(node).map(|i| ws.x[i]).unwrap_or(0.0)
}

#[test]
fn stiff_diode_dc_converges_to_finite_physical_root() {
    let opts = SolverOptions::default();

    // Diodes-off relaxed reference (Is ~ 0): the physical operating point with
    // every junction reverse-biased — rail at 5 V, stretch nodes floating ~0.
    let (cref, rail_n, nref) = build(40, 1e-18);
    let mut wref = Workspace::new(&cref);
    dc_operating_point(&mut wref, &cref, &opts).expect("relaxed reference converges");
    let rail_ref = node_v(&wref, rail_n);
    let stretch_ref: Vec<f64> = nref.iter().map(|&n| node_v(&wref, n)).collect();

    // Full stiff circuit with real 1N4148-grade junctions.
    let (cfull, rail_nf, nfull) = build(40, 4.352e-9);
    let mut wfull = Workspace::new(&cfull);
    dc_operating_point(&mut wfull, &cfull, &opts)
        .expect("the stiff diode circuit must converge to a DC operating point");
    let rail_full = node_v(&wfull, rail_nf);
    let stretch_full: Vec<f64> = nfull.iter().map(|&n| node_v(&wfull, n)).collect();

    assert!((rail_full - 5.0).abs() < 1e-6, "rail {rail_full} != 5 V");
    assert!((rail_ref - 5.0).abs() < 1e-6, "ref rail {rail_ref} != 5 V");

    // Both solvers reach the same floating-node operating point (these reverse-
    // biased, cap-isolated nodes are gmin-defined near 0 in both).
    for (i, (f, r)) in stretch_full.iter().zip(&stretch_ref).enumerate() {
        assert!(f.is_finite(), "stretch[{i}] not finite: {f}");
        assert!(
            (f - r).abs() < 0.05,
            "stretch[{i}] full {f} vs relaxed ref {r} disagree"
        );
    }
}
