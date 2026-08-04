//! Analytic validation of the small-signal AC solver against closed-form
//! responses. These are the confidence bar for the whole feature: every check
//! compares the solver to a textbook transfer function to a tight tolerance.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{AcAnalysis, AcSpec, LoopStability, SolverOptions, Sweep};

fn opts() -> SolverOptions {
    SolverOptions::fixed(1e-6)
}

/// Options selecting the SMOOTH analog pass element rather than the default
/// SPICE3 relay. The switch tests below are about the smooth device: a relay is
/// at `ron` or at `roff` and has no mid-transition bias to modulate, and its two
/// SPDT throws cannot both be part-way on, so a break-before-make contest between
/// analog conductances is not something it can exhibit. Both are real properties
/// of the real pass element, so they are tested on the model that has them.
fn smooth_switch_opts() -> SolverOptions {
    let mut o = opts();
    o.effects.switch_model = hauksbee_solve::SwitchModel::Smooth;
    o
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
        slew: None,
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

/// R21: the op-amp reference feedthrough must be rolled off by the same
/// bandwidth pole as the gain path (matching the transient stamp, which
/// low-passes the WHOLE driven target vref + gain*(vp-vn)). With both inputs
/// grounded the gain path is silent, so the output equals the pole-filtered
/// reference. Before the fix the reference coupling was frequency-independent,
/// leaving a spurious flat 0 dB feedthrough at high frequency (an INA/difference
/// amp with a moving REF would read ~60 dB too high on its reference-path Bode).
#[test]
fn opamp_reference_feedthrough_rolls_off_with_the_bandwidth_pole() {
    let pole_hz = 100.0;
    let mut ckt = Circuit::new();
    let vref = ckt.node("vref");
    let out = ckt.node("out");
    // Unit AC excitation on the reference node.
    ckt.add(Device::Vsource {
        name: "VREF".into(),
        p: vref,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    // Both inputs grounded => gain path contributes nothing; out follows the
    // (pole-filtered) reference. Finite pole at 100 Hz.
    ckt.add(Device::OpAmp {
        name: "U1".into(),
        out,
        inp: NodeId::GROUND,
        inn: NodeId::GROUND,
        reference: Some(vref),
        gain: 1000.0,
        pole_hz: Some(pole_hz),
        slew: None,
        rail_lo: -1e9,
        rail_hi: 1e9,
    });

    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1e5,
        points: 50,
        sweep: Sweep::Decade,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let bode = resp.bode(&ckt, "out");
    let db_at = |f_target: f64| -> f64 {
        bode.iter()
            .min_by(|a, b| {
                (a.0 - f_target)
                    .abs()
                    .partial_cmp(&(b.0 - f_target).abs())
                    .unwrap()
            })
            .map(|&(_, db, _)| db)
            .expect("bode has points")
    };
    // Far below the pole: unity feedthrough (~0 dB), so DC agrees with transient.
    assert!(
        db_at(1.0).abs() < 0.2,
        "low-f reference feedthrough ~0 dB: {}",
        db_at(1.0)
    );
    // ~1 decade above the pole: -20 dB/decade => ~-20 dB (well off the 0 dB flat).
    assert!(
        db_at(1000.0) < -15.0,
        "1 decade past pole rolls off: {} dB",
        db_at(1000.0)
    );
    // Three decades above the pole (100 kHz): ~-60 dB, nowhere near the pre-fix
    // flat 0 dB feedthrough floor.
    assert!(
        db_at(1e5) < -45.0,
        "reference feedthrough must roll off far past the pole, not stay flat: {} dB",
        db_at(1e5)
    );
}

// --- AC-vs-DC solver parity (fix/ac-parity) ----------------------------------
//
// The AC assembly was written separately from the DC/transient assembly; each
// test below gates one device arm's small-signal stamp against the semantics
// the DC path settled on (stamp.rs), through black-box transfer functions.

/// A 0-Ω jumper in the signal path must be a SHORT (the transient stamp's
/// 1e-6 Ω floor), not an open: `mid` follows `in` and the RC transfer at
/// `out` is the analytic corner, jumper or no jumper.
#[test]
fn zero_ohm_jumper_is_a_short_in_ac() {
    let r = 1.0e3_f64;
    let c = 159.155e-9_f64; // fc ~ 1 kHz with R = 1k
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
        name: "R0".into(),
        a: vin,
        b: mid,
        ohms: 0.0, // the jumper
        tc1: None,
    });
    ckt.add(Device::Resistor {
        name: "R1".into(),
        a: mid,
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
    let spec = AcSpec {
        fstart: fc,
        fstop: fc,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    // The node behind the jumper carries the full drive: |V(mid)| = 1, 0 deg.
    let vmid = resp.points[0].node(&ckt, "mid").unwrap();
    assert!(
        (vmid.norm() - 1.0).abs() < 1e-5,
        "mid should follow in through the 0-ohm jumper, |V(mid)|={}",
        vmid.norm()
    );
    assert!(vmid.arg().abs() < 1e-4, "jumper phase shift {}", vmid.arg());
    // And the transfer at out is the untouched analytic RC corner.
    let (_f, db, phase) = resp.bode(&ckt, "out")[0];
    assert!(
        (db + 3.0103).abs() < 1e-3,
        "corner gain {db} dB (want -3.01)"
    );
    assert!(
        (phase + 45.0).abs() < 1e-3,
        "corner phase {phase} deg (want -45)"
    );
}

/// The AC resistor takes the SAME tc1 temperature derating as the DC path:
/// at 77 C with tc1 = 4e-3, R = 1k derates to 1.2k and the RC corner moves to
/// 1/(2π·1.2k·C), where the response must be exactly -3.01 dB / -45 deg.
#[test]
fn ac_resistor_gets_the_dc_paths_tc1_derating() {
    let r = 1.0e3_f64;
    let tc1 = 4e-3_f64;
    let temp_c = 77.0_f64; // +50 C over the 27 C reference
    let r_derated = r * (1.0 + tc1 * (temp_c - 27.0)); // 1.2k, stamp.rs formula
    let c = 159.155e-9_f64;
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
        tc1: Some(tc1),
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: c,
        ic: None,
    });
    let mut o = opts();
    o.temperature_c = temp_c; // effects.temperature defaults on
    let fc = 1.0 / (std::f64::consts::TAU * r_derated * c);
    let spec = AcSpec {
        fstart: fc,
        fstop: fc,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(o).run(&ckt, &spec).unwrap();
    let (_f, db, phase) = resp.bode(&ckt, "out")[0];
    assert!(
        (db + 3.0103).abs() < 1e-3,
        "derated corner gain {db} dB (want -3.01 at fc of the DERATED R)"
    );
    assert!(
        (phase + 45.0).abs() < 1e-3,
        "derated corner phase {phase} deg"
    );
}

/// A comparator output is held at its rail through the DC stamp's 1-S output
/// stage; small-signal that rail is AC ground. AC-coupling a unit drive onto
/// the output through 1 nF at 1 kHz must divide down to ~ωC/1S ≈ 6.3e-6, not
/// float at the full drive through gmin (the pre-fix open).
#[test]
fn comparator_output_is_held_by_its_output_stage_in_ac() {
    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let x = ckt.node("x");
    ckt.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::Capacitor {
        name: "CC".into(),
        a: vin,
        b: x,
        farads: 1e-9,
        ic: None,
    });
    ckt.add(Device::Comparator {
        name: "U1".into(),
        out: x,
        inp: NodeId::GROUND,
        inn: NodeId::GROUND,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.0,
    });
    let spec = AcSpec {
        fstart: 1e3,
        fstop: 1e3,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let vx = resp.points[0].node(&ckt, "x").unwrap().norm();
    let expect = std::f64::consts::TAU * 1e3 * 1e-9; // ωC / 1 S
    assert!(
        vx < 1e-4,
        "comparator output floats in AC: |V(x)|={vx} (want ~{expect:.2e})"
    );
    assert!(
        (vx - expect).abs() < 0.05 * expect,
        "|V(x)|={vx}, want the ωC/gout divider ~{expect:.2e}"
    );
}

/// At a cold operating point (vbe ≈ 0, vbc strongly reverse) the true
/// small-signal gmu is ~0 and the DC stamp leaves gpi/gmu UNFLOORED; a gmin
/// floor on gmu coupled a floating base to a driven collector through the
/// phantom divider gmu/(gpi+gmu+gmin) ≈ 1/3. The base must stay quiet.
#[test]
fn cold_bjt_base_is_not_coupled_through_a_floored_gmu() {
    let mut ckt = Circuit::new();
    let nc = ckt.node("c");
    let nb = ckt.node("b");
    ckt.add(Device::Vsource {
        name: "VC".into(),
        p: nc,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    ckt.add(Device::Bjt {
        name: "Q1".into(),
        c: nc,
        b: nb,
        e: NodeId::GROUND,
        model: hauksbee_ir::BjtModel::default(),
    });
    let spec = AcSpec {
        fstart: 1e3,
        fstop: 1e3,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let vb = resp.points[0].node(&ckt, "b").unwrap().norm();
    let vc = resp.points[0].node(&ckt, "c").unwrap().norm();
    assert!((vc - 1.0).abs() < 1e-9, "collector is driven, |V(c)|={vc}");
    assert!(
        vb / vc < 1e-2,
        "cold base picked up the collector drive: |V(b)|/|V(c)| = {} (floored gmu gives ~0.33)",
        vb / vc
    );
}

/// SPDT break-before-make parity: two paired legs (`*_s0`/`*_s1`, shared
/// common node) both nominally on; the DC stamp's winner-take-all collapses
/// the lower-margin leg to roff. The AC quiescent conductance must match:
/// the losing rail-side leg must NOT short the injection node in `.ac`.
#[test]
fn spdt_bbm_loser_leg_is_open_in_ac_like_dc() {
    let mut ckt = Circuit::new();
    let a = ckt.node("a");
    let out = ckt.node("out");
    let inj = ckt.node("inj");
    let c0 = ckt.node("c0");
    let c1 = ckt.node("c1");
    // Dedicated AC injection: VINJ drives, the control biases are AC-grounded.
    ckt.add(Device::Vsource {
        name: "VINJ".into(),
        p: inj,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::Resistor {
        name: "RS".into(),
        a: inj,
        b: a,
        ohms: 1.0e3,
        tc1: None,
    });
    ckt.add(Device::Vsource {
        name: "VC0".into(),
        p: c0,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0), // margin (5-1.5)/1 = 3.5: firmly selected
    });
    ckt.add(Device::Vsource {
        name: "VC1".into(),
        p: c1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.5), // margin (2.5-1.5)/1 = 1.0: raw-on, loses
    });
    // Selected throw: a -> out.
    ckt.add(Device::VSwitch {
        name: "SW1_s0".into(),
        a,
        b: out,
        ctrl_p: c0,
        ctrl_n: NodeId::GROUND,
        von: 2.0,
        voff: 1.0,
        ron: 1.0,
        roff: 1e9,
    });
    // Losing throw: a -> GND rail. Bare tanh at margin 1.0 is ~0.94 S (a
    // short); the DC break-before-make drives it to ~roff.
    ckt.add(Device::VSwitch {
        name: "SW1_s1".into(),
        a,
        b: NodeId::GROUND,
        ctrl_p: c1,
        ctrl_n: NodeId::GROUND,
        von: 2.0,
        voff: 1.0,
        ron: 1.0,
        roff: 1e9,
    });
    let spec = AcSpec {
        fstart: 1e3,
        fstop: 1e3,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(smooth_switch_opts())
        .run(&ckt, &spec)
        .unwrap();
    let va = resp.points[0].node(&ckt, "a").unwrap().norm();
    let vout = resp.points[0].node(&ckt, "out").unwrap().norm();
    assert!(
        va > 0.9,
        "loser SPDT leg shorted the common node in AC: |V(a)|={va} (bare tanh ~1e-3)"
    );
    // The genuinely selected throw still conducts: out follows a.
    assert!(
        (vout - va).abs() < 1e-3,
        "selected throw should follow: |V(out)|={vout}, |V(a)|={va}"
    );
}

/// The DC limit of an AC sweep: an inductor's branch row at f -> 0 is the
/// transient path's short (v_a - v_b = 0), never a singular/blown stamp, and
/// a capacitor's admittance -> 0 stays regular through the gmin shunt.
#[test]
fn reactive_stamps_are_regular_at_the_dc_limit() {
    let mut ckt = Circuit::new();
    let vin = ckt.node("in");
    let out = ckt.node("out");
    ckt.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(0.0),
    });
    ckt.add(Device::Inductor {
        name: "L1".into(),
        a: vin,
        b: out,
        henries: 1e-3,
        ic: None,
    });
    ckt.add(Device::Resistor {
        name: "R1".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1.0e3,
        tc1: None,
    });
    ckt.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: None,
    });
    // 1 uHz: ωL ~ 6e-9 Ω, ωC ~ 6e-15 S; the DC limit for both elements.
    let spec = AcSpec {
        fstart: 1e-6,
        fstop: 1e-6,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(opts()).run(&ckt, &spec).unwrap();
    let vout = resp.points[0].node(&ckt, "out").unwrap();
    assert!(
        (vout.norm() - 1.0).abs() < 1e-6,
        "inductor should be the DC short at f->0: |V(out)|={}",
        vout.norm()
    );
    assert!(
        vout.arg().abs() < 1e-4,
        "phase {} at the DC limit",
        vout.arg()
    );
}

/// A voltage switch biased MID-TRANSITION is a modulator: the smooth tanh
/// conductance has a nonzero control tangent, so with v_a != v_b an AC signal
/// on the CONTROL node must appear at the output (the switch-as-VGA path the
/// transient stamp captures via `gm_ctrl = vab * dgsw/dvctrl`). Regression for
/// the AC arm stamping only the quiescent conductance and dropping the
/// control transconductance entirely, `.ac` then reported the transfer
/// function of a fixed resistor (output ~0 here, since the through path is
/// AC-grounded) while transient saw the modulation. The AC answer must match
/// the DC sensitivity d v_out / d v_ctrl (the same tangent the transient
/// Newton stamp linearizes) to small-signal accuracy.
#[test]
fn vswitch_mid_transition_control_modulation_appears_in_ac() {
    // von=2, voff=1 -> vmid=1.5, span=1. ron=100, roff=1e6: at vctrl=vmid the
    // switch sits at gsw=sqrt(gon*goff)=1e-4 S, deep in its transition band.
    // VA holds the through path at 5 V (AC-grounded); VINJ biases the control
    // at vmid and carries the unit AC drive (dedicated-injection convention).
    fn build(vctrl_dc: f64) -> Circuit {
        let mut ckt = Circuit::new();
        let a = ckt.node("a");
        let out = ckt.node("out");
        let ctrl = ckt.node("ctrl");
        ckt.add(Device::Vsource {
            name: "VA".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        ckt.add(Device::Vsource {
            name: "VINJ".into(),
            p: ctrl,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(vctrl_dc),
        });
        ckt.add(Device::VSwitch {
            name: "SW1".into(),
            a,
            b: out,
            ctrl_p: ctrl,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 100.0,
            roff: 1e6,
        });
        ckt.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 1.0e3,
            tc1: None,
        });
        ckt
    }

    let vmid = 1.5;
    let ckt = build(vmid);
    let spec = AcSpec {
        fstart: 1.0,
        fstop: 1.0,
        points: 1,
        sweep: Sweep::Linear,
    };
    let resp = AcAnalysis::new(smooth_switch_opts())
        .run(&ckt, &spec)
        .unwrap();
    let vout_ac = resp.points[0].node(&ckt, "out").unwrap().norm();

    // A fixed resistor at the quiescent conductance gives ~0 here (the through
    // path is AC-grounded; only gmin leaks). The modulation path gives ~5.7.
    assert!(
        vout_ac > 1.0,
        "control modulation path missing from .ac: |V(out)|={vout_ac} \
         (fixed-resistor-only stamp gives ~0)"
    );

    // Transient-path cross-check: the small-signal gain the Newton tangent
    // predicts is the DC sensitivity d v_out / d v_ctrl at the bias. Central
    // finite difference over the FULL nonlinear DC solve.
    let vout_dc = |vc: f64| -> f64 {
        let mut c = build(vc);
        let out = c.node("out");
        let mut ws = hauksbee_solve::Workspace::new(&c);
        hauksbee_solve::dc_operating_point(&mut ws, &c, &smooth_switch_opts()).unwrap();
        ws.x[ws.layout.node(out).unwrap()]
    };
    let d = 1e-5;
    let gain_fd = (vout_dc(vmid + d) - vout_dc(vmid - d)) / (2.0 * d);
    assert!(
        (vout_ac - gain_fd.abs()).abs() / gain_fd.abs() < 1e-2,
        "AC gain must match the transient tangent's DC sensitivity: \
         |V(out)|={vout_ac}, finite-difference dVout/dVctrl={gain_fd}"
    );

    // Gate parity: effects.switch_ctrl_gm=false must drop the modulation path
    // in AC exactly as it does in transient, leaving the bare conductance.
    let mut no_gm = smooth_switch_opts();
    no_gm.effects.switch_ctrl_gm = false;
    let resp = AcAnalysis::new(no_gm).run(&ckt, &spec).unwrap();
    let vout_off = resp.points[0].node(&ckt, "out").unwrap().norm();
    assert!(
        vout_off < 1e-3,
        "switch_ctrl_gm=false should stamp only the quiescent conductance: \
         |V(out)|={vout_off}"
    );
}
