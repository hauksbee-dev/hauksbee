//! Regression gate for the silent-hold defect (05 §3b, "refuse rather than
//! fake" applied to chunks). Before the fix, a chunk whose analog solve did not
//! converge silently held the previous chunk's node voltages, so a diverged
//! co-sim looked quiet and could report a fake-green run. The scheduler now:
//!
//!   * counts every non-convergent chunk and records its sim-time window,
//!   * reports `analog_valid == false` (surfaced as `analog_valid:false` in the
//!     co-sim JSON, with the failed windows),
//!   * trips a strict/CI abort once the solve fails `STRICT_CONSECUTIVE_FAILED_ABORT`
//!     chunks in a row, mapping to exit code `EXIT_INVALID_FOR_ANALYSIS` (3).
//!
//! Gate name in `docs/dev-plans/08-validation-and-test-campaign.md` §2.

use std::collections::HashMap;

use hauksbee_engine::binder::BoundBoard;
use hauksbee_engine::report::BindReport;
use hauksbee_engine::result::{
    strict_analog_exit_code, CosimFailedWindow, CosimJson, EXIT_INVALID_FOR_ANALYSIS,
};
use hauksbee_engine::scheduler::{Scheduler, STRICT_CONSECUTIVE_FAILED_ABORT};
use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::SolverOptions;

/// Build a board whose single chunk solve cannot converge. The forcing is an
/// *impossible boundary*: two ideal voltage sources drive the same node to two
/// different, contradictory voltages (5 V and 3 V) with no series impedance
/// between them. That is a structurally singular MNA system with no DC operating
/// point, so the transient's DC solve fails every chunk regardless of warm-start
/// or gmin stepping (gmin regularizes floating nodes, not conflicting hard
/// sources). No firmware and no MCU are needed: the scheduler still runs the
/// analog solve each chunk, which is the code path under test.
fn impossible_board() -> BoundBoard {
    let mut circuit = Circuit::new();
    let n1 = circuit.node("n1");
    // Two conflicting ideal sources on n1 -> GROUND: 5 V and 3 V at once.
    circuit.add(Device::Vsource {
        name: "V1".to_string(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    circuit.add(Device::Vsource {
        name: "V2".to_string(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(3.0),
    });

    let mut net_nodes = HashMap::new();
    net_nodes.insert("n1".to_string(), n1);

    BoundBoard {
        name: "impossible".to_string(),
        circuit,
        net_nodes,
        net_names: vec!["n1".to_string()],
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

/// Mirror the CLI's `build_cosim_json` field wiring so the test exercises the
/// real JSON serialization shape (the binary's builder is not reachable from an
/// integration test, but the struct and its serialization live in the library).
fn cosim_json_from(sched: &Scheduler) -> CosimJson {
    CosimJson {
        mcu_ref: "none".to_string(),
        backend: "test".to_string(),
        requested_part: String::new(),
        substituted: false,
        // Unmeasured in this mirror: the wall accounting lives with the CLI's
        // stepping loop, and 0 is the documented "no claim" value.
        wall_s: 0.0,
        realtime_factor: 0.0,
        total_toggles: 0,
        uart_seen: false,
        activity_summary: Vec::new(),
        timing_coverage: sched.timing_coverage(),
        timing_refusals: sched.timing_refusals().to_vec(),
        analog_valid: sched.analog_valid(),
        failed_windows: sched
            .failed_windows()
            .iter()
            .enumerate()
            .map(|(i, &(start_s, end_s))| CosimFailedWindow {
                start_s,
                end_s,
                reason: sched
                    .failed_window_reasons()
                    .get(i)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect(),
        fallback_windows: sched
            .fallback_windows()
            .iter()
            .map(
                |&(start_s, end_s, method)| hauksbee_engine::result::CosimFallbackWindow {
                    start_s,
                    end_s,
                    method: method.as_str().to_string(),
                    accuracy: method.accuracy_note().to_string(),
                },
            )
            .collect(),
        spi_framing: sched
            .spi_framing_modes()
            .into_iter()
            .map(|(bus, mode)| hauksbee_engine::result::CosimSpiFraming {
                bus,
                mode: mode.as_str().to_string(),
            })
            .collect(),
        adc_dropped: sched
            .adc_dropped()
            .into_iter()
            .map(|d| hauksbee_engine::result::CosimAdcDrop {
                mcu_ref: d.mcu_ref,
                channel: d.channel,
                net: d.net,
                parts: d.parts,
            })
            .collect(),
        short_pulses: sched
            .short_pulses()
            .iter()
            .map(|p| hauksbee_engine::result::CosimShortPulse {
                net: p.net.clone(),
                mcu_ref: p.mcu_ref.clone(),
                pin: format!("P{}{}", p.port, p.bit),
                pulse_s: p.pulse_s,
                chunk_s: p.chunk_s,
                parts: p.parts.clone(),
            })
            .collect(),
        driver_contention: sched
            .driver_contentions()
            .iter()
            .map(|c| hauksbee_engine::result::CosimDriverContention {
                net: c.net.clone(),
                mcu_ref: c.mcu_ref.clone(),
                pin: format!("P{}{}", c.port, c.bit),
                parts: c.parts.clone(),
                t_s: c.t_s,
            })
            .collect(),
        unexercised_buses: sched
            .unexercised_buses()
            .iter()
            .map(|b| hauksbee_engine::result::CosimUnexercisedBus {
                id: b.id.clone(),
                bus: b.bus.to_string(),
                controller: b.controller.clone(),
            })
            .collect(),
        watchdog_limitations: sched
            .watchdog_limitations()
            .into_iter()
            .map(
                |(mcu_ref, limitation)| hauksbee_engine::result::CosimWatchdogLimitation {
                    mcu_ref,
                    limitation,
                },
            )
            .collect(),
        watchdog_resets: sched
            .watchdog_resets()
            .into_iter()
            .map(
                |(mcu_ref, resets)| hauksbee_engine::result::CosimWatchdogResets {
                    mcu_ref,
                    resets,
                },
            )
            .collect(),
    }
}

#[test]
fn cosim_failed_chunk_marks_analog_invalid() {
    let mut sched = Scheduler::new(impossible_board(), None, SolverOptions::default())
        .expect("build scheduler for the impossible board");

    // Fix the chunk so the per-chunk accounting is predictable.
    let chunk = 1e-4_f64;
    sched.chunk_s = chunk;

    // Before any run the analog side is trivially valid: no chunk failed yet.
    assert!(sched.analog_valid(), "a fresh run starts analog-valid");
    assert_eq!(sched.failed_chunk_count(), 0);
    assert!(!sched.analog_abort_tripped());

    // Step one chunk: the solve must fail and the counter must rise off zero.
    sched.step(chunk);
    assert_eq!(
        sched.failed_chunk_count(),
        1,
        "the first non-convergent chunk is counted"
    );
    assert!(
        !sched.analog_valid(),
        "one failed chunk already makes the run analog-invalid"
    );

    // Step two more chunks to cross the consecutive-failure abort threshold
    // (three in a row). Counter keeps rising, one chunk per step.
    sched.step(chunk);
    sched.step(chunk);
    assert_eq!(
        sched.failed_chunk_count(),
        u64::from(STRICT_CONSECUTIVE_FAILED_ABORT),
        "each failed chunk increments the per-run counter"
    );

    // The failed chunks are contiguous, so they merge into a single window that
    // spans the whole run [0, 3*chunk).
    let windows = sched.failed_windows();
    assert_eq!(windows.len(), 1, "contiguous failures merge to one window");
    assert!(
        windows[0].0.abs() < 1e-12 && (windows[0].1 - 3.0 * chunk).abs() < chunk * 1e-6,
        "the failed window covers the whole run, got {windows:?}"
    );

    // The strict abort has tripped: a strict headless run or hauksbee-ci must
    // refuse with the invalid-for-analysis exit code rather than complete.
    assert!(
        sched.analog_abort_tripped(),
        "three consecutive failed chunks trip the strict/CI abort"
    );
    assert_eq!(
        strict_analog_exit_code(sched.analog_abort_tripped()),
        Some(EXIT_INVALID_FOR_ANALYSIS),
        "the strict path resolves to exit 3 (EXIT_INVALID_FOR_ANALYSIS)"
    );
    assert_eq!(
        EXIT_INVALID_FOR_ANALYSIS, 3,
        "the invalid-for-analysis code is 3"
    );

    // The co-sim JSON reports analog_valid:false with the failed window, and the
    // shape stays backward-compatible (analog_valid is always present).
    let json = serde_json::to_string(&cosim_json_from(&sched)).expect("serialize cosim json");
    assert!(
        json.contains("\"analog_valid\":false"),
        "co-sim JSON carries analog_valid:false, got {json}"
    );
    assert!(
        json.contains("\"failed_windows\""),
        "co-sim JSON lists the failed windows, got {json}"
    );
}

#[test]
fn cosim_valid_run_reports_analog_valid_true() {
    // A run with no analog failure must report the backward-compatible shape:
    // analog_valid == true and an empty (omitted) failed_windows list.
    let mut circuit = Circuit::new();
    let n1 = circuit.node("n1");
    circuit.add(Device::Vsource {
        name: "V1".to_string(),
        p: n1,
        n: NodeId::GROUND,
        kind: SourceKind::Dc(5.0),
    });
    circuit.add(Device::Resistor {
        name: "R1".to_string(),
        a: n1,
        b: NodeId::GROUND,
        ohms: 1_000.0,
        tc1: None,
    });
    let mut net_nodes = HashMap::new();
    net_nodes.insert("n1".to_string(), n1);
    let board = BoundBoard {
        name: "valid".to_string(),
        circuit,
        net_nodes,
        net_names: vec!["n1".to_string()],
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
    };

    let mut sched =
        Scheduler::new(board, None, SolverOptions::default()).expect("build valid scheduler");
    sched.chunk_s = 1e-4;
    sched.step(5e-4);

    assert!(sched.analog_valid(), "a converging run stays analog-valid");
    assert_eq!(sched.failed_chunk_count(), 0);
    assert!(!sched.analog_abort_tripped());
    assert_eq!(strict_analog_exit_code(sched.analog_abort_tripped()), None);

    let json = serde_json::to_string(&cosim_json_from(&sched)).expect("serialize");
    assert!(json.contains("\"analog_valid\":true"), "valid run: {json}");
    assert!(
        !json.contains("failed_windows"),
        "failed_windows omitted when empty: {json}"
    );
}
