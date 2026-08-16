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
        peripherals: Vec::new(),
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
    let window = windows[0];
    assert!(
        window.start_s.abs() < 1e-12 && (window.end_s - chunk).abs() < chunk * 1e-6,
        "the fallback window covers the rescued chunk, got {windows:?}"
    );
    assert_eq!(
        window.method,
        ChunkFallbackMethod::BackwardEuler,
        "the firing board is carried by the backward-Euler rung"
    );
    assert!(
        window.method.fidelity_note().contains("first-order")
            && window.method.fidelity_note().contains("error_estimate_v"),
        "the recorded method states the known trade-off and points at the \
         measured estimate field"
    );
    // B12: the window carries a MEASURED error estimate, not a disclaimer.
    // The firing board's rescue must produce one (the refinement companion is
    // the same BE march at a tighter bound, which converges here), and it must
    // be a sane voltage for a 5 V-scale board: positive (the tolerance floor
    // alone guarantees that) and far below the signal scale.
    let est = window
        .error_estimate_v
        .expect("the rescued window carries a measured error estimate");
    assert!(
        est > 0.0 && est < 1.0,
        "chunk-end error estimate must be positive and below the signal scale, got {est}"
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

// --- B12: the measured per-window error estimate, proven two-sided ----------

/// Series RLC (V -> R -> L -> C -> ground), underdamped, stepped 5 us into
/// the chunk: R = 20 ohm, L = 1 mH, C = 100 nF gives alpha = 1e4 1/s,
/// omega_0 = 1e5 rad/s (Q = 5), so the capacitor RINGS through the chunk and
/// a dissipative fallback rung accumulates a real, analytically checkable
/// output error.
fn rlc_ringing_board() -> BoundBoard {
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let mid = c.node("mid");
    let vc = c.node("vc");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Pulse {
            v1: 0.0,
            v2: RLC_V0,
            delay: RLC_DELAY_S,
            rise: 1e-9,
            fall: 1e-9,
            width: 1.0,
            period: 2.0,
        },
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: mid,
        ohms: RLC_R,
        tc1: None,
    });
    c.add(Device::Inductor {
        name: "L1".into(),
        a: mid,
        b: vc,
        henries: RLC_L,
        ic: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: vc,
        b: NodeId::GROUND,
        farads: RLC_C,
        ic: None,
    });
    let vc_id = vc;
    board_from("rlc-ringing", c, &[("vc", vc_id)])
}

const RLC_V0: f64 = 5.0;
const RLC_DELAY_S: f64 = 5e-6;
const RLC_R: f64 = 20.0;
const RLC_L: f64 = 1e-3;
const RLC_C: f64 = 100e-9;

/// The closed-form capacitor voltage of the series RLC step response at
/// `t` seconds after the chunk start (zero initial state, ideal step at
/// `RLC_DELAY_S`).
fn rlc_analytic_vc(t: f64) -> f64 {
    let tau = t - RLC_DELAY_S;
    if tau <= 0.0 {
        return 0.0;
    }
    let alpha = RLC_R / (2.0 * RLC_L);
    let w0 = 1.0 / (RLC_L * RLC_C).sqrt();
    let wd = (w0 * w0 - alpha * alpha).sqrt();
    RLC_V0 * (1.0 - (-alpha * tau).exp() * ((wd * tau).cos() + (alpha / wd) * (wd * tau).sin()))
}

/// Whether a reported chunk-end value with its recorded error estimate
/// BRACKETS the analytic answer. This is the check the two tests below share:
/// the honest-estimator test asserts it holds, the broken-estimator test
/// asserts the same check catches the tampered estimate.
fn estimate_brackets_analytic(reported: f64, estimate_v: f64, analytic: f64) -> bool {
    (reported - analytic).abs() <= estimate_v
}

/// Force the RLC ringing board onto the dissipative backward-Euler rung and
/// check the recorded window's measured error estimate against the CLOSED
/// FORM: the estimate must bracket the analytic chunk-end voltage (it is an
/// error claim about a known-true answer), and it must not be vacuous (a
/// "bound" wider than the whole signal says nothing).
#[test]
fn forced_rlc_fallback_estimate_brackets_the_analytic_answer() {
    let mut sched = Scheduler::new(rlc_ringing_board(), None, SolverOptions::default())
        .expect("build scheduler for the RLC board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;
    sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);

    sched.step(chunk);

    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "one forced chunk, one window: {windows:?}"
    );
    let window = windows[0];
    assert_eq!(window.method, ChunkFallbackMethod::BackwardEuler);
    let est = window
        .error_estimate_v
        .expect("the BE rung's refinement companion converges on this board");
    let reported = sched.net_voltage("vc").expect("vc net exists");
    let analytic = rlc_analytic_vc(chunk);
    assert!(
        estimate_brackets_analytic(reported, est, analytic),
        "estimate must bracket the analytic answer: reported={reported:.6} \
         analytic={analytic:.6} |err|={:.3e} estimate={est:.3e}",
        (reported - analytic).abs()
    );
    assert!(
        est < RLC_V0 / 2.0,
        "a useful estimate is far tighter than the signal itself, got {est:.3e}"
    );
}

/// The same forced rung on a SMOOTH window (a settled RC board: the DC
/// operating point already sits at the source value, nothing moves inside the
/// chunk) must report a SMALL estimate: the two-sidedness that stops the
/// estimator from buying the ringing bracket with a giant blanket number.
#[test]
fn forced_smooth_fallback_reports_a_small_estimate() {
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let out = c.node("out");
    c.add(Device::Vsource {
        name: "V1".into(),
        p: vin,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    c.add(Device::Resistor {
        name: "R1".into(),
        a: vin,
        b: out,
        ohms: 1e3,
        tc1: None,
    });
    c.add(Device::Capacitor {
        name: "C1".into(),
        a: out,
        b: NodeId::GROUND,
        farads: 100e-9,
        ic: None,
    });
    let out_id = out;
    let board = board_from("rc-smooth", c, &[("out", out_id)]);

    let mut sched = Scheduler::new(board, None, SolverOptions::default())
        .expect("build scheduler for the smooth RC board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;
    sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);

    sched.step(chunk);

    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "one forced chunk, one window: {windows:?}"
    );
    let est = windows[0]
        .error_estimate_v
        .expect("the refinement companion converges on a settled RC board");
    assert!(
        est < 2e-2,
        "a smooth settled window must report a small estimate, got {est:.3e}"
    );
}

/// A deliberately BROKEN estimator (the test-only zero hook) must be CAUGHT
/// by the same bracket check the honest test uses: the ringing board's real
/// BE error is nonzero, so a zeroed estimate cannot bracket the analytic
/// answer. This pins that the bracket test above has teeth: an estimator
/// that stops measuring fails it, it does not slip through.
#[test]
fn broken_estimator_is_caught_by_the_bracket_check() {
    let mut sched = Scheduler::new(rlc_ringing_board(), None, SolverOptions::default())
        .expect("build scheduler for the RLC board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;
    sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);
    sched.debug_zero_fallback_error_estimate = true;

    sched.step(chunk);

    let windows = sched.fallback_windows();
    assert_eq!(windows.len(), 1);
    let est = windows[0]
        .error_estimate_v
        .expect("the broken hook still records an estimate (that is the point)");
    assert_eq!(est, 0.0, "the tamper hook zeroes the estimate");
    let reported = sched.net_voltage("vc").expect("vc net exists");
    let analytic = rlc_analytic_vc(chunk);
    assert!(
        !estimate_brackets_analytic(reported, est, analytic),
        "the bracket check must CATCH a zeroed estimator: reported={reported:.6} \
         analytic={analytic:.6}"
    );
}

/// The COARSE companion leg (the one a stiff board falls to when the tight
/// companion will not converge) must be sound on its own: skip the refined
/// leg via the test hook and require the coarse-leg estimate to bracket the
/// closed form too. This is the leg whose Richardson factor was measured
/// unsound at the nominal ratio-4 assumption; the worst-case factor is pinned
/// here against the analytic answer.
#[test]
fn coarse_companion_estimate_still_brackets_the_analytic_answer() {
    let mut sched = Scheduler::new(rlc_ringing_board(), None, SolverOptions::default())
        .expect("build scheduler for the RLC board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;
    sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);
    sched.debug_skip_refined_companion = true;

    sched.step(chunk);

    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "one forced chunk, one window: {windows:?}"
    );
    let est = windows[0]
        .error_estimate_v
        .expect("the coarse companion converges on this board");
    let reported = sched.net_voltage("vc").expect("vc net exists");
    let analytic = rlc_analytic_vc(chunk);
    assert!(
        estimate_brackets_analytic(reported, est, analytic),
        "coarse-leg estimate must bracket the analytic answer: reported={reported:.6} \
         analytic={analytic:.6} |err|={:.3e} estimate={est:.3e}",
        (reported - analytic).abs()
    );
    assert!(
        est < RLC_V0,
        "a useful coarse-leg estimate is still tighter than the signal, got {est:.3e}"
    );
}

/// Two consecutive chunks rescued by the SAME rung merge into one window, and
/// the merged record keeps the WORST per-chunk estimate (never silently the
/// last one, never a sum).
#[test]
fn merged_window_keeps_the_worst_per_chunk_estimate() {
    let mut sched = Scheduler::new(rlc_ringing_board(), None, SolverOptions::default())
        .expect("build scheduler for the RLC board");
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;
    sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);

    sched.step(chunk);
    let first = sched.fallback_windows()[0]
        .error_estimate_v
        .expect("first chunk carries an estimate");

    sched.step(chunk);
    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "same-rung consecutive chunks merge into one window: {windows:?}"
    );
    let window = windows[0];
    assert!(
        (window.end_s - 2.0 * chunk).abs() < chunk * 1e-6,
        "the merged window spans both chunks, got {windows:?}"
    );
    let merged = window
        .error_estimate_v
        .expect("the merged window keeps a measured estimate");
    assert!(
        merged >= first,
        "the merged estimate is the worst of its chunks (max), so it can never \
         drop below an already-recorded chunk: first={first:.3e} merged={merged:.3e}"
    );
}
