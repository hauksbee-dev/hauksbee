//! Transient fidelity of the behavioral op-amp's output dynamics
//! (fix/r9-opamp-transient-pole): the finite-bandwidth pole (`pole_hz`) and
//! the slew-rate limit (`slew`, V/µs) must shape the TRANSIENT output, not
//! just the `.ac` small-signal path. Each test computes the closed-form
//! answer in the test itself.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{
    AcAnalysis, AcSpec, Integration, LoopStability, SolverOptions, StepControl, Sweep, Transient,
};

/// Open-loop op-amp driven by a voltage step on its non-inverting input,
/// inverting input grounded, output lightly loaded (1 MΩ to ground so the
/// node tracks the internal drive EMF through the 1 Ω output stage to within
/// 1e-6 relative). `gain` = 1 keeps the target inside the rails so the pole
/// response is a clean first-order rise, not a rail slam.
fn step_circuit(
    gain: f64,
    pole_hz: Option<f64>,
    slew: Option<f64>,
    v_step: f64,
    rail_hi: f64,
) -> Circuit {
    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let out = ckt.node("out");
    ckt.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        // 0 -> v_step, effectively instantaneous edge (1 ns), single pulse.
        kind: SourceKind::Pulse {
            v1: 0.0,
            v2: v_step,
            delay: 0.0,
            rise: 1e-9,
            fall: 1e-9,
            width: 1e3,
            period: 0.0,
        },
    });
    ckt.add(Device::OpAmp {
        name: "U1".into(),
        out,
        inp: vin,
        inn: NodeId::GROUND,
        reference: None,
        gain,
        pole_hz,
        slew,
        rail_lo: 0.0,
        rail_hi,
    });
    ckt.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e6,
        tc1: None,
    });
    ckt
}

fn run_fixed(ckt: &Circuit, dt: f64, tstop: f64) -> (Vec<f64>, Vec<f64>) {
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(ckt, tstop).unwrap();
    let out = wf.node(ckt, "out").unwrap().to_vec();
    (wf.time, out)
}

/// Sample the waveform at the grid point nearest `t`.
fn at(time: &[f64], wave: &[f64], t: f64) -> f64 {
    let i = time
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - t).abs().partial_cmp(&(b.1 - t).abs()).unwrap())
        .unwrap()
        .0;
    wave[i]
}

/// (1) Finite `pole_hz`: a step input produces the first-order rise
/// `v(t) = V·(1 - exp(-t/τ))`, τ = 1/(2π·pole_hz), NOT an instantaneous
/// jump. Checks the ~63% point at t = τ and the whole trajectory.
#[test]
fn pole_gives_first_order_rise_not_instant_jump() {
    let tau = 1e-4_f64;
    let pole_hz = 1.0 / (std::f64::consts::TAU * tau);
    let v = 1.0;
    let dt = tau / 100.0;
    let ckt = step_circuit(1.0, Some(pole_hz), None, v, 5.0);
    let (time, out) = run_fixed(&ckt, dt, 5.0 * tau);

    // No instantaneous edge: one step after the input step the output has
    // moved only ~dt/τ ≈ 1% of the way, nowhere near the full step.
    let first = at(&time, &out, dt);
    assert!(
        first < 0.05 * v,
        "output jumped to {first} V one step in, pole ignored in transient"
    );

    // ~63.2% at t = τ.
    let v_tau = at(&time, &out, tau);
    let want = v * (1.0 - (-1.0f64).exp()); // 0.6321…
    assert!(
        (v_tau - want).abs() < 0.02 * v,
        "out(τ) = {v_tau}, want ~{want} (first-order rise)"
    );

    // Whole trajectory within 2% of the analytic exponential.
    for (&t, &got) in time.iter().zip(&out) {
        let want = v * (1.0 - (-t / tau).exp());
        assert!(
            (got - want).abs() < 0.02 * v,
            "at t={t:.3e}: got {got}, want {want}"
        );
    }
}

/// (2) Finite `slew` (V/µs): a large fast step produces a linear ramp with
/// per-step change bounded by slew·dt, not an instant edge.
#[test]
fn slew_limits_step_to_linear_ramp() {
    // gain 1e5 rails the target at 5 V immediately; slew 1 V/µs must turn the
    // edge into a 5 µs ramp.
    let slew = 1.0; // V/µs -> 1e6 V/s
    let dt = 1e-7;
    let ckt = step_circuit(1e5, None, Some(slew), 1.0, 5.0);
    let (time, out) = run_fixed(&ckt, dt, 8e-6);

    // Per-sample rate never exceeds the slew limit (loading scales by
    // 1e6/(1e6+1), allow 1% headroom on top).
    let dv_max = slew * 1e6 * dt;
    for w in time.windows(2).zip(out.windows(2)) {
        let (tw, vw) = w;
        let dv = (vw[1] - vw[0]).abs();
        assert!(
            dv <= dv_max * 1.01,
            "slew violated between t={:.3e} and t={:.3e}: dv={dv} > {dv_max}",
            tw[0],
            tw[1]
        );
    }

    // Mid-ramp: v(2 µs) ≈ 2 V.
    let v2 = at(&time, &out, 2e-6);
    assert!((v2 - 2.0).abs() < 0.1, "out(2µs) = {v2}, want ~2.0 (ramp)");
    // Ramp tops out at the rail by ~5 µs and holds.
    let v6 = at(&time, &out, 6e-6);
    assert!((v6 - 5.0).abs() < 0.05, "out(6µs) = {v6}, want 5.0");
}

/// (3) No pole, no slew (absent or zero): the output still matches the old
/// instantaneous ideal; the classic behavior is the degradation target.
#[test]
fn ideal_opamp_still_instantaneous() {
    let dt = 1e-7;
    for (pole_hz, slew) in [(None, None), (Some(0.0), Some(0.0))] {
        let ckt = step_circuit(1e5, pole_hz, slew, 1.0, 5.0);
        let (time, out) = run_fixed(&ckt, dt, 2e-6);
        // One step after the edge the output is already at the rail (through
        // the 1 Ω / 1 MΩ divider).
        let first = at(&time, &out, dt);
        assert!(
            (first - 5.0).abs() < 0.01,
            "pole={pole_hz:?} slew={slew:?}: out(dt) = {first}, want the \
             instantaneous 5.0"
        );
        // And stays there.
        let last = *out.last().unwrap();
        assert!((last - 5.0).abs() < 0.01, "final {last}, want 5.0");
    }
}

/// (4) AC path unchanged: the loop-gain margins of the classic single-pole
/// buffer are identical with and without a transient slew spec (slew is a
/// large-signal limit, no small-signal effect), and the `pole_hz` AC behavior
/// itself is already pinned by `ac_validation.rs`.
#[test]
fn ac_margins_unchanged_by_slew() {
    let build = |slew: Option<f64>| {
        let a0 = 1.0e5;
        let mut ckt = Circuit::new();
        let fb = ckt.node("fb");
        let out = ckt.node("out");
        ckt.add(Device::Vsource {
            name: "VINJ".into(),
            p: fb,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(0.0),
        });
        ckt.add(Device::OpAmp {
            name: "U1".into(),
            out,
            inp: NodeId::GROUND,
            inn: fb,
            reference: None,
            gain: a0,
            pole_hz: Some(100.0),
            slew,
            rail_lo: -1e9,
            rail_hi: 1e9,
        });
        ckt.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 1e6,
            tc1: None,
        });
        ckt
    };
    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1e8,
        points: 50,
        sweep: Sweep::Decade,
    };
    let margins = |slew: Option<f64>| {
        let ckt = build(slew);
        let resp = AcAnalysis::new(SolverOptions::fixed(1e-6))
            .run(&ckt, &spec)
            .unwrap();
        let ls = LoopStability::from_response(&resp, &ckt, "out").unwrap();
        let m = ls.margins();
        (m.gain_crossover_hz, m.phase_margin_deg)
    };
    let (fc_a, pm_a) = margins(None);
    let (fc_b, pm_b) = margins(Some(0.5));
    assert_eq!(fc_a, fc_b, "slew changed the AC gain crossover");
    assert_eq!(pm_a, pm_b, "slew changed the AC phase margin");
}

/// (4) R14: an op-amp with output dynamics that lands in a torn/partitioned
/// island must produce the SAME output as the monolith. The partitioned engine
/// omitted the `Device::OpAmp` arm from BOTH its reactive seed and its advance,
/// so the op-amp's internal drive EMF was never seeded to the DC operating point
/// nor rolled forward, `stamp_opamp` re-derived the output from a frozen v_prev
/// of 0 every step and the bandwidth-limited output collapsed toward 0 instead
/// of tracking gain·(v+−v−). Under `Partitioning::Auto` (fragmented by extra RC
/// legs so the partitioned engine genuinely engages) the output must match
/// `Partitioning::Off` and must NOT sit near 0.
#[test]
fn partitioned_opamp_output_matches_monolith() {
    use hauksbee_solve::Partitioning;
    let tau = 1e-4_f64;
    let pole_hz = 1.0 / (std::f64::consts::TAU * tau);
    let build = || {
        let mut c = Circuit::new();
        let vin = c.node("in");
        let out = c.node("out");
        c.add(Device::Vsource {
            name: "VIN".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::OpAmp {
            name: "U1".into(),
            out,
            inp: vin,
            inn: NodeId::GROUND,
            reference: None,
            gain: 1.0,
            pole_hz: Some(pole_hz),
            slew: None,
            rail_lo: 0.0,
            rail_hi: 5.0,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 1e6,
            tc1: None,
        });
        // Independent RC legs so the partition heuristics fragment and the
        // partitioned engine genuinely engages (the op-amp lands in its island).
        for k in 0..4 {
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
    let c = build();
    let opts_off = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: tau / 20.0 },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let opts_auto = SolverOptions {
        partitioning: Partitioning::Auto,
        ..opts_off
    };
    let tstop = 20.0 * tau;
    let off = Transient::new(opts_off).run(&c, tstop).unwrap();
    let auto = Transient::new(opts_auto).run(&c, tstop).unwrap();
    let w_off = off.node(&c, "out").unwrap();
    let w_auto = auto.node(&c, "out").unwrap();

    // (a) The op-amp visibly tracks its DC target (~1 V). A never-seeded and
    // never-advanced EMF would leave the output collapsed near 0 V.
    let last_auto = *w_auto.last().unwrap();
    assert!(
        (last_auto - 1.0).abs() < 0.02,
        "partitioned op-amp must settle ~1 V, got {last_auto} \
         (near 0 V would mean the reactive seed/advance was skipped)"
    );
    // (b) Auto agrees with the monolith across the whole waveform.
    let mut max_abs = 0.0f64;
    for (x, y) in w_off.iter().zip(w_auto) {
        max_abs = max_abs.max((x - y).abs());
    }
    assert!(
        max_abs < 5e-3,
        "partitioned vs monolithic op-amp waveform diverged: {max_abs:.3e}"
    );
}
