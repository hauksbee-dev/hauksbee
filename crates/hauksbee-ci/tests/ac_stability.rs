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

use std::path::PathBuf;

use hauksbee_ci::assertions::evaluate;
use hauksbee_ci::runner::{AcOutcome, RunOutcome};
use hauksbee_ci::Spec;
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions, Sweep};

fn write_tmp(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hauksbee_ci_ac_tests_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

// --- spec validation --------------------------------------------------------

#[test]
fn phase_margin_assertion_needs_ac_block() {
    let p = write_tmp(
        "pm_no_ac.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"phase_margin\"\nnet=\"FB\"\nmin=45\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("[ac] sweep block"), "got: {err}");
}

#[test]
fn phase_margin_needs_a_bound() {
    let p = write_tmp(
        "pm_no_bound.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=1\nfstop=1e6\npoints=10\n[[assert]]\nkind=\"phase_margin\"\nnet=\"FB\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("min"), "got: {err}");
}

#[test]
fn ac_gain_assertion_parses() {
    let p = write_tmp(
        "ac_gain.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e5\npoints=10\nsweep=\"dec\"\n[[assert]]\nkind=\"ac_gain\"\nnet=\"OUT\"\nmax=-3.0\nfreq_hz=1000\n",
    );
    let spec = Spec::load(&p).unwrap();
    assert!(spec.ac.is_some());
    assert_eq!(spec.asserts.len(), 1);
}

#[test]
fn bad_sweep_mode_is_rejected() {
    let p = write_tmp(
        "bad_sweep.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e5\npoints=10\nsweep=\"octave\"\n[[assert]]\nkind=\"ac_gain\"\nnet=\"OUT\"\nmin=-3\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(
        err.to_string().contains("dec") || err.to_string().contains("lin"),
        "got: {err}"
    );
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
    ckt.add(Device::Vsource {
        name: "VINJ".into(),
        p: fb,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::OpAmp {
        name: "U1".into(),
        out: oa,
        inp: NodeId::GROUND,
        inn: fb,
        reference: None,
        gain: a0,
        pole_hz: None,
        slew: None,
        rail_lo: -1e9,
        rail_hi: 1e9,
    });
    ckt.add(Device::Resistor {
        name: "RP".into(),
        a: oa,
        b: out,
        ohms: r,
        tc1: None,
    });
    ckt.add(Device::Capacitor {
        name: "CP".into(),
        a: out,
        b: NodeId::GROUND,
        farads: c,
        ic: None,
    });
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
    RunOutcome {
        bind: None,
        seed: 0,
        windows: Default::default(),
        uart: Default::default(),
        faults: Vec::new(),
        toggles: Default::default(),
        peak_current: Default::default(),
        peripherals: Default::default(),
        rail_windows: Default::default(),
        protection_tripped: Default::default(),
        protection_tripped_scoped: Default::default(),
        ambient_c: 25.0,
        peak_temp_c: Default::default(),
        sim_ms: 0.0,
        boot_first_cross_ms: Default::default(),
        boot_drop_after_cross_ms: Default::default(),
        driven_nets: Default::default(),
        drive_direction_observable: false,
        first_fault_ms: None,
        ac: Some(ac),
        // AC assertions are seed-independent DC-linearised sweeps, never touched
        // by a failed transient chunk: a clean, valid analog outcome.
        analog_valid: true,
        failed_windows: Vec::new(),
        analog_abort: false,
        sampled_values: Vec::new(),
        net_series: std::collections::HashMap::new(),
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        dead_rails: Vec::new(),
        unexercised_bus_ids: Default::default(),
        spi_framing: Default::default(),
    }
}

#[test]
fn stable_loop_passes_phase_margin_45() {
    // A0 = 1e5, fp = 100 Hz -> single dominant pole, PM ~ 90 deg, comfortably
    // above the 45 deg comfort bar.
    let ckt = representative_loop(1.0e5, 1.0e3, 1.5915e-6);
    let out = outcome_for(&ckt, "out");

    let p = write_tmp(
        "stable.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=1\nfstop=1e8\npoints=50\n[[assert]]\nkind=\"phase_margin\"\nnet=\"out\"\nmin=45\n",
    );
    let spec = Spec::load(&p).unwrap();
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].passed,
        "expected pass, detail: {}",
        results[0].detail
    );
    assert!(
        results[0].detail.contains("phase margin"),
        "{}",
        results[0].detail
    );
}

#[test]
fn marginal_loop_fails_strict_phase_margin() {
    // Same loop, but demand an unrealistic 120 deg margin: a single pole tops
    // out near 90 deg, so this must fail and report the measured margin.
    let ckt = representative_loop(1.0e5, 1.0e3, 1.5915e-6);
    let out = outcome_for(&ckt, "out");

    let p = write_tmp(
        "marginal.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=1\nfstop=1e8\npoints=50\n[[assert]]\nkind=\"phase_margin\"\nnet=\"out\"\nmin=120\n",
    );
    let spec = Spec::load(&p).unwrap();
    let results = evaluate(&spec, &[out]);
    assert!(
        !results[0].passed,
        "expected fail, detail: {}",
        results[0].detail
    );
}

#[test]
fn ac_gain_assertion_evaluates_rc_corner() {
    // RC low-pass, output at the corner is -3 dB. Assert max <= -2.9 dB there.
    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let out = ckt.node("out");
    ckt.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: 1.0e3,
        tc1: None,
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 159.155e-9,
        ic: None,
    });

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
    let outcome = RunOutcome {
        bind: None,
        seed: 0,
        windows: Default::default(),
        uart: Default::default(),
        faults: Vec::new(),
        toggles: Default::default(),
        peak_current: Default::default(),
        peripherals: Default::default(),
        rail_windows: Default::default(),
        protection_tripped: Default::default(),
        protection_tripped_scoped: Default::default(),
        ambient_c: 25.0,
        peak_temp_c: Default::default(),
        sim_ms: 0.0,
        boot_first_cross_ms: Default::default(),
        boot_drop_after_cross_ms: Default::default(),
        driven_nets: Default::default(),
        drive_direction_observable: false,
        first_fault_ms: None,
        ac: Some(ac),
        analog_valid: true,
        failed_windows: Vec::new(),
        analog_abort: false,
        sampled_values: Vec::new(),
        net_series: std::collections::HashMap::new(),
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        dead_rails: Vec::new(),
        unexercised_bus_ids: Default::default(),
        spi_framing: Default::default(),
    };

    // At fc=1000 Hz the gain is -3.01 dB, so "max <= -2.9 at 1000 Hz" passes and
    // "min >= -2.9 at 1000 Hz" fails.
    let pass = write_tmp(
        "acgain_pass.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e6\npoints=30\n[[assert]]\nkind=\"ac_gain\"\nnet=\"out\"\nmax=-2.9\nfreq_hz=1000\n",
    );
    let spec = Spec::load(&pass).unwrap();
    let r = evaluate(&spec, &[outcome.clone()]);
    assert!(
        r[0].passed,
        "expected pass at corner, detail: {}",
        r[0].detail
    );

    let fail = write_tmp(
        "acgain_fail.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[ac]\nfstart=10\nfstop=1e6\npoints=30\n[[assert]]\nkind=\"ac_gain\"\nnet=\"out\"\nmin=-2.9\nfreq_hz=1000\n",
    );
    let spec = Spec::load(&fail).unwrap();
    let r = evaluate(&spec, &[outcome]);
    assert!(
        !r[0].passed,
        "expected fail at corner, detail: {}",
        r[0].detail
    );
}
