//! Power-ramp (pseudo-transient continuation) gates for the `FromZero` /
//! `Ramped` power-on start.
//!
//! The motivating case (ledger e57c0f0, tarski_general_e2e.rs): on the flagship,
//! some solve groups have NO reachable DC operating point (self-resetting
//! oscillator shapes), so the honest way to carry them is to power on: sources
//! ramp from zero, the state integrates from zero, and there is no DC solve to
//! fail.
//!
//! The oscillator fixture here is a COMPARATOR relaxation astable multivibrator,
//! not the classic two-transistor BJT astable named in the spec. That is a
//! deliberate, documented substitution: the BJT astable has a (metastable) DC
//! operating point the homotopy DOES find, so its DC does not "genuinely fail",
//! and its regenerative collector switching is intractable for this solver's
//! fixed-step Newton without the board-specific event apparatus. The comparator
//! astable is a truer fit for the motivating finding: it has NO consistent DC
//! fixed point at all (the comparator cannot rest), so `dc_operating_point`
//! genuinely fails ("DC homotopy failed"), and it marches cleanly through the
//! solver's first-class comparator event handling.

use crate::shared::{add_capacitor, add_resistor, add_vdc};
use hauksbee_ir::{BjtModel, Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::decompose::rails::TearMotive;
use hauksbee_solve::decompose::verify::Decomposition;
use hauksbee_solve::orchestrate::run_staged;
use hauksbee_solve::{
    DcInit, Integration, SolveError, SolvePhase, SolverOptions, StepControl, Transient,
};
use hauksbee_testkit::circuits::{bjt, comparator, vswitch};

/// Inverting comparator relaxation astable: the comparator output `osc` swings
/// 0..5 V, senses the capacitor node `vc` on its inverting input against a
/// 2.5 V reference with hysteresis, and charges/discharges `Cf` through `Rf`.
/// There is no consistent DC (osc = comparator(2.5 - osc) has no solution), so
/// the DC solve genuinely fails; from power-on it oscillates.
fn comparator_astable() -> Circuit {
    let mut c = Circuit::new();
    let vref = c.node("vref");
    add_vdc(&mut c, "VREF", vref, NodeId::GROUND, 2.5);
    let osc = c.node("osc");
    let vc = c.node("vc");
    // inverting: reference on +, cap on -
    comparator(&mut c, "CMP", osc, vref, vc, 1.0);
    crate::shared::add_resistor(&mut c, "Rload", osc, NodeId::GROUND, 10e3);
    crate::shared::add_resistor(&mut c, "Rf", osc, vc, 10e3);
    add_capacitor(&mut c, "Cf", vc, NodeId::GROUND, 10e-9, None);
    c
}

/// Wrap every independent source in a `Ramped` envelope reaching full amplitude
/// at `win` (the power-on retry policy: ramp_window = 200 * dt).
fn ramp_sources(c: &Circuit, win: f64) -> Circuit {
    let mut c = c.clone();
    for d in c.devices.iter_mut() {
        if let Device::Vsource { kind, .. } | Device::Isource { kind, .. } = d {
            let inner = std::mem::replace(kind, SourceKind::Dc(0.0));
            *kind = inner.ramped(win);
        }
    }
    c
}

fn mid_crossings(w: &[f64], mid: f64) -> usize {
    let mut n = 0;
    for i in 1..w.len() {
        if (w[i - 1] - mid).signum() != (w[i] - mid).signum() {
            n += 1;
        }
    }
    n
}

fn fixed(dt: f64) -> SolverOptions {
    SolverOptions {
        step: StepControl::Fixed { dt },
        integration: Integration::Trapezoidal,
        ..SolverOptions::default()
    }
}

/// Gate: an astable with no reachable DC fails `Transient::run` under default
/// options (the DC solve genuinely does not converge), and runs under
/// `FromZero` + `Ramped` sources, producing a real oscillation.
#[test]
fn astable_fails_dc_but_runs_power_on() {
    let c = comparator_astable();
    let tstop = 1e-3;
    let dt = 1e-7;

    // Default options (adaptive, DcInit::Solve): no consistent DC exists, so
    // the operating-point solve fails and the whole run errors.
    let default_run = Transient::new(SolverOptions::default()).run(&c, tstop);
    let err = default_run.expect_err("an astable has no DC operating point to solve");
    assert!(
        matches!(
            err,
            SolveError::NonConvergence {
                phase: SolvePhase::Dc,
                ..
            } | SolveError::Behavioral {
                phase: SolvePhase::Dc,
                ..
            }
        ),
        "expected a DC-init failure, got: {err}"
    );

    // FromZero + Ramped: no DC solve, power on from rest. It must run to
    // completion and oscillate (the output crosses mid-rail many times).
    let ramp_window = 200.0 * dt;
    let ramped = ramp_sources(&c, ramp_window);
    let mut opts = fixed(dt);
    opts.dc_init = DcInit::FromZero;
    let wf = Transient::new(opts)
        .run(&ramped, tstop)
        .expect("power-on from zero must carry the astable");

    // t=0 is a power-on rest state: every node starts at zero.
    for node in 0..c.node_count() {
        assert_eq!(
            wf.node_voltages[node][0], 0.0,
            "power-on t=0 must be zero at node {node}"
        );
    }
    let osc = wf.node(&c, "osc").expect("osc node");
    let crossings = mid_crossings(osc, 2.5);
    assert!(
        crossings >= 3,
        "the astable must oscillate (>=3 mid-rail crossings), saw {crossings}"
    );
}

/// Gate: a well-behaved RC+BJT circuit, run (a) normally and (b) from a power-on
/// ramp, converges to the SAME steady state. Compared past `ramp_window + 5*tau`
/// they agree within 1e-4. Not tighter on purpose: the two runs take DIFFERENT
/// trajectories (a settled DC seed vs an integrated ramp-up) that converge to
/// the same steady state, not to the same round-off; the residual is the tail
/// of the ramp transient, not numerical noise.
#[test]
fn power_on_settles_to_the_same_steady_state() {
    // Common-emitter amp with an output cap: one stable DC, tau = Rc*Cout.
    let mut c = Circuit::new();
    let vcc = c.node("vcc");
    add_vdc(&mut c, "VCC", vcc, NodeId::GROUND, 5.0);
    let col = c.node("col");
    let base = c.node("base");
    let rc = 2.2e3;
    let cout = 10e-9;
    crate::shared::add_resistor(&mut c, "Rc", vcc, col, rc);
    crate::shared::add_resistor(&mut c, "Rb1", vcc, base, 47e3);
    crate::shared::add_resistor(&mut c, "Rb2", base, NodeId::GROUND, 10e3);
    bjt(&mut c, "Q", col, base, NodeId::GROUND, &BjtModel::default());
    add_capacitor(&mut c, "Cout", col, NodeId::GROUND, cout, None);

    let dt = 1e-7;
    let tau = rc * cout; // 22 us
    let ramp_window = 200.0 * dt; // 20 us
                                  // Run well past ramp_window + 5*tau so the ramp transient has fully decayed.
    let tstop = ramp_window + 20.0 * tau;

    let normal = Transient::new(fixed(dt))
        .run(&c, tstop)
        .expect("normal DC-seeded run");

    let ramped = ramp_sources(&c, ramp_window);
    let mut opts = fixed(dt);
    opts.dc_init = DcInit::FromZero;
    let power_on = Transient::new(opts)
        .run(&ramped, tstop)
        .expect("power-on run");

    // Compare every node at the final sample (>= ramp_window + 5*tau).
    for node in 1..c.node_count() {
        let a = *normal.node_voltages[node].last().unwrap();
        let b = *power_on.node_voltages[node].last().unwrap();
        assert!(
            (a - b).abs() <= 1e-4,
            "node {node} did not settle to the same steady state: normal {a:.6} vs power-on {b:.6}"
        );
    }
    // The circuit is not vacuous: the collector sits at a real bias point.
    let colf = *normal.node_voltages[col.0 as usize].last().unwrap();
    assert!(
        colf > 0.5 && colf < 4.5,
        "collector bias looks wrong: {colf}"
    );
}

/// The group index that conducts a named node (mirrors run_staged's routing).
fn group_of_node(d: &Decomposition, node: NodeId) -> Option<usize> {
    let isl = d
        .graph
        .node_island
        .get(node.0 as usize)
        .copied()
        .flatten()?;
    d.dag.groups.iter().position(|g| g.contains(&isl))
}

/// Gate: a staged run whose one group is the astable (its fused DC is
/// unreachable) plus an independent well-behaved neighbour. `run_staged`
/// succeeds by power-ramping the astable group; `ramped_groups` names exactly
/// that group; the neighbour is unaffected.
#[test]
fn run_staged_power_ramps_the_no_dc_group() {
    let mut c = Circuit::new();
    // Group A: the astable (nodes vref, osc, vc). Built inline so we keep the
    // node handles for routing checks.
    let vref = c.node("vref");
    add_vdc(&mut c, "VREF", vref, NodeId::GROUND, 2.5);
    let osc = c.node("osc");
    let vc = c.node("vc");
    comparator(&mut c, "CMP", osc, vref, vc, 1.0);
    crate::shared::add_resistor(&mut c, "Rload", osc, NodeId::GROUND, 10e3);
    crate::shared::add_resistor(&mut c, "Rf", osc, vc, 10e3);
    add_capacitor(&mut c, "Cf", vc, NodeId::GROUND, 10e-9, None);

    // Group B: an independent, well-behaved feedforward RC that solves normally.
    let nin = c.node("nin");
    add_vdc(&mut c, "VN", nin, NodeId::GROUND, 3.0);
    let nout = c.node("nout");
    crate::shared::add_resistor(&mut c, "RN", nin, nout, 1e3);
    add_capacitor(&mut c, "CN", nout, NodeId::GROUND, 10e-9, None);

    let dt = 1e-7;
    let tstop = 1e-3;
    let d = Decomposition::analyze(&c, TearMotive::Profit);
    assert!(d.certificate.sound(), "the decomposition must be sound");

    let staged =
        run_staged(&c, &d, &fixed(dt), tstop).expect("staged run must succeed via power-ramp");

    // The astable group was power-ramped; nothing else was.
    let astable_group = group_of_node(&d, osc).expect("osc must belong to a group");
    assert_eq!(
        staged.ramped_groups,
        vec![astable_group],
        "ramped_groups must name exactly the astable group"
    );

    // The astable actually oscillated in the assembled result.
    let osc_series = staged.waveforms.node(&c, "osc").expect("osc series");
    assert!(
        mid_crossings(osc_series, 2.5) >= 3,
        "the ramped astable group must oscillate in the assembled waveform"
    );

    // The neighbour is unaffected: its RC charged to the source's 3.0 V and its
    // group is not in ramped_groups.
    let neighbour_group = group_of_node(&d, nout).expect("nout must belong to a group");
    assert!(
        !staged.ramped_groups.contains(&neighbour_group),
        "the well-behaved neighbour must not be power-ramped"
    );
    let nout_final = *staged.waveforms.node(&c, "nout").unwrap().last().unwrap();
    assert!(
        (nout_final - 3.0).abs() <= 1e-3,
        "the neighbour RC must settle to 3.0 V, got {nout_final}"
    );
}

/// Gate: run_staged is deterministic. On a fixture with several absorbed
/// drivers, two runs on the same decomposition must produce bitwise-identical
/// executed_groups, torn_groups, and assembled waveforms. This guards the
/// `absorbed`-map ordering fix (device construction order is solver-visible).
#[test]
fn run_staged_is_bitwise_deterministic() {
    let mut c = Circuit::new();
    let sup = c.node("sup");
    add_vdc(&mut c, "VSUP", sup, NodeId::GROUND, 3.3);
    let out = c.node("out"); // shared output couples all switch consumers
    crate::shared::add_resistor(&mut c, "RLcommon", out, NodeId::GROUND, 2e3);
    // Seven asymmetric linear dividers, each SENSED (not conducted) by a switch:
    // the driver pass absorbs each divider into the switch consumer, so the
    // `absorbed` map carries seven entries whose iteration order sets device
    // push order in the consumer sub-circuit.
    let vals = [5.0, 1.2, 4.3, 0.6, 3.9, 2.1, 4.8];
    for (k, &vv) in vals.iter().enumerate() {
        let v = c.node(&format!("vd{k}"));
        add_vdc(&mut c, &format!("VD{k}"), v, NodeId::GROUND, vv);
        let sel = c.node(&format!("sel{k}"));
        add_resistor(
            &mut c,
            &format!("Ra{k}"),
            v,
            sel,
            1e3 * (1.0 + 0.2 * k as f64),
        );
        add_resistor(&mut c, &format!("Rb{k}"), sel, NodeId::GROUND, 1e3);
        vswitch(
            &mut c,
            &format!("SW{k}"),
            sup,
            out,
            (sel, NodeId::GROUND),
            (1.5 + 0.1 * k as f64, 0.5),
            (10.0 + k as f64, 1e9),
        );
    }

    let dt = 1e-7;
    let tstop = 3e-6;
    let d = Decomposition::analyze(&c, TearMotive::Profit);
    assert!(
        d.drivers.len() >= 3,
        "fixture must have several absorbed drivers: {}",
        d.drivers.len()
    );

    let opts = fixed(dt);
    let base = run_staged(&c, &d, &opts, tstop).expect("staged");
    for run in 0..4 {
        let r = run_staged(&c, &d, &opts, tstop).expect("staged");
        assert_eq!(
            r.executed_groups, base.executed_groups,
            "executed_groups drifted on run {run}"
        );
        assert_eq!(
            r.torn_groups, base.torn_groups,
            "torn_groups drifted on run {run}"
        );
        for node in 0..c.node_count() {
            assert_eq!(
                r.waveforms.node_voltages[node], base.waveforms.node_voltages[node],
                "waveform at node {node} not bitwise identical on run {run}"
            );
        }
    }
}
