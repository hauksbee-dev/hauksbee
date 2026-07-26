//! Newton convergence of a behavioral op-amp follower driven to (or past) a
//! rail. A unity-gain follower (out tied to in-) whose + input sits at a rail
//! has its operating point inside a linear window only `span/gain` volts wide;
//! the hard rail clamp used to drop every input tangent outside that window,
//! so Newton degenerated to a Picard fixed-point map v_out <- clamp(gain*(vp -
//! v_out)) that flips rail-to-rail forever (the stormduino LM358 follower on
//! /D13 under firmware GPIO drive). These tests pin the cure: the solve must
//! CONVERGE, to the closed-form operating point, from a cold start on either
//! rail.

use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{
    dc_operating_point, Integration, SolverOptions, StepControl, Transient, Workspace,
};

/// Unity-gain follower: out tied to the inverting input, + input driven by a
/// DC source. LM358-like: gain 1e5, rails 0..32, no pole/slew (the ideal
/// instantaneous stamp, the exact configuration that limit-cycled).
fn follower(vin: f64, gain: f64, rail_lo: f64, rail_hi: f64) -> Circuit {
    let mut c = Circuit::new();
    let inp = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "VIN".into(),
        p: inp,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(vin),
    });
    c.add(Device::OpAmp {
        name: "U1".into(),
        out,
        inp,
        inn: out,
        reference: None,
        gain,
        pole_hz: None,
        slew: None,
        rail_lo,
        rail_hi,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e6,
        tc1: None,
    });
    c
}

fn dc_out(c: &Circuit) -> f64 {
    let opts = SolverOptions::default();
    let mut ws = Workspace::new(c);
    dc_operating_point(&mut ws, c, &opts).expect("follower DC converges");
    let out = c.find_node("out").unwrap();
    ws.layout.node(out).map(|i| ws.x[i]).unwrap_or(0.0)
}

/// Follower's exact operating point when the loop stays linear: the classic
/// finite-gain error, v_out = vin * gain / (1 + gain).
fn linear_op(vin: f64, gain: f64) -> f64 {
    vin * gain / (1.0 + gain)
}

/// The stormduino case: + input at 5 V (MCU GPIO high), LM358 rails 0..32.
/// The operating point 4.99995 V lies in a 320 uV window Newton used to
/// leapfrog forever.
#[test]
fn follower_driven_at_5v_converges() {
    let gain = 1e5;
    let v = dc_out(&follower(5.0, gain, 0.0, 32.0));
    let want = linear_op(5.0, gain);
    assert!(
        (v - want).abs() < 1e-6,
        "follower at 5V: got {v}, want {want}"
    );
}

/// + input exactly AT the high rail: the operating point sits `rail/gain`
/// below the rail, still inside the linear window.
#[test]
fn follower_driven_at_rail_converges() {
    let gain = 1e5;
    let v = dc_out(&follower(5.0, gain, 0.0, 5.0));
    let want = linear_op(5.0, gain);
    assert!(
        (v - want).abs() < 1e-6,
        "follower at rail: got {v}, want {want}"
    );
}

/// + input PAST the high rail: the loop truly saturates and the output must
/// land on the rail itself, not oscillate.
#[test]
fn follower_driven_past_rail_converges_to_rail() {
    let gain = 1e5;
    let v = dc_out(&follower(6.0, gain, 0.0, 5.0));
    // The 1 ohm output stage into the 1 M load drops the rail by 5e-6 V, so
    // allow that divider on top of the rail value.
    assert!(
        (v - 5.0).abs() < 1e-4,
        "follower past rail: got {v}, want 5"
    );
}

/// Same past-the-low-rail case, mirrored.
#[test]
fn follower_driven_past_low_rail_converges_to_rail() {
    let gain = 1e5;
    let v = dc_out(&follower(-1.0, gain, 0.0, 5.0));
    assert!(
        (v - 0.0).abs() < 1e-6,
        "follower past low rail: got {v}, want 0"
    );
}

/// The transient shape of the board failure: the + input square-waves 0..5 V
/// (the GPIO toggle) and every step's Newton must converge, with the output
/// tracking the input to the finite-gain error.
#[test]
fn follower_tracks_square_wave_transient() {
    let gain = 1e5;
    let mut c = Circuit::new();
    let inp = c.node("in");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "VIN".into(),
        p: inp,
        n: NodeId::GROUND,
        kind: SourceKind::Pulse {
            v1: 0.0,
            v2: 5.0,
            delay: 1e-3,
            rise: 1e-6,
            fall: 1e-6,
            width: 2e-3,
            period: 4e-3,
        },
    });
    c.add(Device::OpAmp {
        name: "U1".into(),
        out,
        inp,
        inn: out,
        reference: None,
        gain,
        pole_hz: None,
        slew: None,
        rail_lo: 0.0,
        rail_hi: 32.0,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: out,
        b: NodeId::GROUND,
        ohms: 1e6,
        tc1: None,
    });
    let opts = SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt: 1e-5 },
        ..SolverOptions::default()
    };
    let wf = Transient::new(opts)
        .run(&c, 8e-3)
        .expect("transient converges");
    let vout = wf.node(&c, "out").unwrap();
    let want_hi = linear_op(5.0, gain);
    // Mid-high-phase and mid-low-phase samples must both track.
    let at = |t: f64| {
        let i = wf
            .time
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1 - t).abs().partial_cmp(&(b.1 - t).abs()).unwrap())
            .unwrap()
            .0;
        vout[i]
    };
    assert!(
        (at(2e-3) - want_hi).abs() < 1e-3,
        "high phase: got {}, want {want_hi}",
        at(2e-3)
    );
    assert!(at(3.8e-3).abs() < 1e-3, "low phase: got {}", at(3.8e-3));
}
