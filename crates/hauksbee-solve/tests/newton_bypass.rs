//! S5 gates for the device-evaluation bypass (dev-plan 03 §6.2 / §11).
//!
//! `NewtonBypass::On` may change the Newton iterate PATH, never the accepted
//! answer beyond solver tolerance. These tests run the graded boards (the S2
//! fixtures) and the plan's named high-Z sense-net stress fixture with bypass
//! OFF (the reference) and ON, on identical fixed-step grids so the accepted
//! samples align one-to-one, and assert the waveforms agree within the
//! reltol-scaled convergence tolerance. The stiff flagship-shaped fixture
//! (the linesearch_fixture board, TransientDyn-armed, adaptive) is compared
//! on its physics verdicts (spike count, spike times) because an adaptive
//! grid legitimately re-times under any iterate-path change.
//!
//! The OFF side of gate (b), bypass Off must not move a single ULP, is
//! carried by the existing suites and the linesearch fixture's cross-commit
//! hash (0xb015e2ec03a28c4b), which run with `NewtonBypass::Off` (the
//! default) and therefore never touch the bypass code at all.

use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, SourceKind};
use hauksbee_solve::{
    DcInit, DeviceEffects, EventRetryTuning, Integration, NewtonBypass, Partitioning,
    RobustnessLadder, SolverOptions, StepControl, Strategy, Transient, Waveforms,
};

#[path = "../benches/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
use fixtures::{build_rc_ladder, build_shunt_array};

/// Worst disagreement between two waveform sets on the SAME accepted grid:
/// returns `(max_abs_err, max_normalized_err)` where the normalization is the
/// per-sample convergence tolerance `reltol·max(|a|,|b|) + vntol`. A
/// normalized error <= 1 is "matches the reference to reltol".
fn waveform_err(a: &Waveforms, b: &Waveforms, opts: &SolverOptions) -> (f64, f64) {
    assert_eq!(
        a.time.len(),
        b.time.len(),
        "fixed-step grids must align (got {} vs {} samples)",
        a.time.len(),
        b.time.len()
    );
    let mut max_abs = 0.0f64;
    let mut max_norm = 0.0f64;
    for (col_a, col_b) in a.node_voltages.iter().zip(&b.node_voltages) {
        for (&va, &vb) in col_a.iter().zip(col_b) {
            let err = (va - vb).abs();
            let tol = opts.reltol * va.abs().max(vb.abs()) + opts.vntol;
            max_abs = max_abs.max(err);
            max_norm = max_norm.max(err / tol);
        }
    }
    (max_abs, max_norm)
}

fn run(c: &Circuit, opts: SolverOptions, tstop: f64) -> Waveforms {
    Transient::new(opts).run(c, tstop).expect("march must converge")
}

/// Gate (a), linear rung: the RC ladder. A fully linear board solves in one
/// exact Newton shot per step, so bypass is structurally inert; the ON run
/// must be BIT-IDENTICAL to the reference, not merely within tolerance.
#[test]
fn rc_ladder_bypass_on_is_bit_identical() {
    let c = build_rc_ladder(100);
    let mk = |bypass| SolverOptions {
        step: StepControl::Fixed { dt: 1e-6 },
        partitioning: Partitioning::Off,
        newton_bypass: bypass,
        ..SolverOptions::default()
    };
    let wf_off = run(&c, mk(NewtonBypass::Off), 200e-6);
    let wf_on = run(&c, mk(NewtonBypass::On), 200e-6);
    assert_eq!(wf_off.time.len(), wf_on.time.len());
    for (ca, cb) in wf_off.node_voltages.iter().zip(&wf_on.node_voltages) {
        for (va, vb) in ca.iter().zip(cb) {
            assert_eq!(va.to_bits(), vb.to_bits(), "linear board must be bit-identical");
        }
    }
}

/// Gate (a), nonlinear rungs: the shunt-fed mirror arrays at 24 and 90 blocks
/// (240 in the ignored scaling variant below), monolithic, fixed step. The
/// accepted waveforms with bypass ON must match the no-bypass reference to
/// reltol at every node and sample.
fn mirror_array_gate(blocks: usize, tstop: f64) -> (f64, f64) {
    let (mut c, _membranes) = build_shunt_array(blocks);
    // Modulate the +5 V supply (±0.5 V, 20 us period): the quasi-static DC
    // drive converges every step in <= 2 Newton iterations, and bypass never
    // skips before iteration 3; the gate would be vacuously bit-identical.
    // A moving rail keeps every mirror junction re-solving (3+ iterations),
    // so the ON run genuinely replays cached stamps and the tolerance
    // assertion tests something real (verified via the census skip counter).
    for dev in c.devices.iter_mut() {
        if let Device::Vsource { name, kind, .. } = dev {
            if name == "V5" {
                *kind = SourceKind::Sin {
                    offset: 5.0,
                    amplitude: 0.5,
                    freq: 50e3,
                    delay: 0.0,
                    theta: 0.0,
                    phase: 0.0,
                };
            }
        }
    }
    let mk = |bypass| SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-6 },
        partitioning: Partitioning::Off,
        newton_bypass: bypass,
        ..SolverOptions::default()
    };
    let opts = mk(NewtonBypass::Off);
    let wf_off = run(&c, mk(NewtonBypass::Off), tstop);
    let wf_on = run(&c, mk(NewtonBypass::On), tstop);
    let (max_abs, max_norm) = waveform_err(&wf_off, &wf_on, &opts);
    println!(
        "mirror array {blocks} blocks: bypass-on vs reference max|Δv|={max_abs:.3e} \
         (normalized {max_norm:.3e} of reltol tolerance)"
    );
    assert!(
        max_norm <= 1.0,
        "bypass moved the mirror-array waveform past reltol: {max_abs:.3e} \
         ({max_norm:.2}x tolerance)"
    );
    (max_abs, max_norm)
}

#[test]
fn mirror_array_24_bypass_matches_reference() {
    mirror_array_gate(24, 60e-6);
}

#[test]
fn mirror_array_90_bypass_matches_reference() {
    mirror_array_gate(90, 60e-6);
}

/// The 240-block rung of the scaling sweep (slow; run explicitly).
#[test]
#[ignore = "large board, run explicitly for the S5 report"]
fn mirror_array_240_bypass_matches_reference() {
    mirror_array_gate(240, 60e-6);
}

/// The flagship-shaped stiff fixture (the linesearch_fixture board:
/// integrate-and-fire relaxation oscillator, SPDT pair, TransientDyn armed,
/// staged regularizers + event retry + Armijo line search, FromZero,
/// adaptive). This board exists in `tests/linesearch_fixture.rs`, which must
/// stay the only test in its binary; the construction is replicated verbatim
/// here.
fn spiking_board() -> Circuit {
    let mut c = Circuit::new();
    let m = c.node("m");
    let th = c.node("th");
    let spk = c.node("spk");
    let spkb = c.node("spkb");
    let com = c.node("com");
    let rail = c.node("rail");
    c.add(Device::Vsource {
        name: "VTH".into(),
        p: th,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.5),
    });
    c.add(Device::Vsource {
        name: "VRAIL".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Isource {
        name: "IIN".into(),
        p: NodeId::GROUND,
        n: m,
        kind: SourceKind::Dc(5e-5),
    });
    c.add(Device::Capacitor {
        name: "CM".into(),
        a: m,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: None,
    });
    c.add(Device::Resistor { name: "RL".into(), a: m, b: NodeId::GROUND, ohms: 1e5, tc1: None });
    c.add(Device::Comparator {
        name: "K1".into(),
        out: spk,
        inp: m,
        inn: th,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    c.add(Device::Comparator {
        name: "K2".into(),
        out: spkb,
        inp: th,
        inn: m,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    c.add(Device::VSwitch {
        name: "GATE_s1".into(),
        a: com,
        b: m,
        ctrl_p: spk,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    c.add(Device::VSwitch {
        name: "GATE_s0".into(),
        a: com,
        b: rail,
        ctrl_p: spkb,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    c.add(Device::Resistor { name: "RD".into(), a: com, b: NodeId::GROUND, ohms: 50.0, tc1: None });
    // A junction on the membrane so the stiff path has a bypassable device
    // in it (the linesearch fixture itself is diode-free; the flagship board
    // is not). Anode at the 1 V rail, cathode at the membrane: reverse-biased
    // (quiescent) for the whole inter-spike charge, conducting only in the
    // brief post-reset dip below ~0.45 V; the quiescent-most-of-the-time
    // shape bypass targets, without clamping the firing ramp.
    c.add(Device::Diode {
        name: "DLOAD".into(),
        a: rail,
        k: m,
        model: DiodeModel { is: 4.352e-9, n: 1.906, ..DiodeModel::default() },
    });
    c
}

fn spiking_march(bypass: NewtonBypass) -> (Circuit, Waveforms) {
    let c = spiking_board();
    let opts = SolverOptions {
        step: StepControl::Adaptive {
            dt_initial: 1e-6,
            dt_min: 1e-12,
            dt_max: 2e-6,
        },
        dc_init: DcInit::FromZero,
        ladder: RobustnessLadder::none().with(Strategy::TransientDyn),
        event_retry: EventRetryTuning {
            smooth_comparator_first: true,
            ..Default::default()
        },
        effects: DeviceEffects {
            switch_ctrl_gm: false,
            ..Default::default()
        },
        newton_bypass: bypass,
        ..Default::default()
    };
    let wf = Transient::new(opts)
        .run(&c, 300e-6)
        .expect("armed stiff march must carry the oscillator");
    (c, wf)
}

/// Upward threshold crossings (time of each) of a waveform.
fn crossings(t: &[f64], v: &[f64], thresh: f64) -> Vec<f64> {
    let mut out = Vec::new();
    for k in 1..v.len() {
        if v[k - 1] <= thresh && v[k] > thresh {
            out.push(t[k]);
        }
    }
    out
}

/// Gate (b), ON side: the stiff fixture with bypass ON must reproduce the
/// no-bypass physics, same spike count, spike times within a small fraction
/// of the ~25 us period. (The OFF side, bit-identity of the reference path,
/// is the linesearch fixture's own hash, untouched by this arc.)
#[test]
fn stiff_fixture_bypass_preserves_spike_train() {
    let (c, wf_off) = spiking_march(NewtonBypass::Off);
    let (_c2, wf_on) = spiking_march(NewtonBypass::On);
    let spk_off = wf_off.node(&c, "spk").expect("spk");
    let spk_on = wf_on.node(&c, "spk").expect("spk");
    let x_off = crossings(&wf_off.time, spk_off, 2.5);
    let x_on = crossings(&wf_on.time, spk_on, 2.5);
    println!(
        "stiff fixture: spikes off={} on={} first-spike off={:?} on={:?}",
        x_off.len(),
        x_on.len(),
        x_off.first(),
        x_on.first()
    );
    assert_eq!(
        x_off.len(),
        x_on.len(),
        "bypass changed the spike count: {} vs {}",
        x_off.len(),
        x_on.len()
    );
    // First spike: the pre-drift verdict, held tight.
    let (t0_off, t0_on) = (x_off[0], x_on[0]);
    assert!(
        (t0_off - t0_on).abs() <= 1e-7,
        "bypass moved the FIRST spike: {t0_off} vs {t0_on}"
    );
    // Later spikes: a self-resetting relaxation loop accumulates phase drift
    // under ANY iterate-path change (the fixture's own doctrine; its spike
    // count is asserted as a band, never as exact times). Bound each spike's
    // shift as a fraction of its own elapsed time (phase drift), not as an
    // absolute window.
    let mut worst_frac = 0.0f64;
    let mut worst_dt = 0.0f64;
    for (a, b) in x_off.iter().zip(&x_on) {
        worst_dt = worst_dt.max((a - b).abs());
        worst_frac = worst_frac.max((a - b).abs() / a.max(1e-12));
    }
    println!(
        "stiff fixture: worst spike-time shift {worst_dt:.3e}s ({:.2}% phase drift)",
        worst_frac * 100.0
    );
    assert!(
        worst_frac <= 0.05,
        "bypass drifted a spike by {:.2}% of its elapsed time (> 5%)",
        worst_frac * 100.0
    );
}

/// Gate (c): the plan's named high-Z stress fixture, a weakly-driven
/// sense net in the W1 §2.4 dead-membrane shape. A fixed source behind a
/// large series resistance charges a membrane cap THROUGH A DIODE; the
/// membrane's only other connection is a 1 GΩ leak, so near the comparator
/// threshold every accepted step moves the node by far less than bypasstol,
/// a tiny terminal move that carries the entire signal (the diode's charge
/// current IS the information). If bypass ever wrongly froze that current,
/// the crossing would shift or vanish. The verdict (does the comparator fire,
/// and when) must not change, and on the aligned fixed grid the waveforms
/// must match to reltol.
fn high_z_board(drive_v: f64) -> Circuit {
    let mut c = Circuit::new();
    let drv = c.node("drv");
    let a = c.node("a");
    let mem = c.node("mem");
    let out = c.node("out");
    let th = c.node("th");
    c.add(Device::Vsource {
        name: "VDRV".into(),
        p: drv,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(drive_v),
    });
    // The weak driver: 10 MΩ series (the "driver pass" shape, a fixed source
    // behind series R feeding a kept sense node).
    c.add(Device::Resistor { name: "RDRV".into(), a: drv, b: a, ohms: 10e6, tc1: None });
    c.add(Device::Diode {
        name: "D1".into(),
        a,
        k: mem,
        model: DiodeModel { is: 4.352e-9, n: 1.906, ..DiodeModel::default() },
    });
    c.add(Device::Capacitor {
        name: "CM".into(),
        a: mem,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: Some(0.0),
    });
    // The high-Z anchor: 1 GΩ to ground. Charging current is nA-scale.
    c.add(Device::Resistor { name: "RLEAK".into(), a: mem, b: NodeId::GROUND, ohms: 1e9, tc1: None });
    c.add(Device::Vsource {
        name: "VTH".into(),
        p: th,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.4),
    });
    c.add(Device::Comparator {
        name: "K1".into(),
        out,
        inp: mem,
        inn: th,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.05,
    });
    // The masked-convergence stressor: an UNRELATED stiff junction branch on
    // the same board. The Newton iterations are GLOBAL (one monolithic
    // matrix), so this sine-walked diode forces 3+ iterations on every step,
    // which is exactly when the quiescent sense diode D1 gets bypassed while
    // the rest of the system iterates. Without it every step converges in
    // <= 2 iterations and the bypass never fires (a vacuous gate, measured).
    let aux = c.node("aux");
    let auxm = c.node("auxm");
    c.add(Device::Vsource {
        name: "VAUX".into(),
        p: aux,
        n: NodeId::GROUND,
        kind: SourceKind::Sin {
            offset: 3.5,
            amplitude: 2.0,
            freq: 5e3,
            delay: 0.0,
            theta: 0.0,
            phase: 0.0,
        },
    });
    c.add(Device::Resistor { name: "RAUX".into(), a: aux, b: auxm, ohms: 1e3, tc1: None });
    c.add(Device::Diode {
        name: "DAUX".into(),
        a: auxm,
        k: NodeId::GROUND,
        model: DiodeModel { is: 4.352e-9, n: 1.906, ..DiodeModel::default() },
    });
    c
}

fn high_z_verdict(c: &Circuit, wf: &Waveforms) -> (usize, Option<f64>, f64) {
    let out = wf.node(c, "out").expect("out");
    let x = crossings(&wf.time, out, 2.5);
    let mem_final = wf.final_node(c, "mem").unwrap();
    (x.len(), x.first().copied(), mem_final)
}

#[test]
fn high_z_sense_net_verdict_unchanged() {
    // Super-threshold drive: the membrane must cross 0.4 V and fire once.
    // Sub-threshold drive (0.35 V < the 0.4 V threshold, whatever the diode
    // drops): the membrane can never reach threshold, never fires.
    for (drive, should_fire) in [(1.2, true), (0.35, false)] {
        let c = high_z_board(drive);
        let mk = |bypass| SolverOptions {
            step: StepControl::Fixed { dt: 10e-6 },
            partitioning: Partitioning::Off,
            newton_bypass: bypass,
            ..SolverOptions::default()
        };
        let opts = mk(NewtonBypass::Off);
        let tstop = 20e-3;
        let wf_off = run(&c, mk(NewtonBypass::Off), tstop);
        let wf_on = run(&c, mk(NewtonBypass::On), tstop);
        let (n_off, t_off, mem_off) = high_z_verdict(&c, &wf_off);
        let (n_on, t_on, mem_on) = high_z_verdict(&c, &wf_on);
        println!(
            "high-Z drive={drive}V: fires off={n_off}@{t_off:?} on={n_on}@{t_on:?} \
             mem_final off={mem_off:.6} on={mem_on:.6}"
        );
        assert_eq!(n_off == 1, should_fire, "reference verdict sanity (drive {drive})");
        assert_eq!(n_off, n_on, "bypass changed the fire verdict at drive {drive}");
        if let (Some(a), Some(b)) = (t_off, t_on) {
            assert!(
                (a - b).abs() <= 2.0 * 10e-6,
                "bypass moved the crossing by more than two steps: {a} vs {b}"
            );
        }
        let (max_abs, max_norm) = waveform_err(&wf_off, &wf_on, &opts);
        println!("high-Z drive={drive}V: max|Δv|={max_abs:.3e} (normalized {max_norm:.3e})");
        assert!(
            max_norm <= 1.0,
            "bypass moved the high-Z waveform past reltol: {max_abs:.3e}"
        );
    }
}
