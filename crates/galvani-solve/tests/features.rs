//! Feature coverage: Gear-2 integration, the SpiceLoader -> solver path, the
//! streaming callback, the device-effect toggles, and a switch event.

use galvani_ir::{Circuit, Device, NodeId, SourceKind, SpiceLoader};
use galvani_solve::{
    DeviceEffects, Integration, SolverOptions, StepControl, Transient,
};

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
    let net = "d\nV1 a 0 DC 0.7\nR1 a n 100\nD1 n 0 DMOD\n.model DMOD D(IS=1e-14 N=1)\n.temp 100\n.end\n";
    let mut circuit = SpiceLoader::load(net).unwrap();
    circuit.temp_c = 100.0;

    let hot = {
        let opts = SolverOptions {
            temperature_c: 100.0,
            effects: DeviceEffects { temperature: true, ..DeviceEffects::default() },
            ..SolverOptions::fixed(1e-6)
        };
        Transient::new(opts).run(&circuit, 1e-6).unwrap().final_node(&circuit, "n").unwrap()
    };
    let nominal = {
        let opts = SolverOptions {
            temperature_c: 100.0,
            effects: DeviceEffects { temperature: false, ..DeviceEffects::default() },
            ..SolverOptions::fixed(1e-6)
        };
        Transient::new(opts).run(&circuit, 1e-6).unwrap().final_node(&circuit, "n").unwrap()
    };
    // Hotter diode conducts more -> lower node voltage at n than the nominal-T
    // model. The two must differ measurably.
    assert!((hot - nominal).abs() > 5e-3, "temp toggle had no effect: {hot} vs {nominal}");
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

    let wf = Transient::new(SolverOptions::fixed(1e-6)).run(&c, 1e-6).unwrap();
    // Closed switch (1 ohm) + 1k load divider -> out ~ 5 * 1000/1001.
    let out_v = wf.final_node(&c, "out").unwrap();
    assert!(out_v > 4.9, "switch should conduct, out = {out_v}");
}
