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
use hauksbee_solve::{
    dc_operating_point, run_op, Probe, RobustnessLadder, SolverOptions, Strategy, Workspace,
};

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
    c.add(Device::Vsource {
        name: "VDRV".into(),
        p: drv_hidden,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    c.add(Device::Resistor {
        name: "RDRV".into(),
        a: drv_hidden,
        b: drv,
        ohms: 50.0,
        tc1: None,
    });

    let model = DiodeModel {
        is: diode_is,
        n: 1.9,
        rs: 0.65,
        ..DiodeModel::default()
    };
    let mut stage_nodes = Vec::new();
    for i in 0..n_stage {
        let s = c.node(&format!("S{i}")); // stretch node, floating at rest
        c.add(Device::Diode {
            name: format!("Dfwd{i}"),
            a: drv,
            k: s,
            model,
        });
        c.add(Device::Capacitor {
            name: format!("Cs{i}"),
            a: s,
            b: NodeId::GROUND,
            farads: 5.8e-9,
            ic: None,
        });
        c.add(Device::Diode {
            name: format!("Drev{i}"),
            a: NodeId::GROUND,
            k: s,
            model,
        });
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
    // every junction reverse-biased, rail at 5 V, stretch nodes floating ~0.
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

/// Build a diode-laden circuit (so the staged-DC path engages) carrying a STIFF
/// analog switch whose control is its own output (positive feedback) pinned just
/// above threshold by a bias diode, plus a population of decoy switches near
/// their knees. Self-deciding (the tanh + control tangent), the switch's on/off
/// chatters between staged outer passes on this stiff core, mirroring the
/// 4320-switch Tarski mesh limit cycle. The event-freeze outer loop holds each
/// switch's state fixed per inner solve and re-derives it between solves, so it
/// settles to a consistent root. There is a single physical root: the switch is
/// biased ON, so out latches to the conducting divider value.
fn build_switched(n_decoy: usize) -> (Circuit, NodeId, NodeId) {
    let mut c = Circuit::new();
    let rail = c.node("RAIL");
    c.add(Device::Vsource {
        name: "VR".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    let model = DiodeModel {
        is: 4.352e-9,
        n: 1.9,
        rs: 0.65,
        ..DiodeModel::default()
    };

    // A floating stretch node off the rail through a reverse diode + DC-open cap:
    // forces the staged path (relaxed-seed + branch_reg) to engage, exactly the
    // condition under which the switches limit-cycle on the real board.
    let s = c.node("STRETCH");
    c.add(Device::Diode {
        name: "Dr".into(),
        a: NodeId::GROUND,
        k: s,
        model,
    });
    c.add(Device::Capacitor {
        name: "Cs".into(),
        a: s,
        b: NodeId::GROUND,
        farads: 5.8e-9,
        ic: None,
    });

    // A fixed bias rail at 3 V for the switch controls.
    let bias = c.node("BIAS");
    c.add(Device::Vsource {
        name: "VB".into(),
        p: bias,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.0),
    });

    // Main switch: rail -> out through a STIFF switch with NEGATIVE feedback,
    // vctrl = bias - v(out). As out rises the switch turns OFF, so the loop has a
    // single self-consistent operating point sitting in the tanh knee. Stiff
    // (1e6 on/off ratio) so the self-deciding tanh chatters between staged passes
    // (out flips the switch which flips out); the limit cycle the freeze cures.
    let out = c.node("OUT");
    c.add(Device::VSwitch {
        name: "Smain".into(),
        a: rail,
        b: out,
        ctrl_p: bias,
        ctrl_n: out,
        von: 2.5,
        voff: 1.5,
        ron: 1.0,
        roff: 1e6,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1.0,
        tc1: None,
    });

    // Decoy switches sitting near their own knees with the same negative-feedback
    // control, each fed from `out` through a diode and loaded to ground: they
    // couple to the main node and add discrete states the event loop must settle.
    for i in 0..n_decoy {
        let d = c.node(&format!("DEC{i}"));
        c.add(Device::Diode {
            name: format!("Dd{i}"),
            a: out,
            k: d,
            model,
        });
        c.add(Device::VSwitch {
            name: format!("Sd{i}"),
            a: d,
            b: NodeId::GROUND,
            ctrl_p: bias,
            ctrl_n: d,
            von: 2.5,
            voff: 1.5,
            ron: 10.0,
            roff: 1e6,
        });
        c.add(Device::Resistor {
            name: format!("Rd{i}"),
            a: d,
            b: NodeId::GROUND,
            ohms: 1e4,
            tc1: None,
        });
    }
    (c, rail, out)
}

/// Stiff switch + diode core co-solves to a TRUE root with the control-node
/// Jacobian. This stresses the new VSwitch tangent on a circuit that also carries
/// the staged-DC diode pathology (floating reverse-diode cap node), and confirms
/// the switch settles on its negative-feedback knee with the rail/out KCL closed.
/// (The full event-freeze outer loop's load-bearing proof is the real Tarski
/// board; this miniature converges on the homotopy ladder, so it guards the
/// Jacobian + diode interaction without depending on the staged event path. The
/// ladder grants enable the dynamic-pivot LU + event loop so the path is
/// exercised if the ladder ever needs them.)
#[test]
fn multi_switch_core_converges_via_event_freeze() {
    // Enable the staged-DC dynamic-pivot LU and the event-freeze outer loop
    // (the load-bearing path for the switch-fused core): typed grants, no env.
    let mut opts = SolverOptions {
        ladder: RobustnessLadder::none()
            .with(Strategy::DynamicPivot)
            .with(Strategy::EventFreeze),
        ..Default::default()
    };
    // The SMOOTH analog pass element, which is the device whose control-node
    // Jacobian this test exercises. Every switch here sits in a negative-feedback
    // loop (vctrl = 3 - v(node)) chosen to park it part-way up its transition, and
    // an interior operating point like that is something only a continuous
    // conductance has. The default SPICE3 relay in this same loop is a relaxation
    // oscillator with no DC point at all -- closed it divides `out` to 2.5 V, which
    // is below the 1.5 V break threshold... in fact vctrl = 0.5 V, so it opens;
    // open, `out` falls to ~0 and vctrl = 3 V reopens the make threshold, so it
    // closes -- and the solver correctly refuses to report one. That refusal is
    // covered by `hysteretic_relay_in_positive_feedback_has_no_operating_point`.
    opts.effects.switch_model = hauksbee_solve::SwitchModel::Smooth;
    let (c, rail_n, out_n) = build_switched(8);

    let mut ws = Workspace::new(&c);
    let r = dc_operating_point(&mut ws, &c, &opts);

    r.expect("switched diode core must converge");

    let rail = node_v(&ws, rail_n);
    let out = node_v(&ws, out_n);
    assert!((rail - 5.0).abs() < 1e-3, "rail {rail} != 5 V");
    // The negative-feedback loop settles in the tanh knee (vctrl = 3 - out drives
    // the switch toward partial conduction): a real interior operating point well
    // above the all-off bias (~0) and below the full-on divider (2.5 V). A
    // relaxed-undecided / chattering solve never reaches a consistent interior
    // point here.
    assert!(
        (0.3..=2.4).contains(&out),
        "main switch should settle on the transition knee, got {out}"
    );

    // And it is a TRUE root, not a relaxed adoption: KCL closes at the solution.
    let res = ws.dc_residual_inf_norm(&c, &opts);
    assert!(
        res < 1e-6,
        "switched-core root residual should be ~0, got {res:e}"
    );
}

/// A circuit whose TRUE nonlinear DC has no consistent comparator state (the
/// output feeds its own inverting input through a diode: out HIGH forces the
/// input above the reference which forces out LOW, and vice versa), so every
/// rung of the homotopy/staged ladder limit-cycles. The RELAXED no-diode solve
/// converges (the 1 GΩ diode stand-in cuts the feedback, the input rests at 0 V
/// below the 2 V reference, out sits consistently HIGH), so `dc_solve` adopts
/// it as the power-on surrogate and returns Ok; the right contract for
/// transient seeding. At that adopted point the real diode is forward-biased by
/// nearly the full 5 V rail, so the KCL residual is astronomically wrong.
fn build_comparator_diode_chatter() -> Circuit {
    let mut c = Circuit::new();
    let model = DiodeModel {
        is: 4.352e-9,
        n: 1.9,
        rs: 0.65,
        ..DiodeModel::default()
    };

    let vref = c.node("VREF");
    c.add(Device::Vsource {
        name: "VREF".into(),
        p: vref,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });

    let out = c.node("OUT");
    let d_a = c.node("D_A");
    let fb = c.node("FB");
    c.add(Device::Comparator {
        name: "UCHAT".into(),
        out,
        inp: vref,
        inn: fb,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.003,
    });
    // out -> 1k -> diode -> fb -> 10k -> gnd: with out HIGH the diode conducts
    // and fb rises above vref (flipping out LOW); with out LOW fb rests at 0
    // (flipping out HIGH). No consistent static state exists.
    c.add(Device::Resistor {
        name: "RFB".into(),
        a: out,
        b: d_a,
        ohms: 1.0e3,
        tc1: None,
    });
    c.add(Device::Diode {
        name: "DFB".into(),
        a: d_a,
        k: fb,
        model,
    });
    c.add(Device::Resistor {
        name: "RPD".into(),
        a: fb,
        b: NodeId::GROUND,
        ohms: 10.0e3,
        tc1: None,
    });
    c
}

/// `.op` honesty: when `dc_operating_point` falls back to the staged-DC
/// relaxed surrogate (diodes forced OFF, KCL grossly violated at the adopted
/// point), `run_op` must REFUSE to present it as a converged operating point.
/// The seeding contract is untouched: `dc_operating_point` itself still
/// returns Ok with the surrogate for the transient driver.
#[test]
fn run_op_rejects_staged_dc_relaxed_surrogate() {
    let opts = SolverOptions::default();
    let c = build_comparator_diode_chatter();

    // The seeding-layer contract is preserved: Ok, flagged, huge residual.
    let mut ws = Workspace::new(&c);
    dc_operating_point(&mut ws, &c, &opts)
        .expect("dc_operating_point must still adopt the relaxed surrogate for seeding");
    assert!(
        ws.used_staged_dc(),
        "the staged-DC fallback should have engaged"
    );
    let res = ws.dc_residual_inf_norm(&c, &opts);
    assert!(
        !(res <= 1e-6),
        "test premise: the adopted surrogate must NOT be a true root, residual {res:e}"
    );

    // The .op REPORT layer refuses it.
    let probes = vec![Probe::NodeVoltage("FB".into())];
    let err = run_op(&c, &opts, &probes)
        .expect_err("run_op must not report a relaxed surrogate as a converged .op");
    assert!(
        err.contains("did not converge"),
        "error should say non-convergence, got: {err}"
    );
}

/// Control for the honesty gate: an ordinary convergent circuit (including one
/// that engages the staged path but lands on a GENUINE root) still passes
/// through `run_op` with a near-zero KCL residual.
#[test]
fn run_op_accepts_genuine_roots_including_staged_ones() {
    let opts = SolverOptions::default();

    // Plain resistive divider: trivially convergent.
    let mut c = Circuit::new();
    let a = c.node("A");
    let b = c.node("B");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: a,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a,
        b,
        ohms: 1.0e3,
        tc1: None,
    });
    c.add(Device::Resistor {
        name: "R2".into(),
        a: b,
        b: NodeId::GROUND,
        ohms: 1.0e3,
        tc1: None,
    });
    let out = run_op(&c, &opts, &[Probe::NodeVoltage("B".into())])
        .expect("a convergent divider must pass the .op residual gate");
    assert!(
        (out.rows[0][0] - 2.5).abs() < 1e-6,
        "divider mid: {}",
        out.rows[0][0]
    );

    // The stiff diode board from `stiff_diode_dc_converges_to_finite_physical_root`:
    // it exercises the staged machinery yet converges to a true root, so the
    // residual gate must not reject it (used_staged_dc alone is NOT grounds).
    let (cfull, _, _) = build(40, 4.352e-9);
    let out = run_op(&cfull, &opts, &[Probe::NodeVoltage("RAIL".into())])
        .expect("a genuinely converged stiff-diode .op must pass the residual gate");
    assert!((out.rows[0][0] - 5.0).abs() < 1e-6);
}
