//! Fresh cases for the suppressed rail and the CC-fold floor (E22-adjacent).
//!
//! Both defects were the same shape: a rail that should read one thing reading
//! another with nothing said. A rail with its supply removed must read ~0 V, not
//! hold a stale value. And a draw just barely over a current limit must settle
//! at the limit with the rail still positive, not collapse to zero or bang-bang,
//! because 1% over is where a fold that anchors to the wrong setpoint shows up.

use hauksbee_engine::binder::{bind_board, BoundBoard};
use hauksbee_engine::power_supply::PowerSupply;
use hauksbee_engine::scheduler::Scheduler;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::Device;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::SolverOptions;

/// One resistor from +5V to GND, load value substituted in.
fn load_board(rload_ohms: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value {rload_ohms} (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#
    )
}

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
    let mut baseline = Scheduler::new(
        bound_load_board("1k"),
        None,
        SolverOptions::default(),
    )
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
    // 4.95 V, not collapse it: an input-limit fold that scales the FRESH setpoint
    // instead of anchoring to the previous command bang-bangs here forever.
    let limit = 0.5_f64;
    let r_load = 5.0 / (1.01 * limit);
    let text = load_board(&format!("{r_load:.6}"));
    let mut engine = HauksbeeEngine::from_board_file(&text, None, "/boards/load.kicad_pcb")
        .expect("build engine");
    assert!(
        engine.scheduler_mut().set_power_supply(
            "+5V",
            PowerSupply::Bench {
                volts: 5.0,
                current_limit_a: limit,
            },
        ),
        "bench supply applied to +5V"
    );

    use hauksbee_server::engine::Engine;
    let mut v = 0.0;
    let mut history = Vec::new();
    for _ in 0..400 {
        let frame = engine.step(1e-4);
        v = *frame.net_voltages.get("+5V").unwrap_or(&0.0);
        history.push(v);
    }
    let i = engine
        .scheduler()
        .supply_states()
        .get("+5V")
        .map(|(_, i, _)| *i)
        .unwrap_or(0.0);

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

/// Two-sided: a draw comfortably UNDER the limit must not fold at all.
#[test]
fn a_draw_under_the_limit_does_not_fold() {
    let text = load_board("100");
    let mut engine = HauksbeeEngine::from_board_file(&text, None, "/boards/load.kicad_pcb")
        .expect("build engine");
    engine.scheduler_mut().set_power_supply(
        "+5V",
        PowerSupply::Bench {
            volts: 5.0,
            current_limit_a: 0.5,
        },
    );
    use hauksbee_server::engine::Engine;
    let mut v = 0.0;
    for _ in 0..200 {
        v = *engine
            .step(1e-4)
            .net_voltages
            .get("+5V")
            .unwrap_or(&0.0);
    }
    assert!(
        (v - 5.0).abs() < 0.05,
        "50 mA against a 500 mA limit must not fold the rail, got {v:.4} V"
    );
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
    assert_eq!(bound.circuit.devices.len(), before, "the board is untouched");
    assert!(
        bound.supplies.iter().any(|s| s.net_name == "+5V"),
        "the real rail must survive an unrelated suppression"
    );
}
