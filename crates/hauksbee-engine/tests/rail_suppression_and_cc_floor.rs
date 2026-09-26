//! Fresh cases for the suppressed rail and the CC-fold floor (E22-adjacent).
//!
//! Both defects were the same shape: a rail that should read one thing reading
//! another with nothing said. A rail with its supply removed must read ~0 V, not
//! hold a stale value. And a draw just barely over a current limit must settle
//! at the limit with the rail still positive, not collapse to zero or bang-bang,
//! because 1% over is where a fold that anchors to the wrong setpoint shows up.

use crate::power_supply::load_board;
use hauksbee_engine::binder::{bind_board, BoundBoard};
use hauksbee_engine::power_supply::{PowerSupply, UsbSpec};
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::DynamicLoad;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

fn bound_load_board(rload_ohms: &str) -> BoundBoard {
    let text = load_board(rload_ohms);
    let board = ExtractedBoard::from_auto(&text).expect("parse");
    bind_board(&board, &ModelLibrary::builtin())
}

#[test]
fn a_suppressed_rail_with_no_supply_reads_about_zero() {
    let mut bound = bound_load_board("1k");

    // Sanity, so the zero below is a consequence of the suppression and not of a
    // board that never had a rail: the unsuppressed rail sits at 5 V.
    let mut baseline = Scheduler::new(bound_load_board("1k"), None, SolverOptions::default())
        .expect("baseline scheduler");
    baseline.chunk_s = 1e-4;
    for _ in 0..20 {
        baseline.step(1e-4);
    }
    let v0 = *baseline.net_voltages().get("+5V").unwrap_or(&0.0);
    assert!(
        (v0 - 5.0).abs() < 0.05,
        "unsuppressed rail must sit at 5 V, got {v0:.4} V"
    );

    assert!(
        bound.suppress_rail("+5V"),
        "suppressing a rail that exists must report that it did something"
    );
    assert!(
        bound.supplies.iter().all(|s| s.net_name != "+5V"),
        "the suppressed rail must own no supply leg"
    );
    assert!(
        !bound
            .circuit
            .devices
            .iter()
            .any(|d| matches!(d, Device::Vsource { .. })),
        "the leg's internal source must be OPENED, not merely unregistered: {:?}",
        bound.circuit.devices
    );
    let mut sched =
        Scheduler::new(bound, None, SolverOptions::default()).expect("suppressed scheduler");
    sched.chunk_s = 1e-4;
    for _ in 0..20 {
        sched.step(1e-4);
    }
    assert!(sched.analog_valid(), "a floating rail must still solve");
    let v = *sched.net_voltages().get("+5V").unwrap_or(&0.0);
    assert!(
        v.abs() < 1e-3,
        "a rail with no supply must read ~0 V, got {v:.6} V"
    );
}

#[test]
fn a_one_percent_over_limit_draw_settles_at_the_limit_with_a_positive_rail() {
    // 5 V, 0.5 A limit. A load of 9.90099 Ω draws 0.505 A unlimited, exactly 1%
    // over. The fold must land the current ON the limit and leave the rail near
    // 4.95 V, not collapse it: a fold that scales the FRESH setpoint once per
    // chunk, instead of solving the limit with the rail, bang-bangs here forever.
    let limit = 0.5_f64;
    let r_load = 5.0 / (1.01 * limit);
    let mut sched = bench_board(&format!("{r_load:.6}"), (5.0, limit), |_, _| {});
    sched.chunk_s = 1e-4;
    let history: Vec<f64> = (0..400)
        .map(|_| {
            sched.step(1e-4);
            sched.net_voltage("+5V").unwrap_or(0.0)
        })
        .collect();
    let (v, i) = (history[399], sched.supply_states()["+5V"].1);
    assert!(
        (i - limit).abs() <= 0.01 * limit,
        "current must settle ON the {limit} A limit, got {i:.6} A (V={v:.4})"
    );
    assert!(
        v > 4.0,
        "1% over the limit must leave the rail POSITIVE and near 5 V, got {v:.4} V"
    );
    // And it must be settled, not oscillating: the last 50 samples must agree.
    let tail = &history[history.len() - 50..];
    let lo = tail.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = tail.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        hi - lo < 1e-3,
        "the fold must settle, not bang-bang: tail spread {:.6} V over [{lo:.4}, {hi:.4}]",
        hi - lo
    );
}

/// Two-sided: a draw comfortably UNDER the limit must not fold at all, and
/// neither must current pushed INTO the supply. A stronger source back-driving
/// the rail leaves it at its setpoint, sinking through its output resistance
/// as the CV leg always has; that current is not load. Rows: (load, back-drive
/// volts through 0.1 ohm, rail V, supply A reported).
#[test]
fn a_draw_under_the_limit_does_not_fold() {
    for (r_load, back, v_want, i_want) in
        [("100", None, 5.0, 0.05), ("1k", Some(5.2), 5.002, -1.975)]
    {
        let mut sched = bench_board(r_load, (5.0, 0.5), |c, net| {
            if let Some(volts) = back {
                let (node, dc) = (c.node("back"), SourceKind::Dc(volts));
                c.add(Device::vsource("Vback", node, NodeId::GROUND, dc));
                c.add(Device::resistor("Rback", node, net, 0.1));
            }
        });
        sched.chunk_s = 1e-4;
        for _ in 0..50 {
            sched.step(1e-4);
        }
        let v = sched.net_voltage("+5V").unwrap_or(0.0);
        let i = sched.supply_states()["+5V"].1;
        assert!(
            (v - v_want).abs() < 1e-3 && (i - i_want).abs() < 0.02,
            "R={r_load}, back-drive {back:?}: {v:.5} V, {i:.4} A, want {v_want} V, {i_want} A"
        );
    }
}

/// A profile load past a 0.5 A USB port's rating browns the rail out ONTO
/// 0 V, never below: nothing on this board can drive it negative, and a real
/// load draws nothing at 0 V. An ideal 0.6 A sink once pulled its whole
/// demand through the port's 0.5 ohm droop, the excess out of the source's
/// clamp, and read -0.3 V. Two-sided: a draw the port can supply sags by
/// exactly its droop. Rows: (profile amps, band the rail's low must sit in).
#[test]
fn a_profile_load_over_a_port_limit_floors_the_rail_at_zero() {
    for (amps, band) in [(0.6, (-1e-3, 1.0)), (0.4, (4.79759, 4.79761))] {
        let mut bound = bound_load_board("1k");
        let net = bound.node("+5V").expect("rail node");
        let profile = toml::from_str(&format!("id = \"o\"\n[[segment]]\nlevel_a = {amps}"));
        let load = DynamicLoad::new(&mut bound.circuit, "o", net, profile.unwrap(), 0.0, 0);
        let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
        let spec = UsbSpec::V5_0_5A;
        assert!(sched.set_power_supply("+5V", PowerSupply::Usb { spec }));
        sched.attach_peripheral(Box::new(load));
        let low = (0..50)
            .map(|_| {
                sched.step(1e-4);
                sched.net_voltage("+5V").unwrap_or(f64::NAN)
            })
            .fold(f64::INFINITY, f64::min);
        let i = sched.supply_states()["+5V"].1;
        assert!(
            sched.analog_valid() && low >= band.0 && low <= band.1,
            "{amps} A on 5V/0.5A: rail low {low:.6} V (supply {i:.4} A), want {band:?}"
        );
    }
}

/// Two-sided: suppressing a net that carries no rail reports that it did nothing
/// and leaves the board alone. A suppression that silently "succeeds" on a typo'd
/// net name is how a spec ends up testing a rail it never touched.
#[test]
fn suppressing_a_net_with_no_rail_reports_nothing_done() {
    let mut bound = bound_load_board("1k");
    let before = bound.circuit.devices.len();
    assert!(
        !bound.suppress_rail("NOT_A_NET"),
        "an unknown net cannot be suppressed"
    );
    assert!(
        !bound.suppress_rail("GND"),
        "ground carries no auto-rail to suppress"
    );
    assert_eq!(
        bound.circuit.devices.len(),
        before,
        "the board is untouched"
    );
    assert!(
        bound.supplies.iter().any(|s| s.net_name == "+5V"),
        "the real rail must survive an unrelated suppression"
    );
}

/// A scheduler on the load board (`r_load` on +5V, plus whatever `extra`
/// stamps on the rail) fed by a `(volts, limit)` bench supply.
fn bench_board(
    r_load: &str,
    bench: (f64, f64),
    extra: impl FnOnce(&mut Circuit, NodeId),
) -> Scheduler {
    let mut bound = bound_load_board(r_load);
    let net = bound.node("+5V").expect("rail node");
    extra(&mut bound.circuit, net);
    let mut sched = Scheduler::new(bound, None, SolverOptions::default()).expect("scheduler");
    let (volts, current_limit_a) = bench;
    let supply = PowerSupply::Bench {
        volts,
        current_limit_a,
    };
    assert!(sched.set_power_supply("+5V", supply));
    sched
}

/// The rail every 10 us over 3 ms of a 15 V / 3 A bench supply into 60 uF of
/// bulk, marched in frames of 10 us / `per_sample`: `r_load` plus a sink
/// stepped from `sink_a.0` to `sink_a.1` at 100 us.
fn bench_bulk_trace(r_load: &str, sink_a: (f64, f64), per_sample: u32) -> Vec<f64> {
    let mut sink = DeviceId(0);
    let mut sched = bench_board(r_load, (15.0, 3.0), |c, net| {
        c.add(Device::capacitor("Cbulk", net, NodeId::GROUND, 60e-6));
        let dc = SourceKind::Dc(sink_a.0);
        sink = c.add(Device::isource("Isink", net, NodeId::GROUND, dc));
    });
    let frame = 10e-6 / f64::from(per_sample);
    sched.chunk_s = frame;
    let start = std::time::Instant::now();
    let trace = (0..300)
        .map(|k| {
            if k == 10 {
                if let Some(Device::Isource { kind, .. }) =
                    sched.circuit_mut().devices.get_mut(sink.0 as usize)
                {
                    *kind = SourceKind::Dc(sink_a.1);
                }
            }
            for _ in 0..per_sample {
                sched.step(frame);
            }
            sched.net_voltage("+5V").unwrap_or(f64::NAN)
        })
        .collect();
    let rescued = sched.failed_chunk_count() > 0 || !sched.fallback_windows().is_empty();
    assert!(sched.analog_valid() && !rescued, "a chunk needed rescue");
    // Generous for a debug build: the bound catches a frame that opens on a
    // volt step and grinds the timestep down, not a slow machine.
    let took = start.elapsed();
    assert!(took.as_secs() < 30, "{frame:e} s frames took {took:?}");
    trace
}

/// A bench limit into bulk capacitance is resolved INSIDE the solve, so the
/// frame size cannot change the answer. Rows: (load resistor, sink step, the
/// analytic rail `t` seconds after the step). The step overloads 15 V / 3 A,
/// which holds 3 A while the bulk discharges: through 10 ohm the rail settles
/// at the CC operating point 10*(3-2.5) = 5 V with tau = RC; as a pure sink it
/// ramps down at 1 A / 60 uF and floors at 0 V. A once-per-frame fold read the
/// bulk's inrush as load and banged the source between 15 V and 0 V, so the
/// trace moved with the frame (volts apart, negative minima) and every frame
/// opened with a volt step onto 1 mOhm.
#[test]
fn a_bench_limit_into_bulk_settles_identically_at_every_frame_size() {
    type Rail = fn(f64) -> f64;
    let rows: [(&str, (f64, f64), Rail); 2] = [
        ("10", (1.1, 2.5), |t| {
            10.0f64.mul_add((-t / 600e-6).exp(), 5.0)
        }),
        ("100k", (2.6, 4.0), |t| (15.0 - t / 60e-6).max(0.0)),
    ];
    for (r_load, sink_a, rail) in rows {
        let traces = [4, 2, 1].map(|n| (n, bench_bulk_trace(r_load, sink_a, n)));
        for (n, trace) in &traces[1..] {
            let (worst, k) = (trace.iter().zip(&traces[0].1).enumerate())
                .map(|(k, (v, fine))| ((v - fine).abs(), k))
                .fold((0.0, 0), |m, d| if d.0 > m.0 { d } else { m });
            assert!(
                worst < 0.05,
                "R={r_load}: {n} frames per 10 us sit {worst:.4} V from 4 at sample {k}"
            );
        }
        for (k, v) in (1..).zip(&traces[2].1) {
            let want = rail(f64::from(k).mul_add(10e-6, -100e-6).max(0.0));
            assert!(
                (v - want).abs() < 0.15,
                "R={r_load}: rail {v:.4} V {k}0 us in, analytic {want:.4} V"
            );
        }
    }
}
