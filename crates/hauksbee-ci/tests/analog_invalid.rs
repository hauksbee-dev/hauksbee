//! Finding 3 gate (05 §3b, "refuse rather than fake" for the CI surface). When
//! the analog co-sim fails a chunk under an assertion's evaluation window, that
//! assertion must be reported INVALID (a distinct outcome, not a pass and not an
//! ordinary fail) and the run must exit 3 (invalid-for-analysis), even when the
//! failures never reach the 3-consecutive strict abort. A tripped abort forces
//! exit 3 on its own.
//!
//! These tests exercise the real `assertions::evaluate` overlap logic and the
//! real `CiResult::exit_code` aggregation over a synthesized `RunOutcome`. An
//! end-to-end diverging board cannot be built through the hauksbee-ci spec
//! surface: a singular analog system needs two ideal voltage sources on one node
//! (as the engine-level `cosim_failed_chunk::impossible_board` does directly),
//! but the spec's `net_drive` is a no-op on a rail that already has a source, its
//! `stimulus` injects through a 50 ohm series resistor (never singular), a
//! `[[supply]]` reconfigures the existing leg rather than adding a second, and a
//! KiCad board file cannot express two ideal rails on one net. So the divergence
//! itself is proven at the scheduler level (`cosim_failed_chunk.rs`), and the
//! CI-layer consequences are proven here at the outcome/evaluation boundary.

use std::collections::HashMap;
use std::time::Duration;

use hauksbee_ci::assertions::evaluate;
use hauksbee_ci::report::CiResult;
use hauksbee_ci::runner::{NetWindow, RunOutcome};
use hauksbee_ci::spec::Spec;

/// Write a spec TOML to a uniquely-named temp file and load it (tests run in
/// parallel, so the file name is per-test).
fn load_spec(name: &str, toml: &str) -> Spec {
    let dir = std::env::temp_dir().join("hauksbee_ci_analog_invalid_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, toml).unwrap();
    Spec::load(&path).expect("spec loads")
}

/// A minimal one-seed outcome carrying a single settled voltage window on `net`
/// (a value that would PASS a `>= 3.0 V` check) plus a caller-supplied analog
/// validity state. Everything else is empty. `sim_ms` sets the run length so the
/// assertion's evaluation window is `[after_ms, sim_ms]`.
fn outcome_with(
    net: &str,
    settled_v: f64,
    sim_ms: f64,
    analog_valid: bool,
    failed_windows: Vec<(f64, f64)>,
    analog_abort: bool,
) -> RunOutcome {
    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    // The `voltage` assertion buckets by (net, after_ms-bits); after_ms defaults
    // to 0.0, so key on the 0.0 threshold.
    windows.insert(
        (net.to_string(), 0.0_f64.to_bits()),
        NetWindow {
            min_v: settled_v,
            max_v: settled_v,
            last_v: settled_v,
            samples: 10,
        },
    );
    RunOutcome {
        seed: 0,
        windows,
        uart: HashMap::new(),
        faults: Vec::new(),
        toggles: HashMap::new(),
        peak_current: HashMap::new(),
        peak_temp_c: HashMap::new(),
        peripherals: HashMap::new(),
        rail_windows: HashMap::new(),
        protection_tripped: HashMap::new(),
        protection_tripped_scoped: HashMap::new(),
        ambient_c: 25.0,
        sim_ms,
        first_reach_ms: HashMap::new(),
        driven_nets: Default::default(),
        drive_direction_observable: false,
        first_fault_ms: None,
        ac: None,
        analog_valid,
        failed_windows,
        analog_abort,
        sampled_values: Vec::new(),
        net_series: HashMap::new(),
    }
}

fn ci_result(results: Vec<hauksbee_ci::assertions::AssertResult>, analog_abort: bool) -> CiResult {
    CiResult {
        spec_name: "t".to_string(),
        board: "b.kicad_pcb".to_string(),
        results,
        seeds: 1,
        elapsed: Duration::from_secs(0),
        analog_abort,
        coverage: None,
    }
}

const VOLTAGE_SPEC: &str = "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"voltage\"\nnet=\"n1\"\nmin=3.0\n";

#[test]
fn assertion_over_failed_window_is_invalid_and_exits_3_without_abort() {
    let spec = load_spec("overlap.toml", VOLTAGE_SPEC);
    // The rail settled at 5 V (would pass >= 3 V), but the solver failed a chunk
    // in [0, 0.5 ms], inside the assertion's [0, 1 ms] window. Two failed chunks
    // is BELOW the 3-consecutive abort, so analog_abort is false.
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0005)], false);
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(r.invalid, "the assertion must be INVALID, not evaluated: {}", r.detail);
    assert!(!r.passed, "an invalid assertion is never counted as passed");
    assert!(
        r.detail.contains("INVALID") && r.detail.to_lowercase().contains("held-stale"),
        "detail must explain the invalidity: {}",
        r.detail
    );

    let result = ci_result(results, false);
    assert!(result.analog_invalid(), "the run is invalid for analysis");
    assert_eq!(
        result.exit_code(),
        3,
        "an INVALID assertion exits 3 even without a consecutive abort"
    );
    // The rendered surfaces name the invalidity distinctly from pass/fail.
    assert!(result.render_human().contains("[INVALID]"), "{}", result.render_human());
    assert!(result.render_junit().contains("<error"), "junit uses <error> for invalid");
    assert!(result.render_github_annotations().contains("INVALID"));
}

#[test]
fn assertion_clear_of_failed_window_passes_normally() {
    let spec = load_spec("clear.toml", VOLTAGE_SPEC);
    // A failed chunk at [2 ms, 3 ms] is AFTER the run's 1 ms end, so it cannot
    // overlap the assertion's [0, 1 ms] window: the assertion evaluates normally.
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.002, 0.003)], false);
    let results = evaluate(&spec, &[out]);
    let r = &results[0];
    assert!(!r.invalid, "a non-overlapping failed window does not invalidate");
    assert!(r.passed, "5 V clears the >= 3 V bound: {}", r.detail);

    let result = ci_result(results, false);
    assert_eq!(result.exit_code(), 0, "a clean, passing assertion exits 0");
}

#[test]
fn tripped_abort_forces_exit_3_even_when_assertions_pass() {
    // A UART assertion is not analog-transient-derived, so it is never marked
    // INVALID by a failed analog chunk, but a tripped strict abort still forces
    // the whole run to refuse (exit 3).
    let spec = load_spec(
        "abort.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\ncontains=\"hi\"\n",
    );
    let mut out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0003)], true);
    out.uart.insert("U1".to_string(), "hi there".to_string());
    let results = evaluate(&spec, &[out]);
    assert!(results[0].passed, "the UART assertion itself passes");
    assert!(!results[0].invalid, "a UART assertion is not analog-invalidated");

    let result = ci_result(results, true);
    assert!(result.passed(), "every assertion passed");
    assert_eq!(
        result.exit_code(),
        3,
        "a tripped analog abort refuses even when assertions pass"
    );
}

#[test]
fn tripped_abort_with_no_invalid_assertion_errors_in_junit_not_all_green() {
    // Same state as `tripped_abort_forces_exit_3_even_when_assertions_pass`: the
    // abort tripped but the only assertion (UART, not analog-derived) passed, so
    // no per-assertion result carries INVALID. The run exits 3 — and the JUnit
    // surface must agree: `render_human` and `render_github_annotations` both
    // special-case this state, so `render_junit` must too, with one synthetic
    // errored testcase instead of a false `failures="0" errors="0"` ALL-GREEN.
    let spec = load_spec(
        "abort_junit.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\ncontains=\"hi\"\n",
    );
    let mut out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0003)], true);
    out.uart.insert("U1".to_string(), "hi there".to_string());
    let results = evaluate(&spec, &[out]);
    assert!(results.iter().all(|r| r.passed && !r.invalid));

    let result = ci_result(results, true);
    assert_eq!(result.exit_code(), 3);
    let xml = result.render_junit();
    assert!(
        xml.contains("errors=\"1\"") && !xml.contains("errors=\"0\""),
        "junit must count the abort as an error, not report all-green: {xml}"
    );
    // 1 real assertion + 1 synthetic abort testcase.
    assert!(xml.contains("tests=\"2\""), "{xml}");
    assert!(
        xml.contains("<error") && xml.contains("INVALID for analysis"),
        "the synthetic errored testcase must be present and explain itself: {xml}"
    );
}

#[test]
fn abort_with_an_invalid_assertion_does_not_double_count_in_junit() {
    // When an assertion already carries the INVALID, the synthetic testcase must
    // NOT be added on top: the error count stays at the per-assertion count.
    let spec = load_spec("abort_junit_dup.toml", VOLTAGE_SPEC);
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0005)], true);
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    assert!(results[0].invalid);

    let result = ci_result(results, true);
    let xml = result.render_junit();
    assert!(xml.contains("tests=\"1\""), "no synthetic testcase added: {xml}");
    assert!(xml.contains("errors=\"1\""), "{xml}");
}
