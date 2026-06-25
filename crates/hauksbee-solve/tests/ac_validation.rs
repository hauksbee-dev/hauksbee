//! Analytic validation of the small-signal AC solver against closed-form
//! responses. These are the confidence bar for the whole feature: every check
//! compares the solver to a textbook transfer function to a tight tolerance.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions, Sweep};

fn opts() -> SolverOptions {
    SolverOptions::fixed(1e-6)
}

/// Build an RC low-pass: Vin -> R -> out -> C -> gnd. Unit AC source at `in`.
fn rc_lowpass(r: f64, c: f64) -> (Circuit, f64) {
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
        ohms: r,
        tc1: None,
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: c,
        ic: None,
    });
    let fc = 1.0 / (std::f64::consts::TAU * r * c);
    (ckt, fc)
}

#[test]
fn decade_sweep_includes_endpoints_for_non_integer_decades() {
    // 100 Hz .. 3 kHz is ~1.477 decades: the endpoint must still be present.
    let spec = AcSpec {
        fstart: 100.0,
        fstop: 3000.0,
        points: 10,
        sweep: Sweep::Decade,
    };
    let f = spec.frequencies();
    assert_eq!(f[0], 100.0, "first point should be fstart");
    assert!(
        (f.last().copied().unwrap() - 3000.0).abs() < 1e-6,
        "last={:?}",
        f.last()
    );
    // Monotonic increasing.
    assert!(f.windows(2).all(|w| w[1] > w[0]), "not monotonic: {f:?}");
}

#[test]
fn linear_sweep_hits_both_endpoints() {
    let spec = AcSpec {
        fstart: 10.0,
        fstop: 100.0,
        points: 5,
        sweep: Sweep::Linear,
    };
    let f = spec.frequencies();
    assert_eq!(f.len(), 5);
    assert!((f[0] - 10.0).abs() < 1e-9);
    assert!((f[4] - 100.0).abs() < 1e-9);
    assert!((f[2] - 55.0).abs() < 1e-9, "midpoint {:?}", f[2]);
}

#[test]
fn rc_lowpass_corner_is_minus_3db_minus_45deg() {
    let r = 1.0e3_f64;
    let c = 159.155e-9; // fc = 1/(2 pi RC) ~ 1000.0 Hz
    let (ckt, fc) = rc_lowpass(r, c);
    assert!((fc - 1000.0).abs() < 1.0, "fc={fc}");

    let spec = AcSpec {
        fstart: fc,
        fstop: fc,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let bode = resp.bode(&ckt, "out");
    let (_f, db, phase) = bode[0];

    // At the corner, |H| = 1/sqrt(2) -> -3.0103 dB, phase = -45 deg.
    assert!(
        (db + 3.0103).abs() < 1e-3,
        "corner gain {db} dB (want -3.01)"
    );
    assert!(
        (phase + 45.0).abs() < 1e-3,
        "corner phase {phase} deg (want -45)"
    );
}

#[test]
fn rc_lowpass_rolloff_is_minus_20db_per_decade() {
    let (ckt, fc) = rc_lowpass(1.0e3, 159.155e-9);
    // Two points a decade apart, well above the corner where the asymptote holds.
    let f1 = fc * 100.0;
    let f2 = fc * 1000.0;
    let spec = AcSpec {
        fstart: f1,
        fstop: f2,
        points: 2,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let bode = resp.bode(&ckt, "out");
    let slope = bode[1].1 - bode[0].1; // dB change over one decade
    assert!(
        (slope + 20.0).abs() < 0.1,
        "rolloff {slope} dB/decade (want -20)"
    );
}

#[test]
fn rc_lowpass_matches_closed_form_across_sweep() {
    let r = 4.7e3;
    let c = 22e-9;
    let (ckt, _fc) = rc_lowpass(r, c);
    let spec = AcSpec {
        fstart: 10.0,
        fstop: 1e6,
        points: 20,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    for p in &resp.points {
        let v = p.node(&ckt, "out").unwrap();
        // Closed form H(jw) = 1 / (1 + jwRC).
        let w = std::f64::consts::TAU * p.freq;
        let denom = num_complex::Complex64::new(1.0, w * r * c);
        let h = num_complex::Complex64::new(1.0, 0.0) / denom;
        assert!(
            (v.norm() - h.norm()).abs() < 1e-6,
            "f={} |V|={} want {}",
            p.freq,
            v.norm(),
            h.norm()
        );
        let dphase = (v.arg() - h.arg()).to_degrees().abs();
        assert!(dphase < 1e-3, "f={} phase off by {dphase} deg", p.freq);
    }
}

/// Series RLC bandpass: Vin -> R -> L -> out(C) -> gnd, output across C, sampled
/// as a notch/bandpass. We build the classic series RLC and read the voltage
/// across the capacitor (a 2nd-order low-pass with resonant peak) and check the
/// resonant frequency and Q.
#[test]
fn series_rlc_resonant_frequency_and_q() {
    // f0 = 1/(2 pi sqrt(LC)), Q = (1/R) sqrt(L/C) for a series RLC with output
    // across C (the standard 2nd-order low-pass with peaking).
    let r = 10.0_f64;
    let l = 1e-3_f64;
    let c = 1e-6_f64;
    let f0 = 1.0 / (std::f64::consts::TAU * (l * c).sqrt());
    let q = (1.0 / r) * (l / c).sqrt();

    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let mid = ckt.node("mid");
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
        b: mid,
        ohms: r,
        tc1: None,
    });
    ckt.add(Device::Inductor {
        name: "L1".into(),
        a: mid,
        b: out,
        henries: l,
        ic: None,
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: c,
        ic: None,
    });

    // Sweep finely around f0.
    let spec = AcSpec {
        fstart: f0 * 0.1,
        fstop: f0 * 10.0,
        points: 400,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let bode = resp.bode(&ckt, "out");

    // Peak magnitude and its frequency.
    let (peak_f, peak_db) =
        bode.iter()
            .map(|&(f, db, _)| (f, db))
            .fold(
                (0.0, f64::NEG_INFINITY),
                |acc, x| if x.1 > acc.1 { x } else { acc },
            );

    // The peaking frequency of the |V_C| response is fr = f0*sqrt(1 - 1/(2Q^2)),
    // which for Q=3.16 is ~0.975*f0. Allow a small sweep-resolution tolerance.
    let fr = f0 * (1.0 - 1.0 / (2.0 * q * q)).max(0.0).sqrt();
    assert!(
        (peak_f - fr).abs() / fr < 0.02,
        "peak at {peak_f} Hz, want ~{fr} (f0={f0})"
    );

    // Peak magnitude of V_C at resonance ~ Q (in dB, 20 log10 Q), for high Q.
    // Closed form peak |H| = Q / sqrt(1 - 1/(4Q^2)).
    let peak_mag = q / (1.0 - 1.0 / (4.0 * q * q)).sqrt();
    let want_db = 20.0 * peak_mag.log10();
    assert!(
        (peak_db - want_db).abs() < 0.2,
        "peak {peak_db} dB, want {want_db} (Q={q})"
    );

    // Verify exact closed form at f0 itself: H(jw0) = 1/(jw0 R C) -> |H| = Q.
    let spec0 = AcSpec {
        fstart: f0,
        fstop: f0,
        points: 1,
        sweep: Sweep::Linear,
    };
    let r0 = AcAnalysis::new(opts()).run(&ckt, &spec0).unwrap();
    let v0 = r0.points[0].node(&ckt, "out").unwrap();
    assert!(
        (v0.norm() - q).abs() / q < 1e-4,
        "|H(f0)|={} want Q={q}",
        v0.norm()
    );
    // At resonance the cap voltage lags the source by 90 deg.
    assert!(
        (v0.arg().to_degrees() + 90.0).abs() < 1e-2,
        "phase {} deg",
        v0.arg().to_degrees()
    );
}

/// RLC notch (band-stop): output taken across the series L+C tank to ground is a
/// notch at f0. Built as Vin -> R -> out, with a series LC from out to ground.
/// At f0 the LC is a short, so V_out -> 0 (deep notch).
#[test]
fn series_lc_notch_at_resonance() {
    let r = 1.0e3_f64;
    let l = 1e-3_f64;
    let c = 1e-6_f64;
    let f0 = 1.0 / (std::f64::consts::TAU * (l * c).sqrt());

    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let out = ckt.node("out");
    let tank = ckt.node("tank");
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
        ohms: r,
        tc1: None,
    });
    // Series L then C from out to ground: a series LC trap.
    ckt.add(Device::Inductor {
        name: "L1".into(),
        a: out,
        b: tank,
        henries: l,
        ic: None,
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: tank,
        b: NodeId::GROUND,
        farads: c,
        ic: None,
    });

    let spec = AcSpec {
        fstart: f0,
        fstop: f0,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let v = resp.points[0].node(&ckt, "out").unwrap();
    // At f0 the series LC is ~0 ohms, so out is pulled to ground: deep notch.
    assert!(
        v.norm() < 1e-6,
        "notch depth |V|={} at f0={f0} (want ~0)",
        v.norm()
    );
}

#[test]
fn dedicated_loop_injection_source_keeps_bias_rails_at_ac_ground() {
    let mut ckt = Circuit::new();
    let vcc = ckt.node("vcc");
    let inj = ckt.node("inj");
    let out = ckt.node("out");

    ckt.add(Device::Vsource {
        name: "VCC".into(),
        p: vcc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    ckt.add(Device::Vsource {
        name: "VINJ".into(),
        p: inj,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::Resistor {
        name: "RIN".into(),
        a: inj,
        b: out,
        ohms: 1.0e3,
        tc1: None,
    });
    ckt.add(Device::Resistor {
        name: "RBIAS".into(),
        a: vcc,
        b: out,
        ohms: 1.0e3,
        tc1: None,
    });
    ckt.add(Device::Resistor {
        name: "RLOAD".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1.0e3,
        tc1: None,
    });

    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1.0,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let v = resp.points[0].node(&ckt, "out").unwrap();

    assert!(
        (v.re - 1.0 / 3.0).abs() < 1e-9,
        "VCC should be AC grounded when VINJ is present, got {v}"
    );
    assert!(v.im.abs() < 1e-12, "unexpected imaginary output {v}");
}

/// Textbook op-amp feedback loop with a known phase margin.
///
/// A single-pole op-amp model: open-loop gain A0 with a dominant pole at fp set
/// by an RC on a high-impedance internal node. Configured as a unity-gain
/// buffer, the loop gain is T(jw) = A0/(1 + jw/fp); its unity-gain crossover is
/// at A0*fp and, being a single pole, the phase margin is ~90 deg. We break the
/// loop with an injection source and read the loop gain.
///
/// Topology (loop broken at the inverting input):
///   VINJ injects at `fb` (unit AC).
///   Op-amp: out = A0 * (vp - vn), vp = gnd (0), vn = fb.
///   Dominant pole: out -> `oint` through R, `oint` -> gnd through C, and the
///   op-amp actually drives `oint`; `out` follows `oint` via a unity buffer
///   stage. To keep it simple and analytic we put the RC directly on the op-amp
///   output node and feed `out` back to `fb` directly (unity feedback).
#[test]
fn opamp_unity_buffer_phase_margin_about_90() {
    let a0 = 1.0e5; // 100 dB open-loop DC gain
    let r = 1.0e3_f64;
    let c = 1.5915e-6; // pole fp = 1/(2 pi RC) ~ 100 Hz
    let fp = 1.0 / (std::f64::consts::TAU * r * c);
    assert!((fp - 100.0).abs() < 1.0, "fp={fp}");

    let mut ckt = Circuit::new();
    let fb = ckt.node("fb"); // inverting input / injection node
    let oa = ckt.node("oa"); // raw op-amp output (before pole RC)
    let out = ckt.node("out"); // after the dominant-pole RC

    // Loop-break injection: a unit AC source from ground into `fb`.
    ckt.add(Device::Vsource {
        name: "VINJ".into(),
        p: fb,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    // Op-amp: non-inverting input grounded, inverting input = fb. Large gain.
    // out = A0 * (vp - vn) = A0 * (0 - fb) = -A0 * fb.
    ckt.add(Device::OpAmp {
        name: "U1".into(),
        out: oa,
        inp: NodeId::GROUND,
        inn: fb,
        reference: None,
        gain: a0,
        pole_hz: None,
        rail_lo: -1e9,
        rail_hi: 1e9,
    });
    // Dominant pole: R from oa to out, C from out to ground.
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

    // Sweep wide enough to capture crossover at A0*fp = 10 MHz.
    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1e8,
        points: 50,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    // Read the loop gain T = -V_out at the break point (the LoopStability path
    // applies the summing-junction sign convention).
    let loop_st = LoopStability::from_response(&resp, &ckt, "out").unwrap();
    let m = loop_st.margins();

    let fc = m.gain_crossover_hz.expect("loop should cross 0 dB");
    // Unity-gain crossover ~ A0 * fp.
    assert!(
        (fc - a0 * fp).abs() / (a0 * fp) < 0.1,
        "fc={fc}, want ~{}",
        a0 * fp
    );
    let pm = m.phase_margin_deg.expect("phase margin");
    // Single dominant pole -> ~90 deg phase margin (allowing the inverting sign
    // and a little numerical slack). The buffer is unconditionally stable.
    assert!(pm > 80.0 && pm < 100.0, "phase margin {pm} deg (want ~90)");
}
