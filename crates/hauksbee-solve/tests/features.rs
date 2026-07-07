//! Feature coverage: Gear-2 integration, the SpiceLoader -> solver path, the
//! streaming callback, the device-effect toggles, and a switch event.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind, SpiceLoader};
use hauksbee_solve::{
    DeviceEffects, Integration, Partitioning, SolverOptions, StepControl, Transient,
};

/// `Partitioning::Off` must reproduce the classic monolithic engine exactly.
/// The partitioned `Auto` path may differ within tolerance (Gauss-Seidel / ZOH
/// coupling), but `Off` is the bit-for-bit reference. We pin a multi-device
/// circuit's full waveform so any future change that perturbs the Off path is
/// caught, and confirm Off is deterministic across runs.
#[test]
fn partitioning_off_is_deterministic_reference() {
    // RLC + a second RC leg off the same rail: exercises nodes, branches, and a
    // shared (partitionable) topology so Off and Auto take different code paths.
    let build = || {
        let mut c = Circuit::new();
        let vin = c.node("in");
        let mid = c.node("mid");
        let out = c.node("out");
        let leg = c.node("leg");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: mid,
            ohms: 50.0,
            tc1: None,
        });
        c.add(Device::Inductor {
            name: "L1".into(),
            a: mid,
            b: out,
            henries: 1e-3,
            ic: Some(0.0),
        });
        c.add(Device::Capacitor {
            name: "C1".into(),
            a: out,
            b: NodeId::GROUND,
            farads: 1e-7,
            ic: Some(0.0),
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: vin,
            b: leg,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "C2".into(),
            a: leg,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        c
    };
    let c = build();
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-6 },
        partitioning: Partitioning::Off,
        ..SolverOptions::default()
    };
    let a = Transient::new(opts).run(&c, 5e-4).unwrap();
    let b = Transient::new(opts).run(&c, 5e-4).unwrap();
    // Determinism: two Off runs are bit-identical.
    let wa = a.node(&c, "out").unwrap();
    let wb = b.node(&c, "out").unwrap();
    assert_eq!(wa, wb, "Off path must be deterministic");
    assert_eq!(a.time, b.time);

    // The partitioned Auto path on the same circuit must agree within tolerance.
    let opts_auto = SolverOptions {
        partitioning: Partitioning::Auto,
        ..opts
    };
    let auto = Transient::new(opts_auto).run(&c, 5e-4).unwrap();
    let wauto = auto.node(&c, "out").unwrap();
    let mut max_abs = 0.0f64;
    for (x, y) in wa.iter().zip(wauto) {
        max_abs = max_abs.max((x - y).abs());
    }
    assert!(max_abs < 5e-3, "Auto vs Off diverged: {max_abs:.3e}");
}

/// Build the standard RC low-pass with a DC step.
fn rc(v: f64, r: f64, cap: f64) -> Circuit {
    let mut c = Circuit::new();
    let vin = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(v),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: r,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: cap,
        ic: Some(0.0),
    });
    c
}

#[test]
fn gear2_matches_rc_analytic() {
    let (v, r, cap) = (5.0, 1e3, 1e-6);
    let tau = r * cap;
    let circuit = rc(v, r, cap);
    let opts = SolverOptions {
        integration: Integration::Gear2,
        step: StepControl::Fixed { dt: tau / 200.0 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 5.0 * tau).unwrap();
    let got = wf.final_node(&circuit, "out").unwrap();
    let want = v * (1.0 - (-5.0f64).exp());
    assert!((got - want).abs() < 5e-3, "Gear2 RC final {got} vs {want}");
}

#[test]
fn backward_euler_runs() {
    let circuit = rc(5.0, 1e3, 1e-6);
    let opts = SolverOptions {
        integration: Integration::BackwardEuler,
        step: StepControl::Fixed { dt: 1e-5 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts).run(&circuit, 5e-3).unwrap();
    // BE is dissipative but should still reach ~99% of 5 V after 5 tau.
    assert!(wf.final_node(&circuit, "out").unwrap() > 4.9);
}

#[test]
fn spice_loader_drives_solver() {
    let net = "rc\nV1 in 0 DC 5\nR1 in out 2k\nC1 out 0 1u\n.end\n";
    let circuit = SpiceLoader::load(net).unwrap();
    let opts = SolverOptions::fixed(1e-5);
    let wf = Transient::new(opts).run(&circuit, 2e-2).unwrap();
    assert!(wf.final_node(&circuit, "out").unwrap() > 4.9);
}

#[test]
fn streaming_callback_sees_every_step() {
    let circuit = rc(5.0, 1e3, 1e-6);
    let opts = SolverOptions::fixed(1e-5);
    let mut count = 0usize;
    let mut last_t = -1.0;
    let mut monotonic = true;
    Transient::new(opts)
        .run_streaming(&circuit, 5e-4, |s| {
            count += 1;
            if s.time < last_t {
                monotonic = false;
            }
            last_t = s.time;
        })
        .unwrap();
    assert!(count > 10, "expected many steps, got {count}");
    assert!(monotonic, "stream times must be non-decreasing");
}

#[test]
fn adaptive_takes_fewer_steps_than_fixed() {
    // The RC settles, so an adaptive solver should coast with large steps once
    // the transient is over, beating a fine fixed step on step count.
    let circuit = rc(5.0, 1e3, 1e-6);
    let tstop = 1e-2;

    let fixed = Transient::new(SolverOptions::fixed(1e-6))
        .run(&circuit, tstop)
        .unwrap();
    let adaptive = Transient::new(SolverOptions::adaptive(1e-7, 1e-4))
        .run(&circuit, tstop)
        .unwrap();

    assert!(
        adaptive.time.len() < fixed.time.len(),
        "adaptive {} should be < fixed {}",
        adaptive.time.len(),
        fixed.time.len()
    );
    // Both must reach the same steady state.
    let fa = fixed.final_node(&circuit, "out").unwrap();
    let aa = adaptive.final_node(&circuit, "out").unwrap();
    assert!((fa - aa).abs() < 1e-3, "fixed {fa} adaptive {aa}");
}

#[test]
fn temperature_toggle_changes_diode_drop() {
    // A diode's forward drop falls ~2 mV/C as IS(T) rises. With the temperature
    // effect off, the drop should be the 27 C value regardless of temp_c.
    let net =
        "d\nV1 a 0 DC 0.7\nR1 a n 100\nD1 n 0 DMOD\n.model DMOD D(IS=1e-14 N=1)\n.temp 100\n.end\n";
    let mut circuit = SpiceLoader::load(net).unwrap();
    circuit.temp_c = 100.0;

    let hot = {
        let opts = SolverOptions {
            temperature_c: 100.0,
            effects: DeviceEffects {
                temperature: true,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(1e-6)
        };
        Transient::new(opts)
            .run(&circuit, 1e-6)
            .unwrap()
            .final_node(&circuit, "n")
            .unwrap()
    };
    let nominal = {
        let opts = SolverOptions {
            temperature_c: 100.0,
            effects: DeviceEffects {
                temperature: false,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(1e-6)
        };
        Transient::new(opts)
            .run(&circuit, 1e-6)
            .unwrap()
            .final_node(&circuit, "n")
            .unwrap()
    };
    // Hotter diode conducts more -> lower node voltage at n than the nominal-T
    // model. The two must differ measurably.
    assert!(
        (hot - nominal).abs() > 5e-3,
        "temp toggle had no effect: {hot} vs {nominal}"
    );
}

/// The §3.4 `DeviceEffects` contract for `junction_caps` (dev-plan 04):
/// flipping the toggle on a diode model that carries charge fields must CHANGE
/// the solution (charge terms appear in the stamp), and on a model without
/// them the two runs must be BIT-IDENTICAL — the physics only activates when
/// the model asks for it AND the toggle allows it.
#[test]
fn junction_caps_toggle_is_honored_for_diodes() {
    // A pulse through R into a charge-heavy diode: the depletion+diffusion
    // charge visibly slows the cathode node's edges.
    let net_charge = "d\nV1 in 0 PULSE(0 5 1u 100n 100n 4u 10u)\nR1 in n 1k\nD1 n 0 DMOD\n\
                      .model DMOD D(IS=1e-14 N=1 CJO=100p TT=100n)\n.end\n";
    let net_plain = "d\nV1 in 0 PULSE(0 5 1u 100n 100n 4u 10u)\nR1 in n 1k\nD1 n 0 DMOD\n\
                     .model DMOD D(IS=1e-14 N=1)\n.end\n";
    let run = |net: &str, junction_caps: bool| {
        let circuit = SpiceLoader::load(net).unwrap();
        let opts = SolverOptions {
            effects: DeviceEffects {
                junction_caps,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(10e-9)
        };
        let out = Transient::new(opts).run(&circuit, 8e-6).unwrap();
        out.node(&circuit, "n").unwrap().to_vec()
    };
    // Charge-carrying model: the toggle must change the waveform.
    let w_on = run(net_charge, true);
    let w_off = run(net_charge, false);
    let max_diff = w_on
        .iter()
        .zip(&w_off)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    assert!(
        max_diff > 1e-3,
        "junction_caps=on must change a charge-carrying diode's waveform (max diff {max_diff:e})"
    );
    // Charge-free model: bit-identical whatever the toggle (the existing-deck
    // compatibility bar).
    let p_on = run(net_plain, true);
    let p_off = run(net_plain, false);
    assert_eq!(p_on.len(), p_off.len());
    for (i, (a, b)) in p_on.iter().zip(&p_off).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "charge-free deck must be BIT-identical across the toggle (sample {i}: {a} vs {b})"
        );
    }
}

/// The §3.4 `DeviceEffects` contract for `junction_caps` on BJTs (dev-plan 04
/// §3.2): flipping the toggle on a model carrying cje/cjc/tf must CHANGE the
/// solution (both junction-charge companions appear), and on a default model
/// the two runs must be BIT-IDENTICAL — the diode's contract, extended.
#[test]
fn junction_caps_toggle_is_honored_for_bjts() {
    // A saturated switching transistor driven off: the stored base charge
    // (tf + cje/cjc) visibly delays and slows the collector's rising edge.
    let net_charge = "q\nVCC vcc 0 DC 5\nVB in 0 PULSE(5 0 1u 50n 50n 4u 10u)\n\
                      RB in b 10k\nRC vcc c 1k\nQ1 c b 0 QSW\n\
                      .model QSW NPN(IS=1e-15 BF=100 CJE=20p CJC=8p TF=500n)\n.end\n";
    let net_plain = "q\nVCC vcc 0 DC 5\nVB in 0 PULSE(5 0 1u 50n 50n 4u 10u)\n\
                     RB in b 10k\nRC vcc c 1k\nQ1 c b 0 QSW\n\
                     .model QSW NPN(IS=1e-15 BF=100)\n.end\n";
    let run = |net: &str, junction_caps: bool| {
        let circuit = SpiceLoader::load(net).unwrap();
        let opts = SolverOptions {
            effects: DeviceEffects {
                junction_caps,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(10e-9)
        };
        let out = Transient::new(opts).run(&circuit, 8e-6).unwrap();
        out.node(&circuit, "c").unwrap().to_vec()
    };
    let w_on = run(net_charge, true);
    let w_off = run(net_charge, false);
    let max_diff = w_on
        .iter()
        .zip(&w_off)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    assert!(
        max_diff > 1e-2,
        "junction_caps=on must change a charge-carrying BJT's waveform (max diff {max_diff:e})"
    );
    let p_on = run(net_plain, true);
    let p_off = run(net_plain, false);
    assert_eq!(p_on.len(), p_off.len());
    for (i, (a, b)) in p_on.iter().zip(&p_off).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "charge-free BJT deck must be BIT-identical across the toggle (sample {i}: {a} vs {b})"
        );
    }
}

/// The §3.4 `DeviceEffects` contract for `junction_caps` on MOSFETs
/// (dev-plan 04 §3.3): flipping the toggle on a model carrying gate-charge
/// fields (overlap CGSO/CGDO + TOX intrinsic) must CHANGE the solution (both
/// gate-charge companions appear and reshape the switching edges), and on a
/// default model the two runs must be BIT-IDENTICAL — the diode's and BJT's
/// contract, extended to the third junction device.
#[test]
fn junction_caps_toggle_is_honored_for_mosfets() {
    // The §3.3 load-switch shape: gate driven through RG so the gate charge
    // sets the switching timing; without the charge companions the drain
    // would snap with the (RG-free) gate voltage.
    let net_charge = "m\nVDD vdd 0 DC 5\nVG in 0 PULSE(0 5 1u 100n 100n 4u 10u)\n\
                      RG in g 10k\nRL vdd d 100\nM1 d g 0 0 MSW\n\
                      .model MSW NMOS(VTO=2 KP=1e-2 W=1m L=10u TOX=50n CGSO=2e-9 CGDO=2e-9)\n\
                      .end\n";
    let net_plain = "m\nVDD vdd 0 DC 5\nVG in 0 PULSE(0 5 1u 100n 100n 4u 10u)\n\
                     RG in g 10k\nRL vdd d 100\nM1 d g 0 0 MSW\n\
                     .model MSW NMOS(VTO=2 KP=1e-2 W=1m L=10u)\n.end\n";
    let run = |net: &str, junction_caps: bool| {
        let circuit = SpiceLoader::load(net).unwrap();
        let opts = SolverOptions {
            effects: DeviceEffects {
                junction_caps,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(10e-9)
        };
        let out = Transient::new(opts).run(&circuit, 8e-6).unwrap();
        out.node(&circuit, "d").unwrap().to_vec()
    };
    let w_on = run(net_charge, true);
    let w_off = run(net_charge, false);
    let max_diff = w_on
        .iter()
        .zip(&w_off)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    assert!(
        max_diff > 1e-2,
        "junction_caps=on must change a gate-charge-carrying MOSFET's waveform \
         (max diff {max_diff:e})"
    );
    let p_on = run(net_plain, true);
    let p_off = run(net_plain, false);
    assert_eq!(p_on.len(), p_off.len());
    for (i, (a, b)) in p_on.iter().zip(&p_off).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "charge-free MOS deck must be BIT-identical across the toggle (sample {i}: {a} vs {b})"
        );
    }
}

/// The MOS body diode is STRUCTURAL physics like the BJT's junctions — no
/// toggle gates its DC branch, only the model fields do (`IS` on the card;
/// deliberately default-OFF, unlike ngspice's 1e-14 default, so pre-§3.3
/// decks are bit-identical). This pins both halves: a card with IS conducts
/// in reverse (the synchronous-rectifier path), a default card leaves the
/// drain UNCLAMPED at the current source's rail.
#[test]
fn mos_body_diode_is_structural_and_model_gated() {
    // ~1 mA pulled out of the drain of an off NMOS through a 1k pulldown to
    // -1.5 V: with the body diode the drain clamps near -0.6 V; without it
    // the drain follows the pulldown to -1.5 V. The rail is deliberately
    // BELOW ground by less than vth: any deeper (e.g. -5 V) and the
    // symmetric Level-1 channel itself conducts in INVERTED mode (the
    // grounded source becomes the effective drain and vgd = +5 exceeds the
    // threshold) — real physics ngspice reproduces, but not the body-diode
    // observable this test isolates.
    let net = |model: &str| {
        format!(
            "m\nVNEG neg 0 DC -1.5\nRD neg d 1k\nM1 d 0 0 0 MB\n{model}\n.op\n.end\n"
        )
    };
    let with_diode = net(".model MB NMOS(VTO=2 KP=1e-2 IS=1e-12)");
    let plain = net(".model MB NMOS(VTO=2 KP=1e-2)");
    let run = |net: &str| {
        let circuit = SpiceLoader::load(net).unwrap();
        let mut ws = hauksbee_solve::Workspace::new(&circuit);
        hauksbee_solve::dc_operating_point(&mut ws, &circuit, &SolverOptions::default()).unwrap();
        ws.x[ws.layout.node(circuit.find_node("d").unwrap()).unwrap()]
    };
    let vd_diode = run(&with_diode);
    let vd_plain = run(&plain);
    assert!(
        (-0.75..=-0.4).contains(&vd_diode),
        "body diode must clamp the drain about one forward drop below the bulk \
         (got {vd_diode} V)"
    );
    assert!(
        vd_plain < -1.4,
        "a default model has NO body diode (bit-identity with pre-§3.3 decks): \
         the drain must follow the pulldown rail (got {vd_plain} V)"
    );
}

/// The §3.4 contract for `series_resistance` on BJTs: flipping the toggle on
/// a model carrying rb/re/rc must CHANGE the solution (the core moves onto
/// internal nodes behind real ohmic drops), and on a default model the runs
/// must be BIT-IDENTICAL (no internal node is even allocated).
#[test]
fn series_resistance_toggle_is_honored_for_bjts() {
    // A saturating common-emitter stage (10k base drive, the same shape the
    // junction_caps deck uses): the collector's saturation floor rises by
    // ic·rc and the base path gains rb — a >10 mV waveform contrast. The
    // drive is deliberately NOT stiffer (e.g. a 2k base): hard saturation
    // sits on a legacy-Jacobian Newton marginality that predates this arc
    // (the default-model deck itself fails there at the base commit's
    // bytes), and the plain leg below must run on those pinned bytes.
    let net_r = "q\nVCC vcc 0 DC 5\nVB in 0 PULSE(0 5 1u 50n 50n 4u 10u)\n\
                 RB in b 10k\nRC vcc c 1k\nQ1 c b 0 QR\n\
                 .model QR NPN(IS=1e-15 BF=100 RB=500 RC=50 RE=5)\n.end\n";
    let net_plain = "q\nVCC vcc 0 DC 5\nVB in 0 PULSE(0 5 1u 50n 50n 4u 10u)\n\
                     RB in b 10k\nRC vcc c 1k\nQ1 c b 0 QR\n\
                     .model QR NPN(IS=1e-15 BF=100)\n.end\n";
    let run = |net: &str, series: bool| {
        let circuit = SpiceLoader::load(net).unwrap();
        let opts = SolverOptions {
            effects: DeviceEffects {
                series_resistance: series,
                ..DeviceEffects::default()
            },
            ..SolverOptions::fixed(10e-9)
        };
        let out = Transient::new(opts).run(&circuit, 8e-6).unwrap();
        out.node(&circuit, "c").unwrap().to_vec()
    };
    let w_on = run(net_r, true);
    let w_off = run(net_r, false);
    let max_diff = w_on
        .iter()
        .zip(&w_off)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    assert!(
        max_diff > 1e-2,
        "series_resistance=on must change an rb/rc/re BJT's waveform (max diff {max_diff:e})"
    );
    let p_on = run(net_plain, true);
    let p_off = run(net_plain, false);
    assert_eq!(p_on.len(), p_off.len());
    for (i, (a, b)) in p_on.iter().zip(&p_off).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "default-model BJT deck must be BIT-identical across the toggle (sample {i}: {a} vs {b})"
        );
    }
}

#[test]
fn voltage_switch_conducts_when_closed() {
    // A switch closes when its control exceeds the threshold, connecting a
    // source to a load. Check the load sees the source once closed.
    let mut c = Circuit::new();
    let vin = c.node("in");
    let ctrl = c.node("ctrl");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "VIN".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    c.add(Device::Vsource {
        name: "VCTRL".into(),
        p: ctrl,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.0), // above threshold -> closed
    });
    c.add(Device::VSwitch {
        name: "S1".into(),
        a: vin,
        b: out,
        ctrl_p: ctrl,
        ctrl_n: NodeId::GROUND,
        von: 2.0,
        voff: 1.0,
        ron: 1.0,
        roff: 1e9,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e3,
        tc1: None,
    });

    let wf = Transient::new(SolverOptions::fixed(1e-6))
        .run(&c, 1e-6)
        .unwrap();
    // Closed switch (1 ohm) + 1k load divider -> out ~ 5 * 1000/1001.
    let out_v = wf.final_node(&c, "out").unwrap();
    assert!(out_v > 4.9, "switch should conduct, out = {out_v}");
}
