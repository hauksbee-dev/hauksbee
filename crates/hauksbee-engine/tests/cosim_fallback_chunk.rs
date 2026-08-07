//! Two-sided gate for the per-chunk FALLBACK INTEGRATION ladder (the closure
//! of the "co-sim chunk whose analog solve fails holds stale voltages"
//! limitation).
//!
//! Side one: a board whose primary chunk march genuinely fails (an
//! integrate-and-fire comparator/SPDT loop whose fire step kills the bare
//! Newton; measured: `Newton failed at t=2.45e-5 even at dt_min=1e-18` under
//! the default options) is RESCUED by a fallback rung, and the rescue is
//! RECORDED: the chunk counts as solved (`analog_valid` stays true, no strict
//! abort), and the window carries the method that produced it plus that
//! method's qualitative fidelity note, in the scheduler record and co-sim JSON.
//!
//! Side two: a board no rung can rescue (the impossible two-sources board)
//! must STILL refuse exactly as before: failed chunks counted, stale windows
//! reported, `analog_valid:false`, strict abort tripped, and NO fallback
//! window invented for it. The ladder shrinks the set of windows the run
//! cannot vouch for; it never manufactures a number for one.

use std::collections::HashMap;

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::report::BindReport;
use hauksbee_engine::result::{strict_analog_exit_code, EXIT_INVALID_FOR_ANALYSIS};
use hauksbee_engine::scheduler::{ChunkFallbackMethod, Scheduler, STRICT_CONSECUTIVE_FAILED_ABORT};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::SolverOptions;

fn board_from(name: &str, circuit: Circuit, nets: &[(&str, NodeId)]) -> BoundBoard {
    let mut net_nodes = HashMap::new();
    let mut net_names = Vec::new();
    for &(n, id) in nets {
        net_nodes.insert(n.to_string(), id);
        net_names.push(n.to_string());
    }
    BoundBoard {
        name: name.to_string(),
        circuit,
        net_nodes,
        net_names,
        digital: Vec::new(),
        mcus: Vec::new(),
        dnp_mcus: Vec::new(),
        component_kinds: HashMap::new(),
        input_sources: HashMap::new(),
        supplies: Vec::new(),
        behavioral: Vec::new(),
        device_meta: Vec::new(),
        dacs: Vec::new(),
        report: BindReport::default(),
    }
}

/// An integrate-and-fire relaxation loop with a resting DC point: a pulsed
/// charge current drives a 1 nF membrane into a hysteretic comparator whose
/// output flips an SPDT switch pair, shorting the membrane through a fast
/// discharge leg. The rest state (pulse low) has a clean DC solution, so the
/// chunk's operating point converges; the FIRE inside the chunk is what the
/// bare per-step Newton cannot resolve (the comparator/switch flip), which
/// makes this the primary-fails / fallback-succeeds witness. Backward Euler
/// at the bounded step carries it (the L-stable damping keeps the post-flip
/// discharge integrable), measured before the assert below was written.
fn firing_board() -> BoundBoard {
    let mut c = Circuit::new();
    let m = c.node("m");
    let th = c.node("th");
    let spk = c.node("spk");
    let spkb = c.node("spkb");
    let com = c.node("com");
    let rail = c.node("rail");
    c.add(Device::Vsource {
        name: "VTH".into(),
        p: th,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(2.5),
    });
    c.add(Device::Vsource {
        name: "VRAIL".into(),
        p: rail,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(1.0),
    });
    c.add(Device::Isource {
        name: "IIN".into(),
        p: NodeId::GROUND,
        n: m,
        kind: SourceKind::Pulse {
            v1: 0.0,
            v2: 2e-4,
            delay: 10e-6,
            rise: 1e-9,
            fall: 1e-9,
            width: 500e-6,
            period: 1e-3,
        },
    });
    c.add(Device::Capacitor {
        name: "CM".into(),
        a: m,
        b: NodeId::GROUND,
        farads: 1e-9,
        ic: None,
    });
    c.add(Device::Resistor {
        name: "RL".into(),
        a: m,
        b: NodeId::GROUND,
        ohms: 1e5,
        tc1: None,
    });
    c.add(Device::Comparator {
        name: "K1".into(),
        out: spk,
        inp: m,
        inn: th,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    c.add(Device::Comparator {
        name: "K2".into(),
        out: spkb,
        inp: th,
        inn: m,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.2,
    });
    c.add(Device::VSwitch {
        name: "GATE_s1".into(),
        a: com,
        b: m,
        ctrl_p: spk,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    c.add(Device::VSwitch {
        name: "GATE_s0".into(),
        a: com,
        b: rail,
        ctrl_p: spkb,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 10.0,
        roff: 1e9,
    });
    c.add(Device::Resistor {
        name: "RD".into(),
        a: com,
        b: NodeId::GROUND,
        ohms: 50.0,
        tc1: None,
    });
    let m_id = m;
    board_from("firing", c, &[("m", m_id)])
}

/// Two ideal sources driving one node to contradictory voltages: structurally
/// singular, no rung of any ladder can solve it (same board as the
/// failed-chunk gate).
fn impossible_board() -> BoundBoard {
    let mut c = Circuit::new();
    let n1 = c.node("n1");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    c.add(Device::Vsource {
        name: "V2".into(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.0),
    });
    board_from("impossible", c, &[("n1", n1)])
}

#[test]
fn failed_primary_is_rescued_by_a_recorded_fallback() {
    let mut sched = Scheduler::new(firing_board(), None, SolverOptions::default())
        .expect("build scheduler for the firing board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;

    sched.step(chunk);

    // The chunk is SOLVED: no failed chunk, no stale window, analog stays
    // valid, and the strict abort is untouched.
    assert_eq!(
        sched.failed_chunk_count(),
        0,
        "a fallback-rescued chunk is a solved chunk, not a failed one"
    );
    assert!(sched.failed_windows().is_empty());
    assert!(sched.analog_valid(), "a rescued run stays analog-valid");
    assert!(!sched.analog_abort_tripped());

    // ...and the rescue is RECORDED: exactly one fallback window covering the
    // chunk, naming the rung that produced it. The rung is asserted exactly:
    // if a future solver change lets an earlier (more accurate) rung carry
    // this board, this assert should be re-pointed consciously, not loosened.
    assert_eq!(sched.fallback_chunk_count(), 1);
    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "one rescued chunk, one window: {windows:?}"
    );
    let (start, end, method) = windows[0];
    assert!(
        start.abs() < 1e-12 && (end - chunk).abs() < chunk * 1e-6,
        "the fallback window covers the rescued chunk, got {windows:?}"
    );
    assert_eq!(
        method,
        ChunkFallbackMethod::BackwardEuler,
        "the firing board is carried by the backward-Euler rung"
    );
    assert!(
        method.fidelity_note().contains("first-order")
            && method
                .fidelity_note()
                .contains("no empirical output-error bound"),
        "the recorded method states the known trade-off without inventing a bound"
    );

    // The membrane voltage in the rescued window is a real number, not a
    // stale hold: the rest membrane is ~0 V pre-pulse, and the chunk end sits
    // mid-oscillation well above it (measured 2.65 V; asserted loosely, the
    // point is that the fallback PRODUCED a solved trajectory).
    let m = sched.net_voltage("m").expect("membrane net exists");
    assert!(
        m > 0.5,
        "the rescued chunk carries a solved membrane voltage, got {m}"
    );
}

#[test]
fn unrescuable_board_still_refuses_loudly() {
    let mut sched = Scheduler::new(impossible_board(), None, SolverOptions::default())
        .expect("build scheduler for the impossible board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;

    for _ in 0..STRICT_CONSECUTIVE_FAILED_ABORT {
        sched.step(chunk);
    }

    // No rung can solve a structurally singular board, and none may pretend
    // to: zero fallback windows, every chunk failed, the stale windows
    // reported, and the strict abort tripped exactly as before the ladder
    // existed.
    assert_eq!(
        sched.fallback_chunk_count(),
        0,
        "no fallback may be recorded for a chunk nothing solved"
    );
    assert!(sched.fallback_windows().is_empty());
    assert_eq!(
        sched.failed_chunk_count(),
        u64::from(STRICT_CONSECUTIVE_FAILED_ABORT)
    );
    assert!(!sched.analog_valid());
    assert_eq!(sched.failed_windows().len(), 1);
    assert!(sched.analog_abort_tripped());
    assert_eq!(
        strict_analog_exit_code(sched.analog_abort_tripped()),
        Some(EXIT_INVALID_FOR_ANALYSIS)
    );
}
