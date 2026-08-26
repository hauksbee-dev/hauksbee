//! Controlled sources E/G (VCVS / VCCS): the per-touchpoint gates from
//! §1 and §2.1.
//!
//! The two gates that matter most here, because their failure mode is a
//! plausible wrong waveform rather than a crash:
//!
//! * **The `LinearIsland::compile` refusal.** E/G are `is_linear() == true`,
//!   so an island containing one stays classified linear and reaches the
//!   state-space reducer under `Partitioning::Auto`. The reducer models only
//!   R/C/L/I; it must return `None` for such an island (routing it to the MNA
//!   sub-solve), never compile it with the controlled source silently absent
//!   from the A matrix.
//! * **The partitioner coupling.** A controlled source is never a cut: its
//!   control pair must fuse with its output port in the union-find, or the
//!   control would be replayed across a Gauss-Seidel step lag (the O(dt)
//!   coupling error class the tearing rules exist to prevent).

//! The F/H (CCCS/CCVS) section below mirrors the same gates with the two
//! structurally new hazards of current control (§2.2):
//!
//! * the control is a DEVICE reference (a branch-current read), so the
//!   partitioner must DEMOTE the control Vsource from cut to island member and
//!   fuse it with the F/H; its branch unknown must live in the same
//!   sub-system as the F/H stamp, not behind a boundary pin;
//! * the conduction graph must fuse too, because a branch-current sense can
//!   never be a free-tear candidate (there is no node voltage whose replay
//!   reproduces it), unlike E/G, whose node-voltage senses stay visible as
//!   cross-island sense edges.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_solve::decompose::ConductionGraph;
use hauksbee_solve::{
    dc_operating_point, Integration, LinearIsland, Partition, Partitioning, SolverOptions,
    StepControl, Transient, Workspace,
};

/// Driver leg: `V1 -> R -> ctrl` with a capacitor to ground, the island whose
/// voltage the controlled source senses.
fn add_driver_leg(c: &mut Circuit) -> NodeId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "Rd".into(),
        a: vin,
        b: ctrl,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cd".into(),
        a: ctrl,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    ctrl
}

/// Load leg: an RC island (`out` node) the controlled source drives.
fn add_load_leg(c: &mut Circuit) -> NodeId {
    let out = c.node("out");
    c.add(Device::Resistor {
        name: "Rl".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cl".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    out
}

// --- touchpoint 6: partitioner coupling --------------------------------------

/// Without the controlled source the driver and load legs are two islands;
/// with it they must FUSE into one (the union-find joins control with output,
/// it does not cut). Checked for both E and G.
#[test]
fn controlled_source_fuses_control_and_output_islands() {
    // Baseline: no controlled source, two separate linear islands.
    let mut c0 = Circuit::new();
    add_driver_leg(&mut c0);
    add_load_leg(&mut c0);
    let p0 = Partition::analyze(&c0);
    assert_eq!(
        p0.islands.len(),
        2,
        "baseline legs must be independent: {}",
        p0.summary()
    );

    for vcvs in [false, true] {
        let mut c = Circuit::new();
        let ctrl = add_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        let dev = if vcvs {
            Device::Vcvs {
                name: "E1".into(),
                p: out,
                n: NodeId::GROUND,
                cp: ctrl,
                cn: NodeId::GROUND,
                gain: 2.0,
            }
        } else {
            Device::Vccs {
                name: "G1".into(),
                p: out,
                n: NodeId::GROUND,
                cp: ctrl,
                cn: NodeId::GROUND,
                gm: 1e-3,
            }
        };
        c.add(dev);
        let p = Partition::analyze(&c);
        assert_eq!(
            p.islands.len(),
            1,
            "{} must fuse its control island with its output island, not cut: {}",
            if vcvs { "VCVS" } else { "VCCS" },
            p.summary()
        );
        assert!(
            p.islands[0].linear,
            "constant-gain controlled sources must not taint linearity"
        );
        // And it is NOT treated as a cut source (only independent Vsources are).
        assert_eq!(p.sources.len(), 1, "only V1 is a cut source");
    }
}

/// The decompose layer sees the same structure through the W1 classifier: the
/// two legs stay separate CONDUCTION islands (the control pair carries no
/// current) with the coupling visible as cross-island sense edges; the raw
/// material of a future proven tear, kept explicit rather than fused blindly.
#[test]
fn conduction_graph_keeps_control_coupling_as_sense_edges() {
    let mut c = Circuit::new();
    let ctrl = add_driver_leg(&mut c);
    let out = add_load_leg(&mut c);
    c.add(Device::Vccs {
        name: "G1".into(),
        p: out,
        n: NodeId::GROUND,
        cp: ctrl,
        cn: NodeId::GROUND,
        gm: 1e-3,
    });
    let g = ConductionGraph::analyze(&c);
    assert_eq!(
        g.islands.len(),
        2,
        "sense-only control must not fuse conduction islands: {g:?}"
    );
    let crossing = g.cross_island_sense_edges();
    assert_eq!(
        crossing.len(),
        1,
        "the non-ground control node is one cross-island sense edge: {crossing:?}"
    );
    assert_eq!(crossing[0].node, ctrl);
}

// --- touchpoint 5: the LinearIsland::compile refusal -------------------------

/// THE gate. An island containing an E/G device is still classified linear,
/// but the state-space reducer must refuse it (`None`), forcing the MNA path.
/// Before this workstream the reducer's device walk ended in `_ => {}`: the
/// island compiled with the controlled source dropped from the A matrix.
#[test]
fn linear_island_with_controlled_source_forces_mna() {
    for vcvs in [false, true] {
        let mut c = Circuit::new();
        let ctrl = add_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        let dev = if vcvs {
            Device::Vcvs {
                name: "E1".into(),
                p: out,
                n: NodeId::GROUND,
                cp: ctrl,
                cn: NodeId::GROUND,
                gain: 2.0,
            }
        } else {
            Device::Vccs {
                name: "G1".into(),
                p: out,
                n: NodeId::GROUND,
                cp: ctrl,
                cn: NodeId::GROUND,
                gm: 1e-3,
            }
        };
        c.add(dev);
        let p = Partition::analyze(&c);
        assert_eq!(p.islands.len(), 1);
        let isl = &p.islands[0];
        // The hazard precondition really holds: the island IS linear and has
        // real dynamics, so without the refusal it would take the fast path.
        assert!(isl.linear);
        assert!(
            LinearIsland::compile(&c, isl, 1e-12, 27.0).is_none(),
            "{}-containing island must refuse state-space reduction and \
             route to MNA",
            if vcvs { "VCVS" } else { "VCCS" }
        );
    }
}

/// End-to-end version of the same gate: under `Partitioning::Auto` the full
/// transient must match the bit-exact `Off` monolith AND the analytic value a
/// dropped device could never produce. A silently-dropped VCCS leaves `out`
/// at 0 V; the analytic steady state is -gm*R*v_ctrl = -2 V.
#[test]
fn auto_partitioning_never_drops_controlled_source() {
    let build = |gm: f64| {
        let mut c = Circuit::new();
        let ctrl = add_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        c.add(Device::Vccs {
            name: "G1".into(),
            p: out,
            n: NodeId::GROUND,
            cp: ctrl,
            cn: NodeId::GROUND,
            gm,
        });
        // Extra independent RC legs so the partition heuristics have islands
        // to fragment and genuinely engage the partitioned engine.
        for k in 0..4 {
            let vin = c.node("in");
            let leg = c.node(&format!("leg{k}"));
            c.add(Device::Resistor {
                name: format!("Rx{k}"),
                a: vin,
                b: leg,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Capacitor {
                name: format!("Cx{k}"),
                a: leg,
                b: NodeId::GROUND,
                farads: 1e-9,
                ic: Some(0.0),
            });
        }
        c
    };
    let c = build(1e-3);
    let opts_off = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-6 },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let opts_auto = SolverOptions {
        partitioning: Partitioning::Auto,
        ..opts_off
    };
    let tstop = 1e-3; // 10 tau: settled.
    let off = Transient::new(opts_off).run(&c, tstop).unwrap();
    let auto = Transient::new(opts_auto).run(&c, tstop).unwrap();
    let w_off = off.node(&c, "out").unwrap();
    let w_auto = auto.node(&c, "out").unwrap();

    // (a) The device visibly acts: steady state -gm*R*v_ctrl = -2 V. A dropped
    // stamp would leave out at exactly 0 V forever.
    let last_auto = *w_auto.last().unwrap();
    assert!(
        (last_auto + 2.0).abs() < 0.02,
        "VCCS steady state must be about -2 V, got {last_auto} \
         (0 V would mean the device was silently dropped)"
    );
    // (b) Auto agrees with the bit-exact monolith over the whole waveform.
    let mut max_abs = 0.0f64;
    for (x, y) in w_off.iter().zip(w_auto) {
        max_abs = max_abs.max((x - y).abs());
    }
    assert!(
        max_abs < 5e-3,
        "Auto vs Off diverged on the VCCS waveform: {max_abs:.3e}"
    );
}

// --- touchpoint 3: stamp correctness against analytic answers ----------------

/// VCCS DC: `out` loaded by R, current gm*v_ctrl flows p->n (out -> ground),
/// so v_out = -gm*R*v_ctrl.
#[test]
fn vccs_dc_matches_analytic() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    let out = c.node("out");
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Vccs {
        name: "G1".into(),
        p: out,
        n: NodeId::GROUND,
        cp: vin,
        cn: NodeId::GROUND,
        gm: 1e-3,
    });
    let mut ws = Workspace::new(&c);
    let opts = SolverOptions::default();
    dc_operating_point(&mut ws, &c, &opts).unwrap();
    let v_out = ws.x[ws.layout.node(out).unwrap()];
    assert!(
        (v_out + 2.0).abs() < 1e-6,
        "VCCS: expected -2 V, got {v_out}"
    );
}

/// VCVS DC: divider halves the input, gain 4 restores double, and the branch
/// current equals the load current (2 mA into 1 k at 2 V), with the Vsource
/// sign convention (current p -> n internally, so a sourcing branch is
/// negative).
#[test]
fn vcvs_dc_matches_analytic() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let a = c.node("a");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: a,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Resistor {
        name: "R2".into(),
        a,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    let e1 = c.add(Device::Vcvs {
        name: "E1".into(),
        p: out,
        n: NodeId::GROUND,
        cp: a,
        cn: NodeId::GROUND,
        gain: 4.0,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    let mut ws = Workspace::new(&c);
    let opts = SolverOptions::default();
    dc_operating_point(&mut ws, &c, &opts).unwrap();
    let v_a = ws.x[ws.layout.node(a).unwrap()];
    let v_out = ws.x[ws.layout.node(out).unwrap()];
    // Tolerances sit above the solver's gmin regularization (1e-12 S shunts
    // perturb the exact answer at the 1e-9 level) but far below any real
    // stamp error.
    assert!(
        (v_a - 0.5).abs() < 1e-6,
        "divider: expected 0.5 V, got {v_a}"
    );
    assert!(
        (v_out - 2.0).abs() < 1e-6,
        "VCVS: expected 2 V, got {v_out}"
    );
    // Ideal control port: NO current is drawn from the divider (v_a stays at
    // exactly half despite the finite divider impedance).
    let i_br = ws.x[ws.layout.branch(e1).unwrap()];
    assert!(
        (i_br + 2e-3).abs() < 1e-9,
        "VCVS branch current: expected -2 mA (sourcing), got {i_br}"
    );
}

/// The VCVS control port draws zero current even mid-transient: load the
/// control node with ONLY the controlled source and a capacitor, if the E
/// stamp leaked current into its control rows, the capacitor voltage would
/// drift off the analytic RC trajectory.
#[test]
fn vcvs_control_port_is_high_impedance() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: ctrl,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: ctrl,
        b: NodeId::GROUND,
        farads: 1e-6,
        ic: Some(0.0),
    });
    c.add(Device::Vcvs {
        name: "E1".into(),
        p: out,
        n: NodeId::GROUND,
        cp: ctrl,
        cn: NodeId::GROUND,
        gain: 3.0,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 10.0, // heavy load: a control-port leak would be conspicuous
        tc1: None,
    });
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-6 },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 5e-3).unwrap();
    let w_ctrl = wf.node(&c, "ctrl").unwrap();
    let w_out = wf.node(&c, "out").unwrap();
    let tau = 1e-3;
    for (i, &t) in wf.time.iter().enumerate() {
        let want = 1.0 - (-t / tau).exp();
        assert!(
            (w_ctrl[i] - want).abs() < 2e-3,
            "control node off the analytic RC at t={t}: got {} want {want} \
             (the E control port must draw zero current)",
            w_ctrl[i]
        );
        assert!(
            (w_out[i] - 3.0 * w_ctrl[i]).abs() < 1e-6,
            "output must track gain*control exactly at t={t}"
        );
    }
}

// =============================================================================
// F/H: current-controlled sources (CCCS / CCVS), dev-plan 04 §2.2
// =============================================================================

/// Driver leg with an in-line zero-volt ammeter: `V1 -> Rd -> ctrl -> Cd`,
/// and the sensed branch `ctrl -> Vs(0V) -> cm -> Rs -> gnd`. Returns the
/// ammeter's id (the F/H control); its current is v_ctrl / 1k.
fn add_sensed_driver_leg(c: &mut Circuit) -> DeviceId {
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    let cm = c.node("cm");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "Rd".into(),
        a: vin,
        b: ctrl,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "Cd".into(),
        a: ctrl,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: Some(0.0),
    });
    let vs = c.add(Device::Vsource {
        name: "Vs".into(),
        p: ctrl,
        n: cm,
        kind: SourceKind::Dc(0.0),
    });
    c.add(Device::Resistor {
        name: "Rs".into(),
        a: cm,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    vs
}

// --- touchpoint 6: demote-and-fuse -------------------------------------------

/// The partitioner gate. Baseline: the ammeter is an ordinary ideal-source CUT
/// (its two sides land in different islands) and the load leg is a third
/// island. Add an F/H reading it and the source is DEMOTED to island member:
/// everything fuses into ONE island that CONTAINS the ammeter, and it leaves
/// the global cut-source list. This is what "union the F/H island with the
/// control source's island" means when the control sits on a cut boundary:
/// the boundary stops being one, because the F/H stamp needs the source's
/// branch-current unknown inside its own system.
#[test]
fn current_control_demotes_ammeter_from_cut_to_island_member() {
    // Baseline: floating-cut ammeter splits the driver leg; load leg separate.
    let mut c0 = Circuit::new();
    add_sensed_driver_leg(&mut c0);
    add_load_leg(&mut c0);
    let p0 = Partition::analyze(&c0);
    assert_eq!(
        p0.islands.len(),
        3,
        "baseline: ctrl side | cm side | load leg: {}",
        p0.summary()
    );
    assert_eq!(p0.sources.len(), 2, "baseline: V1 AND Vs are cut sources");

    for cccs in [true, false] {
        let mut c = Circuit::new();
        let vs = add_sensed_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        let dev = if cccs {
            Device::Cccs {
                name: "F1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                gain: 2.0,
            }
        } else {
            Device::Ccvs {
                name: "H1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                transres: 2e3,
            }
        };
        c.add(dev);
        let p = Partition::analyze(&c);
        assert_eq!(
            p.islands.len(),
            1,
            "{} must fuse control island(s) with its output island: {}",
            if cccs { "CCCS" } else { "CCVS" },
            p.summary()
        );
        assert!(
            p.islands[0].devices.contains(&vs),
            "the demoted ammeter must be an island MEMBER (its branch \
             unknown lives in the island's sub-system)"
        );
        assert_eq!(
            p.sources.len(),
            1,
            "only V1 stays a cut source; the demoted ammeter must not be \
             applied twice: {}",
            p.summary()
        );
        assert!(
            p.islands[0].linear,
            "constant-gain F/H must not taint linearity"
        );
    }
}

/// The conduction graph fuses too, and, unlike E/G, contributes NO sense
/// edge. A sense edge is a node-voltage read whose free-tear replay is exact;
/// a branch-current read has no such replay, so surfacing it as a tear
/// candidate would hand the feedforward pass a coupling its one-directionality
/// proof cannot reason about. Fusing is the honest conservative encoding.
#[test]
fn conduction_graph_fuses_branch_current_coupling() {
    let mut c = Circuit::new();
    let vs = add_sensed_driver_leg(&mut c);
    let out = add_load_leg(&mut c);
    let f1 = c.add(Device::Cccs {
        name: "F1".into(),
        p: out,
        n: NodeId::GROUND,
        ctrl_src: vs,
        gain: 2.0,
    });
    let g = ConductionGraph::analyze(&c);
    assert_eq!(
        g.islands.len(),
        1,
        "branch-current coupling must FUSE conduction islands: {g:?}"
    );
    assert!(
        g.sense_edges.iter().all(|e| e.device != f1),
        "an F contributes no sense edges (its control is not a node read): {:?}",
        g.sense_edges
    );
}

// --- touchpoint 5: the LinearIsland::compile refusal --------------------------

/// An island containing an F/H (and its demoted control ammeter) is still
/// classified linear; the state-space reducer must refuse it (`None`) so the
/// MNA sub-solve stamps it exactly. (The demoted Vsource in the island refuses
/// too, either arm alone routes the island to MNA; the never-dropped
/// end-to-end below proves the whole partitioned path stays exact.)
#[test]
fn linear_island_with_current_controlled_source_forces_mna() {
    for cccs in [true, false] {
        let mut c = Circuit::new();
        let vs = add_sensed_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        let dev = if cccs {
            Device::Cccs {
                name: "F1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                gain: 2.0,
            }
        } else {
            Device::Ccvs {
                name: "H1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                transres: 2e3,
            }
        };
        c.add(dev);
        let p = Partition::analyze(&c);
        assert_eq!(p.islands.len(), 1);
        let isl = &p.islands[0];
        assert!(
            isl.linear,
            "the hazard precondition holds: island is linear"
        );
        assert!(
            LinearIsland::compile(&c, isl, 1e-12, 27.0).is_none(),
            "{}-containing island must refuse state-space reduction",
            if cccs { "CCCS" } else { "CCVS" }
        );
    }
}

/// End-to-end never-silently-dropped gate, F and H: under `Partitioning::Auto`
/// the transient must match the bit-exact `Off` monolith AND the analytic
/// steady state a dropped stamp could never produce (out stays 0 V if the
/// device vanishes; the demoted ammeter forces the island through the
/// NonlinearIsland build with ctrl_src retargeting, which is the new code
/// under test).
#[test]
fn auto_partitioning_never_drops_current_controlled_sources() {
    for cccs in [true, false] {
        let mut c = Circuit::new();
        let vs = add_sensed_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        if cccs {
            c.add(Device::Cccs {
                name: "F1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                gain: 2.0,
            });
        } else {
            c.add(Device::Ccvs {
                name: "H1".into(),
                p: out,
                n: NodeId::GROUND,
                ctrl_src: vs,
                transres: 2e3,
            });
        }
        // Extra independent RC legs so the partition heuristics engage.
        for k in 0..4 {
            let vin = c.node("in");
            let leg = c.node(&format!("leg{k}"));
            c.add(Device::Resistor {
                name: format!("Rx{k}"),
                a: vin,
                b: leg,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Capacitor {
                name: format!("Cx{k}"),
                a: leg,
                b: NodeId::GROUND,
                farads: 1e-9,
                ic: Some(0.0),
            });
        }
        let opts_off = SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Fixed { dt: 1e-6 },
            partitioning: Partitioning::Off,
            ..SolverOptions::default()
        };
        let opts_auto = SolverOptions {
            partitioning: Partitioning::Auto,
            ..opts_off
        };
        let tstop = 1e-3; // >= 10 tau of every RC here: settled.
        let off = Transient::new(opts_off).run(&c, tstop).unwrap();
        let auto = Transient::new(opts_auto).run(&c, tstop).unwrap();
        let w_off = off.node(&c, "out").unwrap();
        let w_auto = auto.node(&c, "out").unwrap();

        // Settled control current: v_ctrl -> 2 V * (1k/(1k+1k)) = 1 V across
        // Rs' path... precisely: DC path V1 -> Rd -> Vs -> Rs, i = 2V/2k = 1 mA.
        // (a) CCCS: out receives -gain*i through RL=1k -> -2 V.
        //     CCVS: out is PINNED to transres*i = +2 V.
        let want = if cccs { -2.0 } else { 2.0 };
        let last_auto = *w_auto.last().unwrap();
        assert!(
            (last_auto - want).abs() < 0.02,
            "{} steady state must be about {want} V, got {last_auto} \
             (0 V would mean the device was silently dropped)",
            if cccs { "CCCS" } else { "CCVS" },
        );
        // (b) Auto agrees with the bit-exact monolith over the whole waveform.
        let mut max_abs = 0.0f64;
        for (x, y) in w_off.iter().zip(w_auto) {
            max_abs = max_abs.max((x - y).abs());
        }
        assert!(
            max_abs < 5e-3,
            "Auto vs Off diverged on the {} waveform: {max_abs:.3e}",
            if cccs { "CCCS" } else { "CCVS" },
        );
    }
}

// --- touchpoint 3: stamp correctness against analytic answers -----------------

/// CCCS DC. Control loop: V1(2V) -> R1(1k) -> m -> Vs(0V ammeter) -> gnd, so
/// i_ctrl = 2 mA (flowing p->n through the ammeter). F1 out 0 gain=3 pulls
/// 3*i_ctrl OUT of node `out`: v_out = -3 * 2mA * 1k = -6 V. The ammeter node
/// must sit at EXACTLY 0 V (a control-side perturbation would mean the F
/// leaked into rows it must not touch).
#[test]
fn cccs_dc_matches_analytic() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let m = c.node("m");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: m,
        ohms: 1e3,
        tc1: None,
    });
    let vs = c.add(Device::Vsource {
        name: "Vs".into(),
        p: m,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    c.add(Device::Cccs {
        name: "F1".into(),
        p: out,
        n: NodeId::GROUND,
        ctrl_src: vs,
        gain: 3.0,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    let mut ws = Workspace::new(&c);
    let opts = SolverOptions::default();
    dc_operating_point(&mut ws, &c, &opts).unwrap();
    let i_ctrl = ws.x[ws.layout.branch(vs).unwrap()];
    let v_m = ws.x[ws.layout.node(m).unwrap()];
    let v_out = ws.x[ws.layout.node(out).unwrap()];
    assert!(
        (i_ctrl - 2e-3).abs() < 1e-9,
        "ammeter current: expected 2 mA, got {i_ctrl}"
    );
    assert!(
        v_m.abs() < 1e-9,
        "zero-volt ammeter must hold its node at 0 V, got {v_m}"
    );
    assert!(
        (v_out + 6.0).abs() < 1e-6,
        "CCCS: expected -6 V, got {v_out}"
    );
}

/// CCVS DC. Same 2 mA control current; H1 out 0 transres=2k PINS
/// v_out = 2k * 2mA = 4 V, and its own branch sources the 4 mA the load
/// draws (Vsource sign convention: sourcing is negative). The control loop is
/// undisturbed, H reads the ammeter's branch COLUMN, never its rows.
#[test]
fn ccvs_dc_matches_analytic() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let m = c.node("m");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: m,
        ohms: 1e3,
        tc1: None,
    });
    let vs = c.add(Device::Vsource {
        name: "Vs".into(),
        p: m,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    let h1 = c.add(Device::Ccvs {
        name: "H1".into(),
        p: out,
        n: NodeId::GROUND,
        ctrl_src: vs,
        transres: 2e3,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    let mut ws = Workspace::new(&c);
    let opts = SolverOptions::default();
    dc_operating_point(&mut ws, &c, &opts).unwrap();
    let i_ctrl = ws.x[ws.layout.branch(vs).unwrap()];
    let v_out = ws.x[ws.layout.node(out).unwrap()];
    let i_h = ws.x[ws.layout.branch(h1).unwrap()];
    assert!(
        (i_ctrl - 2e-3).abs() < 1e-9,
        "ammeter current: expected 2 mA, got {i_ctrl}"
    );
    assert!(
        (v_out - 4.0).abs() < 1e-6,
        "CCVS: expected 4 V, got {v_out}"
    );
    assert!(
        (i_h + 4e-3).abs() < 1e-9,
        "CCVS branch current: expected -4 mA (sourcing), got {i_h}"
    );
}

/// Subckt composition end-to-end: an F inside a `.subckt` naming an ammeter in
/// the same body must resolve per-instance to the MANGLED name (X1.Vsense /
/// X2.Vsense) and solve correctly, two instances with different drive
/// currents prove the references did not cross-bind.
#[test]
fn cccs_in_subckt_resolves_per_instance_and_solves() {
    let net = "\
mirror pair
.subckt mir inp outp
Vsense inp 0 0
F1 0 outp Vsense 2
.ends
V1 a 0 DC 1
V2 c 0 DC 3
R1 a am 1k
R2 c cm 1k
X1 am b mir
X2 cm d mir
RL1 b 0 1k
RL2 d 0 1k
.end
";
    let c = hauksbee_ir::SpiceLoader::load(net).unwrap();
    let mut ws = Workspace::new(&c);
    let opts = SolverOptions::default();
    dc_operating_point(&mut ws, &c, &opts).unwrap();
    let node = |name: &str| {
        let mut cc = c.clone();
        let id = cc.node(name);
        ws.x[ws.layout.node(id).unwrap()]
    };
    // Instance 1: i = 1V/1k = 1 mA, F injects 2 mA INTO b: v_b = +2 V.
    // Instance 2: i = 3V/1k = 3 mA -> v_d = +6 V. Cross-bound references
    // would show 6 V on b or 2 V on d.
    let (v_b, v_d) = (node("b"), node("d"));
    assert!(
        (v_b - 2.0).abs() < 1e-6,
        "X1 output: expected 2 V, got {v_b}"
    );
    assert!(
        (v_d - 6.0).abs() < 1e-6,
        "X2 output: expected 6 V, got {v_d}"
    );
}
