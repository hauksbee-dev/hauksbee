//! AC / loop-stability CI surface: spec validation for the `phase_margin` and
//! `ac_gain` assertion kinds, and an end-to-end evaluation of a representative
//! compensated-feedback op-amp loop through the assertion evaluator.
//!
//! No corpus board in the tree exposes a compensated regulator feedback loop
//! that binds to a continuous small-signal model (the regulators bind as
//! behavioral blocks), so the loop-stability demo here is built on a
//! REPRESENTATIVE single-pole op-amp loop constructed in the IR. It exercises
//! the exact CI path a board spec would: an `[ac]` sweep feeds a `phase_margin`
//! assertion, and the evaluator passes/fails on the computed margin.

use hauksbee_ci::assertions::{evaluate, AssertResult};
use hauksbee_ci::runner::{AcOutcome, RunOutcome};
use hauksbee_ir::{Circuit, Device, NodeId, Rail, SourceKind};
use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions, Sweep};

use crate::support::{load_error, load_spec};

// --- spec validation --------------------------------------------------------

#[test]
fn phase_margin_assertion_needs_ac_block() {
    let err = load_error(
        "pm_no_ac.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"phase_margin\"\nnet=\"FB\"\nmin=45\n",
    );
    assert!(err.contains("[ac] sweep block"), "got: {err}");
}

#[test]
fn phase_margin_without_a_bound_is_a_measurement_not_a_silent_gate() {
    // An unbounded numeric assertion used to be a loud spec error. It now
    // loads as a MEASUREMENT: it reports the margin and judges nothing, which
    // is a different thing from a gate that passes without a bound.
    let spec = load_spec(
        "pm_no_bound.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=1\nfstop=1e6\npoints=10\n[[assert]]\nkind=\"phase_margin\"\nnet=\"FB\"\n",
    );
    assert!(spec.asserts[0].measurement(), "{:?}", spec.asserts[0]);
}

#[test]
fn ac_gain_assertion_parses() {
    let spec = load_spec(
        "ac_gain.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e5\npoints=10\nsweep=\"dec\"\n[[assert]]\nkind=\"ac_gain\"\nnet=\"OUT\"\nmax=-3.0\nfreq_hz=1000\n",
    );
    assert!(spec.ac.is_some());
    assert_eq!(spec.asserts.len(), 1);
}

#[test]
fn bad_sweep_mode_is_rejected() {
    let err = load_error(
        "bad_sweep.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e5\npoints=10\nsweep=\"octave\"\n[[assert]]\nkind=\"ac_gain\"\nnet=\"OUT\"\nmin=-3\n",
    );
    assert!(err.contains("dec") || err.contains("lin"), "got: {err}");
}

// --- end-to-end: representative compensated op-amp loop ----------------------

/// Build the representative single-pole op-amp loop (same topology as the solver
/// validation): inverting summing node `fb`, op-amp `oa`, dominant-pole RC to
/// `out`, loop closed by reading T = -V_out at `out`. PM ~ 90 deg, stable.
fn representative_loop(a0: f64, r: f64, c: f64) -> Circuit {
    let mut ckt = Circuit::new();
    let fb = ckt.node("fb");
    let oa = ckt.node("oa");
    let out = ckt.node("out");
    ckt.add(Device::vsource(
        "VINJ",
        fb,
        NodeId::GROUND,
        SourceKind::Dc(0.0),
    ));
    ckt.add(Device::OpAmp {
        name: "U1".into(),
        out: oa,
        inp: NodeId::GROUND,
        inn: fb,
        reference: None,
        gain: a0,
        pole_hz: None,
        slew: None,
        rail_lo: Rail::fixed(-1e9),
        rail_hi: Rail::fixed(1e9),
    });
    ckt.add(Device::resistor("RP", oa, out, r));
    ckt.add(Device::capacitor("CP", out, NodeId::GROUND, c));
    ckt
}

/// Assemble a RunOutcome carrying the AC outcome the runner would build for the
/// `out` net, so we can drive the assertion evaluator directly.
fn outcome_for(ckt: &Circuit, net: &str) -> RunOutcome {
    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1e8,
        points: 50,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(SolverOptions::default())
        .run(ckt, &spec)
        .unwrap();
    let st = LoopStability::from_response(&resp, ckt, net).unwrap();
    let mut ac = AcOutcome::default();
    ac.margins.insert(net.to_string(), st.margins());
    ac.bode.insert(net.to_string(), resp.bode(ckt, net));
    ac_outcome(ac)
}

/// A RunOutcome carrying only `ac`. AC assertions are seed-independent
/// DC-linearised sweeps, never touched by a failed transient chunk: a clean,
/// valid analog outcome.
fn ac_outcome(ac: AcOutcome) -> RunOutcome {
    RunOutcome {
        ambient_c: 25.0,
        ac: Some(ac),
        analog_valid: true,
        ..Default::default()
    }
}

/// The one result of a spec with `[ac] {ac}` and the single `[[assert]] {assert}`,
/// evaluated over `out`.
fn ac_result(name: &str, ac: &str, assert: &str, out: RunOutcome) -> AssertResult {
    let body = format!("board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\n{ac}\n[[assert]]\n{assert}\n");
    let mut results = evaluate(&load_spec(name, &body), &[out]);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// The `[ac]` sweep the loop specs declare, matching [`outcome_for`]'s.
const LOOP_SWEEP: &str = "fstart=1\nfstop=1e8\npoints=50";

fn phase_margin(min_deg: u32) -> String {
    format!("kind=\"phase_margin\"\nnet=\"out\"\nmin={min_deg}")
}

#[test]
fn stable_loop_passes_phase_margin_45() {
    // A0 = 1e5, fp = 100 Hz -> single dominant pole, PM ~ 90 deg, comfortably
    // above the 45 deg comfort bar.
    let ckt = representative_loop(1.0e5, 1.0e3, 1.5915e-6);
    let out = outcome_for(&ckt, "out");

    let r = ac_result("stable.toml", LOOP_SWEEP, &phase_margin(45), out);
    assert!(r.passed, "expected pass, detail: {}", r.detail);
    assert!(r.detail.contains("phase margin"), "{}", r.detail);
}

#[test]
fn marginal_loop_fails_strict_phase_margin() {
    // Same loop, but demand an unrealistic 120 deg margin: a single pole tops
    // out near 90 deg, so this must fail and report the measured margin.
    let ckt = representative_loop(1.0e5, 1.0e3, 1.5915e-6);
    let out = outcome_for(&ckt, "out");

    let r = ac_result("marginal.toml", LOOP_SWEEP, &phase_margin(120), out);
    assert!(!r.passed, "expected fail, detail: {}", r.detail);
}

#[test]
fn ac_gain_assertion_evaluates_rc_corner() {
    // RC low-pass, output at the corner is -3 dB. Assert max <= -2.9 dB there.
    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let out = ckt.node("out");
    ckt.add(Device::vsource(
        "VIN",
        vin,
        NodeId::GROUND,
        SourceKind::Dc(0.0),
    ));
    ckt.add(Device::resistor("R1", vin, out, 1.0e3));
    ckt.add(Device::capacitor("C1", out, NodeId::GROUND, 159.155e-9));

    let spec_ac = AcSpec {
        fstart: 10.0,
        fstop: 1e6,
        points: 30,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(SolverOptions::default())
        .run(&ckt, &spec_ac)
        .unwrap();
    let mut ac = AcOutcome::default();
    ac.bode.insert("out".to_string(), resp.bode(&ckt, "out"));
    let outcome = ac_outcome(ac);

    // At fc=1000 Hz the gain is -3.01 dB, so "max <= -2.9 at 1000 Hz" passes and
    // "min >= -2.9 at 1000 Hz" fails.
    let sweep = "fstart=10\nfstop=1e6\npoints=30";
    let gain = |bound: &str| format!("kind=\"ac_gain\"\nnet=\"out\"\n{bound}=-2.9\nfreq_hz=1000");
    let r = ac_result("acgain_pass.toml", sweep, &gain("max"), outcome.clone());
    assert!(r.passed, "expected pass at corner, detail: {}", r.detail);
    let r = ac_result("acgain_fail.toml", sweep, &gain("min"), outcome);
    assert!(!r.passed, "expected fail at corner, detail: {}", r.detail);
}
