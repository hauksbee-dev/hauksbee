//! Controlled sources E/G (VCVS / VCCS): the per-touchpoint gates from
//! `docs/dev-plans/04-spice-compat.md` §1 and §2.1.
//!
//! The two gates that matter most here, because their failure mode is a
//! plausible wrong waveform rather than a crash:
//!
//! * **The `LinearIsland::compile` refusal.** E/G are `is_linear() == true`,
//!   so an island containing one stays classified linear and reaches the
//!   state-space reducer under `Partitioning::Auto`. The reducer models only
//!   R/C/L/I; it must return `None` for such an island (routing it to the MNA
//!   sub-solve) — never compile it with the controlled source silently absent
//!   from the A matrix.
//! * **The partitioner coupling.** A controlled source is never a cut: its
//!   control pair must fuse with its output port in the union-find, or the
//!   control would be replayed across a Gauss-Seidel step lag (the O(dt)
//!   coupling error class the tearing rules exist to prevent).

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
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
/// current) with the coupling visible as cross-island sense edges — the raw
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
            LinearIsland::compile(&c, isl, 1e-12).is_none(),
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
    assert!((v_a - 0.5).abs() < 1e-6, "divider: expected 0.5 V, got {v_a}");
    assert!((v_out - 2.0).abs() < 1e-6, "VCVS: expected 2 V, got {v_out}");
    // Ideal control port: NO current is drawn from the divider (v_a stays at
    // exactly half despite the finite divider impedance).
    let i_br = ws.x[ws.layout.branch(e1).unwrap()];
    assert!(
        (i_br + 2e-3).abs() < 1e-9,
        "VCVS branch current: expected -2 mA (sourcing), got {i_br}"
    );
}

/// The VCVS control port draws zero current even mid-transient: load the
/// control node with ONLY the controlled source and a capacitor — if the E
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
