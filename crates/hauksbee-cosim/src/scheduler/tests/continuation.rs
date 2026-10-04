//! A co-sim run is ONE march cut into windows: chunked must equal unchunked.
//!
//! Each case marches a small board through the scheduler in chunks and
//! compares every chunk-end voltage with one uninterrupted `Transient` march
//! of the same circuit over the same span. Forcing that changes mid-run is
//! applied to the scheduler's circuit between chunks, the way the co-sim
//! itself updates sources, and as a PWL corner in the long march.

use super::tests::bound_board;
use super::*;
use hauksbee_ir::{DiodeModel, PwlPoint, Rail, SourceKind, Supply};

const GND: NodeId = NodeId::GROUND;

fn res(c: &mut Circuit, name: &str, a: NodeId, b: NodeId, ohms: f64) {
    c.add(Device::Resistor {
        name: name.into(),
        a,
        b,
        ohms,
        tc1: None,
    });
}

fn cap(c: &mut Circuit, name: &str, a: NodeId, farads: f64) {
    c.add(Device::Capacitor {
        name: name.into(),
        a,
        b: GND,
        farads,
        ic: None,
    });
}

fn vsrc(c: &mut Circuit, name: &str, p: NodeId, kind: SourceKind) -> DeviceId {
    c.add(Device::Vsource {
        name: name.into(),
        p,
        n: GND,
        kind,
    })
}

/// A PWL that is `v` from `t0` (after a `rise` ramp from 0) until `t1`,
/// ramping back to `v_end` by `t1 + rise`.
fn pulse_pwl(v: f64, v_end: f64, t0: f64, t1: f64, rise: f64) -> SourceKind {
    let p = |t, v| PwlPoint { t, v };
    SourceKind::Pwl(vec![
        p(0.0, 0.0),
        p(t0, 0.0),
        p(t0 + rise, v),
        p(t1, v),
        p(t1 + rise, v_end),
    ])
}

/// March `circuit` as `n` chunks of `chunk` through a scheduler built with
/// `opts` and as one long march; after the first chunk, `settle` (the forcing that has ended)
/// replaces the source `src`. Returns `(chunk-end values, long-march values
/// at the same times)` of `node`.
fn chunked_and_long(
    circuit: &Circuit,
    (latching, opts): (bool, SolverOptions),
    (chunk, n): (f64, usize),
    (src, settle): (Option<DeviceId>, SourceKind),
    node: NodeId,
) -> Vec<(f64, f64, f64)> {
    let mut bound = bound_board(
        "continuity",
        circuit.clone(),
        HashMap::new(),
        vec![],
        vec![],
    );
    bound.latching_switches = latching;
    let mut sched = Scheduler::new(bound, None, opts).expect("scheduler");
    sched.chunk_s = chunk;
    let mut chunked = Vec::new();
    for k in 0..n {
        if k == 1 {
            if let Some(Device::Vsource { kind, .. }) =
                src.and_then(|s| sched.circuit.devices.get_mut(s.0 as usize))
            {
                *kind = settle.clone();
            }
        }
        sched.step(chunk);
        assert_eq!(sched.failed_chunk_count(), 0, "chunk {k} failed");
        assert_eq!(sched.fallback_chunk_count(), 0, "chunk {k} needed a rescue");
        chunked.push((sched.sim_time, sched.node_volts[node.0 as usize]));
    }
    let mut opts = sched.opts;
    opts.dc_init = if latching {
        DcInit::FromZero
    } else {
        DcInit::Solve
    };
    // The same span the chunks covered: the scheduler's clock after them.
    let wf = Transient::new(opts)
        .run(circuit, sched.sim_time)
        .expect("long march");
    let v = &wf.node_voltages[node.0 as usize];
    chunked
        .into_iter()
        .map(|(t, got)| {
            let i = wf
                .time
                .partition_point(|&s| s < t)
                .clamp(1, wf.time.len() - 1);
            let (t0, t1) = (wf.time[i - 1], wf.time[i]);
            let w = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
            (t, got, (v[i] - v[i - 1]).mul_add(w, v[i - 1]))
        })
        .collect()
}

fn assert_same(case: &str, rows: &[(f64, f64, f64)], tol: f64) {
    for &(t, got, want) in rows {
        assert!(
            (got - want).abs() <= tol,
            "{case}: at t={t:.3e} s chunked {got:.4} V, uninterrupted {want:.4} V \
             (all: {rows:?})"
        );
    }
}

/// A hysteretic relay that a kick closes and its own divider then holds in
/// the hysteresis band: the latch is the only memory that it is closed. A
/// continuation that re-solved a DC point to learn its latch found the relay
/// OPEN there (the open circuit's steady state is below `voff`, so nothing
/// closes it) and let the hold node decay to 0 V in the second chunk.
#[test]
fn a_latched_relay_stays_latched_across_chunks() {
    let mut c = Circuit::new();
    let (vcc, kick, hold, h) = (c.node("VCC"), c.node("KICK"), c.node("HOLD"), c.node("H"));
    vsrc(&mut c, "V1", vcc, SourceKind::Dc(5.0));
    let vk = vsrc(&mut c, "VK", kick, pulse_pwl(5.0, 0.0, 1e-6, 5e-6, 1e-6));
    c.add(Device::Diode {
        name: "DK".into(),
        a: kick,
        k: hold,
        model: DiodeModel::default(),
    });
    res(&mut c, "RPD", hold, GND, 10e3);
    cap(&mut c, "C1", hold, 1e-9);
    c.add(Device::VSwitch {
        name: "S1".into(),
        a: vcc,
        b: h,
        ctrl_p: hold,
        ctrl_n: GND,
        von: 3.0,
        voff: 1.0,
        ron: 1.0,
        roff: 1e9,
    });
    res(&mut c, "RH", h, hold, 10e3);
    let rows = chunked_and_long(
        &c,
        (false, SolverOptions::default()),
        (20e-6, 5),
        (Some(vk), SourceKind::Dc(0.0)),
        hold,
    );
    assert_same("latched relay", &rows, 5e-3);
    // The physics, not just agreement: the divider holds 5 V * 10k/20k.
    let end = rows.last().expect("rows").1;
    assert!((end - 2.5).abs() < 1e-2, "hold node {end} V");
}

/// A slew-limited follower mid-slew at a chunk boundary. Its lagged output
/// EMF is reactive state the unknowns do not hold; re-seeding it from them
/// snapped it to the rail its saturated target named, so the output leapt
/// at every boundary instead of slewing on.
#[test]
fn a_slewing_op_amp_keeps_slewing_across_chunks() {
    let mut c = Circuit::new();
    let (inp, out) = (c.node("IN"), c.node("OUT"));
    let vin = vsrc(&mut c, "VIN", inp, pulse_pwl(4.0, 4.0, 1e-6, 2e-6, 1e-7));
    c.add(Device::OpAmp {
        name: "U1".into(),
        out,
        inp,
        inn: out,
        reference: None,
        // 1 MHz GBW, a real part's pole. Without one (infinite bandwidth at
        // gain 1e5) the step where the slew clip lets go at the 4 V corner
        // toggles between clipped and free, and even ONE uninterrupted march
        // spends the co-sim's 64-failed-step budget there: a model weakness
        // independent of chunking, so it would test the corner, not the cut.
        gain: 1e5,
        pole_hz: Some(10.0),
        slew: Some(0.1),
        rail_lo: Rail::fixed(0.0),
        rail_hi: Rail::fixed(5.0),
        supply: Supply::default(),
    });
    res(&mut c, "RL", out, GND, 10e3);
    let rows = chunked_and_long(
        &c,
        (false, SolverOptions::default()),
        (10e-6, 6),
        (Some(vin), SourceKind::Dc(4.0)),
        out,
    );
    assert_same("slewing follower", &rows, 5e-3);
    // 0.1 V/us from t = 1 us: 0.9 V at the first boundary, 1.9 V at the next.
    assert!((rows[1].1 - 1.9).abs() < 0.05, "{rows:?}");
}

/// A free-running relay relaxation oscillator on a LATCHING board (power-on
/// from rest, latch carried): chunk ends fall at arbitrary phases of a
/// ~7 us cycle and still read the uninterrupted march's waveform.
#[test]
fn a_relay_oscillator_runs_through_chunk_boundaries() {
    let mut c = Circuit::new();
    let (vcc, osc, d) = (c.node("VCC"), c.node("OSC"), c.node("D"));
    vsrc(&mut c, "V1", vcc, SourceKind::Dc(5.0));
    res(&mut c, "R1", vcc, osc, 10e3);
    cap(&mut c, "C1", osc, 1e-9);
    c.add(Device::VSwitch {
        name: "S1".into(),
        a: osc,
        b: d,
        ctrl_p: osc,
        ctrl_n: GND,
        von: 3.0,
        voff: 1.0,
        ron: 1.0,
        roff: 1e9,
    });
    res(&mut c, "RD", d, GND, 100.0);
    let rows = chunked_and_long(
        &c,
        (true, SolverOptions::default()),
        (5e-6, 10),
        (None, SourceKind::Dc(0.0)),
        osc,
    );
    // On the ~0.4 V/us charging ramp 50 mV is ~0.12 us of phase over 7 cycles.
    assert_same("relay oscillator", &rows, 5e-2);
}

/// An RC charging through many short windows. Each window opens with a
/// backward-Euler step whose companion must not also add the carried
/// trapezoid derivative: counted twice, it ran the charge fast by a fixed
/// amount per window (a 74HC14 RC oscillator ran 15% fast at 1 us windows).
#[test]
fn an_rc_charges_at_its_own_rate_through_many_windows() {
    let mut c = Circuit::new();
    let (vin, out) = (c.node("VIN"), c.node("OUT"));
    let vs = vsrc(&mut c, "V1", vin, pulse_pwl(5.0, 5.0, 1e-6, 1.5e-6, 1e-7));
    res(&mut c, "R1", vin, out, 10e3);
    cap(&mut c, "C1", out, 1e-9);
    // A first step a tenth of the window, as a co-sim's frame-scaled step
    // opens one; at the default 1 ns the double count is too small to see.
    let opts = SolverOptions {
        step: hauksbee_solve::StepControl::Adaptive {
            dt_initial: 2e-7,
            dt_min: 1e-18,
            dt_max: f64::INFINITY,
        },
        ..SolverOptions::default()
    };
    let rows = chunked_and_long(
        &c,
        (false, opts),
        (2e-6, 25),
        (Some(vs), SourceKind::Dc(5.0)),
        out,
    );
    assert_same("rc charge", &rows, 5e-3);
    // tau = 10 us from t = 1 us: 5 V * (1 - e^-4.9) at 50 us.
    let end = rows.last().expect("rows").1;
    assert!((end - 4.963).abs() < 0.02, "charged to {end} V");
}
