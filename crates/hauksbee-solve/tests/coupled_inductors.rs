//! Coupled inductors K (dev-plan 04 §2.3): the per-touchpoint gates.
//!
//! The gates that matter most, because their failure mode is a plausible
//! wrong waveform rather than a crash:
//!
//! * **The k=1 trap.** A perfect-coupling group's inductance matrix
//!   [[L1,M],[M,L2]] is exactly singular, and k=1 is legal on the card. The
//!   companion stamps L directly (never inverts it), so the deck must SOLVE,
//!   the analytic gate below holds the k=1 transformer to the ideal-ratio
//!   closed form.
//! * **Island binding.** Two galvanically separate loops joined only by
//!   mutual flux must fuse into ONE island (a branch-current coupling is
//!   never a tear candidate), and the node-less Coupling device itself must
//!   land IN that island's device list, a dropped Coupling would silently
//!   decouple the windings inside the island's own Layout.
//! * **The LinearIsland refusal.** Coupling is `is_linear() == true`, so a
//!   coupled RL island reaches the state-space reducer under Auto; the
//!   reducer must return `None` (its inductor model assumes di/dt = v/L per
//!   winding; the group form needs L INVERTED, which k=1 forbids), never
//!   compile with the mutual terms silently absent.
//! * **Assembly/partition parity.** The planned two-tier assembly folds the
//!   −M cross terms into the reactive backbone and the partitioned executor
//!   solves the fused island in its own sub-system; both must match the
//!   monolithic interpreted reference to solver tolerance.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_solve::{
    AcAnalysis, AcSpec, AssemblyMode, Integration, LinearIsland, Partition, Partitioning,
    SolverOptions, StepControl, Sweep, Transient, Waveforms,
};
use num_complex::Complex64;

/// The shared transformer fixture: `V1(sin) -> Rs -> L1 || (K) || L2 -> RL`,
/// primary and secondary sharing ONLY ground. Returns the circuit and the
/// (l1, l2) device ids.
fn transformer(k: f64, l1_h: f64, l2_h: f64) -> (Circuit, DeviceId, DeviceId) {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let pri = c.node("pri");
    let sec = c.node("sec");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Sin {
            offset: 0.0,
            amplitude: 1.0,
            freq: 10e3,
            delay: 0.0,
            theta: 0.0,
            phase: 0.0,
        },
    });
    c.add(Device::Resistor {
        name: "Rs".into(),
        a: vin,
        b: pri,
        ohms: 50.0,
        tc1: None,
    });
    let l1 = c.add(Device::Inductor {
        name: "L1".into(),
        a: pri,
        b: NodeId::GROUND,
        henries: l1_h,
        ic: None,
    });
    let l2 = c.add(Device::Inductor {
        name: "L2".into(),
        a: sec,
        b: NodeId::GROUND,
        henries: l2_h,
        ic: None,
    });
    c.add(Device::Coupling {
        name: "K1".into(),
        l1,
        l2,
        k,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: sec,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    (c, l1, l2)
}

fn tran_opts(part: Partitioning, assembly: AssemblyMode) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-7 },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        partitioning: part,
        assembly,
        ..SolverOptions::default()
    }
}

/// Steady-state amplitude of a node over the LAST full 10 kHz cycle.
fn last_cycle_amplitude(c: &Circuit, wf: &Waveforms, node: &str, tstop: f64) -> f64 {
    let v = wf.node(c, node).expect("probed node exists");
    let t0 = tstop - 1e-4; // one 10 kHz period
    v.iter()
        .zip(&wf.time)
        .filter(|(_, &t)| t >= t0)
        .map(|(&x, _)| x.abs())
        .fold(0.0f64, f64::max)
}

// --- the k=1 trap -------------------------------------------------------------

/// Exact phasor solve of the two-winding fixture at frequency `f` for unit
/// drive, valid for EVERY k including 1 (no matrix inversion, mirroring the
/// stamp): eliminate I2 = −Vs/RL from
///   Vin = I1·Rs + jw(L1 I1 + M I2),   Vs = jw(M I1 + L2 I2).
fn closed_form_vsec(f: f64, k: f64, l1: f64, l2: f64, rs: f64, rl: f64) -> Complex64 {
    let m = k * (l1 * l2).sqrt();
    let jw = Complex64::new(0.0, std::f64::consts::TAU * f);
    let denom_s = Complex64::new(1.0, 0.0) + jw * l2 / rl;
    let vs_over_i1 = jw * m / denom_s;
    let i1 = Complex64::new(1.0, 0.0)
        / (Complex64::new(rs, 0.0) + jw * l1 - jw * m * vs_over_i1 / rl);
    vs_over_i1 * i1
}

/// Perfect coupling MUST solve (the singular group matrix is stamped, never
/// inverted), and the steady-state secondary amplitude must land on the
/// two-winding closed form (which at k=1 includes the magnetizing droop:
/// wL1 = 62.8 ohm at 10 kHz shunts the 250-ohm reflected load, so the exact
/// value is well below the ideal 1.667 V, grading against the ideal would
/// be grading the wrong physics). The ngspice deck xfmr_k1 pins the exact
/// waveform; this gate needs no oracle installed.
#[test]
fn k1_singular_group_solves_to_closed_form() {
    let (c, _, _) = transformer(1.0, 1e-3, 4e-3);
    let tstop = 500e-6;
    let wf = Transient::new(tran_opts(Partitioning::Off, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("k=1 transformer must SOLVE: L is stamped, never inverted");
    for row in &wf.node_voltages {
        assert!(row.iter().all(|x| x.is_finite()), "non-finite sample at k=1");
    }
    let amp = last_cycle_amplitude(&c, &wf, "sec", tstop);
    let expect = closed_form_vsec(10e3, 1.0, 1e-3, 4e-3, 50.0, 1e3).norm();
    println!("k=1 secondary amplitude {amp:.5} V, closed form {expect:.5} V");
    assert!(
        (amp - expect).abs() / expect < 0.02,
        "k=1 secondary amplitude {amp:.4} V departs closed form {expect:.4} V"
    );
}

/// Zero coupling sanity inverse: WITHOUT the K card the same layout leaves
/// the secondary dead (no galvanic path), proving the mutual term is the
/// only thing that can have produced the k=1 amplitude above.
#[test]
fn without_coupling_secondary_is_dead() {
    let (full, _, _) = transformer(1.0, 1e-3, 4e-3);
    // Rebuild without the K card: same node table, same device order.
    let mut c = Circuit::new();
    for name in ["in", "pri", "sec"] {
        c.node(name);
    }
    for (_, d) in full.iter() {
        if !matches!(d, Device::Coupling { .. }) {
            c.add(d.clone());
        }
    }
    let tstop = 500e-6;
    let wf = Transient::new(tran_opts(Partitioning::Off, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("uncoupled pair converges");
    let amp = last_cycle_amplitude(&c, &wf, "sec", tstop);
    assert!(
        amp < 1e-9,
        "secondary must be dead without mutual flux, saw {amp:.3e} V"
    );
}

// --- island binding -----------------------------------------------------------

/// Two loops sharing only ground are two islands without the K card; with it
/// they must fuse into ONE island whose device list contains BOTH windings
/// AND the node-less Coupling itself (sub-circuit extraction rebuilds the
/// island's own Layout from that list, a missing Coupling would silently
/// decouple the windings there, the §1 hazard class).
#[test]
fn coupling_binds_would_be_separate_islands() {
    let (c, l1, l2) = transformer(0.9, 1e-3, 4e-3);
    // Baseline: identical circuit minus the K card -> two islands.
    let mut c0 = Circuit::new();
    for (_, d) in c.iter() {
        if !matches!(d, Device::Coupling { .. }) {
            c0.add(d.clone());
        }
    }
    let p0 = Partition::analyze(&c0);
    assert_eq!(
        p0.islands.len(),
        2,
        "baseline loops must be independent: {}",
        p0.summary()
    );

    let p = Partition::analyze(&c);
    assert_eq!(
        p.islands.len(),
        1,
        "mutual flux must union the loops: {}",
        p.summary()
    );
    let isl = &p.islands[0];
    assert!(isl.linear, "R/L/K island is linear");
    let kid = c
        .iter()
        .find(|(_, d)| matches!(d, Device::Coupling { .. }))
        .map(|(id, _)| id)
        .unwrap();
    for (what, id) in [("L1", l1), ("L2", l2), ("K1", kid)] {
        assert!(
            isl.devices.contains(&id),
            "{what} missing from the fused island's device list: {isl:?}"
        );
    }
}

// --- LinearIsland refusal -------------------------------------------------------

/// A coupled island is linear-classified, so it reaches the state-space
/// reducer under Auto; compiling it would drop the mutual terms from the A
/// matrix (and k=1 would need a singular inverse). It must refuse.
#[test]
fn linear_island_with_coupling_forces_mna() {
    let (c, _, _) = transformer(0.9, 1e-3, 4e-3);
    let p = Partition::analyze(&c);
    assert_eq!(p.islands.len(), 1);
    let isl = &p.islands[0];
    assert!(isl.linear, "the coupled RL island is linear-classified");
    assert!(
        LinearIsland::compile(&c, isl, 1e-12, 27.0).is_none(),
        "state-space reducer must refuse a coupled island (di/dt = L^-1 v \
         does not exist at k=1; MNA stamps L directly)"
    );
}

// --- assembly / partition parity ------------------------------------------------

fn max_deviation(a: &Waveforms, b: &Waveforms, reltol: f64, vntol: f64) -> f64 {
    assert_eq!(a.time.len(), b.time.len(), "different sample grids");
    let mut worst = 0.0f64;
    for (wa, wb) in a.node_voltages.iter().zip(b.node_voltages.iter()) {
        for (&va, &vb) in wa.iter().zip(wb.iter()) {
            assert!(va.is_finite() && vb.is_finite());
            let bound = reltol * va.abs().max(vb.abs()) + vntol;
            worst = worst.max((va - vb).abs() / bound);
        }
    }
    worst
}

/// Planned two-tier assembly (−M folded into the reactive backbone, history
/// through the windings' RhsOnly restamps) vs the interpreted reference.
#[test]
fn planned_assembly_matches_interpreted_on_coupled_deck() {
    let (c, _, _) = transformer(0.999, 1e-3, 4e-3);
    let tstop = 200e-6;
    let interp = Transient::new(tran_opts(Partitioning::Off, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("interpreted converged");
    let planned = Transient::new(tran_opts(Partitioning::Off, AssemblyMode::Planned))
        .run(&c, tstop)
        .expect("planned converged");
    let ratio = max_deviation(&interp, &planned, 1e-9, 1e-9);
    println!("coupled planned-vs-interpreted worst tol ratio = {ratio:.3e}");
    assert!(ratio <= 1.0, "planned assembly out of tolerance: {ratio}");
}

/// Partitioned Auto (the fused island solves in its own sub-system, with the
/// Coupling carried into the sub-circuit and retargeted) vs monolithic Off.
#[test]
fn partitioned_auto_matches_monolithic_on_coupled_deck() {
    let (c, _, _) = transformer(0.999, 1e-3, 4e-3);
    let tstop = 200e-6;
    let mono = Transient::new(tran_opts(Partitioning::Off, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("monolithic converged");
    let auto = Transient::new(tran_opts(Partitioning::Auto, AssemblyMode::Interpreted))
        .run(&c, tstop)
        .expect("partitioned converged");
    let ratio = max_deviation(&mono, &auto, 1e-9, 1e-9);
    println!("coupled auto-vs-monolithic worst tol ratio = {ratio:.3e}");
    assert!(ratio <= 1.0, "partitioned path out of tolerance: {ratio}");
}

// --- integration-rule coverage ---------------------------------------------------

/// The mutual history term has a distinct expression per rule (BE / trap /
/// Gear2). All three must agree on the steady-state amplitude to integration
/// accuracy at this dt (BE is O(dt): loosest band).
#[test]
fn all_integration_rules_agree_on_transformer_amplitude() {
    let (c, _, _) = transformer(0.999, 1e-3, 4e-3);
    let tstop = 500e-6;
    let mut amps = Vec::new();
    for rule in [
        Integration::Trapezoidal,
        Integration::BackwardEuler,
        Integration::Gear2,
    ] {
        let opts = SolverOptions {
            integration: rule,
            ..tran_opts(Partitioning::Off, AssemblyMode::Interpreted)
        };
        let wf = Transient::new(opts)
            .run(&c, tstop)
            .unwrap_or_else(|e| panic!("{rule:?} run failed: {e}"));
        amps.push((rule, last_cycle_amplitude(&c, &wf, "sec", tstop)));
    }
    let base = amps[0].1;
    for (rule, a) in &amps {
        assert!(
            (a - base).abs() / base < 0.02,
            "{rule:?} amplitude {a:.4} departs trapezoidal {base:.4}"
        );
    }
}

// --- AC: jwM against the closed form ---------------------------------------------

/// Solve the two-winding phasor network by hand and hold the AC engine to it
/// at every sweep point: with I1/I2 the winding currents (a->b),
///   (Vin - Vp)/Rs = I1,  Vp = jw(L1 I1 + M I2),
///   Vs = jw(M I1 + L2 I2),  I2 = -Vs/RL.
#[test]
fn ac_matches_two_winding_closed_form() {
    let (c, _, _) = transformer(0.999, 1e-3, 4e-3);
    let (l1, l2, k, rs, rl) = (1e-3f64, 4e-3f64, 0.999f64, 50.0f64, 1e3f64);
    let resp = AcAnalysis::new(SolverOptions::default())
        .run(
            &c,
            &AcSpec {
                fstart: 100.0,
                fstop: 1e6,
                points: 10,
                sweep: Sweep::Decade,
            },
        )
        .expect("ac sweep runs");

    let mut checked = 0;
    for p in &resp.points {
        let vs = closed_form_vsec(p.freq, k, l1, l2, rs, rl);
        let got = p
            .node(&c, "sec")
            .expect("sec phasor present");
        let err = (got - vs).norm();
        assert!(
            err <= 1e-6 + 1e-9 * vs.norm().max(got.norm()),
            "f={} Hz: engine {got:?} vs closed form {vs:?} (|err|={err:.3e})",
            p.freq
        );
        checked += 1;
    }
    assert!(checked > 30, "sweep produced too few points: {checked}");
}
