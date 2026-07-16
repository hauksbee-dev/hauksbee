//! Regression tests for defects found in the 2026-07 bug hunt. Each test pins
//! a specific fixed defect so it cannot silently return.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind, SpiceLoader};
use hauksbee_solve::{Partitioning, SolverOptions, StepControl, Transient};

fn rc(farads: f64) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
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
        b: out,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads,
        ic: Some(0.0),
    });
    c
}

/// Bug-hunt #2: the final Fixed step must not overshoot `tstop`. With dt = 1 ns
/// and tstop = 3.5 ns (not a multiple of dt), the last emitted sample used to
/// land at 4 ns, because the `dt_min` floor was applied AFTER the `tstop` clamp
/// — forcing the sub-dt final step back up to a full dt past the stop time.
#[test]
fn fixed_step_does_not_overshoot_tstop() {
    let c = rc(1e-9);
    let dt = 1e-9;
    let tstop = 3.5e-9;
    let opts = SolverOptions {
        step: StepControl::Fixed { dt },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, tstop).unwrap();
    let last = *wf.time.last().expect("at least one sample");
    assert!(
        last <= tstop + dt * 1e-6,
        "final sample time {last:.4e} s overshoots tstop {tstop:.4e} s (by {:.4e} s)",
        last - tstop
    );
}

/// Bug-hunt #3 (black-box): a zero-farad capacitor must not poison the solve
/// with NaN/Inf. The linear fast path used to divide the state matrix by the
/// capacitance; with the guard, the island falls back to the MNA path (0 F = an
/// open) and every emitted voltage stays finite.
#[test]
fn zero_farad_cap_yields_finite_waveform() {
    let c = rc(0.0);
    let opts = SolverOptions {
        step: StepControl::Fixed { dt: 1e-6 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 1e-4).unwrap();
    let out = wf.node(&c, "out").expect("out node");
    assert!(
        out.iter().all(|v| v.is_finite()),
        "zero-farad cap produced a non-finite node voltage: {out:?}"
    );
}

/// Bug-hunt (solver r2 #3): a 0-Ω resistor is a SHORT, not an open. The
/// resistor stamp computes conductance 1/R and used to SKIP a non-positive R,
/// leaving the two nodes coupled only through the 1e-12 gmin shunt — a 0-Ω
/// jumper silently broke its net (node 2 floated near 0 V instead of
/// following node 1). ngspice treats R=0 as a short; hauksbee now clamps to a
/// 1e-6 Ω floor in the SPICE loader AND stamps a stiff conductance for any
/// non-positive R on all three assembly paths (interpreted stamp,
/// linear-island compile, planned backbone).
#[test]
fn zero_ohm_resistor_is_a_short_not_an_open() {
    // The user-facing path: a deck with a 0-Ω jumper from the driven node to
    // the loaded node. V(2) must follow V(1), not float.
    let deck = "jumper\nV1 1 0 DC 1\nR0 1 2 0\nRL 2 0 1k\n.end\n";
    let c = SpiceLoader::load(deck).expect("deck parses");
    for partitioning in [Partitioning::Off, Partitioning::Auto] {
        let opts = SolverOptions {
            step: StepControl::Fixed { dt: 1e-6 },
            partitioning,
            ..SolverOptions::default()
        };
        let wf = Transient::new(opts).run(&c, 1e-5).unwrap();
        let v1 = *wf.node(&c, "1").expect("node 1").last().unwrap();
        let v2 = *wf.node(&c, "2").expect("node 2").last().unwrap();
        assert!(
            (v2 - v1).abs() < 1e-6,
            "0-ohm jumper must short node 2 to node 1 under {partitioning:?}: \
             V(1)={v1:.6} vs V(2)={v2:.6}"
        );
    }

    // The IR path (no loader clamp in the way): a raw Circuit with ohms: 0.0
    // must still short through the stamp's own floor.
    let mut c = Circuit::new();
    let n1 = c.node("1");
    let n2 = c.node("2");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Resistor {
        name: "R0".into(),
        a: n1,
        b: n2,
        ohms: 0.0,
        tc1: None,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: n2,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });
    let opts = SolverOptions {
        step: StepControl::Fixed { dt: 1e-6 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&c, 1e-5).unwrap();
    let v1 = *wf.node(&c, "1").unwrap().last().unwrap();
    let v2 = *wf.node(&c, "2").unwrap().last().unwrap();
    assert!(
        (v2 - v1).abs() < 1e-6,
        "raw ohms=0.0 must stamp a short: V(1)={v1:.6} vs V(2)={v2:.6}"
    );
}

/// Build the RC divider used by the two Auto-vs-Off differential gates below,
/// with an optional `tc1` temperature coefficient on the series resistor.
/// V + R + C is exactly the topology `Partitioning::Auto` peels into a
/// one-state linear island, so the fast state-space path (not the monolithic
/// stamp) owns the resistor value and the step exponential.
fn rc_tc1(tc1: Option<f64>) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
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
        b: out,
        ohms: 1e3,
        tc1,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 1e-6,
        ic: Some(0.0),
    });
    c
}

/// Run the circuit under the given partitioning and return (time, v(out)).
fn run_out(c: &Circuit, opts: SolverOptions, tstop: f64) -> (Vec<f64>, Vec<f64>) {
    let wf = Transient::new(opts).run(c, tstop).unwrap();
    let out = wf.node(c, "out").expect("out node").to_vec();
    (wf.time.clone(), out)
}

/// Bug-hunt #4: a `tc1` resistor classified into a linear island must be
/// temperature-derated exactly like the monolithic stamp derates it. The
/// island compiler used to bake the NOMINAL ohms into its A/B matrices, so at
/// `temperature_c` != 27 the default `Partitioning::Auto` waveform silently
/// disagreed with the `Partitioning::Off` reference (~95% relative error on
/// this fixture) — a wrong thermal-derating sweep with no error anywhere.
#[test]
fn tc1_resistor_matches_monolithic_in_linear_island() {
    // tc1 = 4000 ppm/K at 100 C scales R by 1 + 0.004 * 73 = 1.292: the two
    // paths' time constants differ by ~29% if the derating is dropped.
    let c = rc_tc1(Some(0.004));
    let dt = 1e-5;
    let tstop = 2e-3; // two nominal time constants, well into the knee
    let run = |partitioning: Partitioning| {
        run_out(
            &c,
            SolverOptions {
                step: StepControl::Fixed { dt },
                partitioning,
                temperature_c: 100.0,
                ..SolverOptions::default()
            },
            tstop,
        )
    };
    let (_, mono) = run(Partitioning::Off);
    let (_, auto) = run(Partitioning::Auto);
    assert_eq!(mono.len(), auto.len(), "sample counts must agree");
    let mut worst = 0.0f64;
    for (m, a) in mono.iter().zip(&auto) {
        worst = worst.max((m - a).abs() / m.abs().max(0.1));
    }
    assert!(
        worst < 1e-3,
        "Auto vs Off diverge under tc1 derating at 100 C: worst rel err {worst:.3e}"
    );
}

/// Bug-hunt #5: when `tstop` is not an integer multiple of `dt`, the linear
/// island's step loop truncates the FINAL step to `h = tstop - t < dt` — but
/// the cached matrix exponential was built for the full `dt`, so the last
/// sample (and a co-sim chunk's exit state) replayed a full-dt advance over
/// the short interval (~9.5% relative error on this fixture, ~170x the
/// engine's own tolerance). The cache must be rebuilt at the actual `h`.
#[test]
fn truncated_final_step_matches_monolithic() {
    let c = rc_tc1(None);
    // tau = 1 ms; dt = 0.05 tau keeps the two paths' interior integration
    // mismatch (trapezoidal vs exact exponential) well under the tolerance,
    // while a full-dt replay of the dt/4 final step is still ~20x over it.
    let dt = 5e-5;
    let tstop = 1.0125e-3; // 20 full steps + one truncated step of dt/4
    let run = |partitioning: Partitioning| {
        run_out(
            &c,
            SolverOptions {
                step: StepControl::Fixed { dt },
                partitioning,
                ..SolverOptions::default()
            },
            tstop,
        )
    };
    let (t_mono, mono) = run(Partitioning::Off);
    let (t_auto, auto) = run(Partitioning::Auto);
    let (tm, m) = (*t_mono.last().unwrap(), *mono.last().unwrap());
    let (ta, a) = (*t_auto.last().unwrap(), *auto.last().unwrap());
    assert!(
        (tm - ta).abs() < dt * 1e-6,
        "final sample times diverge: mono {tm:.6e} vs auto {ta:.6e}"
    );
    let rel = (m - a).abs() / m.abs().max(0.1);
    assert!(
        rel < 1e-3,
        "final sample diverges on the truncated step: mono {m:.6} vs auto {a:.6} \
         (rel err {rel:.3e})"
    );
}

/// Bug-hunt r4 #9: `stamp_bjt` fed the vbc pnjlim call a critical voltage
/// built from nf·Vt while limiting on the nr·Vt scale, so an NF != NR model
/// gated/clamped the reverse junction at the wrong threshold on every Newton
/// iteration. Black-box gate for the fix (the vcrit arithmetic itself is
/// pinned by a unit test in stamp.rs): a DEEPLY SATURATED NF != NR BJT — base
/// overdriven so vbc is a forward junction voltage and the reverse limiter is
/// exercised on the way in — must converge to a sane saturation point.
#[test]
fn saturated_bjt_with_nf_ne_nr_converges() {
    use hauksbee_ir::BjtModel;
    use hauksbee_solve::{run_op, Probe};

    let mut c = Circuit::new();
    let vcc = c.node("vcc");
    let nb = c.node("b");
    let nc = c.node("c");
    c.add(Device::Vsource {
        name: "VCC".into(),
        p: vcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    // Rb sets ib ~ 0.42 mA; bf·ib far exceeds the ~4.8 mA Rc allows: hard
    // saturation, vbc forward.
    c.add(Device::Resistor { name: "RB".into(), a: vcc, b: nb, ohms: 10e3, tc1: None });
    c.add(Device::Resistor { name: "RC".into(), a: vcc, b: nc, ohms: 1e3, tc1: None });
    c.add(Device::Bjt {
        name: "Q1".into(),
        c: nc,
        b: nb,
        e: NodeId::GROUND,
        model: BjtModel { nr: 1.5, ..BjtModel::default() },
    });
    let out = run_op(
        &c,
        &SolverOptions::default(),
        &[Probe::NodeVoltage("c".into()), Probe::NodeVoltage("b".into())],
    )
    .expect("NF != NR saturated BJT must converge");
    let vc = out.rows[0][0];
    let vb = out.rows[0][1];
    // The NR = 1.5 root: solving ic = (cf-cr) - cr/br, ib = cf/bf + cr/br at
    // ib ~ 0.42 mA / ic ~ 5 mA gives cf ~ 5.7 mA, cr ~ 0.36 mA, so
    // vbe = Vt·ln(cf/is) ~ 0.82 V and vbc = 1.5·Vt·ln(cr/is) ~ 1.12 V — the
    // widened reverse junction puts vce slightly NEGATIVE (~-0.30 V), unlike
    // the familiar NR = 1 saturation floor. The gate here is convergence to
    // that sane root, not a particular millivolt.
    assert!(
        vc > -0.6 && vc < 0.2,
        "collector must sit at the deep-saturation root, got {vc} V"
    );
    assert!(
        vb > 0.5 && vb < 1.0,
        "base must sit at a forward junction voltage, got {vb} V"
    );
    assert!(
        vb - vc > 0.5,
        "saturation means a hard-forward b-c junction, got vbc = {} V",
        vb - vc
    );
}

/// Bug-hunt r6 #F7: `IntegCoeffs::for_step` built the Gear2/BDF2 stencil from
/// the CURRENT h alone, assuming a uniform grid — but the adaptive controller
/// changes h at almost every step (growth factor up to 2x), so the 2nd-order
/// stencil was applied to unequally spaced history and silently degraded to
/// FIRST order exactly where Gear2 is selected. The fix threads the previous
/// accepted step into the coefficients (variable-step BDF2, r = h/h_prev; the
/// derivation and order proof live in stamp.rs unit tests). Black-box gate:
/// an adaptive-step Gear2 RC charge must track the analytic exponential to
/// the controller's accuracy class across the whole waveform, not just at the
/// settled tail.
#[test]
fn adaptive_gear2_rc_tracks_analytic() {
    let (r, cap) = (1e3, 1e-6);
    let tau = r * cap;
    let c = rc(cap);
    let opts = SolverOptions {
        integration: hauksbee_solve::Integration::Gear2,
        // dt_initial well below tau and dt_max well above force the
        // controller through a long ramp of h-doubling steps: the exact
        // regime where the uniform-grid stencil is wrong at every step.
        step: StepControl::Adaptive {
            dt_initial: tau / 1e4,
            dt_min: tau / 1e7,
            dt_max: tau / 4.0,
        },
        ..SolverOptions::default()
    };
    let (t, out) = run_out(&c, opts, 5.0 * tau);
    let mut worst = 0.0f64;
    for (ti, oi) in t.iter().zip(&out) {
        let want = 1.0 - (-ti / tau).exp();
        worst = worst.max((oi - want).abs());
    }
    // Measured on this fixture: 1.9e-2 worst error with the uniform-grid
    // coefficients (first-order pollution at every h change), 1.5e-3 with
    // variable-step BDF2 (the reltol=1e-3 accuracy class the controller
    // promises). Gate between them with margin on both sides.
    assert!(
        worst < 4e-3,
        "adaptive Gear2 RC deviates from analytic: worst abs err {worst:.3e} \
         (uniform-grid BDF2 regression gives ~1.9e-2 here)"
    );
}
