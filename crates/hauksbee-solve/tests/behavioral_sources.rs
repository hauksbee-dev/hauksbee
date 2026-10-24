//! Behavioral B-source: the per-touchpoint gates from
//! §1 and §2.5.
//!
//! The B-source is the maximal-coupling device: its expression may read node
//! voltages (`V(node)`, sense edges, like an E/G control pair), branch
//! currents (`I(vname)`, device references, like an F/H control), and time,
//! all at once. The gates that matter most:
//!
//! * **Island fusion, both dependency kinds.** A `V(node)` dep must fuse the
//!   B-source's island with the sensed island in the partitioner (never a
//!   cut), while the conduction graph keeps them separate islands with the
//!   coupling visible as a cross-island SENSE edge (the free-tear vocabulary).
//!   An `I(vname)` dep must demote the ammeter from cut to island member and
//!   fuse, a branch-current read has no node-replay, so it is never a tear
//!   candidate and contributes NO sense edge.
//! * **The named-fault refusal.** An expression that errors mid-solve
//!   (`ln` of a negative iterate, division by zero) must surface as a
//!   device-named refusal from the DC / transient drivers, never a silent
//!   NaN in the matrix, never a truncated waveform.
//! * **Never silently dropped.** Under `Partitioning::Auto` the waveform must
//!   match the bit-exact `Off` monolith AND the analytic value a dropped
//!   stamp could never produce.

use crate::shared::{
    add_driver_leg, add_load_leg, add_partition_rc_legs, add_resistor, add_sensed_driver_leg,
    add_vdc, assert_auto_settles_like_off, dc_solved, max_abs_diff, trap_fixed_off,
};
use hauksbee_ir::{BDep, BOutput, Circuit, CompiledExpr, Device, DeviceId, NodeId, SpiceLoader};
use hauksbee_solve::decompose::ConductionGraph;
use hauksbee_solve::{
    dc_operating_point, run_tran, AcAnalysis, AcSpec, AssemblyMode, LinearIsland, Partition, Probe,
    SolveError, SolverOptions, StepControl, Sweep, Transient, Workspace,
};
use hauksbee_testkit::circuits::cap;

/// A B-source with one V(ctrl) dep, either output flavor.
fn behavioral_vdep(out: NodeId, ctrl: NodeId, voltage_out: bool) -> Device {
    let (output, expr) = if voltage_out {
        (BOutput::Voltage, "2.0*math::tanh(__d0)")
    } else {
        (BOutput::Current, "1e-3*math::tanh(__d0)")
    };
    Device::behavioral("B1", (out, NodeId::GROUND), output, expr, &[ctrl]).unwrap()
}

/// A current B-source of `2*I(vs)` into `out`.
fn behavioral_idep(out: NodeId, vs: DeviceId) -> Device {
    Device::Behavioral {
        name: "B1".into(),
        p: out,
        n: NodeId::GROUND,
        output: BOutput::Current,
        expr: CompiledExpr::compile("2.0*__d0").unwrap(),
        deps: vec![BDep::Branch(vs)],
    }
}

// --- touchpoint 6: partitioner + conduction-graph coupling, V(node) deps -----

/// A V(node) dep fuses the sensed island with the output island in the
/// partitioner (like an E/G control pair), and the fused island is TAINTED
/// nonlinear, which is itself the `LinearIsland` gate: `compile` refuses any
/// non-linear island before it ever walks devices.
#[test]
fn behavioral_vdep_fuses_islands_and_taints_nonlinear() {
    // Baseline: no B-source, two independent linear islands.
    let mut c0 = Circuit::new();
    add_driver_leg(&mut c0);
    add_load_leg(&mut c0);
    let p0 = Partition::analyze(&c0);
    assert_eq!(p0.islands.len(), 2, "baseline: {}", p0.summary());

    for voltage_out in [true, false] {
        let mut c = Circuit::new();
        let ctrl = add_driver_leg(&mut c);
        let out = add_load_leg(&mut c);
        c.add(behavioral_vdep(out, ctrl, voltage_out));
        let p = Partition::analyze(&c);
        assert_eq!(
            p.islands.len(),
            1,
            "B ({}) must fuse its sensed island with its output island: {}",
            if voltage_out { "V-out" } else { "I-out" },
            p.summary()
        );
        assert!(
            !p.islands[0].linear,
            "a behavioral source must taint its island nonlinear"
        );
        assert_eq!(p.sources.len(), 1, "only V1 is a cut source");
        // The LinearIsland walk refuses the island (nonlinear gate); the
        // exhaustive-match arm exists for the day a `linear`-classified
        // island ever carries one, and refusing is exact either way.
        assert!(
            LinearIsland::compile(&c, &p.islands[0], 1e-12, 27.0).is_none(),
            "an island containing a B-source must route to MNA"
        );
    }
}

/// The decompose layer sees the same structure through the W1 classifier: the
/// two legs stay separate CONDUCTION islands (a V(node) dep carries no
/// current) with the coupling visible as a cross-island sense edge; the plan
/// §2.5 declaration that the B-source reads across the tear boundary.
#[test]
fn behavioral_vdep_is_a_cross_island_sense_edge() {
    let mut c = Circuit::new();
    let ctrl = add_driver_leg(&mut c);
    let out = add_load_leg(&mut c);
    let b1 = c.add(behavioral_vdep(out, ctrl, false));
    let g = ConductionGraph::analyze(&c);
    assert_eq!(
        g.islands.len(),
        2,
        "sense-only V(node) dep must not fuse conduction islands: {g:?}"
    );
    let crossing = g.cross_island_sense_edges();
    assert_eq!(
        crossing.len(),
        1,
        "the V(ctrl) dep is one cross-island sense edge: {crossing:?}"
    );
    assert_eq!(crossing[0].node, ctrl);
    assert_eq!(crossing[0].device, b1);
}

// --- touchpoint 6: I(vname) deps demote-and-fuse ------------------------------

/// An I(vname) dep must DEMOTE the ammeter from cut to island member and fuse
/// everything into one island containing it; the F/H rule, reached through
/// the B-source's plural `controlling_sources` path.
#[test]
fn behavioral_idep_demotes_ammeter_and_fuses() {
    let mut c = Circuit::new();
    let vs = add_sensed_driver_leg(&mut c);
    let out = add_load_leg(&mut c);
    c.add(behavioral_idep(out, vs));
    let p = Partition::analyze(&c);
    assert_eq!(
        p.islands.len(),
        1,
        "I(vname) dep must fuse control island(s) with the output island: {}",
        p.summary()
    );
    assert!(
        p.islands[0].devices.contains(&vs),
        "the demoted ammeter must be an island MEMBER"
    );
    assert_eq!(p.sources.len(), 1, "only V1 stays a cut source");

    // Conduction graph: fused, and NO sense edge from the branch-current read
    // (there is no node voltage whose replay reproduces a branch current).
    let g = ConductionGraph::analyze(&c);
    assert_eq!(
        g.islands.len(),
        1,
        "branch-current dep must FUSE conduction islands: {g:?}"
    );
    assert!(
        g.sense_edges.is_empty(),
        "an I(...) dep contributes no sense edges: {:?}",
        g.sense_edges
    );
}

// --- touchpoint 3: stamp correctness against analytic answers -----------------

/// V-output DC: divider halves 1 V to 0.5 V, `V={4*v(a)}` restores 2 V; the
/// branch sources the 2 mA the load draws; the sensed divider is undisturbed
/// (ideal sense: no current drawn). Numerically identical to the VCVS gate,
/// but through the finite-difference tangents of a genuinely compiled
/// expression.
#[test]
fn behavioral_voltage_dc_matches_analytic() {
    let net = "b\n\
               V1 in 0 DC 1\n\
               R1 in a 1k\n\
               R2 a 0 1k\n\
               B1 out 0 V={4*v(a)}\n\
               RL out 0 1k\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let ws = dc_solved(&c);
    let node = |name: &str| ws.x[ws.layout.node(c.find_node(name).unwrap()).unwrap()];
    let v_a = node("a");
    let v_out = node("out");
    // FD tangent of a linear expression is exact to rounding; the residual
    // tolerance below is the solver's own convergence bar, not FD error.
    assert!((v_a - 0.5).abs() < 1e-6, "divider: {v_a}");
    assert!(
        (v_out - 2.0).abs() < 1e-6,
        "B V-out: expected 2 V, got {v_out}"
    );
    let b1 = c
        .iter()
        .find(|(_, d)| d.name() == "B1")
        .map(|(id, _)| id)
        .unwrap();
    let i_br = ws.x[ws.layout.branch(b1).unwrap()];
    assert!(
        (i_br + 2e-3).abs() < 1e-8,
        "B branch current: expected -2 mA (sourcing), got {i_br}"
    );
}

/// I-output DC with a genuinely nonlinear expression: `I={1m*tanh(v(a))}`
/// pulled out of a 1 k load gives `v_out = -1000 * 1e-3 * tanh(v_a)`; the
/// closed form a dropped or mislinearized stamp cannot hit. Also proves the
/// mixed V+I dependency stamp: a second term reads the ammeter current.
#[test]
fn behavioral_current_dc_matches_analytic() {
    let net = "b\n\
               V1 a 0 DC 1\n\
               Rm a m 1k\n\
               Vs m 0 0\n\
               B1 out 0 I={0.001*tanh(v(a)) + 2*i(Vs)}\n\
               RL out 0 1k\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let ws = dc_solved(&c);
    let node = |name: &str| ws.x[ws.layout.node(c.find_node(name).unwrap()).unwrap()];
    // i(Vs) = 1 V / 1 k = 1 mA; f = 1m*tanh(1) + 2 mA; v_out = -1k * f.
    let want = -1e-3f64.mul_add(1.0f64.tanh(), 2e-3) * 1e3;
    let v_out = node("out");
    assert!(
        (v_out - want).abs() < 1e-5,
        "B I-out: expected {want:.6} V, got {v_out}"
    );
}

/// Self-referencing expression (the nonlinear-resistor idiom): the B-source
/// senses its own output port. `I={1m*(exp(v(out)/0.2)-1)}` from `out` to
/// ground with a 1 V drive through 1 k solves the diode-like balance
/// `(1 - v)/1k = 1m*(exp(v/0.2) - 1)`; Newton on the FD tangent must converge
/// and satisfy that balance.
#[test]
fn behavioral_self_reference_converges() {
    let net = "b\n\
               V1 in 0 DC 1\n\
               R1 in out 1k\n\
               B1 out 0 I={0.001*(exp(v(out)/0.2)-1)}\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let ws = dc_solved(&c);
    let v = ws.x[ws.layout.node(c.find_node("out").unwrap()).unwrap()];
    let lhs = (1.0 - v) / 1e3;
    let rhs = 1e-3 * (v / 0.2).exp_m1();
    assert!(
        (lhs - rhs).abs() < 1e-9,
        "KCL balance violated: {lhs:.3e} vs {rhs:.3e} at v={v}"
    );
    assert!(
        v > 0.0 && v < 1.0,
        "root must sit inside the drive range: {v}"
    );
}

// --- end-to-end: Auto partitioning never drops the device ---------------------

/// Under `Partitioning::Auto` the transient must match the bit-exact `Off`
/// monolith AND the analytic steady state (0 V would mean the device was
/// silently dropped). Exercises the `NonlinearIsland` build with the B-source's
/// per-slot control retargeting (the plural F/H path) and V-dep remapping.
#[test]
fn auto_partitioning_never_drops_behavioral() {
    for idep in [false, true] {
        let mut c = Circuit::new();
        let out;
        if idep {
            let vs = add_sensed_driver_leg(&mut c);
            out = add_load_leg(&mut c);
            c.add(behavioral_idep(out, vs));
        } else {
            let ctrl = add_driver_leg(&mut c);
            out = add_load_leg(&mut c);
            c.add(behavioral_vdep(out, ctrl, false));
        }
        // Extra independent RC legs so the partition heuristics engage.
        add_partition_rc_legs(&mut c);
        // Settled: V-dep case ctrl -> 2 V, f = 1m*tanh(2), v_out = -1k*f.
        //          I-dep case i(Vs) = 2V/2k = 1 mA, f = 2 mA, v_out = -2 V.
        let want = if idep {
            -2.0
        } else {
            -(1e-3 * 2.0f64.tanh()) * 1e3
        };
        assert_auto_settles_like_off(&c, want, if idep { "B (idep)" } else { "B (vdep)" });
    }
}

// --- the named-fault refusal path ---------------------------------------------

/// A DC solve whose B expression is poisoned at the operating point (`ln` of
/// a pinned negative voltage) must refuse with the DEVICE NAME, never emit a
/// NaN operating point.
#[test]
fn dc_fault_names_the_device() {
    let net = "b\n\
               V1 a 0 DC -1\n\
               R1 a 0 1k\n\
               B1 out 0 V={ln(v(a))}\n\
               RL out 0 1k\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let mut ws = Workspace::new(&c);
    let err = dc_operating_point(&mut ws, &c, &SolverOptions::default()).unwrap_err();
    assert!(
        matches!(&err, SolveError::Behavioral { device, .. } if device == "B1"),
        "the refusal must name the device: {err}"
    );
}

/// Division by zero (an INF value, not an eval error, IEEE semantics) is the
/// same named refusal.
#[test]
fn dc_division_by_zero_names_the_device() {
    let net = "b\n\
               R1 a 0 1k\n\
               Bdiv out 0 I={0.001/v(a)}\n\
               RL out 0 1k\n\
               .end\n";
    // v(a) floats at exactly 0 (only gmin holds it): 1/0 = inf on iterate 1.
    let c = SpiceLoader::load(net).unwrap();
    let mut ws = Workspace::new(&c);
    let err = dc_operating_point(&mut ws, &c, &SolverOptions::default()).unwrap_err();
    assert!(
        matches!(&err, SolveError::Behavioral { device, .. } if device == "Bdiv"),
        "the refusal must name the device: {err}"
    );
}

/// A transient march whose expression goes bad MID-RUN (`ln(0.5 - time)`
/// collapses at t = 0.5 s) must refuse loudly with the device name; the
/// waveform is never silently truncated or continued through NaN.
#[test]
fn transient_fault_names_the_device_and_refuses() {
    let net = "b\n\
               B1 out 0 V={ln(0.5 - time)}\n\
               RL out 0 1k\n\
               .tran 10m 1\n\
               .end\n";
    let (c, d) = SpiceLoader::load_with_directives(net).unwrap();
    let td = d.tran.unwrap();
    let opts = SolverOptions {
        step: StepControl::Adaptive {
            dt_initial: 1e-4,
            dt_min: 1e-12,
            dt_max: td.tstep,
        },
        ..SolverOptions::default()
    };
    let probes = vec![Probe::parse("v(out)").unwrap()];
    let err = run_tran(&c, &opts, td.tstop, &probes).unwrap_err();
    assert!(
        matches!(&err, SolveError::Behavioral { device, .. } if device == "B1"),
        "transient refusal must name the device: {err}"
    );
}

// --- AC: linearized small-signal stamp at the OP -------------------------------

/// AC contract: the B-source stamps its FD tangents frozen at the operating
/// point. For `V={10*v(a)}` that is exactly a gain-10 VCVS: the output phasor
/// must be 10x the sensed phasor at every frequency of an RC-filtered drive.
#[test]
fn ac_linearizes_at_the_operating_point() {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let a = c.node("a");
    let out = c.node("out");
    add_vdc(&mut c, "V1", vin, NodeId::GROUND, 0.0);
    add_resistor(&mut c, "R1", vin, a, 1e3);
    cap(&mut c, "C1", a, NodeId::GROUND, 1e-6);
    let amp = Device::behavioral(
        "B1",
        (out, NodeId::GROUND),
        BOutput::Voltage,
        "10.0*__d0",
        &[a],
    );
    c.add(amp.unwrap());
    add_resistor(&mut c, "RL", out, NodeId::GROUND, 1e3);
    let ac = AcAnalysis::new(SolverOptions::default());
    let resp = ac
        .run(
            &c,
            &AcSpec {
                fstart: 10.0,
                fstop: 100e3,
                points: 10,
                sweep: Sweep::Decade,
            },
        )
        .unwrap();
    for pt in &resp.points {
        let va = pt.node_phasor[a.0 as usize];
        let vo = pt.node_phasor[out.0 as usize];
        let err = (vo - va * 10.0).norm();
        assert!(
            err < 1e-6 * va.norm().max(1e-12),
            "AC: out must equal 10*a at f={} (|err|={err:.3e})",
            pt.freq
        );
    }
}

// --- time dependence + planned assembly ----------------------------------------

/// A time-only B-source `V={sin(2*pi*1k*time)}` must reproduce the built-in
/// `SIN(0 1 1k)` source sample-for-sample (both nodes march on the same
/// grid, evaluating the same formula).
#[test]
fn time_dependent_behavioral_matches_sin_source() {
    let net = "b\n\
               Vref r 0 SIN(0 1 1k)\n\
               Rr r 0 1k\n\
               B1 out 0 V={sin(6.283185307179586*1000*time)}\n\
               RL out 0 1k\n\
               .tran 10u 2m\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let wf = Transient::new(trap_fixed_off(1e-6)).run(&c, 2e-3).unwrap();
    let worst = max_abs_diff(wf.node(&c, "r").unwrap(), wf.node(&c, "out").unwrap());
    assert!(
        worst < 1e-9,
        "time-dependent B must track the SIN source exactly: worst {worst:.3e}"
    );
}

/// The compiled two-tier assembly must agree with the interpreted walk on a
/// B-source deck (the Slotted table now carries branch and control columns,
/// new plan.rs code under test).
#[test]
fn planned_assembly_matches_interpreted_on_behavioral_deck() {
    let net = "b\n\
               V1 in 0 SIN(0 2 1k)\n\
               R1 in a 1k\n\
               Ca a 0 100n\n\
               Vs a m 0\n\
               Rm m 0 1k\n\
               B1 out 0 V={tanh(v(a)) + 50*i(Vs)}\n\
               RL out 0 1k\n\
               .end\n";
    let c = SpiceLoader::load(net).unwrap();
    let base = trap_fixed_off(1e-6);
    let planned = SolverOptions {
        assembly: AssemblyMode::Planned,
        ..base
    };
    let w0 = Transient::new(base).run(&c, 2e-3).unwrap();
    let w1 = Transient::new(planned).run(&c, 2e-3).unwrap();
    let worst = max_abs_diff(w0.node(&c, "out").unwrap(), w1.node(&c, "out").unwrap());
    assert!(
        worst < 1e-6,
        "planned assembly diverged from interpreted on a B deck: {worst:.3e}"
    );
}
