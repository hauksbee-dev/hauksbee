//! Two-sided gate for the per-chunk FALLBACK INTEGRATION ladder (the closure
//! of the "co-sim chunk whose analog solve fails holds stale voltages"
//! limitation).
//!
//! Side one: a board whose primary chunk march genuinely fails (an
//! integrate-and-fire comparator/SPDT loop marched at a fixed 100 ns step,
//! which has no self-consistent comparator/switch state at the fire step) is
//! RESCUED by a fallback rung, and the rescue is
//! RECORDED: the chunk counts as solved (`analog_valid` stays true, no strict
//! abort), and the window carries the method that produced it plus that
//! method's qualitative fidelity note, in the scheduler record and co-sim JSON.
//!
//! Side two: a board no rung can rescue (the impossible two-sources board)
//! must STILL refuse exactly as before: failed chunks counted, stale windows
//! reported, `analog_valid:false`, strict abort tripped, and NO fallback
//! window invented for it. The ladder shrinks the set of windows the run
//! cannot vouch for; it never manufactures a number for one.

use crate::support::{bound_board, capacitor, impossible_board, resistor, vsource};

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::result::{strict_analog_exit_code, EXIT_INVALID_FOR_ANALYSIS};
use hauksbee_engine::scheduler::{
    ChunkFallbackMethod, FallbackWindow, Scheduler, STRICT_CONSECUTIVE_FAILED_ABORT,
};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind::Dc, SourceKind::Pulse};
use hauksbee_solve::SolverOptions;

/// Every test steps 100 us chunks.
const CHUNK: f64 = 1e-4;

/// A scheduler on `board` stepping [`CHUNK`]s, optionally forced onto the
/// backward-Euler rung.
fn scheduler(board: BoundBoard, opts: SolverOptions, force_be: bool) -> Scheduler {
    let mut sched = Scheduler::new(board, None, opts).expect("build scheduler");
    sched.chunk_s = CHUNK;
    if force_be {
        sched.debug_force_fallback_rung = Some(ChunkFallbackMethod::BackwardEuler);
    }
    sched
}

/// An integrate-and-fire relaxation loop with a resting DC point: a pulsed
/// charge current drives a 1 nF integrator into a hysteretic comparator whose
/// output flips an SPDT switch pair, shorting the integrator through a fast
/// discharge leg (1 ohm switch into 1 ohm, a ~2 ns time constant). The rest
/// state (pulse low) has a clean DC solution, so the chunk's operating point
/// converges; the FIRE inside the chunk is what a fixed 100 ns primary cannot
/// resolve: the discharge is 50x faster than the step, so no comparator/switch
/// state is consistent at the fire step and the march fails at it. The
/// reduced-step rung (100 ns / 16) resolves it, measured before the asserts
/// below were written. A fixed step keeps the witness independent of the
/// adaptive event locator: an adaptive primary used to fail here only because
/// the locator bisected the comparator-driven SPDT controls, which step
/// rail to rail, down to 1e-17 s; it now solves the board.
fn firing_board() -> BoundBoard {
    let mut c = Circuit::new();
    let m = c.node("m");
    let th = c.node("th");
    let spk = c.node("spk");
    let spkb = c.node("spkb");
    let com = c.node("com");
    let rail = c.node("rail");
    c.add(vsource("VTH".into(), th, NodeId::GROUND, Dc(2.5)));
    c.add(vsource("VRAIL".into(), rail, NodeId::GROUND, Dc(1.0)));
    c.add(Device::Isource {
        name: "IIN".into(),
        p: NodeId::GROUND,
        n: m,
        kind: Pulse {
            v1: 0.0,
            v2: 2e-4,
            delay: 10e-6,
            rise: 1e-9,
            fall: 1e-9,
            width: 500e-6,
            period: 1e-3,
        },
    });
    c.add(capacitor("CM".into(), m, NodeId::GROUND, 1e-9));
    c.add(resistor("RL".into(), m, NodeId::GROUND, 1e5));
    c.add(Device::Comparator {
        name: "K1".into(),
        out: spk,
        inp: m,
        inn: th,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.4,
        tpd_s: 0.0,
        supply: hauksbee_ir::Supply::default(),
    });
    c.add(Device::Comparator {
        name: "K2".into(),
        out: spkb,
        inp: th,
        inn: m,
        out_lo: 0.0,
        out_hi: 5.0,
        hysteresis: 0.4,
        tpd_s: 0.0,
        supply: hauksbee_ir::Supply::default(),
    });
    c.add(Device::VSwitch {
        name: "GATE_s1".into(),
        a: com,
        b: m,
        ctrl_p: spk,
        ctrl_n: NodeId::GROUND,
        von: 3.0,
        voff: 2.0,
        ron: 1.0,
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
        ron: 1.0,
        roff: 1e9,
    });
    c.add(resistor("RD".into(), com, NodeId::GROUND, 1.0));
    bound_board("firing", c, &[("m", m)])
}

#[test]
fn failed_primary_is_rescued_by_a_recorded_fallback() {
    let mut sched = scheduler(firing_board(), SolverOptions::fixed(1e-7), false);

    sched.step(CHUNK);

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
    // if a future solver change lets the primary carry this board, the board
    // should be re-measured and rebuilt consciously, not the assert loosened.
    assert_eq!(sched.fallback_chunk_count(), 1);
    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "one rescued chunk, one window: {windows:?}"
    );
    let window = windows[0];
    assert!(
        window.start_s.abs() < 1e-12 && (window.end_s - CHUNK).abs() < CHUNK * 1e-6,
        "the fallback window covers the rescued chunk, got {windows:?}"
    );
    assert_eq!(
        window.method,
        ChunkFallbackMethod::ReducedStep,
        "the firing board is carried by the reduced-step rung"
    );
    assert!(
        window
            .method
            .fidelity_note()
            .contains("smaller maximum step")
            && window.method.fidelity_note().contains("error_estimate_v"),
        "the recorded method states the known trade-off and points at the \
         measured estimate field"
    );
    // B12: the window carries a MEASURED error estimate, not a disclaimer.
    // The firing board's rescue must produce one (measured 0.030 V), and it must
    // be a sane voltage for a 5 V-scale board: positive (the tolerance floor
    // alone guarantees that) and far below the signal scale.
    let est = window
        .error_estimate_v
        .expect("the rescued window carries a measured error estimate");
    assert!(
        est > 0.0 && est < 1.0,
        "chunk-end error estimate must be positive and below the signal scale, got {est}"
    );

    // The integrator voltage in the rescued window is a real number, not a
    // stale hold: the rest integrator is ~0 V pre-pulse, and the chunk end sits
    // mid-oscillation well above it (measured 2.70 V; asserted loosely, the
    // point is that the fallback PRODUCED a solved trajectory).
    let m = sched.net_voltage("m").expect("membrane net exists");
    assert!(
        m > 0.5,
        "the rescued chunk carries a solved membrane voltage, got {m}"
    );
}

#[test]
fn unrescuable_board_still_refuses_loudly() {
    let mut sched = scheduler(impossible_board(), SolverOptions::default(), false);

    for _ in 0..STRICT_CONSECUTIVE_FAILED_ABORT {
        sched.step(CHUNK);
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
/// `omega_0` = 1e5 rad/s (Q = 5), so the capacitor RINGS through the chunk and
/// a dissipative fallback rung accumulates a real, analytically checkable
/// output error.
fn rlc_ringing_board() -> BoundBoard {
    let mut c = Circuit::new();
    let vin = c.node("vin");
    let mid = c.node("mid");
    let vc = c.node("vc");
    c.add(vsource(
        "V1".into(),
        vin,
        NodeId::GROUND,
        Pulse {
            v1: 0.0,
            v2: RLC_V0,
            delay: RLC_DELAY_S,
            rise: 1e-9,
            fall: 1e-9,
            width: 1.0,
            period: 2.0,
        },
    ));
    c.add(resistor("R1".into(), vin, mid, RLC_R));
    c.add(Device::inductor("L1", mid, vc, RLC_L));
    c.add(capacitor("C1".into(), vc, NodeId::GROUND, RLC_C));
    bound_board("rlc-ringing", c, &[("vc", vc)])
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
    let ring = (alpha / wd).mul_add((wd * tau).sin(), (wd * tau).cos());
    RLC_V0 * (-alpha * tau).exp().mul_add(-ring, 1.0)
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
    let mut sched = scheduler(rlc_ringing_board(), SolverOptions::default(), true);

    sched.step(CHUNK);

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
    let analytic = rlc_analytic_vc(CHUNK);
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
    c.add(vsource("V1".into(), vin, NodeId::GROUND, Dc(5.0)));
    c.add(resistor("R1".into(), vin, out, 1e3));
    c.add(capacitor("C1".into(), out, NodeId::GROUND, 100e-9));
    let board = bound_board("rc-smooth", c, &[("out", out)]);

    let mut sched = scheduler(board, SolverOptions::default(), true);

    sched.step(CHUNK);

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
    let mut sched = scheduler(rlc_ringing_board(), SolverOptions::default(), true);
    sched.debug_zero_fallback_error_estimate = true;

    sched.step(CHUNK);

    let windows = sched.fallback_windows();
    assert_eq!(windows.len(), 1);
    let est = windows[0]
        .error_estimate_v
        .expect("the broken hook still records an estimate (that is the point)");
    assert_eq!(est.to_bits(), 0.0_f64.to_bits(), "tamper hook zeroes it");
    let reported = sched.net_voltage("vc").expect("vc net exists");
    let analytic = rlc_analytic_vc(CHUNK);
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
    let mut sched = scheduler(rlc_ringing_board(), SolverOptions::default(), true);
    sched.debug_skip_refined_companion = true;

    sched.step(CHUNK);

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
    let analytic = rlc_analytic_vc(CHUNK);
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
    let mut sched = scheduler(rlc_ringing_board(), SolverOptions::default(), true);

    sched.step(CHUNK);
    let first = sched.fallback_windows()[0]
        .error_estimate_v
        .expect("first chunk carries an estimate");

    sched.step(CHUNK);
    let windows = sched.fallback_windows();
    assert_eq!(
        windows.len(),
        1,
        "same-rung consecutive chunks merge into one window: {windows:?}"
    );
    let window = windows[0];
    assert!(
        2.0f64.mul_add(-CHUNK, window.end_s).abs() < CHUNK * 1e-6,
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

/// A 5 V step at 1 us charging 10 nF through 10 kOhm (a 100 us time
/// constant), bound with its cap net.
fn charging_board() -> BoundBoard {
    let mut c = Circuit::new();
    let (src, cap) = (c.node("src"), c.node("cap"));
    c.add(vsource(
        "VSTEP".into(),
        src,
        NodeId::GROUND,
        Pulse {
            v1: 0.0,
            v2: 5.0,
            delay: 1e-6,
            rise: 1e-9,
            fall: 1e-9,
            width: 1.0,
            period: 2.0,
        },
    ));
    c.add(resistor("R".into(), src, cap, 10e3));
    c.add(capacitor("C".into(), cap, NodeId::GROUND, 10e-9));
    bound_board("step", c, &[("cap", cap)])
}

/// The one fallback window after a primary first chunk and a second chunk
/// forced onto `rung`, with the cap voltage the first chunk carried.
fn forced_second_chunk(rung: ChunkFallbackMethod) -> (FallbackWindow, f64) {
    let mut sched = scheduler(charging_board(), SolverOptions::default(), false);
    sched.step(CHUNK);
    let carried = sched.net_voltage("cap").expect("cap sampled");
    sched.debug_force_fallback_rung = Some(rung);
    sched.step(CHUNK);
    let windows = sched.fallback_windows();
    assert_eq!(windows.len(), 1, "{windows:?}");
    assert_eq!(windows[0].method, rung);
    (windows[0], carried)
}

/// The cold-start rung re-derives its chunk from a fresh operating point,
/// discarding the state the previous chunk carried; its window says how far
/// the trajectory jumped there.
#[test]
fn cold_start_window_states_its_jump_from_the_carried_state() {
    let (window, carried) = forced_second_chunk(ChunkFallbackMethod::ColdStartBackwardEuler);
    assert!(
        carried > 1.0 && carried < 4.0,
        "the cap is partway up: {carried:.3} V"
    );
    let jump = window.state_jump_v.expect("a cold start states its jump");
    assert!(
        jump > 0.5,
        "the cap's {carried:.3} V carried state is discarded: jump {jump:.3} V"
    );
}

/// A rung that continues the carried state has no jump to state.
#[test]
fn continuing_rung_window_states_no_jump() {
    let (window, _) = forced_second_chunk(ChunkFallbackMethod::BackwardEuler);
    assert_eq!(window.state_jump_v, None);
}
